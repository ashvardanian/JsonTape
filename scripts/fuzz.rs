//! A dependency-free parser stress harness.
//!
//! Build and run it from the repository root:
//!
//! ```text
//! cargo build
//! rustc --edition=2021 scripts/fuzz.rs --extern jsontape=target/debug/libjsontape.rlib \
//!     -L dependency=target/debug/deps -o /tmp/jsontape-fuzz
//! /tmp/jsontape-fuzz 100000 512 0x9E3779B97F4A7C15
//! ```
//!
//! Arguments are `cases`, `maximum input length`, and an optional hexadecimal
//! seed.
//!
//! Every accepted input is checked against several invariants, needing no
//! external oracle: the owned and zero-copy parsers must agree on acceptance and
//! serialize identically, the re-emitted text must be valid strict JSON that
//! round-trips by value and serializes to a fixed point, and a comment-preserving
//! JSON5 parse must round-trip its comments. The harness fails on any panic or
//! violated invariant.
//!
//! An optional strict-mode cross-check against `serde_json` compiles only under
//! `--cfg 'feature="fuzz_differential"'` with `--extern serde_json=...`, so the
//! standalone build above stays dependency-free. It is a signal, not an oracle:
//! the two libraries differ on some edges such as nesting limits.

use std::env;
use std::panic::{self, AssertUnwindSafe};

use jsontape::{parse, parse_with, view, view_with, FormatOptions, ParseOptions};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut value = self.0;
        value ^= value << 7;
        value ^= value >> 9;
        value ^= value << 8;
        self.0 = value;
        value
    }

    fn byte(&mut self) -> u8 {
        self.next() as u8
    }

    fn below(&mut self, upper: usize) -> usize {
        (self.next() as usize) % upper.max(1)
    }
}

fn parse_argument(position: usize, default: usize) -> usize {
    env::args()
        .nth(position)
        .map(|argument| argument.parse().expect("argument must be a positive integer"))
        .unwrap_or(default)
}

fn parse_seed() -> u64 {
    // Parsed separately from `parse_argument`: the seed is hexadecimal (radix 16),
    // not a decimal count.
    env::args()
        .nth(3)
        .map(|argument| u64::from_str_radix(argument.trim_start_matches("0x"), 16).expect("seed must be hexadecimal"))
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
}

fn arbitrary(rng: &mut Rng, maximum: usize) -> Vec<u8> {
    // Bind the target length: `with_capacity` may round up, so filling to
    // `capacity()` could overshoot `maximum`.
    let length = rng.below(maximum.saturating_add(1));
    (0..length).map(|_| rng.byte()).collect()
}

fn mutate_jsonish(rng: &mut Rng, maximum: usize) -> Vec<u8> {
    // Byte-string literals must be ASCII, so multi-byte UTF-8 seeds use a normal
    // string with `.as_bytes()`, and the JSON5 escape seed stays ASCII.
    const SEEDS: &[&[u8]] = &[
        br#"null"#,
        br#"[true,false,null,0,-1,1.5,1e9]"#,
        br#"{"key":"value","nested":[{},[]]}"#,
        br#"{name:'json5',hex:0xFF,ratio:.5,trail:5.,}"#,
        br#"{"big":123456789012345678901234567890,"tiny":1e-400,"huge":1e400}"#,
        br#"['\x41\v\0\'',"\t\r\n\b\f\/"]"#,
        br#"[[[[[[[[[[0]]]]]]]]]]"#,
        br#"{ a: 1, // trailing
  b: [2, /* mid */ 3], /* tail */ }"#,
        br#"{"dup":1,"dup":2,"dup":3}"#,
        "/* open */ [ // line\n \"\u{1F600}\" ]".as_bytes(),
        "[\"café\",\"\u{1D11E}\",\"日本語\"]".as_bytes(),
    ];
    let mut bytes = SEEDS[rng.below(SEEDS.len())].to_vec();
    let changes = 1 + rng.below(16);
    for _ in 0..changes {
        match rng.below(3) {
            0 if !bytes.is_empty() => {
                let position = rng.below(bytes.len());
                bytes[position] = rng.byte();
            }
            1 if bytes.len() < maximum => bytes.insert(rng.below(bytes.len() + 1), rng.byte()),
            _ if !bytes.is_empty() => {
                bytes.remove(rng.below(bytes.len()));
            }
            _ => {}
        }
    }
    bytes
}

