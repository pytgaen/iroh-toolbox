//! Codes: the word list, generation, parsing and the rendezvous key.
use anyhow::{Result, ensure};
use hkdf::Hkdf;
use iroh::SecretKey;
use sha2::Sha256;
use std::sync::OnceLock;
use zeroize::Zeroizing;

/// Words that locate the rendezvous endpoint; the rest form the SPAKE2 password.
const RENDEZVOUS_WORDS: usize = 3;
pub const MIN_WORDS: usize = 6;
pub(super) const MAX_WORDS: usize = 32;
/// EFF large wordlist (CC BY 3.0), unmodified; hyphenated entries are skipped.
const WORDLIST: &str = include_str!("../eff_large_wordlist.txt");

fn words() -> &'static [&'static str] {
    static WORDS: OnceLock<Vec<&'static str>> = OnceLock::new();
    WORDS.get_or_init(|| {
        WORDLIST
            .lines()
            .filter_map(|line| line.split('\t').nth(1))
            .map(str::trim)
            .filter(|word| !word.contains('-'))
            .collect()
    })
}

/// A uniformly random code of `count` words.
pub(super) fn generate_code(count: usize) -> Result<String> {
    let list = words();
    let size = list.len() as u32;
    // Rejection sampling keeps every word equally likely.
    let limit = (1u32 << 16) - (1u32 << 16) % size;
    let mut code = Vec::with_capacity(count);
    while code.len() < count {
        let mut bytes = [0u8; 2];
        getrandom::fill(&mut bytes)
            .map_err(|error| anyhow::anyhow!("system randomness unavailable: {error}"))?;
        let value = u32::from(u16::from_le_bytes(bytes));
        if value < limit {
            code.push(list[(value % size) as usize]);
        }
    }
    Ok(code.join("-"))
}

pub(super) struct Code {
    pub(super) rendezvous: String,
    pub(super) password: Zeroizing<String>,
}

pub(super) fn parse_code(value: &str) -> Result<Code> {
    let value = value.trim().to_lowercase();
    let parts: Vec<&str> = value
        .split(|c: char| c == '-' || c.is_whitespace())
        .filter(|word| !word.is_empty())
        .collect();
    ensure!(
        (MIN_WORDS..=MAX_WORDS).contains(&parts.len()),
        "a code has {MIN_WORDS} to {MAX_WORDS} words; copy the complete code"
    );
    for (index, word) in parts.iter().enumerate() {
        ensure!(
            words().binary_search(word).is_ok(),
            "word {} of the code is not in the word list; check its spelling",
            index + 1
        );
    }
    Ok(Code {
        rendezvous: parts[..RENDEZVOUS_WORDS].join("-"),
        password: Zeroizing::new(parts[RENDEZVOUS_WORDS..].join("-")),
    })
}

/// Rejects a malformed code locally, before any network activity.
pub fn check_code(value: &str) -> Result<()> {
    parse_code(value).map(|_| ())
}

/// The canonical spelling of a code: lowercase words joined by `-`.
pub fn normalize_code(value: &str) -> Result<String> {
    parse_code(value).map(|code| format!("{}-{}", code.rendezvous, *code.password))
}

/// The rendezvous key is derived from public words: it locates, it does not authenticate.
pub(super) fn rendezvous_key(rendezvous: &str) -> SecretKey {
    let mut seed = Zeroizing::new([0u8; 32]);
    Hkdf::<Sha256>::new(
        Some(b"iroh-toolbox/pairing/v1/rendezvous"),
        rendezvous.as_bytes(),
    )
    .expand(b"secret-key", &mut *seed)
    .expect("32 bytes is a valid HKDF length");
    SecretKey::from_bytes(&seed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_list_is_the_eff_large_list_without_hyphenated_words() {
        let list = words();
        assert_eq!(list.len(), 7772);
        assert!(list.is_sorted() && list.windows(2).all(|w| w[0] != w[1]));
        assert!(
            list.iter()
                .all(|w| w.bytes().all(|b| b.is_ascii_lowercase()))
        );
    }

    #[test]
    fn codes_round_trip_and_reject_unknown_or_short_input() {
        let code = generate_code(MIN_WORDS).unwrap();
        assert_eq!(code.split('-').count(), MIN_WORDS);
        assert_ne!(code, generate_code(MIN_WORDS).unwrap());
        let parsed = parse_code(&code).unwrap();
        let spaced = parse_code(&format!(" {} ", code.to_uppercase().replace('-', " "))).unwrap();
        assert_eq!(parsed.rendezvous, spaced.rendezvous);
        assert_eq!(parsed.password, spaced.password);
        assert_eq!(parsed.rendezvous.split('-').count(), RENDEZVOUS_WORDS);
        for count in [6, 8] {
            let parsed = parse_code(&generate_code(count).unwrap()).unwrap();
            assert_eq!(parsed.rendezvous.split('-').count(), 3);
            assert_eq!(parsed.password.split('-').count(), count - 3);
        }
        for input in [
            "abacus-abdomen-abdominal",
            "abacus-abdomen-abdominal-abide-abiding-notaword",
            "",
        ] {
            assert!(parse_code(input).is_err(), "{input}");
        }
        let error = parse_code("abacus-abdomen-abdominal-abide-abiding-secretz")
            .err()
            .unwrap()
            .to_string();
        assert!(!error.contains("secretz"));
    }

    #[test]
    fn rendezvous_keys_are_deterministic_and_distinct() {
        assert_eq!(
            rendezvous_key("abacus-abdomen-abdominal").public(),
            rendezvous_key("abacus-abdomen-abdominal").public()
        );
        assert_ne!(
            rendezvous_key("abacus-abdomen-abdominal").public(),
            rendezvous_key("abacus-abide-abiding").public()
        );
    }

    /// Frozen vector: a change to an expected value is a wire-format change
    /// (machines on either side of it can no longer find each other).
    #[test]
    fn rendezvous_words_derive_a_fixed_endpoint_id() {
        for (words, id) in [
            (
                "abacus-abdomen-abdominal",
                "6531cabcbb64657cc73f48c9bbfdd3c211be0327910984c4a1ee6b66e87a159a",
            ),
            (
                "abacus-abide-abiding",
                "cac53b2a88c2b16ab39fec1af16a6f3377085b0c22ae03e2648c7f72ab633102",
            ),
            (
                "abacus-abacus-abacus",
                "0a59ad0b39cf249e9b8cd4f8c3af5c4c345307836119ef0f52b15781ebb4357d",
            ),
        ] {
            let key = rendezvous_key(words);
            assert_eq!(hex::encode(key.public().as_bytes()), id, "{words}");
        }
        // The same words reach the same endpoint whatever follows them.
        let code = parse_code("Abacus abdomen ABDOMINAL abide-abiding-abacus").unwrap();
        assert_eq!(code.rendezvous, "abacus-abdomen-abdominal");
        assert_eq!(&*code.password, "abide-abiding-abacus");
    }
}
