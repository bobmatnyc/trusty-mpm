//! Human-memorable session naming.
//!
//! Why: tmux sessions were named `trusty-mpm-<full-uuid>`, which is unreadable
//! and impossible to tell apart at a glance — an operator running 16 sessions
//! sees 16 indistinguishable rows. A two-part adjective+noun name (Docker-style)
//! is glanceable, while deriving it deterministically from the UUID keeps it
//! stable and round-trippable (the same session always renders the same name).
//! What: two embedded wordlists and [`name_from_uuid`], which indexes into them
//! using the UUID's 128-bit value so the mapping is pure and deterministic.
//! Test: `cargo test -p trusty-mpm-core` asserts determinism, format, and that
//! distinct UUIDs generally produce distinct names.

use uuid::Uuid;

/// Adjective half of the wordlist (50 short, neutral words).
const ADJECTIVES: &[&str] = &[
    "quiet", "brave", "silent", "swift", "calm", "bold", "bright", "clever", "gentle", "happy",
    "keen", "lively", "merry", "noble", "proud", "rapid", "sharp", "sleek", "steady", "sunny",
    "warm", "wise", "agile", "amber", "azure", "crisp", "deep", "eager", "fancy", "fierce",
    "frosty", "golden", "humble", "icy", "jolly", "lucky", "mellow", "mighty", "misty", "nimble",
    "polar", "royal", "rustic", "shiny", "snowy", "solar", "stark", "vivid", "witty", "zesty",
];

/// Noun half of the wordlist (50 short, neutral words).
const NOUNS: &[&str] = &[
    "falcon", "river", "crane", "otter", "maple", "comet", "harbor", "ember", "willow", "canyon",
    "meadow", "boulder", "cedar", "delta", "fjord", "glade", "harvest", "island", "jungle",
    "lagoon", "marsh", "nebula", "oasis", "pine", "quartz", "ridge", "summit", "tundra", "valley",
    "anchor", "badger", "cobra", "dolphin", "eagle", "ferret", "gibbon", "heron", "ibis", "jaguar",
    "koala", "lynx", "marten", "newt", "osprey", "puffin", "raven", "sparrow", "tiger", "viper",
    "walrus",
];

/// Short prefix kept distinct from the legacy `trusty-mpm-` prefix.
///
/// Why: the full `trusty-mpm-` prefix plus two words would push names past the
/// ~25-char budget; `tmpm-` keeps the result short while staying recognizable.
const PREFIX: &str = "tmpm-";

/// Derive a stable, human-memorable session name from a UUID.
///
/// Why: gives tmux sessions glanceable names while keeping the name a pure
/// function of the session id, so any component can recompute it without a
/// lookup table.
/// What: returns `tmpm-<adjective>-<noun>`, choosing each word by indexing the
/// wordlists with the UUID's 128-bit integer value (modulo each list length).
/// Test: `deterministic`, `format_matches`, `distinct_uuids_distinct_names`.
pub fn name_from_uuid(uuid: &Uuid) -> String {
    let value = uuid.as_u128();
    let adj = ADJECTIVES[(value % ADJECTIVES.len() as u128) as usize];
    // Shift before the second modulo so the adjective and noun are not derived
    // from overlapping low bits (which would correlate the two words).
    let noun = NOUNS[((value / ADJECTIVES.len() as u128) % NOUNS.len() as u128) as usize];
    format!("{PREFIX}{adj}-{noun}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic() {
        let id = Uuid::parse_str("367c6c51-1025-419c-b6d6-be9a753e8914").unwrap();
        assert_eq!(name_from_uuid(&id), name_from_uuid(&id));
    }

    #[test]
    fn format_matches() {
        for _ in 0..200 {
            let name = name_from_uuid(&Uuid::new_v4());
            let rest = name.strip_prefix("tmpm-").expect("tmpm- prefix");
            let mut parts = rest.split('-');
            let adj = parts.next().expect("adjective");
            let noun = parts.next().expect("noun");
            assert!(parts.next().is_none(), "exactly two words: {name}");
            assert!(ADJECTIVES.contains(&adj), "adjective from list: {adj}");
            assert!(NOUNS.contains(&noun), "noun from list: {noun}");
            assert!(name.len() <= 25, "name under 25 chars: {name}");
        }
    }

    #[test]
    fn distinct_uuids_distinct_names() {
        // Across many random UUIDs the 2500-name space yields mostly-unique
        // names; assert a healthy unique ratio rather than total uniqueness
        // (collisions are expected by the pigeonhole principle).
        let mut names = std::collections::HashSet::new();
        for _ in 0..500 {
            names.insert(name_from_uuid(&Uuid::new_v4()));
        }
        assert!(
            names.len() > 400,
            "expected mostly-distinct names: {}",
            names.len()
        );
    }

    #[test]
    fn known_uuid_is_stable() {
        // Nil UUID maps to index 0 of both lists — pins the algorithm.
        assert_eq!(name_from_uuid(&Uuid::nil()), "tmpm-quiet-falcon");
    }
}
