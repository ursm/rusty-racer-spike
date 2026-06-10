# rusty_racer

Embed [V8](https://v8.dev/) in Ruby, built on [rusty_v8](https://crates.io/crates/v8)
(the `v8` crate) and [Magnus](https://github.com/matsadler/magnus) via
[rb-sys](https://github.com/oxidize-rb/rb-sys).

> Early and experimental — the API still moves. A dedicated V8 thread per
> `Context`, with Ruby threads rendezvousing over channels.

## What it can do

```ruby
require "rusty_racer"

ctx = RustyRacer::Context.new(timeout_ms: 1000)

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
ctx2.compile_module(src, cached_data: blob)   # skips reparse
```

Also: `Snapshot` (startup blobs), `Context#create_realm` (isolated globals in
one isolate), `Context#perform_microtask_checkpoint` (manual event-loop
control — microtasks never auto-drain), `Context#dynamic_import_resolver=`,
`Platform.set_flags!`.

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
