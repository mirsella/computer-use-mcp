pub mod accessibility;
pub mod atspi_adapter;
pub mod capture;
pub mod cli;
pub mod contract;
pub mod desktop_launcher;
pub mod encoder;
pub mod errors;
pub mod geometry;
pub mod input;
pub mod portal;
pub mod runtime;
pub mod screenshot;
pub mod server;
pub mod validation;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
