# rusty_racer

Embed [V8](https://v8.dev/) in Ruby, built on [rusty_v8](https://crates.io/crates/v8)
(the `v8` crate) and [Magnus](https://github.com/matsadler/magnus) via
[rb-sys](https://github.com/oxidize-rb/rb-sys).

> Early and experimental — the API still moves. Each `Isolate` runs V8 in-thread
> on the Ruby thread that created it (the GVL is released around the JS run), and
> is **thread-confined**: every operation must happen on that owner thread, or it
> raises. `Isolate#terminate` is the exception — it is safe from any thread.

## What it can do

Names follow V8's: an `Isolate` is the VM; it hands out `Context`s (v8::Context,
a realm) that you run JS in.

```ruby
require "rusty_racer"

iso = RustyRacer::Isolate.new(timeout_ms: 1000)
ctx = iso.context                        # the default context

ctx.eval("1 + 1")                        # => 2
ctx.eval("({a: 1, b: [true, 'x']})")     # => {"a"=>1, "b"=>[true, "x"]}

# Call a JS function with marshalled args (BigInt/Date/Map/Set/shared refs
# all round-trip faithfully).
ctx.eval("function add(a, b) { return a + b }")
ctx.call("add", 20, 22)                  # => 42
ctx.call_void("doSideEffect")            # runs it; never marshals the return

# Ruby callbacks into JS; a raised Ruby exception becomes a JS exception.
ctx.attach("rubyUpcase", ->(s) { s.upcase })
ctx.eval("rubyUpcase('hi')")             # => "HI"

# Stack traces: JS errors carry the JS stack as the Ruby backtrace.
begin
  ctx.eval("throw new Error('boom')", filename: "app.js")
rescue RustyRacer::RuntimeError => e
  e.message     # => "Error: boom"
  e.backtrace   # => ["app.js:1:7"]  (named frames read "app.js:1:25:in 'fn'")
end
```

ES modules (the embedder owns the URL→module registry):

```ruby
dep = ctx.compile_module("export const x = 21;", filename: "/dep.js")
app = ctx.compile_module('import {x} from "./dep.js"; export const r = x * 2;',
                         filename: "/app.js")
app.instantiate { |specifier, referrer| dep if specifier == "./dep.js" }
app.evaluate
app.namespace["r"]                       # => 42
```

Classic `<script>`s work the same way: `ctx.compile("1 + 1").run` # => 2.

### Bytecode caching

V8 compiles lazily: the top level up front, each function body on first call.
Caches can be produced two ways, matching that.

```ruby
src = "function double(x) { return x * 2 }; double(21)"

# produce_cache: — a cold cache taken at compile time (top level only). Persist
# it, then pass it back via cached_data: to skip the reparse on the next boot,
# even in another process or isolate.
blob  = ctx.compile(src, produce_cache: true).cached_data
other = RustyRacer::Isolate.new.context.compile(src, cached_data: blob)
other.cache_rejected?            # => false (true if the blob was stale)

# create_code_cache — a warm cache from the current compile state. Run a script
# (or evaluate a module) first, and it also captures the inner functions that
# actually ran — the warm cache a browser keeps; produce_cache can't see them.
s = ctx.compile(src)
s.run
warm = s.create_code_cache       # binary String, or nil if V8 can't serialize

# eager: compiles every function up front instead of lazily (~2× compile time,
# more memory) — worth it only when producing a cache. Ignored with cached_data:.
ctx.compile(src, produce_cache: true, eager: true)
```

Both `compile` (classic scripts → `Script#run`) and `compile_module` (ES modules
→ `#instantiate`/`#evaluate`) take `cached_data:`, `produce_cache:`, and `eager:`,
and expose `#cached_data` / `#cache_rejected?` / `#create_code_cache`.

Also available:

- **`Snapshot`** — startup blobs: boot an isolate from a baked-in heap and code
  cache.
- **`Isolate#create_context`** — an extra realm with its own globals, sharing the
  isolate's heap. All realms are mutually same-origin (with a host namespace,
  `NS.contextGlobal(id)` reaches another realm's `globalThis`, like a same-origin
  `iframe.contentWindow`), so this is **not** an isolation boundary.
- **`Isolate#perform_microtask_checkpoint`** — manual microtask drain. The default
  `microtasks: :auto` also drains at the end of each outermost eval/call/evaluate;
  `microtasks: :explicit` leaves it fully manual. There is no event loop or timers
  either way.
- **`Isolate#terminate`**, **`Isolate#dynamic_import_resolver=`**,
  **`Context#reset`** (below), and **`Platform.set_flags!`**.

### `Context#reset`

`reset` swaps the realm's `globalThis` for a fresh `v8::Context`, reusing the
warm isolate — a per-visit reset that avoids rebuilding the VM. Its contract:

- **The snapshot is replayed.** On a snapshotted isolate the fresh context is
  re-deserialized from the snapshot, so the snapshot's baked-in globals — and
  its precompiled code cache — come back. `reset` means "back to the snapshot"
  (or to an empty realm, with no snapshot).
- **Runtime mutations are dropped.** Anything set on the realm at runtime is gone.
- **Host fns are dropped.** Functions `attach`/`attach_many`'d into the realm are
  released (their GC roots freed); re-attach them after a reset.
- **Modules and classic scripts are dropped.** Handles compiled in the realm die
  with the old context.
- **The realm id and the shared same-origin token are preserved** — the id keeps
  addressing the realm, now backed by the fresh context.
- **`reset` is refused (raises), leaving the realm untouched, when** a microtask
  checkpoint is draining, the realm is unknown/disposed, or a request for it is
  suspended on the V8 stack (e.g. resetting a realm from inside one of its own
  host fns).

## ExecJS

rusty_racer ships an optional [ExecJS](https://github.com/rails/execjs) runtime,
so any ExecJS consumer (asset pipelines, CoffeeScript/Babel/Uglify wrappers, …)
can run on V8-in-Ruby with no code change:

```ruby
require "rusty_racer/execjs"
ExecJS.runtime = RustyRacer::ExecJSRuntime.new

ExecJS.eval("'foo bar'.toUpperCase()")   # => "FOO BAR"
ctx = ExecJS.compile("function add(a, b) { return a + b }")
ctx.call("add", 1, 2)                     # => 3
```

The adapter is **opt-in** — `rusty_racer` never requires `execjs` itself, so it
stays a non-dependency; `require "rusty_racer/execjs"` pulls it in only when you
ask. Values cross with ExecJS's JSON semantics (functions and `undefined` drop
out, Dates become ISO strings), matching what ExecJS's external runtimes give, so
results are identical whatever runtime a library picked. The integration is
verified against ExecJS's own runtime contract suite (`test/execjs_test.rb`).

## Threading

An `Isolate` runs V8 **in-thread** on the Ruby thread that created it, and is
**thread-confined**: every operation on it — and on the `Context`s, `Module`s,
and `Script`s it hands out — must run on that owner thread. A V8 isolate is bound
to one native thread (rusty_v8 exposes no `v8::Locker`), so using it from another
thread raises `RustyRacer::WrongThreadError` rather than corrupting the VM.

- **`Isolate#terminate` is the one exception** — it is safe to call from any
  thread (it stops a runaway script on the owner thread).
- **Dispose on the owner thread.** `Isolate#dispose` must run on the owner
  thread. If the last reference to an isolate is instead garbage-collected on a
  *different* thread (e.g. its owner thread already exited), it cannot be
  disposed and the V8 isolate **leaks** until the process exits. To avoid this,
  call `iso.dispose` on the owner thread before that thread ends — and watch
  `RustyRacer.leaked_isolate_count` (and `RustyRacer.live_isolate_count`) to
  confirm a long-running, thread-churning workload isn't leaking.

One isolate per thread is the supported model; share work between threads by
giving each thread its own isolate.

### Fibers

In-thread V8 runs on whatever stack the calling Ruby code is on — including a
**Fiber**'s separate stack (a plain `Enumerator` is a Fiber, so this is common:
`Capybara::Result#find`, lazy enumerators, …). This works on the **main thread**,
where the process stack is the highest address and every Fiber sits below it.

