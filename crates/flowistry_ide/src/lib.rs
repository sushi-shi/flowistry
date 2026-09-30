#![feature(rustc_private, unboxed_closures)]
#![allow(
  clippy::single_match,
  clippy::needless_lifetimes,
  clippy::needless_return,
  clippy::len_zero,
  clippy::let_and_return
)]

extern crate either;
extern crate rustc_data_structures;
extern crate rustc_driver;
extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_hir_pretty;
extern crate rustc_index;
extern crate rustc_interface;
extern crate rustc_lexer;
extern crate rustc_macros;
extern crate rustc_middle;
extern crate rustc_mir_dataflow;
extern crate rustc_serialize;
extern crate rustc_session;
extern crate rustc_span;

mod cache;
#[cfg(feature = "decompose")]
mod decompose;
mod file_focus;
mod focus;
mod playground;
mod plugin;
mod positions;
mod project;
mod project_coordinator;
#[cfg(target_os = "linux")]
mod project_process;
mod replay;
mod result_store;
mod spans;
mod summary_cache;

pub use plugin::{FlowistryPlugin, replay_request};
pub use replay::{prepare as prepare_replay, try_replay};

pub mod fast_cache;
