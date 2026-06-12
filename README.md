# rusty_racer

Embed [V8](https://v8.dev/) in Ruby, built on [rusty_v8](https://crates.io/crates/v8)
(the `v8` crate) and [Magnus](https://github.com/matsadler/magnus) via
[rb-sys](https://github.com/oxidize-rb/rb-sys).

> Early and experimental — the API still moves. A dedicated V8 thread per
> `Context`, with Ruby threads rendezvousing over channels.

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
  e.backtrace   # => ["app.js:1:7:in '<anonymous>'", ...]
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

# Bytecode cache for cross-process boot:
blob = ctx.compile_module(src, produce_cache: true).cached_data
iso2.context.compile_module(src, cached_data: blob)   # skips reparse

# Warm cache: run/evaluate first, then create_code_cache picks up the inner
# functions V8 compiled while running (produce_cache only sees the top level).
s = ctx.compile(src, filename: "/app.js")
s.run
warm = s.create_code_cache                            # includes hot inner fns
```

Also: `Snapshot` (startup blobs), `Isolate#create_context` (an extra realm —
its own globals sharing the isolate's heap; all realms are mutually
same-origin, so with a host namespace `NS.contextGlobal(id)` reaches another
realm's `globalThis` like a same-origin `iframe.contentWindow`, and is not an
isolation boundary), `Isolate#perform_microtask_checkpoint` (manual drain; the
default `microtasks: :auto` also drains at the end of each outermost
eval/call/evaluate, while `microtasks: :explicit` leaves draining fully manual
— there is no event loop or timers either way), `Isolate#terminate`,
`Isolate#dynamic_import_resolver=`, `Context#reset`, `Platform.set_flags!`.

### `Context#reset`

`reset` swaps the realm's `globalThis` for a fresh `v8::Context`, reusing the
warm isolate (csim's per-visit reset). Its contract:

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

## Building

The stock `v8` crate prebuilt links as a binary (initial-exec TLS), which a Ruby
extension's shared object can't use. The extension therefore needs a
**library-TLS** `librusty_v8.a`. Either:

- point `RUSTY_V8_ARCHIVE` at a prebuilt library-TLS archive, or
- set `V8_FROM_SOURCE=1` to build V8 from the `denoland/rusty_v8` git tree
  (large: lots of disk/RAM/time).

```ruby
# In a consuming Gemfile, while developing:
gem "rusty_racer", path: "../rusty_racer"
```

## License

MIT.
