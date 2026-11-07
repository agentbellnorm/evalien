//! Quality checks over a stored run.

mod analyze;

pub use analyze::{Check, Report, THRESHOLDS, Thresholds, analyze, last_run, render};
