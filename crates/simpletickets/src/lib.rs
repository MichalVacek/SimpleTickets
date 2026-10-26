//! SimpleTickets command line, daemon and HTTP API.

pub mod api;
pub mod cli;
pub mod daemon;
pub mod events;
pub mod idempotency;
pub mod logger;
pub mod plugins;
pub mod supervisor;
pub mod token;
pub mod update;
pub mod webhooks;
