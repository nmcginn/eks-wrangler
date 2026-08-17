//! One module per user-facing command.
//!
//! Commands are written as pure functions returning the text they want printed
//! wherever that is possible. Rendering and I/O stay separable so output can be
//! asserted on in unit tests instead of eyeballed.

pub mod contexts;
