//! Versioned project stream over individually bounded, restartable body workers.
use std::path::PathBuf;
#[cfg(not(target_os = "linux"))]
use std::process::ExitCode;

use clap::Args;
use serde::{Deserialize, Serialize};
#[cfg(not(target_os = "linux"))]
use serde_json::Value;

#[derive(Args, Serialize, Deserialize)]
pub(crate) struct Options {
  /// Explicitly select the project event protocol.
  #[arg(long, default_value = "ndjson-v1", value_parser = ["ndjson-v1"])]
  stream: String,
  #[arg(long, requires_all = ["cursor_line", "cursor_column"])]
  cursor_file: Option<PathBuf>,
  /// Zero-based character position, like focus/file-focus.
  #[arg(long, requires = "cursor_file")]
  cursor_line: Option<u64>,
  #[arg(long, requires = "cursor_file")]
  cursor_column: Option<u64>,
  #[arg(long)]
  priority_file: Vec<PathBuf>,
  /// Saved files follow the cursor body and precede affected callers elsewhere.
  #[arg(long)]
  saved_file: Vec<PathBuf>,
  #[arg(long)]
  priority_body: Vec<String>,
  /// Per-worker memory cap including Cargo and compiler descendants (Linux/systemd).
  #[arg(long, default_value_t = 6144, value_parser = clap::value_parser!(u64).range(1..))]
  memory_mib: u64,
  #[arg(long, default_value_t = 600, value_parser = clap::value_parser!(u64).range(1..))]
  timeout_seconds: u64,
  /// Creating this file cancels the run; SIGINT/SIGTERM also cancel it.
  #[arg(long)]
  cancel_file: Option<PathBuf>,
}

#[cfg(target_os = "linux")]
pub(crate) use linux::run;

#[cfg(not(target_os = "linux"))]
pub(crate) fn run(_: Options, _: Vec<String>, _: Value) -> ExitCode {
  eprintln!(
    "flowistry project currently requires Linux with a systemd user session for bounded workers"
  );
  ExitCode::FAILURE
}

#[cfg(target_os = "linux")]
mod linux {
  use std::{
    collections::BTreeMap,
    io::{self, Read},
    path::PathBuf,
    process::ExitCode,
    sync::{
      Arc,
      atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
  };

  use anyhow::{Context, ensure};
  use base64::Engine;
  use serde::{
    Deserialize,
    de::{IgnoredAny, SeqAccess, Visitor},
  };
  use serde_json::{Value, json};

  use super::Options;
  use crate::project_process::{self, INTERRUPTED, Outcome, Request};

  const INVENTORY_LIMIT: u64 = 32 * 1024 * 1024;
  const RESPONSE_LIMIT: u64 = 80 * 1024 * 1024;
  const DECODE_LIMIT: u64 = 1024 * 1024 * 1024;

  impl Options {
    fn canceled(&self) -> bool {
      INTERRUPTED.load(Ordering::Relaxed)
        || self.cancel_file.as_ref().is_some_and(|p| p.exists())
        || output_closed()
    }
  }

  fn output_closed() -> bool {
    let mut descriptor = libc::pollfd {
      fd: libc::STDOUT_FILENO,
      events: 0,
      revents: 0,
    };
    (unsafe { libc::poll(&mut descriptor, 1, 0) }) > 0
      && descriptor.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0
  }

  struct Stream {
    id: String,
    sequence: u64,
    started: Instant,
    stdout_flags: libc::c_int,
  }

  impl Stream {
    fn new() -> anyhow::Result<Self> {
      let stdout_flags = unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_GETFL) };
      ensure!(
        stdout_flags >= 0,
        "cannot inspect project output: {}",
        io::Error::last_os_error()
      );
      ensure!(
        unsafe {
          libc::fcntl(
            libc::STDOUT_FILENO,
            libc::F_SETFL,
            stdout_flags | libc::O_NONBLOCK,
          )
        } >= 0,
        "cannot configure project output: {}",
        io::Error::last_os_error()
      );
      Ok(Self {
        id: project_process::unit_name()?,
        sequence: 0,
        started: Instant::now(),
        stdout_flags,
      })
    }

