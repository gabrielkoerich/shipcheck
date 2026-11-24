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
pub mod autostart;
pub mod background;
pub mod base;
pub mod cache;
pub mod check;
pub mod clock;
pub mod comment;
pub mod config;
pub mod daemon;
pub mod daemon_config;
pub mod doctor;
pub mod git;
pub mod github;
pub mod history;
pub mod java;
pub mod onboard;
pub mod paths;
pub mod queue;
pub mod record;
pub mod report;
pub mod ruby;
pub mod run;
pub mod runner;
pub mod secret;
pub mod service;
pub mod setup;
pub mod templates;
pub mod trust;
pub mod ui;
pub mod workflow;
