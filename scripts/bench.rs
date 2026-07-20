use allocator_api2::alloc::{AllocError, Allocator, Layout};
use core::cell::Cell;
use core::ops::Range;
use core::ptr::NonNull;
use criterion::measurement::WallTime;
use criterion::{black_box, criterion_group, criterion_main, BenchmarkGroup, BenchmarkId, Criterion, Throughput};
use jsontape::{parse, parse_in, parse_with, view, view_bound, view_in, view_in_bound_with, view_with, ParseOptions};

const SMALL: usize = 256;
const KIB_4: usize = 4 * 1024;
const KIB_64: usize = 64 * 1024;

/// Arena slab size for the reuse benches. Sized well past any single document's
/// DOM (and any NDJSON record's) plus the geometric-growth slack the parser's
/// vectors leave behind between resets.
const ARENA_BYTES: usize = 256 * 1024 * 1024;

/// A single-slab bump arena for the reuse benchmark: an allocation bumps a
/// cursor, deallocation is a no-op, and `reset` rewinds the cursor so one region
/// of memory serves document after document without ever calling the global
/// allocator. This is the packed, drop-once (here reset-once) layout JsonTape is
/// built for.
///
/// `reset` takes `&self` so it fits criterion's `Fn` closures; that makes it a
/// footgun (resetting with a live allocation aliases freed memory), sound here
/// only because criterion drops each result before the next reset. Real code
/// should reach for an arena with a checked `&mut self` reset — see the README.
struct BumpArena {
    base: *mut u8,
    capacity: usize,
    head: Cell<usize>,
}

impl BumpArena {
    fn with_capacity(capacity: usize) -> Self {
        let layout = Layout::from_size_align(capacity, 64).unwrap();
        // Freed in `Drop`; `capacity` is non-zero for every call site here.
        let base = unsafe { std::alloc::alloc(layout) };
        assert!(!base.is_null(), "arena slab allocation failed");
        BumpArena {
            base,
            capacity,
            head: Cell::new(0),
        }
    }

    /// Rewinds the cursor so the next document reuses the same memory. Any values
    /// still borrowing the arena must be dropped first (criterion drops each
    /// iteration's result before the next call, and the streaming loops below drop
    /// each record's value before the next `reset`).
    fn reset(&self) {
        self.head.set(0);
    }
}

impl Drop for BumpArena {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(self.capacity, 64).unwrap();
        unsafe { std::alloc::dealloc(self.base, layout) };
    }
}

// Implemented for the shared reference, matching the `parse_in` doctest: `&BumpArena`
// is `Copy`, so it satisfies the parser's `Allocator + Clone` bound directly.
unsafe impl Allocator for &BumpArena {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let start = (self.head.get() + layout.align() - 1) & !(layout.align() - 1);
        let end = start.checked_add(layout.size()).ok_or(AllocError)?;
        if end > self.capacity {
            return Err(AllocError);
        }
        self.head.set(end);
        // `start` is within the slab and aligned; the returned slice covers the request.
        let ptr = unsafe { NonNull::new_unchecked(self.base.add(start)) };
        Ok(NonNull::slice_from_raw_parts(ptr, layout.size()))
    }

    unsafe fn deallocate(&self, _ptr: NonNull<u8>, _layout: Layout) {}
}

struct Workload {
    name: String,
    source: Vec<u8>,
}

fn named_sizes(kind: &str, make: impl Fn(usize) -> Vec<u8>) -> Vec<Workload> {
    [SMALL, KIB_4, KIB_64]
        .into_iter()
        .map(|bytes| Workload {
            name: format!("{kind}-{bytes}b"),
            source: make(bytes),
        })
        .collect()
}

fn plain_string(bytes: usize) -> Vec<u8> {
    let mut source = Vec::with_capacity(bytes + 4);
    source.extend_from_slice(b"[\"");
    while source.len() + 2 < bytes {
        source.extend_from_slice(b"plain-string-scanner-run-");
    }
    source.extend_from_slice(b"\"]");
    source
}

