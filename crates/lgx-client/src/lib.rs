//! Shared client modules, exposed so the integration tests can exercise the
//! pure parts without creating a window or a PipeWire stream.
//!
//! The binary keeps its own `mod` declarations; these are the same files, so
//! the tests cover exactly the code the client runs.

pub mod config;
pub mod hid;
