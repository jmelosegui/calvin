//! One adapter per AI harness. Each finds its log files and turns lines into [`Event`]s.
//!
//! [`Event`]: crate::event::Event

pub mod claude_code;
pub mod copilot_cli;
