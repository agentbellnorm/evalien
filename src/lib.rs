//! evalien: an autonomous agent whose only tool is `eval()`.
//!
//! Folders are modules. They share types through [`domain`] and never import
//! each other; `tests/boundaries.rs` enforces this. `main.rs` and
//! `bin/report.rs` are the composition roots, and the top-level files
//! (`config`, `lifecycle`, `system_prompt`) are what they share.

#![forbid(unsafe_code)]

pub mod chat;
pub mod config;
pub mod db;
pub mod domain;
pub mod evaluation;
pub mod harness;
pub mod inference;
pub mod lifecycle;
pub mod quality;
pub mod system_prompt;
pub mod terminal;
