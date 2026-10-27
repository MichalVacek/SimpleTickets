//! The sync plugin: mirrors tickets to other trackers through connector processes. It
//! owns all sync state; the core knows nothing about it. See `docs/sync.md`.

pub mod api;
pub mod cli;
pub mod config;
pub mod engine;
pub mod mapping;
pub mod service;
pub mod state;