fn escaped_string(bytes: usize) -> Vec<u8> {
    let mut source = Vec::with_capacity(bytes + 4);
    source.extend_from_slice(b"[\"");
    while source.len() + 2 < bytes {
        source.extend_from_slice(br#"plain\u00e9\\\"run\n"#);
    }
    source.extend_from_slice(b"\"]");
    source
}

fn numeric_array(bytes: usize) -> Vec<u8> {
    let mut source = Vec::with_capacity(bytes + 2);
    source.push(b'[');
    let mut value = 0_u64;
    while source.len() + 24 < bytes {
        if source.len() > 1 {
            source.push(b',');
        }
        source.extend_from_slice(value.to_string().as_bytes());
        value = value.wrapping_mul(1_000_003).wrapping_add(97);
    }
    source.push(b']');
    source
}

fn object_keys(bytes: usize) -> Vec<u8> {
    let mut source = Vec::with_capacity(bytes + 2);
    source.push(b'{');
    let mut key = 0_usize;
    while source.len() + 48 < bytes {
        if source.len() > 1 {
            source.push(b',');
        }
        source.extend_from_slice(format!(r#""field_{key:06}":"value-{key:06}-payload""#).as_bytes());
        key += 1;
    }
    source.push(b'}');
    source
}

fn nested_document() -> Vec<u8> {
    let mut source = Vec::new();
    for level in 0..32 {
        source.extend_from_slice(format!(r#"{{"level_{level}":"#).as_bytes());
    }
    source.extend_from_slice(br#"[0,1,2,{"leaf":true}]"#);
    source.extend(core::iter::repeat_n(b'}', 32));
    source
}

fn json5_comments(bytes: usize) -> Vec<u8> {
    let mut source = Vec::with_capacity(bytes + 8);
    source.extend_from_slice(b"{\n");
    let mut key = 0_usize;
    while source.len() + 56 < bytes {
        source.extend_from_slice(
            format!("  // key {key}\n  field_{key}: 'escaped \\u00e9 value', /* tail */\n").as_bytes(),
        );
        key += 1;
    }
    source.extend_from_slice(b"}\n");
    source
}

fn strict_workloads() -> Vec<Workload> {
    let mut workloads = Vec::new();
    workloads.extend(named_sizes("plain-string", plain_string));
    workloads.extend(named_sizes("escaped-string", escaped_string));
    workloads.extend(named_sizes("numeric-array", numeric_array));
    workloads.extend(named_sizes("object-keys", object_keys));
    workloads.push(Workload {
        name: "nested-32".to_owned(),
        source: nested_document(),
    });
    workloads
}

fn json5_workloads() -> Vec<Workload> {
    named_sizes("comments-trailing", json5_comments)
}

/// JsonTape's global-allocator parses: decoded owned DOM, zero-copy view, and
/// source-bound view. Shared by every whole-document parse group.
fn bench_jsontape_global(group: &mut BenchmarkGroup<'_, WallTime>, name: &str, source: &[u8]) {
    group.bench_function(BenchmarkId::new("owned", name), |bench| {
        bench.iter(|| black_box(parse(black_box(source)).unwrap()))
    });
    group.bench_function(BenchmarkId::new("view", name), |bench| {
        bench.iter(|| black_box(view(black_box(source)).unwrap()))
    });
    group.bench_function(BenchmarkId::new("bound-view", name), |bench| {
        bench.iter(|| black_box(view_bound(black_box(source)).unwrap()))
    });
}

/// The fully-owned-DOM peers: `serde_json` and `simd-json`. Both eagerly decode
/// every number and string into owned storage, so they are same-output-shape
/// comparisons to `owned`. simd-json rewrites its input in place, so it clones a
/// fresh buffer each run, and reuses one set of scratch `Buffers` across runs (its
/// analogue to JsonTape reusing an arena).
fn bench_dom_peers(group: &mut BenchmarkGroup<'_, WallTime>, name: &str, source: &[u8]) {
    group.bench_function(BenchmarkId::new("serde-json-owned", name), |bench| {
        bench.iter(|| black_box(serde_json::from_slice::<serde_json::Value>(black_box(source)).unwrap()))
    });
    group.bench_function(BenchmarkId::new("simd-json-owned", name), |bench| {
        let mut buffers = simd_json::Buffers::new(source.len());
        bench.iter(|| {
            let mut buffer = source.to_vec();
            black_box(simd_json::to_owned_value_with_buffers(black_box(&mut buffer), &mut buffers).unwrap())
        })
    });
}

fn bench_strict_parse(c: &mut Criterion) {
    let workloads = strict_workloads();
    let mut group = c.benchmark_group("parse-strict");
    for workload in &workloads {
        group.throughput(Throughput::Bytes(workload.source.len() as u64));
        bench_jsontape_global(&mut group, &workload.name, &workload.source);
        bench_dom_peers(&mut group, &workload.name, &workload.source);
    }
    group.finish();
}

fn bench_json5_parse(c: &mut Criterion) {
    let options = ParseOptions::json5();
    let workloads = json5_workloads();
    let mut group = c.benchmark_group("parse-json5");
    for workload in &workloads {
        group.throughput(Throughput::Bytes(workload.source.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("owned", &workload.name),
            &workload.source,
            |bench, source| bench.iter(|| black_box(parse_with(black_box(source), &options).unwrap())),
        );
        group.bench_with_input(
            BenchmarkId::new("view", &workload.name),
            &workload.source,
            |bench, source| bench.iter(|| black_box(view_with(black_box(source), &options).unwrap())),
        );
        group.bench_with_input(
            BenchmarkId::new("bound-view", &workload.name),
            &workload.source,
            |bench, source| {
                bench.iter(|| {
                    black_box(view_in_bound_with(black_box(source), allocator_api2::alloc::Global, &options).unwrap())
                })
            },
        );
        // The json5 crate is the natural JSON5 peer; it parses from a &str.
        group.bench_with_input(
            BenchmarkId::new("json5-crate", &workload.name),
            &workload.source,
            |bench, source| {
                let text = core::str::from_utf8(source).unwrap();
                bench.iter(|| black_box(json5::from_str::<serde_json::Value>(black_box(text)).unwrap()))
            },
        );
    }
    group.finish();
}

fn bench_serialize(c: &mut Criterion) {
    let cases = [
        ("object-4k", object_keys(KIB_4)),
        ("numeric-array-4k", numeric_array(KIB_4)),
        ("nested-32", nested_document()),
    ];
    let mut group = c.benchmark_group("serialize");
    for (name, source) in &cases {
        let owned = parse(source).unwrap();
        let borrowed = view(source).unwrap();
        let bound = view_bound(source).unwrap();
        group.throughput(Throughput::Bytes(source.len() as u64));
        group.bench_with_input(BenchmarkId::new("owned", name), &owned, |bench, document| {
            bench.iter(|| black_box(document.to_string()))
        });
        group.bench_with_input(BenchmarkId::new("view", name), &borrowed, |bench, document| {
            bench.iter(|| black_box(document.to_json_string(black_box(source))))
        });
        group.bench_with_input(BenchmarkId::new("bound-view", name), &bound, |bench, document| {
            bench.iter(|| black_box(document.to_json_string()))
        });
    }
    group.finish();
}

/// Reads the dataset directory named by the `JSONTAPE_DATA` environment variable,
/// keeping only files whose extension is in `extensions`. Empty when the variable
/// is unset, so the synthetic benches run standalone. See the README for the
/// download commands.
fn read_data_dir(extensions: &[&str]) -> Vec<(String, Vec<u8>)> {
    let Ok(dir) = std::env::var("JSONTAPE_DATA") else {
        return Vec::new();
    };
    let mut files = Vec::new();
    let entries =
        std::fs::read_dir(&dir).unwrap_or_else(|error| panic!("JSONTAPE_DATA dir {dir:?} unreadable: {error}"));
    for entry in entries.flatten() {
        let path = entry.path();
        let extension = path.extension().and_then(|extension| extension.to_str());
        if !extension.is_some_and(|extension| extensions.contains(&extension)) {
            continue;
        }
        let source = std::fs::read(&path).unwrap_or_else(|error| panic!("reading {path:?}: {error}"));
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        files.push((name, source));
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
}

/// Single-document datasets: each `*.json` file becomes one whole-document workload.
fn load_docs() -> Vec<Workload> {
    read_data_dir(&["json"])
        .into_iter()
        .map(|(name, source)| Workload { name, source })
        .collect()
}

struct StreamWorkload {
    name: String,
    source: Vec<u8>,
    records: Vec<Range<usize>>,
}

/// Byte ranges of each non-blank line in `source`, with any trailing `\r` and the
/// `\n` excluded. A final line without a newline is included.
fn split_records(source: &[u8]) -> Vec<Range<usize>> {
    let mut records = Vec::new();
    let mut start = 0;
    for index in 0..=source.len() {
        if index != source.len() && source[index] != b'\n' {
            continue;
        }
        let mut end = index;
        if end > start && source[end - 1] == b'\r' {
            end -= 1;
        }
        if source[start..end].iter().any(|byte| !byte.is_ascii_whitespace()) {
            records.push(start..end);
        }
        start = index + 1;
    }
    records
}

/// NDJSON / JSON Lines datasets: each `*.ndjson`/`*.jsonl` file becomes one
/// streaming workload whose records are parsed one per line.
fn load_streams() -> Vec<StreamWorkload> {
    read_data_dir(&["ndjson", "jsonl"])
        .into_iter()
        .map(|(name, source)| {
            let records = split_records(&source);
            StreamWorkload { name, source, records }
        })
        .filter(|workload| !workload.records.is_empty())
        .collect()
}

fn bench_data(c: &mut Criterion) {
    let workloads = load_docs();
    if workloads.is_empty() {
        return;
    }
    // One arena, reused (reset, not freed) across every file and iteration.
    let arena = BumpArena::with_capacity(ARENA_BYTES);
    let mut group = c.benchmark_group("parse-data");
    for workload in &workloads {
        let source = &workload.source;
        group.throughput(Throughput::Bytes(source.len() as u64));
        bench_jsontape_global(&mut group, &workload.name, source);
        // The flagship configurations: parse into a reused bump arena, so no run
        // touches the global allocator. `owned-arena` is a decoded DOM in the
        // arena; `view-arena` keeps zero-copy spans in the arena.
        group.bench_function(BenchmarkId::new("owned-arena", &workload.name), |bench| {
            bench.iter(|| {
                arena.reset();
                black_box(parse_in(black_box(source), &arena).unwrap())
            })
        });
        group.bench_function(BenchmarkId::new("view-arena", &workload.name), |bench| {
            bench.iter(|| {
                arena.reset();
                black_box(view_in(black_box(source), &arena).unwrap())
            })
        });
        bench_dom_peers(&mut group, &workload.name, source);
    }
    group.finish();
}

fn bench_ndjson(c: &mut Criterion) {
    let workloads = load_streams();
    if workloads.is_empty() {
        return;
    }
    // One arena reused across the whole record stream, reset between documents —
    // the genuine "one arena, many documents" case.
    let arena = BumpArena::with_capacity(ARENA_BYTES);
    let mut group = c.benchmark_group("parse-ndjson");
    for workload in &workloads {
        let source = &workload.source;
        let records = &workload.records;
        let total: usize = records.iter().map(|record| record.len()).sum();
        group.throughput(Throughput::Bytes(total as u64));
        group.bench_function(BenchmarkId::new("owned", &workload.name), |bench| {
            bench.iter(|| {
                for record in records {
                    black_box(parse(black_box(&source[record.clone()])).unwrap());
                }
            })
        });
        group.bench_function(BenchmarkId::new("view", &workload.name), |bench| {
            bench.iter(|| {
                for record in records {
                    black_box(view(black_box(&source[record.clone()])).unwrap());
                }
            })
        });
        group.bench_function(BenchmarkId::new("owned-arena", &workload.name), |bench| {
            bench.iter(|| {
                for record in records {
                    arena.reset();
                    black_box(parse_in(black_box(&source[record.clone()]), &arena).unwrap());
                }
            })
        });
        group.bench_function(BenchmarkId::new("view-arena", &workload.name), |bench| {
            bench.iter(|| {
                for record in records {
                    arena.reset();
                    black_box(view_in(black_box(&source[record.clone()]), &arena).unwrap());
                }
            })
        });
        group.bench_function(BenchmarkId::new("serde-json-owned", &workload.name), |bench| {
            bench.iter(|| {
                for record in records {
                    black_box(serde_json::from_slice::<serde_json::Value>(black_box(&source[record.clone()])).unwrap());
                }
            })
        });
        group.bench_function(BenchmarkId::new("simd-json-owned", &workload.name), |bench| {
            let capacity = records.iter().map(|record| record.len()).max().unwrap_or(0);
            let mut buffers = simd_json::Buffers::new(capacity + 64);
            bench.iter(|| {
                // simd-json mutates its input in place, so clone the file once, then
                // parse each record's slice in place while reusing one set of scratch
                // buffers across the whole stream — the fair analogue to arena reuse.
                let mut whole = source.to_vec();
                for record in records {
                    let slice = &mut whole[record.clone()];
                    black_box(simd_json::to_owned_value_with_buffers(black_box(slice), &mut buffers).unwrap());
                }
            })
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_strict_parse,
    bench_json5_parse,
    bench_serialize,
    bench_data,
    bench_ndjson
);
criterion_main!(benches);
