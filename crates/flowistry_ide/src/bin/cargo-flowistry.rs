#![feature(rustc_private)]

fn main() -> std::process::ExitCode {
  env_logger::init();
  flowistry_ide::fast_cache::run()
}
