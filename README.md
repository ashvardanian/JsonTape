# JsonTape

__JsonTape__ is a minimalistic, allocator-aware JSON parser in pure Rust.
It's built to address the high memory usage & fragmentation as well as the high traversal latency in typical DOM implementations.
Its sibling project — [StringTape] — does the same for Arrow-like string collections.

Every container allocates through any [`allocator_api2`] allocator, so a whole document can live in a bump arena and free in one step.
The parser is strictly RFC 8259-conformant, never panics on bad input, and runs on `no_std`.

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

## Details

- __Strict numbers.__
  Leading zeros, bare `.5` and `1.`, malformed exponents, and `+`-prefixed values are rejected.
  Integers keep their width as `i64` or `u64`, and an integer too wide for 64 bits is preserved as its exact decimal text rather than rounded into an `f64`.
- __Strict strings.__
  Unescaped control characters, invalid `\u` escapes, and lone surrogates are rejected; surrogate pairs are combined.
  Both paths validate UTF-8 and escapes at parse time; `JsonView` then keeps the raw spans while `Json` stores the decoded strings.
- __Rich errors.__
  A syntax fault carries a byte offset and a category, and `JsonError::location(source)` resolves it to a line and column on demand.
- __Configurable.__
  `ParseOptions` sets the nesting limit and the duplicate-key policy (last-wins by default, or first-wins, reject, keep-all), passed to the `*_with` parsers.
- __Fallible parsing.__
  Parsing untrusted input uses fallible allocation and returns `JsonError` rather than aborting; build-from-code mutators allocate infallibly, like the standard collections.
- __Optional serde.__
  Enable the `serde` feature for `Serialize`/`Deserialize`, plus `to_value`/`from_value` that convert any type to and from a document without a text round trip.

## Allocators

Every container allocates through the `allocator` you pass to `parse_in` or `view_in`, any [`allocator_api2`] allocator.
With the default global heap each array and object is a separate allocation scattered across the heap.
Pass a bump or slab arena instead and the whole document is packed into one contiguous region that frees in O(1) when the arena is dropped — parse a large payload, read it, and reclaim it all at once.
This is the closest the pointer-tree design gets to a flat tape; the per-node `Vec`s still grow by doubling, so an arena keeps some slack, but allocation and teardown are cheap and fragmentation-free.
Bring any arena crate that implements `allocator_api2::Allocator` version 0.3; see the [`parse_in`](https://docs.rs/jsontape/latest/jsontape/fn.parse_in.html) docs for a worked custom-allocator example.

## License

Apache-2.0.

[StringTape]: https://github.com/ashvardanian/StringTape
[`allocator_api2`]: https://docs.rs/allocator-api2
