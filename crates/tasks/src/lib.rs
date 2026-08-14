//! Language-neutral task discovery and output decoding.
#![forbid(unsafe_code)]

mod cargo;
mod java;
mod node;
mod output;
mod python;
mod service;
mod text_problems;

pub use cargo::AdapterTaskService;
pub use output::CargoOutputDecoder;
pub use service::{DecodedTaskOutput, TaskOutputDecoder, TaskService};
pub use text_problems::TextProblemDecoder;
