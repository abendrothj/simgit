//! simgit-sdk — public types and control socket client.
//!
//! Both the `sg` CLI and external agent integrations use this crate to talk
//! to a running `simgitd` instance over its Unix domain socket.

pub mod types;
pub mod client;
pub mod error;

pub use client::Client;
pub use error::SdkError;
pub use types::*;
