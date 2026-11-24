//! Host checks, which run outside Docker

#[path = "../common/mod.rs"]
mod common;

mod host_e2e;
mod host_leftover;
mod host_reuse_e2e;
mod host_shard_e2e;
mod host_sigterm;
mod timeout;