/// Checks that the owned and zero-copy parsers agree on `source` under `options`,
/// and, when they accept it, that serialization and round-tripping hold.
fn check_consistency(source: &[u8], options: &ParseOptions, label: &str) {
    let owned = parse_with(source, options);
    let borrowed = view_with(source, options);
    if owned.is_ok() != borrowed.is_ok() {
        panic!(
            "{label} acceptance mismatch: owned={:?}, view={:?}, input={source:?}",
            owned.err(),
            borrowed.err(),
        );
    }
    let (Ok(document), Ok(borrowed)) = (&owned, &borrowed) else {
        return;
    };

    // The owned form re-emits valid strict JSON, equal by value, and idempotent.
    let owned_text = document.to_string();
    match parse(owned_text.as_bytes()) {
        Ok(reparsed) => {
            if reparsed != *document {
                panic!("{label} round-trip value mismatch: text={owned_text:?}, input={source:?}");
            }
            if reparsed.to_string() != owned_text {
                panic!("{label} serialization not idempotent: text={owned_text:?}");
            }
        }
        Err(error) => {
            panic!("{label} re-emitted output rejected: {error:?}, text={owned_text:?}");
        }
    }

    // The view serializes to valid strict JSON of the same value. It may differ
    // byte-for-byte from the owned form, since the view preserves the original
    // escaping while the owned tree normalizes it, so compare by value.
    let view_text = borrowed.to_json_string(source);
    match parse(view_text.as_bytes()) {
        Ok(reparsed) => {
            if reparsed != *document {
                panic!("{label} owned/view value mismatch: owned={owned_text:?}, view={view_text:?}, input={source:?}");
            }
        }
        Err(error) => {
            panic!("{label} view output rejected: {error:?}, text={view_text:?}");
        }
    }
}

/// Checks that a comment-preserving JSON5 parse round-trips its comments by value.
fn check_comment_round_trip(source: &[u8]) {
    let options = ParseOptions::json5().preserve_comments(true);
    let Ok(document) = parse_with(source, &options) else {
        return;
    };
    let commented = document.to_string_with(FormatOptions::pretty().with_comments(true));
    match parse_with(commented.as_bytes(), &options) {
        Ok(reparsed) => {
            if reparsed != document {
                panic!("comment round-trip value mismatch: text={commented:?}, input={source:?}");
            }
        }
        Err(error) => panic!("commented output rejected: {error:?}, text={commented:?}"),
    }
}

/// Optional strict-acceptance cross-check against `serde_json`. A signal only:
/// mismatches are reported, not fatal, since the two libraries differ on edges
/// like nesting depth. Compiled only under the `fuzz_differential` cfg.
#[cfg(feature = "fuzz_differential")]
fn report_serde_json_divergence(source: &[u8]) {
    let ours = parse(source).is_ok();
    let theirs = serde_json::from_slice::<serde_json::Value>(source).is_ok();
    if ours != theirs {
        eprintln!("differential: jsontape={ours}, serde_json={theirs}, input={source:?}");
    }
}

#[cfg(not(feature = "fuzz_differential"))]
fn report_serde_json_divergence(_source: &[u8]) {}

fn check_case(source: &[u8], strict: &ParseOptions, json5: &ParseOptions) {
    check_consistency(source, strict, "strict");
    check_consistency(source, json5, "json5");
    check_comment_round_trip(source);
    report_serde_json_divergence(source);
}

fn main() {
    let cases = parse_argument(1, 100_000);
    let maximum = parse_argument(2, 512);
    let seed = parse_seed();
    let mut rng = Rng(seed);
    let strict = ParseOptions::default();
    let json5 = ParseOptions::json5();

    for case in 0..cases {
        let source = if case % 2 == 0 {
            arbitrary(&mut rng, maximum)
        } else {
            mutate_jsonish(&mut rng, maximum)
        };
        if panic::catch_unwind(AssertUnwindSafe(|| check_case(&source, &strict, &json5))).is_err() {
            eprintln!("failure at case {case} with seed 0x{seed:016X}, input={source:?}");
            std::process::exit(1);
        }
        if case != 0 && case % 10_000 == 0 {
            eprintln!("completed {case} cases");
        }
    }

    // Keep the default strict entry points exercised too.
    assert!(parse(br#"{"complete":true}"#).is_ok());
    assert!(view(br#"{"complete":true}"#).is_ok());
    eprintln!("completed {cases} cases with seed 0x{seed:016X}");
}
