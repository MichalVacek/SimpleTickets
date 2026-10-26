//! Ticket ID generation: sequential, with a random fallback on collision.

use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Returns the next free ID of the form `{prefix}-{n}`.
///
/// `n` follows the highest sequential number among `taken`. If that candidate
/// is already used or `exists` reports it as taken elsewhere (for example on
/// disk), a random `{prefix}-r{hex}` ID is used instead. Random IDs always
/// contain a letter, so they never influence the sequence.
pub fn next_id(
    prefix: &str,
    taken: &HashSet<String>,
    mut exists: impl FnMut(&str) -> bool,
) -> String {
    let max = taken
        .iter()
        .filter_map(|id| sequence_number(prefix, id))
        .max()
        .unwrap_or(0);
    let candidate = format!("{prefix}-{}", max + 1);
    if !taken.contains(&candidate) && !exists(&candidate) {
        return candidate;
    }
    loop {
        let candidate = format!("{prefix}-r{}", random_hex());
        if !taken.contains(&candidate) && !exists(&candidate) {
            return candidate;
        }
    }
}

/// The numeric part of a sequential ID such as `ST-12`.
pub fn sequence_number(prefix: &str, id: &str) -> Option<u64> {
    let digits = id.strip_prefix(prefix)?.strip_prefix('-')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn random_hex() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut hasher = Sha256::new();
    hasher.update(nanos.to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(count.to_le_bytes());
    let digest = hasher.finalize();
    digest[..4].iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn starts_at_one() {
        assert_eq!(next_id("ST", &set(&[]), |_| false), "ST-1");
    }

    #[test]
    fn follows_highest_number() {
        let taken = set(&["ST-1", "ST-7", "OTHER-99", "ST-abc"]);
        assert_eq!(next_id("ST", &taken, |_| false), "ST-8");
    }

    #[test]
    fn falls_back_to_random_on_collision() {
        let id = next_id("ST", &set(&["ST-1"]), |c| c == "ST-2");
        assert!(id.starts_with("ST-r"), "{id}");
        assert_eq!(sequence_number("ST", &id), None);
    }

    #[test]
    fn random_ids_differ() {
        let a = next_id("ST", &set(&[]), |c| c == "ST-1");
        let b = next_id("ST", &set(&[]), |c| c == "ST-1");
        assert_ne!(a, b);
    }

    #[test]
    fn sequence_number_parsing() {
        assert_eq!(sequence_number("ST", "ST-12"), Some(12));
        assert_eq!(sequence_number("ST", "ST-"), None);
        assert_eq!(sequence_number("ST", "STX-1"), None);
        assert_eq!(sequence_number("ST", "ST-1a"), None);
    }
}
