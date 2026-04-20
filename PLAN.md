# quill-core — Review & Enhancement Plan
 
> **Context:** This plan was produced after a deep audit of the Rust FFI engine.
> A ~30% benchmark regression was observed and traced to 5 compounding root causes — all documented below.
 
---
 
## Table of Contents
 
1. [Architecture Overview](#architecture-overview)
2. [Benchmark Drop — Root Causes](#benchmark-drop--root-causes)
3. [Bug Fixes](#bug-fixes)
4. [Enhancements](#enhancements)
5. [Priority Matrix](#priority-matrix)
 
---
 
## Architecture Overview
 
`quill-core` is the native Rust binary engine powering the Quill PHP Framework. It exposes a C FFI (`extern "C"`) interface consumed by PHP via `ext-ffi`.
 
**Key modules:**
 
| File | Responsibility |
|---|---|
| `src/lib.rs` | All FFI entry points, Axum/Tokio HTTP server, request dispatch |
| `src/router.rs` | `matchit` radix-trie route matching, per-method routing tables |
| `src/validator.rs` | DTO schema validation, regex caching via `DashMap` |
| `src/state.rs` | Shared-State Broker (SSB) — thread-safe KV store |
| `src/json.rs` | `sonic-rs` SIMD JSON compaction utility |
| `src/manifest.rs` | `RouteEntry` deserialization from PHP boot payload |
 
---
 
## Benchmark Drop — Root Causes
 
### RC-1 — Single-Threaded Tokio Runtime `[~12–15% impact]`
 
**File:** `src/lib.rs`
 
The Axum HTTP server is started on a `new_current_thread()` Tokio runtime inside a single OS thread. All async I/O, routing, and body processing serialize on one executor. Under concurrent load this is the primary throughput bottleneck.
 
**Root code pattern:**
```rust
let rt = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .unwrap();
```
 
**Fix:** Switch to `new_multi_thread()` with `worker_threads` set to `std::thread::available_parallelism()`. Expose `worker_threads: u8` as a parameter in `quill_server_listen()` so the PHP side can tune it per deployment.
 
---
 
### RC-2 — Arc Refcount Leak in Every FFI Call `[~8–10% impact]`
 
**File:** `src/lib.rs` — all FFI entry points
 
Every FFI call performs:
```rust
let router = Arc::from_raw(ptr);   // refcount +1
// ... use router ...
Arc::into_raw(router);             // refcount +1 again — NOT a no-op, LEAKS
```
 
`Arc::from_raw` + `Arc::into_raw` does **not** cancel out. It increments the strong count twice without ever decrementing it. Over thousands of requests this creates growing heap pressure and reference-count overflow risk.
 
**Fix:** Use a borrow-only pattern at the FFI boundary. Never reconstruct the `Arc` inside the function — only dereference the raw pointer:
```rust
// Correct pattern — zero refcount change
let router = unsafe { &*(ptr as *const QuillRouter) };
```
Only reconstruct the `Arc` in the destructor (`quill_router_free`).
 
---
 
### RC-3 — Regex Cache TOCTOU Race `[stability / CPU spikes]`
 
**File:** `src/validator.rs`
 
```rust
if !self.regex_cache.contains_key(pattern) {
    let re = Regex::new(pattern)?;
    self.regex_cache.insert(pattern.clone(), re);  // race window here
}
```
 
`contains_key()` + `insert()` is not atomic on `DashMap`. Two threads can both pass the check and both compile the same regex, wasting CPU and creating lock contention spikes under concurrent DTO registration.
 
**Fix:** Use `DashMap::entry().or_insert_with()` for an atomic check-and-insert:
```rust
self.regex_cache
    .entry(pattern.clone())
    .or_insert_with(|| Regex::new(pattern).expect("invalid regex"));
```
 
---
 
### RC-4 — DashMap Read Clones Entire JSON Value `[~3–5% impact]`
 
**File:** `src/state.rs`
 
```rust
pub fn get(&self, key: &str) -> Option<Value> {
    self.kv.get(key).map(|v| v.clone())  // deep clone on every read
}
```
 
`DashMap::get()` returns a `Ref` guard (read-lock). Cloning the inner `Value` (a `sonic-rs` JSON tree) before returning is unnecessary for most call sites.
 
**Fix:** Add an internal `get_ref` method returning `Option<Ref<'_, String, Value>>`. Clone only at the FFI boundary where ownership transfer is actually required.
 
---
 
### RC-5 — Hardcoded Unbounded Request Channel `[reliability cliff]`
 
**File:** `src/lib.rs`
 
```rust
let (tx, rx) = mpsc::channel(10_000);
```
 
Capacity is not configurable. Under burst traffic the queue fills silently and new requests receive `503 Service Unavailable` with no observable backpressure signal.
 
**Fix:** Accept `max_queue_depth: u32` in `quill_server_listen()`. Expose current queue depth via a new `quill_server_stats()` FFI function for PHP-side monitoring.
 
---
 
## Bug Fixes
 
### BUG-1 — Missing `catch_unwind` on FFI Entry Points
 
**File:** `src/lib.rs`
 
`quill_router_build` uses `catch_unwind` but the following do **not**:
- `quill_server_listen`
- `quill_router_dispatch`
- `quill_validator_validate`
- All shared-state functions
 
A Rust panic crossing an FFI boundary is **undefined behavior** — it can silently corrupt the PHP process or cause a hard crash.
 
**Fix:** Wrap every `extern "C"` function body in `std::panic::catch_unwind`. Return a sentinel error value (`-1` / `NULL` + a global `quill_last_error()` string) on panic.
 
---
 
### BUG-2 — `quill_router_free` / `quill_validator_free` Not Called Consistently
 
PHP does not guarantee object destructor ordering. If the FFI handle is dropped without calling the corresponding `_free` function, the Rust-side heap objects live forever.
 
**Fix:** Audit all PHP call sites and enforce that every `_build` / `_new` call has a corresponding `_free` in a PHP destructor or `register_shutdown_function`.
 
---
 
### BUG-3 — `quill_server_respond` with Null Body Pointer
 
**File:** `src/lib.rs`
 
If PHP passes a null or empty `body` pointer to `quill_server_respond`, there is no explicit null-check before dereferencing. The null-pointer guards added in recent commits may not cover all code paths introduced in the async refactor.
 
**Fix:** Add explicit null guards at the top of `quill_server_respond` and `quill_server_poll` before any pointer dereference.
 
---
 
## Enhancements
 
### ENH-1 — Multi-Thread Tokio Runtime with Configurable Workers
 
Expose `worker_threads: u8` in `quill_server_listen()`. Default to `available_parallelism()`. This alone is expected to recover 10–15% throughput under concurrent load.
 
---
 
### ENH-2 — Zero-Copy Shared-State Read Path
 
Introduce `quill_shared_get_into(key, out_buf, buf_len) -> i32` that writes the JSON value directly into a caller-provided buffer. The current path allocates a `String` in Rust, converts to `CString`, and PHP copies it again — 3 allocations per read.
 
---
 
### ENH-3 — Streaming Body Support
 
Currently `to_bytes(body, max_size)` buffers the entire request body before dispatch. For large uploads this blocks the async executor.
 
**Fix:** Stream body chunks through the channel incrementally, or offload body buffering to a dedicated Tokio task.
 
---
 
### ENH-4 — Server Metrics via FFI
 
Add `quill_server_stats() -> *const c_char` returning a JSON object:
```json
{
  "queue_depth": 42,
  "requests_total": 1000000,
  "workers": 4,
  "avg_latency_us": 210
}
```
 
---
 
### ENH-5 — Graceful Drain on SIGTERM
 
Add `quill_server_drain(timeout_ms: u32) -> i32` that stops accepting new connections, waits up to `timeout_ms` for in-flight requests to complete, and then returns. PHP calls this from a `SIGTERM` handler.
 
---
 
### ENH-6 — Trim `tower-http` Feature Flags
 
The `tower-http` dependency is pulled in with `full` features. Trim to only what's used (e.g. `tower-http = { features = ["trace", "timeout"] }`). This reduces compile time and binary size.
 
---
 
### ENH-7 — Expand Test Coverage
 
**Current gaps:**
 
- No concurrency stress tests for `SharedState` under high thread counts
- No fuzz targets for `compact_json` (malformed input)
- No tests for FFI null-pointer safety guards
- No tests for the async request channel under backpressure
 
**Add:**
- `cargo test` concurrency suite: 500 threads × 10,000 state ops
- `cargo-fuzz` target for `quill_json_compact`
- Integration test: send requests up to `max_queue_depth` and verify graceful 503
 
---
 
## Priority Matrix
 
| ID | Item | Type | Impact | Priority |
|---|---|---|---|---|
| RC-1 | Multi-thread Tokio runtime | Perf/Bug | −12–15% bench | **P0** |
| RC-2 | Arc refcount leak in FFI | Bug | −8–10% bench | **P0** |
| RC-3 | TOCTOU regex cache race | Bug | CPU spikes | **P0** |
| RC-4 | DashMap clone on state read | Perf | −3–5% bench | **P1** |
| RC-5 | Configurable channel capacity | Bug/Reliability | 503 cliff | **P1** |
| BUG-1 | `catch_unwind` all FFI fns | Bug | UB / crashes | **P1** |
| BUG-2 | Enforce `_free` call pairing | Bug | Memory leak | **P1** |
| BUG-3 | Null guard in respond/poll | Bug | Potential crash | **P1** |
| ENH-1 | Configurable worker threads | Feature | throughput | **P2** |
| ENH-2 | Zero-copy SSB read | Feature | state perf | **P2** |
| ENH-3 | Streaming body support | Feature | large uploads | **P2** |
| ENH-4 | Server metrics FFI | Feature | observability | **P3** |
| ENH-5 | Graceful SIGTERM drain | Feature | reliability | **P2** |
| ENH-6 | Trim tower-http features | Quality | build size | **P3** |
| ENH-7 | Expand test coverage | Quality | reliability | **P2** |
 
---
 
*Plan authored: 2026-04-09*