On a **non-main thread** it does not: V8 anchors its "is this the central stack?"
check to that thread's native stack top (a pthread value it caches, with no API
to retarget), and a Fiber allocated *above* that top — the usual case off the
main thread — falls outside the check, so V8 aborts the process on the next GC or
thrown exception. So **don't call into an isolate from inside a Fiber on a
worker thread**; drive isolate ops directly on the thread, or keep
Fiber/Enumerator-mediated JS calls on the main thread.

## Installation

Precompiled gems bundle V8 — no V8 build, no Rust toolchain — for Ruby 3.3, 3.4,
and 4.0 on:

- **Linux:** x86_64 and arm64 (aarch64)
- **macOS:** arm64 (Apple silicon) and x86_64 (Intel)

```ruby
gem "rusty_racer"
```

or `gem install rusty_racer`. On any other platform or Ruby, the source gem
builds the extension at install time — see below.

## Building from source

The stock `v8` crate prebuilt links as a binary (initial-exec TLS), which a Ruby
extension's shared object can't use. A source build therefore needs a
**library-TLS** `librusty_v8.a`. Either:

- point `RUSTY_V8_ARCHIVE` at a prebuilt library-TLS archive, or
- set `V8_FROM_SOURCE=1` to build V8 from the `denoland/rusty_v8` git tree
  (large: lots of disk, RAM, and time).

## License

MIT.
