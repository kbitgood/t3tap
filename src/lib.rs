//! t3tap: stream a T3 Code thread as it happens.
//!
//! `watcher` reads and classifies items, `cli` has options, filters and help,
//! `format` prints events and `where_expr` runs `--where` expressions.

pub mod cli;
pub mod format;
pub mod watcher;
pub mod where_expr;
