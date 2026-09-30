#![feature(rustc_private)]

fn main() -> std::process::ExitCode {
  env_logger::init();
  // Run the driver directly if nothing cargo checks has changed (see `replay`).
  let request = flowistry_ide::replay_request();
  if let Some((args, file)) = &request
    && let Some(status) = flowistry_ide::try_replay(file, args)
  {
    return status;
  }
  let pending = request.and_then(|(_, file)| flowistry_ide::prepare_replay(&file));
  let status = rustc_plugin::cli_main(flowistry_ide::FlowistryPlugin);
  if let Some(pending) = pending {
    let _ = std::fs::remove_file(pending);
  }
  status
}
