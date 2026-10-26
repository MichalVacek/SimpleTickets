//! Core library for SimpleTickets: ticket model, configuration and store.

pub mod frontmatter;
pub mod id;
pub mod ticket;

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
