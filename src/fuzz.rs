//! Deterministic inputs for the robustness tests of the parsers: a seeded
//! generator, no dependency, no cargo-fuzz. Every run feeds the same inputs.
use anyhow::Result;
use serde::Serialize;
use serde_json::Value;

/// SplitMix64: small, seeded, good enough to spread mutations.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..bound` (`bound` > 0).
    pub fn below(&mut self, bound: usize) -> usize {
        (self.next_u64() % bound as u64) as usize
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

const NUMBERS: &[&str] = &[
    "18446744073709551616",
    "4294967296",
    "-1",
    "1e999",
    "0.5",
    "99999999999999999999999999999999",
    "0",
    "255",
    "65536",
];
const VALUES: &[&str] = &[
    "null",
    "true",
    "[]",
    "{}",
    "[[[[[]]]]]",
    r#"{"a":{"b":[{"c":null}]}}"#,
    r#""é\ud800""#,
    r#""\u0000""#,
    "\"\u{202e}\u{1F600}\"",
    r#""""#,
];
const TEXTS: &[&str] = &[
    "\u{e9}",
    "\u{202e}",
    "\u{1F600}",
    "\0",
    "\\ud800",
    "\\u0000",
    "\u{feff}",
    "\r\n",
];

fn mutate(base: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut bytes = base.to_vec();
    for _ in 0..=rng.below(3) {
        match rng.below(9) {
            0 if !bytes.is_empty() => {
                let index = rng.below(bytes.len());
                bytes[index] ^= 1 << rng.below(8);
            }
            1 => bytes.truncate(rng.below(bytes.len() + 1)),
            2 if !bytes.is_empty() => {
                let start = rng.below(bytes.len());
                let end = start + rng.below(bytes.len() - start + 1);
                let copy = bytes[start..end].to_vec();
                let at = rng.below(bytes.len() + 1);
                bytes.splice(at..at, copy);
            }
            3 | 4 => {
                // Replace a whole value with a number or another JSON value.
                let colons: Vec<usize> = (0..bytes.len()).filter(|&i| bytes[i] == b':').collect();
                if !colons.is_empty() {
                    let start = colons[rng.below(colons.len())] + 1;
                    let end = (start..bytes.len())
                        .find(|&i| matches!(bytes[i], b',' | b'}'))
                        .unwrap_or(bytes.len());
                    let value = if rng.below(2) == 0 {
                        *rng.pick(NUMBERS)
                    } else {
                        *rng.pick(VALUES)
                    };
                    bytes.splice(start..end, value.bytes());
                }
            }
            5 => {
                let at = rng.below(bytes.len() + 1);
                bytes.splice(at..at, rng.pick(TEXTS).bytes());
            }
            6 => {
                // A duplicated or unknown field right after the opening brace.
                if let Some(open) = bytes.iter().position(|&b| b == b'{') {
                    let field: &[u8] = match rng.below(3) {
                        0 => br#""type":"hello","#,
                        1 => br#""x":1,"#,
                        _ => br#""type":"type","#,
                    };
                    bytes.splice(open + 1..open + 1, field.iter().copied());
                }
            }
            7 => {
                let depth = 1 + rng.below(300);
                let mut nested = vec![b'['; depth];
                nested.extend_from_slice(&bytes);
                nested.extend(std::iter::repeat_n(b']', depth));
                bytes = nested;
            }
            _ if !bytes.is_empty() => {
                let index = rng.below(bytes.len());
                bytes[index] = rng.next_u64() as u8;
            }
            _ => {}
        }
    }
    bytes
}

/// `valid` itself, mutations of it and unstructured bytes: `count` inputs.
pub fn inputs(valid: &[&str], count: usize, seed: u64) -> Vec<Vec<u8>> {
    let mut rng = Rng::new(seed);
    let mut all: Vec<Vec<u8>> = valid.iter().map(|text| text.as_bytes().to_vec()).collect();
    while all.len() < count {
        if rng.below(8) == 0 {
            let length = rng.below(200);
            all.push((0..length).map(|_| rng.next_u64() as u8).collect());
        } else {
            let base = valid[rng.below(valid.len())].as_bytes();
            all.push(mutate(base, &mut rng));
        }
    }
    // Deeper than any recursion limit, and oversized numbers and strings.
    all.push([b"[".repeat(100_000), b"]".repeat(100_000)].concat());
    all.push(format!("{{\"type\":\"{}\"}}", "x".repeat(70_000)).into_bytes());
    all
}

/// Feeds a length-prefixed JSON decoder `count` inputs derived from `valid`
/// and checks what it accepts. `tags` lists each message type with the keys
/// it may carry. The decoder must not panic; every valid message must be
/// accepted; an accepted input must be a JSON object of a known type that
/// carries only its keys; and re-encoding what was decoded must decode again
/// to the same bytes.
pub fn check_decoder<T: Serialize>(
    valid: &[&str],
    tags: &[(&str, &[&str])],
    decode: impl Fn(&[u8]) -> Result<T>,
) {
    let all = inputs(valid, 4000, 0x1805_2026);
    let (mut accepted, mut refused) = (0, 0);
    for input in &all {
        let Ok(message) = decode(input) else {
            refused += 1;
            continue;
        };
        accepted += 1;
        let value: Value = serde_json::from_slice(input).expect("accepted input is JSON");
        let object = value.as_object().expect("accepted input is an object");
        let tag = object["type"].as_str().expect("accepted type is text");
        let keys = tags
            .iter()
            .find(|(name, _)| *name == tag)
            .unwrap_or_else(|| panic!("accepted an unknown type {tag}"))
            .1;
        for key in object.keys() {
            assert!(
                key == "type" || keys.contains(&key.as_str()),
                "accepted the unknown field {key} of {tag}"
            );
        }
        let encoded = serde_json::to_vec(&message).unwrap();
        let again = decode(&encoded).expect("re-encoded message decodes");
        assert_eq!(serde_json::to_vec(&again).unwrap(), encoded);
    }
    for text in valid {
        assert!(decode(text.as_bytes()).is_ok(), "{text}");
    }
    assert!(accepted >= valid.len(), "{accepted} accepted");
    assert!(refused > all.len() / 4, "only {refused} refused");
}