    fn emit(&mut self, mut event: Value, options: &Options) -> anyhow::Result<()> {
      event["schema"] = json!(1);
      event["run"] = json!(self.id);
      event["sequence"] = json!(self.sequence);
      event["elapsed_seconds"] = json!(self.started.elapsed().as_secs_f64());
      let mut data = serde_json::to_vec(&event)?;
      data.push(b'\n');
      let mut offset = 0;
      while offset < data.len() {
        let count = (data.len() - offset).min(64 * 1024);
        let written = unsafe {
          libc::write(libc::STDOUT_FILENO, data[offset ..].as_ptr().cast(), count)
        };
        if written > 0 {
          offset += written as usize;
          continue;
        }
        let error = io::Error::last_os_error();
        if matches!(
          error.kind(),
          io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
        ) {
          ensure!(
            !options.canceled(),
            "project canceled while output was blocked"
          );
          thread::sleep(Duration::from_millis(10));
        } else {
          return Err(error.into());
        }
      }
      self.sequence += 1;
      Ok(())
    }
  }

  impl Drop for Stream {
    fn drop(&mut self) {
      unsafe {
        libc::fcntl(libc::STDOUT_FILENO, libc::F_SETFL, self.stdout_flags);
      }
    }
  }

  fn request(
    options: &Options,
    args: Vec<String>,
    result_protocol: bool,
  ) -> anyhow::Result<Outcome> {
    let request = Request {
      unit: project_process::unit_name()?,
      args,
      memory_mib: options.memory_mib,
      timeout_seconds: options.timeout_seconds,
      result_protocol,
    };
    let mut child = project_process::spawn(&request)?;
    let mut lifetime = child.stdin.take();
    let stdout = child.stdout.take().context("missing supervisor stdout")?;
    let oversized = Arc::new(AtomicBool::new(false));
    let reader_overflow = oversized.clone();
    let reader = thread::spawn(move || -> io::Result<Vec<u8>> {
      let mut bytes = Vec::new();
      let mut stdout = stdout;
      let mut chunk = [0; 16 * 1024];
      loop {
        let count = stdout.read(&mut chunk)?;
        if count == 0 {
          break;
        }
        let remaining = (RESPONSE_LIMIT as usize).saturating_sub(bytes.len());
        bytes.extend_from_slice(&chunk[.. count.min(remaining)]);
        if count > remaining {
          reader_overflow.store(true, Ordering::Relaxed);
        }
      }
      Ok(bytes)
    });
    let start = Instant::now();
    let mut cancel_started = None;
    let status = loop {
      if options.canceled()
        || oversized.load(Ordering::Relaxed)
        || start.elapsed().as_secs() > options.timeout_seconds.saturating_add(15)
      {
        lifetime.take();
        cancel_started.get_or_insert_with(Instant::now);
      }
      if cancel_started.is_some_and(|at| at.elapsed() > Duration::from_secs(10)) {
        project_process::force_stop(&mut child, &request.unit);
        break None;
      }
      match child.try_wait() {
        Ok(Some(status)) => break Some(status),
        Ok(None) => thread::sleep(Duration::from_millis(20)),
        Err(_) => {
          lifetime.take();
          project_process::force_stop(&mut child, &request.unit);
          break None;
        }
      }
    };
    drop(lifetime);
    let bytes = reader
      .join()
      .map_err(|_| anyhow::anyhow!("supervisor reader panicked"))??;
    ensure!(
      !oversized.load(Ordering::Relaxed),
      "supervisor exceeded its response budget"
    );
    ensure!(
      status.is_some_and(|status| status.success()),
      "worker supervisor failed or did not stop after cancellation"
    );
    Ok(serde_json::from_slice(&bytes).context("invalid supervisor response")?)
  }

  fn decompress(
    encoded: &str,
  ) -> anyhow::Result<flate2::read::GzDecoder<std::io::Cursor<Vec<u8>>>> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(encoded.trim())?;
    Ok(flate2::read::GzDecoder::new(std::io::Cursor::new(bytes)))
  }

  struct CheckedReader<'a, R> {
    inner: R,
    remaining: u64,
    options: &'a Options,
  }
  impl<R: Read> Read for CheckedReader<'_, R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
      if self.options.canceled() {
        return Err(io::Error::other("project canceled"));
      }
      if self.remaining == 0 {
        return Err(io::Error::other("decoded output exceeds its budget"));
      }
      let count = bytes.len().min(self.remaining as usize);
      let count = self.inner.read(&mut bytes[.. count])?;
      self.remaining -= count as u64;
      Ok(count)
    }
  }

  #[derive(Deserialize)]
  struct BodyProbe {
    focus: Option<Result<IgnoredAny, IgnoredAny>>,
    range: Value,
  }
  #[derive(Default)]
  struct Counts {
    ok: usize,
    error: usize,
    selected_range: Option<Value>,
  }
  impl<'de> Deserialize<'de> for Counts {
    fn deserialize<D: serde::Deserializer<'de>>(
      deserializer: D,
    ) -> Result<Self, D::Error> {
      struct CountVisitor;
      impl<'de> Visitor<'de> for CountVisitor {
        type Value = Counts;
        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
          formatter.write_str("file-focus body outcomes")
        }
        fn visit_seq<A: SeqAccess<'de>>(
          self,
          mut sequence: A,
        ) -> Result<Counts, A::Error> {
          let mut counts = Counts::default();
          while let Some(body) = sequence.next_element::<BodyProbe>()? {
            match body.focus {
              Some(Ok(_)) => {
                counts.ok += 1;
                counts.selected_range = Some(body.range);
              }
              Some(Err(_)) => counts.error += 1,
              None => {}
            }
          }
          Ok(counts)
        }
      }
      deserializer.deserialize_seq(CountVisitor)
    }
  }
  #[derive(Deserialize)]
  struct FileProbe {
    bodies: Counts,
  }
  #[derive(Deserialize)]
  struct Probe {
    #[serde(rename = "Ok")]
    ok: Option<FileProbe>,
  }

  fn successful_body(encoded: &str, options: &Options) -> anyhow::Result<Option<Value>> {
    // Inspect only the result tags. The range tables and dependency sets may be
    // hundreds of MiB; streaming IgnoredAny never builds that object in the coordinator.
    let reader = CheckedReader {
      inner: decompress(encoded)?,
      remaining: DECODE_LIMIT,
      options,
    };
    let mut deserializer = serde_json::Deserializer::from_reader(reader);
    let probe = Probe::deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(probe.ok.and_then(|p| {
      (p.bodies.ok == 1 && p.bodies.error == 0)
        .then_some(p.bodies.selected_range)
        .flatten()
    }))
  }

  fn point(value: &Value) -> Option<(u64, u64)> {
    Some((value["line"].as_u64()?, value["column"].as_u64()?))
  }

  fn prioritize(
    bodies: &mut [Value],
    options: &Options,
    stream: &mut Stream,
  ) -> anyhow::Result<()> {
    let canonical = |path: &PathBuf| {
      path
        .canonicalize()
        .ok()
        .and_then(|p| p.to_str().map(str::to_owned))
    };
    let files = options
      .priority_file
      .iter()
      .filter_map(canonical)
      .collect::<Vec<_>>();
    let cursor_file = options.cursor_file.as_ref().and_then(canonical);
    let saved = options
      .saved_file
      .iter()
      .filter_map(canonical)
      .collect::<Vec<_>>();
    let cursor = options.cursor_line.zip(options.cursor_column);
    let bounds = bodies
      .iter()
      .filter(|body| {
        body["range"]["filename"].as_str() == cursor_file.as_deref() && cursor.is_some()
      })
      .filter_map(|body| {
        Some((
          point(&body["range"]["start"])?,
          point(&body["range"]["end"])?,
        ))
      })
      .filter(|(start, end)| *start <= cursor.unwrap() && cursor.unwrap() < *end)
      .max_by_key(|(start, end)| (*start, std::cmp::Reverse(*end)));
    for identity in &options.priority_body {
      if !bodies
        .iter()
        .any(|b| b["identity"].as_str() == Some(identity))
      {
        stream.emit(json!({"event":"diagnostic", "level":"warning", "message":"priority body is absent from this target", "identity":identity}), options)?;
      }
    }
    if options.cursor_file.is_some() && bounds.is_none() {
      stream.emit(json!({"event":"diagnostic", "level":"warning", "message":"cursor did not select a discovered body"}), options)?;
    }
    let mut identities = BTreeMap::new();
    for body in bodies.iter() {
      *identities
        .entry(body["identity"].as_str().unwrap_or_default().to_owned())
        .or_insert(0usize) += 1;
    }
    for body in bodies.iter_mut() {
      if identities[body["identity"].as_str().unwrap_or_default()] != 1 {
        body["error"] = json!("ambiguous inventory identity");
      }
    }
    bodies.sort_by_cached_key(|body| {
      let file = body["range"]["filename"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
      let start = point(&body["range"]["start"]).unwrap_or_default();
      let end = point(&body["range"]["end"]).unwrap_or_default();
      let identity = body["identity"].as_str().unwrap_or_default().to_owned();
      let priority =
        if cursor_file.as_deref() == Some(&file) && bounds == Some((start, end)) {
          (0, 0)
        } else if let Some(index) = saved.iter().position(|name| *name == file) {
          (1, index)
        } else if let Some(index) =
          options.priority_body.iter().position(|id| *id == identity)
        {
          (2, index)
        } else if !saved.is_empty()
          && matches!(
            body["save_plan"]["status"].as_str(),
            Some("affected" | "changed" | "unknown")
          )
        {
          (3, 0)
        } else if let Some(index) = files.iter().position(|name| *name == file) {
          (4, index)
        } else {
          (5, 0)
        };
      (priority, file, start, end, identity)
    });
    Ok(())
  }

  fn execute(
    options: &Options,
    prefix: &[String],
    selection: Value,
    stream: &mut Stream,
  ) -> anyhow::Result<ExitCode> {
    stream.emit(json!({"event":"started", "selection":selection, "workers":1, "bodies_per_worker":1,
    "worker_memory_mib":options.memory_mib, "worker_timeout_seconds":options.timeout_seconds,
    "resume":"validated-shared-cache", "coverage":"initial-inventory",
    "currentness":"per-body-at-publication"}), options)?;
    if options.canceled() {
      stream.emit(
        json!({"event":"finished", "status":"canceled", "completed":0}),
        options,
      )?;
      return Ok(ExitCode::from(130));
    }
    let mut inventory_args = prefix.to_vec();
    inventory_args.extend(["project-bodies".into(), ".".into()]);
    let inventory = request(options, inventory_args, false)?;
    if options.canceled()
      || inventory.status != "exited"
      || inventory.exit_code != Some(0)
    {
      stream.emit(json!({"event":"diagnostic", "phase":"inventory", "status":inventory.status, "message":inventory.stderr,
      "worker_seconds":inventory.elapsed_seconds, "observed_peak_memory_bytes":inventory.peak_memory_bytes}), options)?;
      let canceled = options.canceled();
      stream.emit(json!({"event":"finished", "status":if canceled {"canceled"} else {"failed"}, "completed":0}), options)?;
      return Ok(ExitCode::from(if canceled { 130 } else { 1 }));
    }
    let mut bytes = Vec::new();
    CheckedReader {
      inner: decompress(&inventory.stdout)?,
      remaining: INVENTORY_LIMIT + 2,
      options,
    }
    .take(INVENTORY_LIMIT + 1)
    .read_to_end(&mut bytes)?;
    ensure!(
      bytes.len() as u64 <= INVENTORY_LIMIT,
      "project inventory exceeds its memory budget"
    );
    let mut inventory: Value = serde_json::from_slice(&bytes)?;
    drop(bytes);
    ensure!(
      inventory["Ok"]["schema"] == 1,
      "unsupported project inventory"
    );
    let bodies = inventory["Ok"]["bodies"]
      .as_array_mut()
      .context("missing compiler body inventory")?;
    prioritize(bodies, options, stream)?;
    let total = bodies.len();
    stream.emit(json!({"event":"inventory", "total":total}), options)?;
    let (mut completed, mut succeeded, mut failed) = (0, 0, 0);
    let mut first_result_seconds = None;
    for body in bodies {
      if options.canceled() {
        break;
      }
      stream.emit(
        json!({"event":"body-started", "body":body, "ordinal":completed, "total":total}),
        options,
      )?;
      if let Some(error) = body.get("error") {
        stream.emit(
        json!({"event":"body", "body":body, "status":"inventory_error", "message":error}),
        options,
      )?;
        completed += 1;
        failed += 1;
        continue;
      }
      let file = body["range"]["filename"]
        .as_str()
        .context("inventory body lacks a filename")?;
      let identity = body["identity"]
        .as_str()
        .context("inventory body lacks an identity")?;
      let mut args = prefix.to_vec();
      args.extend(["body-focus".into(), file.into(), identity.into()]);
      let result = match request(options, args, true) {
        Ok(result) => result,
        Err(error) => {
          if options.canceled() {
            break;
          }
          stream.emit(json!({"event":"body", "body":body, "status":"supervisor_error", "message":format!("{error:#}")}), options)?;
          completed += 1;
          failed += 1;
          continue;
        }
      };
      if options.canceled() {
        break;
      }
      let mut event = json!({"event":"body", "body":body, "status":result.status, "exit_code":result.exit_code,
      "worker_seconds":result.elapsed_seconds, "observed_peak_memory_bytes":result.peak_memory_bytes,
      "oom_kills":result.oom_kills, "diagnostics":result.stderr, "diagnostics_truncated":result.diagnostics_truncated});
      let mut success = false;
      if (result.status == "exited" && result.exit_code == Some(0))
        || (result.status == "worker_error" && result.exit_code == Some(75))
      {
        let parsed = (|| -> anyhow::Result<()> {
          let publication: Value = serde_json::from_str(&result.stdout)?;
          ensure!(
            publication["schema"] == 1,
            "unsupported publication protocol"
          );
          ensure!(
            result.exit_code == Some(0) || publication["status"] == "superseded",
            "unsuccessful worker returned an invalid publication status"
          );
          event["status"] = publication["status"].clone();
          event["revision"] = publication["revision"].clone();
          event["generation"] = publication["generation"].clone();
          if matches!(publication["status"].as_str(), Some("current" | "uncached")) {
            let encoded = publication["output"]
              .as_str()
              .context("missing body output")?;
            if let Some(mut range) = successful_body(encoded, options)? {
              // The compiler may have observed a later save than the inventory.
              // Use that worker's current range, never the old inventory offsets.
              range["filename"] = json!(file);
              event["body"]["range"] = range;
              success = true;
            }
            event["encoding"] = json!("file-focus-base64-gzip-json");
            event["output"] = json!(encoded);
            event["current"] = json!(success && publication["status"] == "current");
            if !success {
              event["status"] = json!("analysis_error");
            }
          }
          Ok(())
        })();
        if let Err(error) = parsed {
          event["status"] = json!("protocol_error");
          event["message"] = json!(error.to_string());
        }
      }
      if success {
        succeeded += 1;
        first_result_seconds
          .get_or_insert_with(|| stream.started.elapsed().as_secs_f64());
      } else {
        failed += 1;
      }
      completed += 1;
      stream.emit(event, options)?;
    }
    let canceled = options.canceled();
    stream.emit(json!({"event":"finished", "status":if canceled {"canceled"} else if failed > 0 {"partial"} else {"complete"},
    "total":total, "completed":completed, "succeeded":succeeded, "failed":failed, "pending":total-completed,
    "first_result_seconds":first_result_seconds, "coverage":"initial-inventory",
    "project_current":false}), options)?;
    Ok(ExitCode::from(if canceled {
      130
    } else if failed > 0 {
      1
    } else {
      0
    }))
  }

  pub(crate) fn run(options: Options, prefix: Vec<String>, selection: Value) -> ExitCode {
    project_process::handle_signals();
    let mut stream = match Stream::new() {
      Ok(stream) => stream,
      Err(error) => {
        eprintln!("flowistry project: {error}");
        return ExitCode::FAILURE;
      }
    };
    match execute(&options, &prefix, selection, &mut stream) {
      Ok(code) => code,
      Err(error) => {
        let _ = stream.emit(
          json!({"event":"diagnostic", "level":"error", "message":format!("{error:#}")}),
          &options,
        );
        let _ = stream.emit(json!({"event":"finished", "status":if options.canceled() {"canceled"} else {"failed"}}), &options);
        ExitCode::from(if options.canceled() { 130 } else { 1 })
      }
    }
  }

  #[cfg(test)]
  mod tests {
    use std::io::Write;

    use super::*;

    fn options() -> Options {
      Options {
        stream: "ndjson-v1".into(),
        cursor_file: None,
        cursor_line: None,
        cursor_column: None,
        priority_file: vec![],
        saved_file: vec![],
        priority_body: vec![],
        memory_mib: 1024,
        timeout_seconds: 60,
        cancel_file: None,
      }
    }

    fn encoded(json: &str) -> String {
      let mut writer =
        flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
      writer.write_all(json.as_bytes()).unwrap();
      base64::engine::general_purpose::STANDARD.encode(writer.finish().unwrap())
    }

    #[test]
    fn selected_body_range_comes_from_worker() {
      let value = json!({"Ok":{"bodies":[
        {"range":{"start":{"line":0,"column":0}}, "focus":null},
        {"range":{"start":{"line":12,"column":2}}, "focus":{"Ok":{"ranges":[1,2,3],"comments":[4]}}}
      ]}});
      let range = successful_body(&encoded(&value.to_string()), &options())
        .unwrap()
        .unwrap();
      assert_eq!(range["start"]["line"], 12);
    }

    #[test]
    fn error_and_ambiguous_worker_results_are_not_successes() {
      for value in [
        json!({"Err":{"AnalysisError":{"error":"body removed"}}}),
        json!({"Ok":{"bodies":[{"range":{},"focus":{"Err":"type error"}}]}}),
        json!({"Ok":{"bodies":[{"range":{},"focus":{"Ok":{}}},{"range":{},"focus":{"Ok":{}}}]}}),
      ] {
        assert!(
          successful_body(&encoded(&value.to_string()), &options())
            .unwrap()
            .is_none()
        );
      }
      assert!(
        successful_body(&encoded("{\"Ok\":{\"bodies\":[]}} trailing"), &options())
          .is_err()
      );
    }

    #[test]
    fn decompressed_reader_obeys_budget_and_file_cancellation() {
      let options = options();
      let mut reader = CheckedReader {
        inner: std::io::Cursor::new(b"12345"),
        remaining: 3,
        options: &options,
      };
      assert!(reader.read_to_end(&mut Vec::new()).is_err());
      let mut canceled = options;
      // An existing directory is also an existing cancellation path.
      canceled.cancel_file = Some(std::env::temp_dir());
      assert!(successful_body(&encoded("{\"Ok\":{\"bodies\":[]}}"), &canceled).is_err());
    }
  }
}
