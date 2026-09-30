//! A short-lived supervisor owns each bounded compiler process tree.
//!
//! Its stdin is a lifetime pipe held only by the project coordinator. EOF also
//! cancels work when that coordinator is SIGKILLed; no signal handler in the
//! coordinator is needed for that case. The supervisor has its own process group.
use std::{
  env, fs,
  io::{self, Read, Write},
  os::unix::process::CommandExt,
  path::PathBuf,
  process::{Child, Command, ExitCode, Stdio},
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
  },
  thread,
  time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

const OUTPUT_LIMIT: usize = 32 * 1024 * 1024;
const DIAGNOSTIC_LIMIT: usize = 1024 * 1024;
pub(crate) static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn interrupted(_: libc::c_int) {
  INTERRUPTED.store(true, Ordering::Relaxed);
}

pub(crate) fn handle_signals() {
  unsafe {
    libc::signal(libc::SIGINT, interrupted as *const () as libc::sighandler_t);
    libc::signal(
      libc::SIGTERM,
      interrupted as *const () as libc::sighandler_t,
    );
  }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Request {
  pub unit: String,
  pub args: Vec<String>,
  pub memory_mib: u64,
  pub timeout_seconds: u64,
  pub result_protocol: bool,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Outcome {
  pub status: String,
  pub exit_code: Option<i32>,
  pub stdout: String,
  pub stderr: String,
  pub diagnostics_truncated: bool,
  pub elapsed_seconds: f64,
  pub peak_memory_bytes: Option<u64>,
  pub oom_kills: u64,
}

fn limited_reader(
  mut reader: impl Read,
  limit: usize,
  overflow: &AtomicBool,
) -> io::Result<Vec<u8>> {
  let mut bytes = Vec::new();
  let mut chunk = [0; 16 * 1024];
  loop {
    let count = reader.read(&mut chunk)?;
    if count == 0 {
      break;
    }
    let available = limit.saturating_sub(bytes.len());
    bytes.extend_from_slice(&chunk[.. count.min(available)]);
    if count > available {
      overflow.store(true, Ordering::Relaxed);
    }
  }
  Ok(bytes)
}

/// Control-plane commands cannot themselves hold cancellation hostage.
fn control(arguments: &[&str]) -> Option<Vec<u8>> {
  let mut child = Command::new("systemctl")
    .arg("--user")
    .args(arguments)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .spawn()
    .ok()?;
  let start = Instant::now();
  loop {
    match child.try_wait() {
      Ok(Some(status)) => {
        let mut output = Vec::new();
        child
          .stdout
          .take()?
          .take(16 * 1024)
          .read_to_end(&mut output)
          .ok()?;
        return status.success().then_some(output);
      }
      Ok(None) if start.elapsed() < Duration::from_secs(2) => {
        thread::sleep(Duration::from_millis(10))
      }
      _ => {
        let _ = child.kill();
        let _ = child.wait();
        return None;
      }
    }
  }
}

struct Worker {
  child: Child,
  unit: String,
  cgroup: Option<PathBuf>,
  peak: Option<u64>,
  oom: u64,
  stopped: bool,
}

impl Worker {
  fn sample(&mut self) {
    if let Some(path) = &self.cgroup {
      if let Some(peak) = fs::read_to_string(path.join("memory.peak"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
      {
        self.peak = Some(self.peak.unwrap_or(0).max(peak));
      }
      if let Ok(events) = fs::read_to_string(path.join("memory.events")) {
        if let Some(value) = events
          .lines()
          .find_map(|line| line.strip_prefix("oom_kill "))
          .and_then(|s| s.parse::<u64>().ok())
        {
          self.oom = self.oom.max(value);
        }
      }
    }
  }

  fn discover_group(&mut self) {
    if self.cgroup.is_some() {
      return;
    }
    if let Some(output) =
      control(&["show", "--property=ControlGroup", "--value", &self.unit])
    {
      let name = String::from_utf8_lossy(&output);
      let name = name.trim();
      if name.starts_with('/') && !name.contains("..") {
        self.cgroup =
          Some(PathBuf::from("/sys/fs/cgroup").join(name.trim_start_matches('/')));
      }
    }
  }

  fn stop(&mut self) {
    if self.stopped {
      return;
    }
    self.stopped = true;
    self.sample();
    // cgroup membership catches children which change process group/session.
    let _ = control(&["kill", "--signal=KILL", "--kill-whom=all", &self.unit]);
    // Also cover systemd-run itself and failures before scope registration.
    if matches!(self.child.try_wait(), Ok(None)) {
      unsafe {
        libc::kill(-(self.child.id() as i32), libc::SIGKILL);
      }
      let _ = self.child.kill();
    }
    let _ = self.child.wait();
    let _ = control(&["stop", "--no-block", &self.unit]);
  }
}

impl Drop for Worker {
  fn drop(&mut self) {
    self.stop();
  }
}

fn supervise(request: Request) -> anyhow::Result<Outcome> {
  anyhow::ensure!(
    request.unit.starts_with("flowistry-body-")
      && request.unit.ends_with(".scope")
      && request
        .unit
        .bytes()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.')),
    "invalid worker unit name"
  );
  anyhow::ensure!(
    request.memory_mib > 0 && request.memory_mib <= u64::MAX / (1024 * 1024),
    "invalid worker memory budget"
  );
  anyhow::ensure!(request.timeout_seconds > 0, "invalid worker timeout");
  let canceled = Arc::new(AtomicBool::new(false));
  let lifetime = canceled.clone();
  thread::spawn(move || {
    let mut bytes = [0; 128];
    loop {
      match io::stdin().read(&mut bytes) {
        Ok(0) | Err(_) => {
          lifetime.store(true, Ordering::Relaxed);
          break;
        }
        Ok(_) => {}
      }
    }
  });
  let unit = request.unit;
  let mut command = Command::new("systemd-run");
  command.args([
    "--user",
    "--scope",
    "--quiet",
    "--collect",
    "--unit",
    &unit,
    "-p",
    &format!("MemoryMax={}", request.memory_mib * 1024 * 1024),
    "-p",
    "MemorySwapMax=0",
    "-p",
    "OOMPolicy=continue",
  ]);
  // A fresh unit's invocation ID must not invent a different semantic cache
  // environment for every body. Preserve precisely the caller's prior state.
  command.arg("env");
  if let Some(id) = env::var_os("INVOCATION_ID") {
    let mut value = std::ffi::OsString::from("INVOCATION_ID=");
    value.push(id);
    command.arg(value);
  } else {
    command.args(["-u", "INVOCATION_ID"]);
  }
  command
    .arg(env::current_exe()?)
    .args(&request.args)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .process_group(0);
  if request.result_protocol {
    command.env("FLOWISTRY_RESULT_PROTOCOL", "1");
  } else {
    command.env_remove("FLOWISTRY_RESULT_PROTOCOL");
  }
  let start = Instant::now();
  let child = command.spawn()?;
  let mut worker = Worker {
    child,
    unit,
    cgroup: None,
    peak: None,
    oom: 0,
    stopped: false,
  };
  let stdout_overflow = Arc::new(AtomicBool::new(false));
  let stderr_overflow = Arc::new(AtomicBool::new(false));
  let stdout = worker.child.stdout.take().unwrap();
  let stderr = worker.child.stderr.take().unwrap();
  let out_flag = stdout_overflow.clone();
  let err_flag = stderr_overflow.clone();
  let out_reader = thread::spawn(move || limited_reader(stdout, OUTPUT_LIMIT, &out_flag));
  let err_reader =
    thread::spawn(move || limited_reader(stderr, DIAGNOSTIC_LIMIT, &err_flag));
  let mut reason = "exited";
  let mut next_discovery = Instant::now();
  let status = loop {
    if canceled.load(Ordering::Relaxed) || INTERRUPTED.load(Ordering::Relaxed) {
      reason = "canceled";
      break None;
    }
    if stdout_overflow.load(Ordering::Relaxed) {
      reason = "output_limit";
      break None;
    }
    if start.elapsed() >= Duration::from_secs(request.timeout_seconds) {
      reason = "timeout";
      break None;
    }
    if worker.cgroup.is_none() && Instant::now() >= next_discovery {
      worker.discover_group();
      next_discovery = Instant::now() + Duration::from_millis(100);
    }
    worker.sample();
    if let Some(status) = worker.child.try_wait()? {
      break Some(status);
    }
    thread::sleep(Duration::from_millis(20));
  };
  worker.stop();
  // Closing all descendants' descriptors makes these joins bounded even when a
  // build script leaves a subprocess behind after its Cargo parent has exited.
  let stdout = out_reader
    .join()
    .map_err(|_| anyhow::anyhow!("stdout reader panicked"))??;
  let stderr = err_reader
    .join()
    .map_err(|_| anyhow::anyhow!("stderr reader panicked"))??;
  if reason == "exited" && worker.oom > 0 {
    reason = "oom";
  }
  if reason == "exited" && stdout_overflow.load(Ordering::Relaxed) {
    reason = "output_limit";
  }
  Ok(Outcome {
    status: reason.into(),
    exit_code: status.and_then(|s| s.code()),
    stdout: String::from_utf8(stdout)?,
    stderr: String::from_utf8_lossy(&stderr).into_owned(),
    diagnostics_truncated: stderr_overflow.load(Ordering::Relaxed),
    elapsed_seconds: start.elapsed().as_secs_f64(),
    peak_memory_bytes: worker.peak,
    oom_kills: worker.oom,
  })
}

/// Intercept the private supervisor before normal Cargo/plugin argument parsing.
pub(crate) fn internal() -> Option<ExitCode> {
  let mut args = env::args().skip(1);
  if args.next().as_deref() != Some("--flowistry-project-worker") {
    return None;
  }
  handle_signals();
  let result = (|| -> anyhow::Result<Outcome> {
    let request = serde_json::from_str(
      &args
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing supervisor request"))?,
    )?;
    anyhow::ensure!(args.next().is_none(), "unexpected supervisor arguments");
    supervise(request)
  })();
  let result = match result {
    Ok(result) => result,
    Err(error) => Outcome {
      status: "supervisor_error".into(),
      exit_code: None,
      stdout: String::new(),
      stderr: format!("{error:#}"),
      diagnostics_truncated: false,
      elapsed_seconds: 0.0,
      peak_memory_bytes: None,
      oom_kills: 0,
    },
  };
  let success = serde_json::to_writer(io::stdout(), &result)
    .and_then(|_| io::stdout().flush().map_err(serde_json::Error::io))
    .is_ok();
  Some(if success {
    ExitCode::SUCCESS
  } else {
    ExitCode::FAILURE
  })
}

pub(crate) fn spawn(request: &Request) -> anyhow::Result<Child> {
  Ok(
    Command::new(env::current_exe()?)
      .args([
        "--flowistry-project-worker",
        &serde_json::to_string(request)?,
      ])
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .stderr(Stdio::inherit())
      .process_group(0)
      .spawn()?,
  )
}

pub(crate) fn unit_name() -> anyhow::Result<String> {
  let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
  Ok(format!(
    "flowistry-body-{}-{nonce}.scope",
    std::process::id()
  ))
}

pub(crate) fn force_stop(child: &mut Child, unit: &str) {
  let _ = control(&["kill", "--signal=KILL", "--kill-whom=all", unit]);
  if matches!(child.try_wait(), Ok(None)) {
    let _ = child.kill();
  }
  let _ = child.wait();
  let _ = control(&["stop", "--no-block", unit]);
}
