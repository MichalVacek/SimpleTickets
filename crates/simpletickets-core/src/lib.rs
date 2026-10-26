//! Core library for SimpleTickets: ticket model, configuration and store.

pub mod audit;
pub mod config;
pub mod doctor;
pub mod frontmatter;
pub mod hash;
pub mod id;
pub mod rules;
pub mod state;
pub mod store;
pub mod ticket;
pub mod time;
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
