# JsonTape

![JsonTape banner](https://github.com/ashvardanian/ashvardanian/blob/master/repositories/JsonTape.png?raw=true)

__JsonTape__ is a minimalistic, allocator-aware JSON and JSON5 parser in pure Rust.
It's built to address the high memory usage & fragmentation as well as the high traversal latency in typical DOM implementations.
Its sibling project — [StringTape] — does the same for Arrow-like string collections.

Every container allocates through any [`allocator_api2`] allocator, so a whole document can live in a bump arena and free in one step.
The parser is strictly RFC 8259-conformant by default, never panics on bad input, runs on `no_std`, and opts into JSON5 leniency per parse.

## Two Flavors, One Scanner

JsonTape parses into either of two document types, sharing one strict scanner:

- __`Json`__ — owned and mutable, the default.
  Strings are eagerly unescaped into the document's allocator at parse time, so `as_str` needs no source and the tree can be edited freely with `insert`, `remove`, `push`, `pop`, `get_mut`, or built from scratch.
  Produce one with `parse` or `parse_in`.
- __`JsonView`__ — immutable and zero-copy.
  String values and object keys stay as `Span`s into the original source; nothing is decoded.
  Key lookups compare against those still-escaped spans with an escape-aware, allocation-free comparison.
  Produce one with `view` or `view_in`.

## Usage

Owned document — decoded strings, freely mutable, no source needed:

```rust
use jsontape::{parse, Json};

let mut document = parse(br#"{ "greeting": "a\nb", "peers": [1, 2, 3] }"#).unwrap();

// Navigate with indexing; a miss anywhere in the chain yields Null, not a panic.
assert_eq!(document["greeting"].as_str(), Some("a\nb"));
assert_eq!(document["peers"][0].as_u64(), Some(1));
assert!(document["absent"].is_null());

// Or look things up fallibly, by key or by position, and edit in place.
assert_eq!(document.get("peers").and_then(|v| v.get(2)).and_then(|v| v.as_u64()), Some(3));
assert_eq!(document.pointer("/peers/1").and_then(|v| v.as_u64()), Some(2));
document.insert("count", Json::from(3u64));
```

Zero-copy view — keep the source alive and resolve against it:

```rust
use jsontape::view;

let source = br#"{ "metric": "ip", "nodes": 20000000 }"#;
let document = view(source).unwrap();

assert_eq!(document.get(source, "metric").and_then(|v| v.as_str(source)), Some("ip"));
assert_eq!(document.get(source, "nodes").and_then(|v| v.as_u64()), Some(20_000_000));
```

For a source-bound view that cannot accidentally be resolved against different
bytes, use `view_bound` and navigate without passing the source again:

```rust
use jsontape::view_bound;

let document = view_bound(br#"{ "a": [10, 20] }"#).unwrap();
assert_eq!(document.get("a").get(1).as_u64(), Some(20));
```

### Strict by Default, JSON5 by Opt-In

The default is strict RFC 8259.
`ParseOptions` turns on individual JSON5 extensions, or a whole preset, passed to the `*_with` parsers:

```rust
use jsontape::{parse_with, ParseOptions};

// The jsonc preset: line and block comments plus trailing commas.
let config = br#"{
    "port": 8080, // default
    "hosts": ["a", "b",], /* note the trailing comma */
}"#;
let document = parse_with(config, &ParseOptions::jsonc()).unwrap();
assert_eq!(document["port"].as_u64(), Some(8080));

// The json5 preset also allows unquoted keys, single quotes, and extended numbers.
let json5 = br#"{ name: 'tape', ratio: .5, mask: 0xFF }"#;
let document = parse_with(json5, &ParseOptions::json5()).unwrap();
assert_eq!(document["name"].as_str(), Some("tape"));
assert_eq!(document["mask"].as_u64(), Some(255));

// Whatever lenient input goes in, strict JSON comes out.
assert_eq!(document.to_string(), r#"{"name":"tape","ratio":0.5,"mask":255}"#);
```

`ParseOptions` also sets the nesting limit and the duplicate-key policy — last-wins by default, or first-wins, reject, or keep-all.

For untrusted inputs, `ParseLimits` can additionally cap values, entries per
container, string bytes, and comment bytes.
These limits are opt-in; the
default accepts any in-memory valid document subject to the nesting limit.

### Comment-Preserving Round Trip

With `preserve_comments`, the owned tree keeps comments and can write them back.
This is the one feature `serde_json` and the `json5` crate both drop.

```rust
use jsontape::{parse_with, FormatOptions, ParseOptions};

let source = b"{\n  // the service port\n  \"port\": 8080 // default\n}";
let options = ParseOptions::json5().preserve_comments(true);
let document = parse_with(source, &options).unwrap();

// Read the comments back off the tree.
assert_eq!(document.leading_comments(0).next().unwrap().text(), " the service port");

// Or serialize them back into place, byte for byte.
let formatted = document.to_string_with(FormatOptions::pretty().with_comments(true));
assert_eq!(formatted.as_bytes(), source);
```

### Formatting

`FormatOptions` drives serialization, from compact to width-aware wrapping:

```rust
use jsontape::{parse, FormatOptions};

let document = parse(br#"{"a":[1,2],"b":{}}"#).unwrap();

assert_eq!(document.to_string(), r#"{"a":[1,2],"b":{}}"#);
assert_eq!(
    document.to_string_with(FormatOptions::pretty_width(20)),
    "{\n  \"a\": [1, 2],\n  \"b\": {}\n}",
);
```

### Errors With a Location

A syntax fault carries a byte offset, resolved to a line and column on demand:

```rust
use jsontape::parse;

let source = b"{\n  \"a\": ,\n}";
let error = parse(source).unwrap_err();
let location = error.location(source).unwrap();
assert_eq!((location.line, location.column), (2, 8));
```

### Lossless Numbers

An integer wider than 64 bits, or a value outside the `f64` range, is kept as its exact decimal text rather than rounded:

```rust
use jsontape::parse;

let document = parse(b"123456789012345678901234567890").unwrap();
assert_eq!(document.to_string(), "123456789012345678901234567890");
```

### Optional Serde

Enable the `serde` feature for `Serialize` and `Deserialize`, plus `to_value` and `from_value` that convert any type to and from a document without a text round trip.

## API Tour

- __Parse__ — `parse`, `parse_in`, `parse_with`, `parse_in_with`; `view`, `view_in`, `view_with`.
- __Navigate__ — indexing with `[]`, `get`, `get_mut`, `pointer`; `iter`, `entries`, `keys`, `values`, `values_mut`.
- __Inspect__ — `as_bool`, `as_i64`, `as_u64`, `as_f64`, `as_str`, `as_array`, `as_object`, `as_number_str`; `is_null`, `is_number`, `is_string`, `is_array`, `is_object`, `len`, `is_empty`.
- __Mutate__ — `insert`, `remove`, `push`, `pop`, `get_mut`, `as_array_mut`, `as_object_mut`; build with `object_in`, `array_in`, `string_in`, `From`, and `FromIterator`.
- __Comments__ — `has_comments`, `leading_comments`, `trailing_comments`, `tail_comments`.
- __Serialize__ — `to_string`, `to_string_pretty`, `to_string_with`, `write_json`, `write_json_with`; the `FormatOptions` presets `compact`, `pretty`, `pretty_width`, with `with_indent`, `with_max_width`, and `with_comments`.
- __Errors__ — `JsonError`, `SyntaxKind`, and `JsonError::location` yielding a `Location`.
- __View cursor__ — `JsonView::bind` returns a `Resolved` that carries its source, so indexing and `Display` need no extra argument.

## Where It Wins

JsonTape is positioned on its memory model and safety, not on raw SIMD throughput.

- __Embedded and `no_std`__ — a full mutable DOM in a bump arena, where `serde_json`'s `Value` cannot follow and SIMD parsers need `std`.
  The parse path uses fallible allocation and returns an error instead of aborting, and never panics on hostile input.
  The library contains no `unsafe` at all, enforced by `forbid(unsafe_code)` rather than by convention, and CI builds it for a bare-metal `thumbv7em-none-eabihf` target so the `no_std` claim is checked and not merely asserted.
- __WASM and the edge__ — small and dependency-free beyond the allocator shim, and deterministic, with a configurable depth limit that bounds the stack on adversarial nesting.
- __Blockchain and financial__ — integer amounts wider than `u64` and decimals outside the `f64` range survive verbatim, so money never silently rounds, under deterministic `no_std` execution.
- __Per-request arenas and telemetry__ — parse a payload into an arena, read it, and reclaim it all in one `O(1)` teardown; or take the zero-copy `JsonView` and never decode at all.

A blunt, honest comparison, where ● is first-class, ◐ is partial or behind a feature, and ○ is unsupported:

| Capability                          | `serde_json` | `json5` | `jsontape` |
| :---------------------------------- | -----------: | ------: | ---------: |
| Strict RFC 8259 by default          |            ● |       ○ |          ● |
| JSON5 leniency, opt-in              |            ○ |       ● |          ● |
| Comment-preserving round trip       |            ○ |       ○ |          ● |
| Configurable duplicate-key policy   |            ○ |       ○ |          ● |
| Source key order preserved          |            ◐ |       ○ |          ● |
| Zero-copy borrowed DOM              |            ○ |       ○ |          ● |
| Custom [`allocator_api2`] allocator |            ○ |       ○ |          ● |
| `O(1)` arena teardown               |            ○ |       ○ |          ● |
| `no_std` with `alloc`               |            ◐ |       ○ |          ● |
| Fallible allocation on parse        |            ○ |       ○ |          ● |
| No `unsafe` anywhere in the library |            ○ |       ● |          ● |
| Lossless numbers past 64 bits       |            ◐ |       ○ |          ● |
| Width-based pretty-printing         |            ○ |       ○ |          ● |
| Serde derive to and from user types |            ● |       ● |          ◐ |
| Streaming reader and writer         |            ● |       ○ |          ○ |
| Ecosystem maturity and adoption     |            ● |       ◐ |          ○ |

Peak throughput on string-dense documents still belongs to SIMD parsers like [`simd-json`], and JsonTape trades that for its memory model, safety, and `no_std` reach.
It is worth knowing how narrow the gap is, though: on number-dense and object-key-dense documents the arena configurations measured below come out ahead, and on an NDJSON stream the zero-copy view lands within a few percent.

## Allocators

Every container allocates through the `allocator` you pass to `parse_in` or `view_in`, any [`allocator_api2`] allocator.
With the default global heap each array and object is a separate allocation scattered across the heap.
Pass a bump or slab arena instead and the whole document is packed into one contiguous region that frees in `O(1)` when the arena is dropped — parse a large payload, read it, and reclaim it all at once.
This is the closest the pointer-tree design gets to a flat tape; the per-node `Vec`s still grow by doubling, so an arena keeps some slack, but allocation and teardown are cheap and fragmentation-free.
See the [`parse_in`] docs for a worked custom-allocator example.

Bring any arena crate that implements `allocator_api2::Allocator`.
To process a stream of documents through one arena, reset it between them:

```rust
let mut arena = bump_scope::Bump::new();
for input in &documents {
    let document = jsontape::parse_in(input, &arena)?;
    consume(&document);   // last use of the document
    arena.reset();        // takes &mut self, so the borrow checker proves the document is gone
}
```

Because `reset` takes `&mut self` and every allocation borrows the arena with `&self`, the compiler forbids resetting while any document is still alive — arena reuse with no use-after-free footgun.

## JSON5 Support and Its Boundary

JsonTape implements the widely-used JSON5 extensions as `ParseOptions` flags under the `strict`, `jsonc`, and `json5` presets:
line and block comments, trailing commas, unquoted keys, single-quoted strings, hexadecimal integers, a leading plus, leading and trailing decimal points, and the `Infinity` and `NaN` literals.
The extended string escapes — `\'`, `\v`, `\0`, `\xHH`, and backslash-newline line continuations — do not have a flag of their own; they ride on `allow_single_quotes`, so enabling that also accepts them inside double-quoted strings.

A few edges are deliberately out of scope, so the crate stays a single strict-scanner file:

- Unquoted keys are ASCII identifiers only, with no `\u` escapes or non-ASCII letters.
- Trivia recognizes ASCII whitespace only, not the extra Unicode spaces JSON5 allows.
- An unknown escape is rejected rather than passed through as its own character.
- `Infinity` and `NaN` parse as non-finite floats and serialize back to `null`, since strict JSON has no literal for them, while big numbers round-trip losslessly.
- A decimal-point number beyond the `f64` range is rejected rather than emitted in a form that would not be valid strict JSON.
- A hexadecimal integer wider than 64 bits is rejected rather than preserved. Decimal integers of any width round-trip losslessly; only the `0x` form is capped, so nothing is silently rounded — it errors instead.
- Comment fidelity is leading, trailing, and tail placement; blank lines, comment indentation, and interior comments between a key and its value are not preserved.

## Benchmarks

A criterion suite lives at [`scripts/bench.rs`], comparing the owned, view, source-bound, and reused-arena parsers against `serde_json`, [`simd-json`], and the [`json5`] crate across string, number, object, and nested workloads.
Run it with `cargo bench`; [`simd-json`] is always included as a fully-owned-DOM throughput ceiling.
The table below is indicative — one machine, one run — so read it as relative, not absolute, and re-run on your own hardware.

Every benchmark id names the output shape it produces, because the peers do not all build the same thing.
Each row fully parses and validates its input — UTF-8, escapes, structure — and produces a fully navigable, non-lazy document with last-wins duplicate keys; what differs is what comes out the other side:

| id                           | DOM           | mutable | strings                    | key order         | numbers                    |
| :--------------------------- | :------------ | :------ | :------------------------- | :---------------- | :------------------------- |
| `jsontape-owned-mut-dom`     | owned         | yes     | decoded at parse           | source            | i64/u64/f64 + lossless big |
| `jsontape-view-imm-spans`    | zero-copy     | no      | validated, decode deferred | source            | i64/u64/f64 + lossless big |
| `serde-json-owned-mut-dom`   | owned         | yes     | decoded at parse           | sorted (BTreeMap) | i64/u64/f64                |
| `simd-json-owned-mut-dom`    | owned         | yes     | decoded at parse           | hashed            | i64/u64/f64                |
| `simd-json-borrowed-cow-dom` | borrows input | yes     | decoded in place, `Cow`    | hashed            | i64/u64/f64                |
| `json5-crate-serde-mut-dom`  | owned         | yes     | decoded at parse           | sorted (BTreeMap) | everything as `f64`        |

So the owned rows read against each other, and `jsontape-view-imm-spans` reads against `simd-json-borrowed-cow-dom`.
Two asymmetries are worth holding in mind.
JsonTape is alone in preserving source key order — `serde_json` sorts and `simd-json` hashes — so it runs an explicit dedup pass where the peers get deduplication free as a side effect of their map insert; the matching comparison would be `serde_json` with its `preserve_order` feature, which is slower than the `BTreeMap` default measured here.
And the JsonTape views validate escapes at parse but keep strings escaped, deferring the decode to first use, where `simd-json` unescapes into its input buffer eagerly.

To benchmark real-world documents, download the canonical datasets into a `data/` directory and point the suite at it with the `JSONTAPE_DATA` variable.
Single JSON documents (`*.json`) and NDJSON / JSON Lines streams (`*.ndjson`, `*.jsonl`) are both recognized:

```sh
mkdir -p data
# Single documents (serde-rs/json-benchmark, identical to simdjson's copies):
for f in twitter canada citm_catalog; do
  curl -sSL "https://raw.githubusercontent.com/serde-rs/json-benchmark/master/data/$f.json" -o "data/$f.json"
done
# NDJSON stream (simdjson's streaming-benchmark file):
curl -sSL "https://raw.githubusercontent.com/simdjson/simdjson/master/jsonexamples/amazon_cellphones.ndjson" -o "data/amazon_cellphones.ndjson"
JSONTAPE_DATA=data cargo bench --bench bench -- parse-data parse-ndjson
```

`canada.json` exercises the number path, `twitter.json` the zero-copy view over unicode strings, and `citm_catalog.json` object-key handling; the NDJSON `parse-ndjson` group streams every record through one reused bump arena, the flagship allocator-reuse case.
Any other `.ndjson`/`.jsonl` works too — for a larger real stream, a GitHub Archive hour: `curl -L https://data.gharchive.org/2015-01-01-15.json.gz | gunzip > data/gharchive.ndjson`.
The `data` directory is git-ignored.

Parse throughput in MiB/s (higher is better) on an Intel Xeon Platinum 8468, criterion defaults pinned to one core, with `float_roundtrip` on `serde_json` for a fair number comparison and `simd-json` reusing its `Buffers` (its analogue to arena reuse).
The `-arena` rows reuse one bump arena; on the NDJSON stream they reset between records, the true one-arena-many-documents case, and `amazon` throughput counts record bytes only.
Rows are split by output shape, because comparing across shapes is not meaningful — bold marks the winner within each class.

Owned, mutable DOM — strings decoded into owned storage, document independent of the input:

| Parser                         |  canada |    citm | twitter | amazon (ndjson) |
| :----------------------------- | ------: | ------: | ------: | --------------: |
| `jsontape-owned-mut-dom`       |     138 |     348 |     191 |             379 |
| `jsontape-owned-mut-dom-arena` | __213__ | __573__ |     299 |             419 |
| `serde-json-owned-mut-dom`     |     158 |     382 |     208 |             583 |
| `simd-json-owned-mut-dom`      |     208 |     360 | __320__ |         __758__ |
| `json5-crate-serde-mut-dom`    |      27 |      17 |      10 |               — |

Borrowed, zero-copy DOM — the document borrows the input buffer:

| Parser                          |  canada |    citm | twitter | amazon (ndjson) |
| :------------------------------ | ------: | ------: | ------: | --------------: |
| `jsontape-view-imm-spans`       |     146 |     556 |     361 |             738 |
| `jsontape-view-imm-spans-arena` | __235__ | __710__ |     426 |             854 |
| `jsontape-bound-view-imm-spans` |     143 |     560 |     358 |               — |
| `simd-json-borrowed-cow-dom`    |     208 |     471 | __617__ |         __889__ |

The arena rows are the configuration this crate is built for, and they take `canada` and `citm` in both classes — a scalar parser in safe Rust ahead of a SIMD one on number-dense and object-key-dense documents, because the memory model does more work here than vectorized scanning does.
`twitter` remains the column where SIMD pulls away: it is 58% string bytes across 18k short strings, which is precisely what vector compares are best at.
On the `amazon` NDJSON stream the zero-copy view now runs within a few percent of `simd-json`'s borrowed value.

The `json5` crate row is worth reading twice.
It parses the same strict documents 6–23x slower than `serde_json`, which is the honest measure of what JSON5 support usually costs — and JsonTape offers the same leniency at full speed.

## Strictness

Even in lenient mode, the scanner stays strict about what it accepts and re-emits.
Unescaped control characters, invalid `\u` escapes, and lone surrogates are always rejected, and surrogate pairs are combined.
Both flavors validate UTF-8 and escapes at parse time; `JsonView` then keeps the raw spans while `Json` stores the decoded strings.
Parsing untrusted input uses fallible allocation and returns a `JsonError` rather than aborting, while the build-from-code mutators allocate infallibly, like the standard collections.

A leading UTF-8 byte order mark is rejected rather than skipped.
RFC 8259 §8.1 permits either choice, but plenty of tools emit one, so strip it before parsing if your input may carry it.

The default nesting limit of 128 bounds the stack, but note where the bound actually comes from: parsing itself is iterative over an explicit frame stack and never recurses, so it is dropping, formatting, and comparing a document that recurse.
Raising `max_depth` far above the default therefore moves the risk out of the parser and into those operations.

## License

Apache-2.0.

[StringTape]: https://github.com/ashvardanian/StringTape
[`scripts/bench.rs`]: scripts/bench.rs
[`allocator_api2`]: https://docs.rs/allocator-api2
[`parse_in`]: https://docs.rs/jsontape/latest/jsontape/fn.parse_in.html
[`simd-json`]: https://docs.rs/simd-json
[`json5`]: https://docs.rs/json5
[`bump-scope`]: https://docs.rs/bump-scope
[`blink-alloc`]: https://docs.rs/blink-alloc
