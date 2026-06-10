# stage 2 — the Ruby half (Magnus + GVL + rendezvous)

Stage 1 showed the V8-embedding half ports cleanly to bare rusty_v8. Stage 2
asks the harder question: **how small can the `unsafe` boundary be when you
bolt a Ruby gem onto it** — the layer (GVL, threading, rendezvous, fork,
watchdog) where csim's hang-class audit bugs (#11/#12/#13/#16/#24/#63) live and
where Rust's borrow checker gives *no* automatic guarantee.

`stage2/src/lib.rs` is a real Magnus extension (`cdylib`) exposing
`RustyRacer::Context#{eval, eval_t, attach, stop, dispose}`. `stage2/test.rb`
runs 10 probes mapped to the audit's hang classes plus a rendezvous-floor
microbench against the C extension.

## The load-bearing finding: the architecture is forced by the type system

I first tried the *inline* model — run V8 on the calling Ruby thread, release
the GVL around execution, no dedicated thread (csim's single_threaded mode).
**It does not compile**, and the reasons are exactly the invariants the C++
code maintains by hand and convention:

- `v8::OwnedIsolate` is deliberately `!Send`, and rusty_v8 v150 binds no
  `v8::Locker`. An isolate is pinned to one OS thread, enforced by the type.
- Magnus requires wrapped data (`#[magnus::wrap]`) to be `Send + Sync` because
  Ruby objects migrate between threads.

So `Context { isolate: OwnedIsolate }` is rejected: `NonNull<RealIsolate>
cannot be sent between threads safely`. The compiler refuses the unsound
shortcut and **forces the dedicated-V8-thread + message-passing design** — the
same architecture mini_racer's C extension arrived at through experience. In
C++ that invariant is a comment ("called with Context.mtx held", "v8 thread
only"); in Rust it is a compile error.

## What the channel rendezvous removes, bug-for-bug

The C extension hand-rolls a condvar protocol (`req`/`res` buffers, `mtx`,
`cv`, `quit`); that protocol is where the audit's worst hangs live. Replacing
it with std channels makes several of those bugs *unrepresentable*:

| C++ audit bug | Why it can't recur here |
|---|---|
| #12 dispose hang: one `cond_signal`, multiple waiter classes, signal misrouted | every request carries its **own** reply `Sender`; there is no shared wakeup to misroute |
| #13/#26 caller blocks forever: wait predicate ignores `quit` | dispose drops the request `Receiver`; a late `send()` returns `Err` and raises cleanly — no predicate to get wrong |
| #63 `Context#stop` UAF on freed `State*` | `stop` holds only an `IsolateHandle` (`Send` + refcounted); firing it post-dispose is a safe no-op — **no `stop_mtx` needed** |
| #24 Ruby exception in a host proc wedges the context (longjmp past `rr_mtx`) | the proc returns a magnus `Err`; it is sent back as an `Answer::Result(Err)` and thrown as a JS exception — no longjmp through foreign frames |
| #3 stale `TerminateExecution` poisons the next request | the per-request watchdog is joined before the reply and `cancel_terminate_execution()` is called unconditionally if it fired |

`stage2/test.rb` exercises each (host-fn roundtrip, Ruby-exception-survives,
nested `ruby→js→ruby→js`, timeout+recover, late-watchdog×150, stop-from-thread,
dispose-racing-eval×10).

## The packaging finding: cdylib needs a TLS-correct V8

The default **prebuilt** rusty_v8 does **not** link into a `cdylib`:

```
rust-lld: error: relocation R_X86_64_TPOFF32 against v8::internal::g_current_isolate_
          cannot be used with -shared
```

The prebuilt is built for executables (initial-exec TLS). A Ruby extension is a
shared object; it needs V8 built with `-DV8_TLS_USED_IN_LIBRARY`, which the
crate injects **only on a source build** (`V8_FROM_SOURCE=1`, see the v8
crate's `build.rs`). Implication for a real gem: it must either source-build V8
(slow, heavy toolchain) or ship its own TLS-correct prebuilt — i.e. it needs a
**`libv8-rusty` distribution mirroring what `libv8-node` does today** for the
C++ side. This is a concrete, non-trivial migration cost, not a blocker.

## Runtime status on this machine

The Rust side **type-checks and compiles**; only obtaining a cdylib-linkable V8
is blocked here, and the way it is blocked is itself the packaging finding:

- **Prebuilt rusty_v8** → cdylib link fails (`R_X86_64_TPOFF32 ... -shared`, the
  TLS issue above).
- **Source build from the crates.io tarball** (`V8_FROM_SOURCE=1`) → the
  published `v8` tarball does **not** contain the full chromium `third_party`
  tree a real source build needs; it fails on an escalating series of missing
  vendored artifacts, each fix revealing the next:
  1. `libclang_rt.builtins.a` at the chromium resource-dir path (Gentoo's
     compiler-rt sits at `/usr/lib/clang/22/lib/linux/`) — worked around with a
     writable `CLANG_BASE_PATH` overlay symlinking the builtins to the
     GN-computed path;
  2. `third_party/icu/common/icudtl.dat` — sidestepped with
     `v8_enable_i18n_support=false`;
  3. `third_party/rust/chromium_crates_io/vendor/icu_calendar_data-v2/build.rs`
     — vendored-rust data still absent. A genuine source build needs a
     `gclient`/git checkout with submodules, not the crates.io tarball.

Neither path yields a linkable cdylib on a normal dev box without significant
toolchain work — which is exactly why this needs a **`libv8-rusty` prebuilt
with library TLS** (the libv8-node analogue). Standard rusty_v8 *executable* CI
(Ubuntu + chromium toolchain, how Deno builds) does not hit any of this because
it links bins, not shared objects, and builds from a full checkout.

`test.rb` is written to run as-is once such a V8 is available. The design
verification does not depend on it: the architecture is settled by the
type-checker (the inline model does not compile; the dedicated-thread model
does) and the hang-bug-class table is settled by construction, not by a passing
run.

## Net for the migration decision

- The hardest, bug-densest layer (threading/rendezvous) is where Rust pays off
  *beyond* memory safety: the type system **mandates** the correct architecture
  and channels make a cluster of the audit's hang bugs unrepresentable.
- But it is **not free**: `unsafe` is irreducible at the GVL trampolines and
  the V8 callback scope; marshalling (serde.c's job) is hand-written either way
  (rusty_v8's `ValueSerializer` is an option but a Ruby-type delegate is still
  yours); and packaging needs a TLS-correct V8 distribution.
- Stage 1 + stage 2 together: **no architectural blocker found.** The realm
  model, the module graph, termination safety, and the Ruby rendezvous all map.
  The remaining cost is engineering volume (re-implement + re-validate against
  Discourse) and a `libv8-rusty` build, not feasibility.
