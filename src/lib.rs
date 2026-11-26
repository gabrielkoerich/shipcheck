//! The library behind the `shipcheck` binary and its integration tests.
#![expect(
    missing_docs,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::exhaustive_enums,
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "the binary and its tests are the only users of this lib, so it follows the rules of a binary as passbox does"
)]

pub mod agents;
pub mod base;
pub mod cache;
pub mod clock;
pub mod config;
pub mod daemon;
pub mod daemon_config;
pub mod doctor;
pub mod git;
pub mod history;
pub mod onboard;
pub mod paths;
pub mod post;
pub mod run;
pub mod runner;
pub mod secret;
pub mod setup;
pub mod ui;
pub mod workflow;
