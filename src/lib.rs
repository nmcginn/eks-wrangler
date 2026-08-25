//! `eks` — a fast, keyboard-driven explorer for AWS EKS clusters.
//!
//! The binary is a thin shell around this library so that every piece of
//! behaviour is reachable from unit tests. Anything that can be tested without
//! a live cluster or a real terminal belongs here, not in `main.rs`.

pub mod cluster;
pub mod commands;
pub mod format;
pub mod fuzzy;
pub mod k8s;
pub mod kubeconfig;
pub mod theme;
pub mod ui;

/// The command-line surface, kept in its own module so `main.rs` stays trivial.
pub mod cli;
