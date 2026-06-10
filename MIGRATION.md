# mini_racer-csim → rusty_v8 migration

Decision (2026-06-10): migrate mini_racer-csim's V8 layer from C/C++ to a
rusty_v8 + Magnus (rb-sys) Rust gem. The spike answered every feasibility
question; this is the execution plan. **No architectural blocker exists** — the
remaining work is implementation volume + a `libv8-rusty` build pipeline.

## Why (from the deliberation)

- **Proof, not testing.** The borrow checker makes whole bug classes
  *unrepresentable* — use-after-free, dangling handles, data races — that the
  C/C++ extension can only *hunt* for (cf. the 2026-06-10 audit: #5/#14/#50/#63/
  #1002 were all this class). "Provably absent" > "didn't find it this round".
- **The type system forces the right architecture.** The inline-on-Ruby-thread
  model does not compile (`OwnedIsolate: !Send`, no `Locker`; Magnus needs
  `Send`); the compiler mandates the dedicated-V8-thread + message-passing
  design mini_racer reached by experience. Channels make the audit's hang
  classes (#12/#13/#26 dispose hangs, #63 stop UAF, #24 proc-exception wedge,
  #3 stale terminate) unrepresentable.
- **Growth compounds the win.** WPT conformance (correctness surface) and
  load_module_graph-style native-perf work (performance surface) both grow the
  V8 layer; on growing code the compile-time guarantee compounds while the
  C/C++ audit/fuzz tax recurs.

What Rust does NOT fix (carry these forward): deadlock/protocol *logic* bugs,
V8 *semantic* misuse (the `.Check()`-abort class becomes an `Option` you can
still `.unwrap()`), and the irreducible `unsafe` at the GVL/callback boundary.

## Spike findings = the validated foundation

`stage1/` (bare rusty_v8): per-frame realms (multi-Context, shared security
token), thread-safe watchdog termination via `IsolateHandle`, the
load_module_graph slice (83-module graph, batched fetch/resolve, native
instantiate) — all map directly.

`stage2/` + `gem/` (Magnus): dedicated V8 thread + channel rendezvous; eval,
Context#call, attach, host-fn roundtrip, nested ruby→js→ruby→js, timeout,
stop, dispose — `cargo check` green. Error classes mapped to
`RustyRacer::ParseError/RuntimeError/ScriptTerminatedError`.

**Packaging (the load-bearing constraint), proved by running CI twice:** the
crates.io `v8` crate cannot yield a cdylib-linkable V8 — the prebuilt is
initial-exec TLS (`R_X86_64_TPOFF32` under `-shared`) and the crates.io
*source* build is missing vendored data (`icudtl.dat`, `icu_calendar_data`)
even on clean Ubuntu. A from-**git** rusty_v8 source build, by contrast,
defaults to `V8_TLS_USED_IN_LIBRARY` and has the full third_party — so it
produces a cdylib-linkable archive. **∴ a `libv8-rusty` (library-TLS
`librusty_v8.a`, built from git, hosted) is required and unavoidable** — the
same role `libv8-node` plays for the C++ side.

## Milestones (critical path)

1. **M1 — libv8-rusty (THE GATE).** CI builds a library-TLS `librusty_v8.a`
   from `denoland/rusty_v8` git (`V8_FROM_SOURCE=1 cargo build --release`,
   submodules + python3/curl/libclang-19/glib-2.0), uploads it as a release
   artifact per platform. Start: linux x86_64. *Risk: V8 source builds are
   disk/RAM/time-heavy — a standard GH runner may be too small; may need a
   larger runner. Iteration expected.* Everything runnable blocks on this.
2. **M2 — rusty_racer API parity with csim.** Have: eval/eval_t/call/attach/
   stop/dispose + error classes. Need: `Snapshot.new`/`warmup!`,
   `compile_module`/`load_module_graph`/`module_loader` (port stage1 through
   the rendezvous), `create_realm`/`reset_realm`, `perform_microtask_checkpoint`,
   `Platform.set_flags!`, `host_namespace:`, richer marshalling (beyond
   primitives — arrays/hashes/Date/etc., the serde.c equivalent). Proceeds in
   parallel via `cargo check`; testable once M1 lands.
3. **M3 — ship via cibuildgem.** Gem build sets `RUSTY_V8_ARCHIVE` to M1's
   artifact; cibuildgem packages native platform gems (V8 baked in). Green
   `compile` + `test_native` on linux, then widen the matrix.
4. **M4 — csim integration.** Add a `:rusty` engine to csim's engine selection
   (browser.rb `ENGINE_GEM`), run csim's suite against rusty_racer.
5. **M5 — Discourse validation.** The real gate: run the Discourse system-spec
   suite on the rusty engine; compare correctness + perf against the C++ csim.

## Structure (proposal)

The spike repo (`rusty-racer-spike`: stage1/stage2/gem) is exploratory. For the
real project, graduate to clean repos: the gem (`rusty_racer` or a
`mini_racer-csim` successor) and `libv8-rusty` as separate repos. Until then,
work continues here under `gem/` + `libv8-rusty/`.

## Open risks / decisions

- libv8-rusty build/maintenance is real recurring cost (per V8 bump × platform).
- `unsafe` boundary audit (GVL trampolines, callback scopes) before trusting it.
- Marshalling fidelity vs the hand-rolled serde.c (consider rusty_v8's
  `ValueSerializer` + a Ruby-type delegate).
- macOS/Windows V8 source builds (defer; linux-first for csim/Discourse).
- Naming/coexistence with the C++ csim during the transition.
