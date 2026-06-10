# rusty-racer-spike

Can bare [rusty_v8](https://crates.io/crates/v8) (crate `v8` v150 = V8 ~15.0)
carry mini_racer-csim's V8-embedding half? Three probes, ordered by
architectural risk, all in `src/main.rs` (~330 lines, **one `unsafe`**, written
against an API a year newer than my C++ knowledge and compiling clean on the
second attempt).

Run: `cargo run --release` · C++ comparison: `ruby bench_cpp.rb` (needs the
sibling mini_racer checkout built).

## Results (2026-06-10, Linux x86_64)

| Probe | Result |
|---|---|
| 1. realms — multiple `v8::Context` in ONE isolate, shared security token, cross-realm global read | **works directly** (`Context::new` × N + `set_security_token`). This is the csim model deno_core removed — bare rusty_v8 has no opinion about it |
| 2. termination — watchdog `terminate_execution()` from another thread via `IsolateHandle`, then recover | **works**; fired at ~50ms, isolate usable after `cancel_terminate_execution()` |
| 3. load_module_graph slice — 83-module graph, level-walk, batched fetch/resolve, native instantiate via resolver, fresh realm/visit | **works**; mean **144µs/visit** (Rust, in-process closures) vs **894µs/visit** (C++ ext through Ruby, real rendezvous) |

### Perf caveat (do not over-read the 6×)

The Rust bench calls in-process closures; the C++ number includes ~13 real
Ruby↔V8 thread roundtrips + wire serde + Ruby block execution (~depth×2
crossings for a depth-7 graph). A Rust *gem* would pay the same boundary costs
through Magnus/GVL. Different V8 versions too (15.0 vs libv8-node's 13.6).
Honest claim: **no perf cliff — the V8 work itself is at least as fast**, and
the slice's native portion is microseconds either way. The boundary, as
always, is where the time lives.

### Audit-bug classes, structurally re-checked against rusty_v8

| C++ audit bug (fixed 2026-06-10) | In rusty_v8 v150 |
|---|---|
| #63 `Context#stop` UAF on freed `State*` | unrepresentable: `IsolateHandle` is `Send` + refcounted; `terminate_execution()` after isolate death is a safe no-op. Probe 2 moves the handle into a thread |
| #4/#1004 `Set(...).Check()` process abort under termination | unrepresentable as written: there is no `.Check()`; fallible ops return `#[must_use] Option` — ignoring is a compile error, aborting requires an explicit `.unwrap()` |
| #1002 snapshot `StartupData` use-after-return | unrepresentable: `CreateParams::snapshot_blob(data)` takes **ownership** and keeps the blob alive with the isolate |
| #5/#14 dangling realm/state pointers | the registry holds `v8::Global<Module>` (refcounted); `cur().at()`-style dangling-id aborts become `Option` misses |
| #2 O(N) `module_filename` scan per import edge | the natural Rust shape is the hash map the audit asked for (see `Registry::url_by_hash`) |

### What the spike does NOT show

- The Ruby half: Magnus, GVL release, the rendezvous protocol, fork safety,
  watchdog-vs-GVL — the layer where the hang-class bugs (#11/#12/#13/#16)
  live. Rust does not structurally fix those; a stage-2 spike (Magnus +
  `rb_thread_call_without_gvl` + one host-function roundtrip) is the next
  question.
- Module resolve callbacks are plain `fn` (no closures) exactly like C++ —
  state goes through a `thread_local`, so that pattern ports 1:1, it does not
  improve.
- `v8::scope!`/`tc_scope!` pinned-scope macros (v150) are unusual but pleasant;
  borrow errors replaced every scope-discipline footgun the C++ audit found.
