//! Talking to the Kubernetes API.
//!
//! This is the tool's only I/O boundary besides the filesystem, and it is kept
//! narrow on purpose: functions in here fetch bytes and hand back plain
//! Kubernetes types. Everything that decides what those types *mean* — status
//! wording, ages, colours, table layout — is a pure function next door, so the
//! interesting logic never needs a cluster to be tested.
//!
//! Nothing in this module runs before first paint. Building a client is
//! offline; the credential helper and the first request happen when a command
//! actually asks for data.

pub mod client;
pub mod nodes;
pub mod pods;
pub mod quantity;
pub mod selector;

pub use client::{Failure, connect, explain};
pub use quantity::Quantity;
