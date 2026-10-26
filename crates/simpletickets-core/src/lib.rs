//! Core library for SimpleTickets: ticket model, configuration and store.

pub mod audit;
pub mod config;
pub mod diff;
pub mod doctor;
pub mod frontmatter;
pub mod hash;
pub mod id;
pub mod known;
pub mod lease;
pub mod migration;
pub mod ops;
pub mod plugin;
pub mod query;
pub mod rules;
pub mod search;
pub mod state;
pub mod store;
pub mod templates;
pub mod ticket;
pub mod time;
pub mod view;
pub mod warnings;
pub mod write;

/// Crate version, as declared in the manifest.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_set() {
        assert!(!VERSION.is_empty());
    }
}
