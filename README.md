# JsonTape

![JsonTape banner](https://github.com/ashvardanian/ashvardanian/blob/master/repositories/JsonTape.png?raw=true)

__JsonTape__ is a minimalistic, allocator-aware JSON and JSON5 parser in pure Rust.
It exists to fix what a typical DOM costs in memory and fragmentation: every container allocates through any [`allocator_api2`] allocator, so a whole document can live in one bump arena and free in a single step.
Its sibling project — [StringTape] — does the same for Arrow-like string collections.

- __Allocator-aware__ — parse a payload into an arena, read it, and reclaim it all in one `O(1)` teardown; or take the zero-copy view and never decode at all.
- __Safe by construction__ — no `unsafe` anywhere in the library, enforced by `forbid(unsafe_code)` rather than by convention.
  The parse path allocates fallibly and returns an error instead of aborting, and never panics on hostile input.
- __Genuinely `no_std`__ — CI builds the crate for a bare-metal `thumbv7em-none-eabihf` target, so the claim is checked rather than asserted.
- __Lossless numbers__ — integers wider than `u64` and decimals outside the `f64` range survive verbatim, so money never silently rounds.
- __Round-trips comments and layout__ — comments survive a parse and come back byte for byte, and the printer wraps to a column budget rather than one element per line.
  That is enough to build a config-file editor or a formatter on top.
- __Strict by default, JSON5 on request__ — RFC 8259 conformant unless you opt in, with the lenient extensions running on the same scanner at the same speed.

## Quick Start

Two document types share one strict scanner.
`Json` is owned and mutable: strings are unescaped into the document's allocator at parse time, so `as_str` needs no source and the tree can be edited freely.

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

`JsonView` is the immutable, zero-copy counterpart: string values and object keys stay as `Span`s into the original source and nothing is decoded, so key lookups compare against those still-escaped spans with an escape-aware, allocation-free comparison.
Keep the source alive and resolve against it:

```rust
use jsontape::view;

let source = br#"{ "metric": "ip", "nodes": 20000000 }"#;
let document = view(source).unwrap();

assert_eq!(document.get(source, "metric").and_then(|v| v.as_str(source)), Some("ip"));
assert_eq!(document.get(source, "nodes").and_then(|v| v.as_u64()), Some(20_000_000));
```

For a source-bound view that cannot accidentally be resolved against different bytes, use `view_bound` and navigate without passing the source again:

```rust
use jsontape::view_bound;

let document = view_bound(br#"{ "a": [10, 20] }"#).unwrap();
assert_eq!(document.get("a").get(1).as_u64(), Some(20));
```

## Feature Tour

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

For untrusted inputs, `ParseLimits` can additionally cap values, entries per container, string bytes, and comment bytes.
These limits are opt-in; the default accepts any in-memory valid document subject to the nesting limit.

### Comment-Preserving Round Trip

With `preserve_comments`, the owned tree keeps comments and can write them back byte for byte.
Most JSON parsers treat comments as trivia and discard them, so a document that goes through them loses its annotations.

```rust
use jsontape::{parse_with, FormatOptions, ParseOptions};

let source = br#"{
  // the service port
  "port": 8080 // default
}"#;
let options = ParseOptions::json5().preserve_comments(true);
let document = parse_with(source, &options).unwrap();

// Read the comments back off the tree.
assert_eq!(document.leading_comments(0).next().unwrap().text(), " the service port");

// Or serialize them back into place, byte for byte.
let formatted = document.to_string_with(FormatOptions::pretty().with_comments(true));
assert_eq!(formatted.as_bytes(), source);
```

`formatted` is the input again, byte for byte — both comments back in their original places, one leading and one trailing.
Drop `.with_comments(true)` and the same tree serializes to plain strict JSON instead:

```json
{"port":8080}
```

### Formatting

`FormatOptions` drives serialization, from compact to width-aware wrapping:

```rust
use jsontape::{parse, FormatOptions};

let source = br#"{ "name": "tape", "tags": ["json", "json5"],
    "limits": { "depth": 128, "nodes": null },
    "ratios": [0.5, 0.25, 0.125, 0.0625] }"#;
let document = parse(source).unwrap();

let compact = document.to_string();
let wrapped = document.to_string_with(FormatOptions::pretty_width(40));
```

`compact` carries no insignificant whitespace at all:

```json
{"name":"tape","tags":["json","json5"],"limits":{"depth":128,"nodes":null},"ratios":[0.5,0.25,0.125,0.0625]}
```

`wrapped` keeps each container on one line for as long as it fits the column budget, and expands only the ones that do not.
That is why `limits` breaks across lines here while `tags` and `ratios` stay inline:

```json
{
  "name": "tape",
  "tags": ["json", "json5"],
  "limits": {
    "depth": 128,
    "nodes": null
  },
  "ratios": [0.5, 0.25, 0.125, 0.0625]
}
```

`FormatOptions::pretty()` is the familiar unconditional form, one element per line, and `FormatOptions::compact()` is the default.

### Errors With a Location

A syntax fault carries a byte offset, resolved to a line and column on demand:

```rust
use jsontape::parse;

let source = br#"{
  "a": ,
}"#;
let error = parse(source).unwrap_err();
assert_eq!(error.to_string(), "unexpected byte at byte offset 9");

let location = error.location(source).unwrap();
assert_eq!((location.line, location.column), (2, 8));
```

Line and column are computed only when asked for, so the common path of discarding the error costs nothing beyond the offset.

### Lossless Numbers

An integer wider than 64 bits, or a value outside the `f64` range, is kept as its exact decimal text rather than rounded:

```rust
use jsontape::parse;

let document = parse(b"123456789012345678901234567890").unwrap();
assert_eq!(document.to_string(), "123456789012345678901234567890");
```

### Optional Serde

Enable the `serde` feature for `Serialize` and `Deserialize`, plus `to_value` and `from_value` that convert any type to and from a document without a text round trip.

## Where It Wins

JsonTape is positioned on its memory model and safety, not on raw SIMD throughput.
It suits embedded and `no_std` targets, where `serde_json`'s `Value` cannot follow and SIMD parsers need `std`; WASM and the edge, being small, dependency-free beyond the allocator shim, and deterministic; blockchain and financial work, where amounts must not silently round; and per-request arenas, where a payload is parsed, read, and reclaimed in one step.

A blunt, honest comparison, where ● is first-class, ◐ is partial or behind a feature, and ○ is unsupported:

| Capability                          | `serde_json` | `json5` | `simd-json` | `jsontape` |
| :---------------------------------- | -----------: | ------: | ----------: | ---------: |
| Strict RFC 8259 by default          |            ● |       ○ |           ● |          ● |
| JSON5 leniency, opt-in              |            ○ |       ● |           ○ |          ● |
| Comment-preserving round trip       |            ○ |       ○ |           ○ |          ● |
| Configurable duplicate-key policy   |            ○ |       ○ |           ○ |          ● |
| Source key order preserved          |            ◐ |       ○ |           ○ |          ● |
| Zero-copy borrowed DOM              |            ○ |       ○ |           ● |          ● |
| Custom [`allocator_api2`] allocator |            ○ |       ○ |           ○ |          ● |
| `O(1)` arena teardown               |            ○ |       ○ |           ○ |          ● |
| `no_std` with `alloc`               |            ◐ |       ○ |           ○ |          ● |
| Fallible allocation on parse        |            ○ |       ○ |           ○ |          ● |
| No `unsafe` anywhere in the library |            ○ |       ● |           ○ |          ● |
| Lossless numbers past 64 bits       |            ◐ |       ○ |           ◐ |          ● |
| Width-based pretty-printing         |            ○ |       ○ |           ○ |          ● |
| Serde derive to and from user types |            ● |       ● |           ● |          ◐ |
| Streaming reader and writer         |            ● |       ○ |           ◐ |          ○ |
| Ecosystem maturity and adoption     |            ● |       ◐ |           ◐ |          ○ |

Peak throughput belongs to SIMD parsers like [`simd-json`], and JsonTape trades that for its memory model, safety, and `no_std` reach.
The gap is narrower than that trade usually implies: on the object-key-dense `citm` the arena configurations measured below come out ahead, and on an NDJSON stream the zero-copy view lands within half a percent.

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

## Conformance and Limits

Even in lenient mode the scanner stays strict about what it accepts and re-emits.
Unescaped control characters, invalid `\u` escapes, and lone surrogates are always rejected, and surrogate pairs are combined.
A leading UTF-8 byte order mark is rejected rather than skipped — RFC 8259 §8.1 permits either choice, but plenty of tools emit one, so strip it before parsing if your input may carry it.

The default nesting limit of 128 bounds the stack, but note where the bound comes from: parsing is iterative over an explicit frame stack and never recurses, so it is dropping, formatting, and comparing a document that recurse.
Raising `max_depth` far above the default therefore moves the risk out of the parser and into those operations.

JsonTape implements the widely-used JSON5 extensions as `ParseOptions` flags under the `strict`, `jsonc`, and `json5` presets:
line and block comments, trailing commas, unquoted keys, single-quoted strings, hexadecimal integers, a leading plus, leading and trailing decimal points, and the `Infinity` and `NaN` literals.
The extended string escapes — `\'`, `\v`, `\0`, `\xHH`, and backslash-newline line continuations — do not have a flag of their own; they ride on `allow_single_quotes`, so enabling that also accepts them inside double-quoted strings.

A few edges are deliberately out of scope, so the crate stays a single strict-scanner file:

- Unquoted keys are ASCII identifiers only, with no `\u` escapes or non-ASCII letters.
- Trivia recognizes ASCII whitespace only, not the extra Unicode spaces JSON5 allows.
- An unknown escape is rejected rather than passed through as its own character.
- `Infinity` and `NaN` parse as non-finite floats and serialize back to `null`, since strict JSON has no literal for them, while big numbers round-trip losslessly.
- A decimal-point number beyond the `f64` range is rejected rather than emitted in a form that would not be valid strict JSON.
- A hexadecimal integer wider than 64 bits is rejected rather than preserved.
  Decimal integers of any width round-trip losslessly; only the `0x` form is capped, so nothing is silently rounded — it errors instead.
- Comment fidelity is leading, trailing, and tail placement; blank lines, comment indentation, and interior comments between a key and its value are not preserved.

## Benchmarks

Parse throughput in MiB/s, higher is better, on Intel Sapphire Rapids.
Rows are split by output shape, since comparing across shapes is not meaningful, and bold marks the winner within each class.

Owned, mutable DOM — strings decoded into owned storage, document independent of the input:

|                        | canada.json | citm.json | twitter.json | amazon.ndjson |
| ---------------------- | ----------: | --------: | -----------: | ------------: |
| `jsontape-owned`       |         138 |       370 |          197 |           347 |
| `jsontape-owned-arena` |         232 |   __573__ |          311 |           432 |
| `serde-owned`          |         169 |       413 |          219 |           576 |
| `simdjson-owned`       |     __256__ |       448 |      __401__ |       __742__ |
| `json5-owned`          |          26 |        17 |           11 |            10 |

Borrowed, zero-copy DOM — the document borrows the input buffer:

|                       | canada.json | citm.json | twitter.json | amazon.ndjson |
| --------------------- | ----------: | --------: | -----------: | ------------: |
| `jsontape-view`       |         148 |       440 |          358 |           725 |
| `jsontape-view-arena` |         235 |   __641__ |          417 |           872 |
| `jsontape-view-bound` |         146 |       422 |          360 |           734 |
| `simdjson-borrowed`   |     __249__ |       538 |      __681__ |       __875__ |

The arena rows are the configuration this crate is built for.
They take `citm` outright in both classes — the most object-key-dense document here — and on the `amazon` NDJSON stream the zero-copy view lands within half a percent of `simd-json`.
`canada` and `twitter` go to SIMD, which is what vector scanning is for: one is number-dense, and the other is 58% string bytes across 18k short strings.

> __Method.__
> Criterion defaults, pinned to one core, each row in its own process, with `float_roundtrip` on `serde_json` and `simd-json` reusing its `Buffers`.
> Isolation matters here: run back to back, `serde_json`'s NDJSON figure swings by more than half depending on what ran before it, because its `Value` makes many small `BTreeMap` allocations and inherits whatever state the global allocator is left in.
> The arena rows are immune to that by construction, which is part of the point.
> One machine — read these as relative, not absolute, and re-run on your own hardware.

> __Reading the comparison.__
> Two asymmetries shape it.
> JsonTape alone preserves source key order, so it runs an explicit deduplication pass where the peers get that free from their map insert; the like-for-like peer would be `serde_json` with `preserve_order`, which is slower than the `BTreeMap` default measured here.
> And the views validate escapes at parse but keep strings escaped, deferring the decode to first use, where `simd-json` unescapes into its input buffer eagerly.

> __The `json5` row.__
> The [`json5`] crate is what most people reach for when they need JSON5, so it belongs here even though its PEG-based design optimizes for grammar clarity rather than throughput.
> It is the measure of what JSON5 leniency usually costs, and of the fact that on this scanner it costs nothing.

Run the suite with `cargo bench`.
[`scripts/bench.rs`] documents what every row builds, how the comparison is kept fair, and the commands for downloading the canonical datasets.

## License

Apache-2.0.

[StringTape]: https://github.com/ashvardanian/StringTape
[`scripts/bench.rs`]: scripts/bench.rs
[`allocator_api2`]: https://docs.rs/allocator-api2
[`parse_in`]: https://docs.rs/jsontape/latest/jsontape/fn.parse_in.html
[`simd-json`]: https://docs.rs/simd-json
[`json5`]: https://docs.rs/json5
