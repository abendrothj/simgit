//! simgit-sdk — public types and TCP loopback client.
//!
//! Both the `sg` CLI and external agent integrations use this crate to talk
//! to a running `simgitd` instance over TCP loopback (port discovered via
//! `control.port` file).

pub mod types;
pub mod client;
pub mod error;

pub use client::Client;
pub use error::SdkError;
pub use types::*;
