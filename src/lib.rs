//! Dumpling — MySQL copy & anonymization library used by the CLI.

pub mod config;
pub mod copy;
pub mod db;
pub mod error;
pub mod planner;
pub mod progress;
pub mod safety;
pub mod sync;
pub mod transform;

pub mod cli;
