# frozen_string_literal: true

require "minitest/autorun"
require "rusty_racer"

# The stage-2 probes as a suite cibuildgem runs natively on each platform —
# proving the from-source V8 build links and runs, not just compiles. Mapped to
# the mini_racer-csim audit's hang classes where relevant.
class RustyRacerTest < Minitest::Test
  def setup
    @iso = RustyRacer::Isolate.new
    @ctx = @iso.context
  end

  def test_classic_script_compile_run
    s = @ctx.compile("globalThis.X = 40; X + 2", filename: "/inline.js")
    assert_equal 42, s.run
    assert_equal 40, @ctx.eval("globalThis.X")
    assert_equal 42, s.run # re-runnable
  end

  def test_classic_script_top_level_lexical_visible_to_later_eval
    # const/let/class at script top level must persist for later evals/scripts
    # (shared global lexical environment) — load-bearing for csim.
    @ctx.compile("const SHARED = 7;").run
    assert_equal 7, @ctx.eval("SHARED")
  end

  def test_classic_script_parse_error_is_parse_error
    assert_raises(RustyRacer::ParseError) { @ctx.compile("function (", filename: "/bad.js") }
  end

  def test_classic_script_runtime_error_is_runtime_error
    s = @ctx.compile('throw new Error("scriptboom")', filename: "/t.js")
    e = assert_raises(RustyRacer::RuntimeError) { s.run }
    assert_includes e.message, "scriptboom"
  end

  def test_classic_script_bytecode_cache_round_trip
    src = "(() => 1 + 2)()"
    blob = @ctx.compile(src, filename: "/c.js", produce_cache: true).cached_data
    refute_nil blob
    assert_equal Encoding::ASCII_8BIT, blob.encoding

    iso2 = RustyRacer::Isolate.new
    s = iso2.context.compile(src, filename: "/c.js", cached_data: blob)
    assert_equal false, s.cache_rejected?
    assert_equal 3, s.run
  end

  def test_classic_script_run_honours_timeout
    iso = RustyRacer::Isolate.new(timeout_ms: 50)
    s = iso.context.compile("for(;;){}", filename: "/spin.js")
    assert_raises(RustyRacer::ScriptTerminatedError) { s.run }
    assert_equal 2, iso.context.eval("1 + 1") # isolate still usable
  end

  def test_classic_script_dispose
    s = @ctx.compile("1")
    assert_equal false, s.disposed?
    s.dispose
    assert_equal true, s.disposed?
    assert_raises(::RuntimeError) { s.run }
  end

  def test_cached_data_version_tag
    tag = RustyRacer.cached_data_version_tag
    assert_kind_of Integer, tag
    assert_operator tag, :!=, 0
  end

  def test_context_has_stable_id
    assert_equal 0, @ctx.id # the default context
    a = @iso.create_context
    b = @iso.create_context
    assert_operator a.id, :>, 0
    refute_equal a.id, b.id
  end

  def test_eval_roundtrip
    assert_equal 2, @ctx.eval("1 + 1")
    assert_equal 3.0, @ctx.eval("1.5 * 2")
    assert_equal "hello", @ctx.eval('"he" + "llo"')
    assert_equal true, @ctx.eval("1 < 2")
    assert_nil @ctx.eval("null")
  end

  def test_js_exception_becomes_ruby_exception
    e = assert_raises(RustyRacer::RuntimeError) { @ctx.eval('throw new Error("boom")') }
    assert_includes e.message, "boom"
  end

  def test_syntax_error_is_parse_error
    assert_raises(RustyRacer::ParseError) { @ctx.eval("this is not valid js ===") }
  end

  def test_parse_error_includes_location
    e = assert_raises(RustyRacer::ParseError) do
      @ctx.eval("let x = ;", filename: "boot.js")
    end
    assert_includes e.message, "boot.js"
  end

  def test_runtime_error_carries_js_stack_as_backtrace
    src = <<~JS
      function inner() { throw new Error("kaboom") }
      function outer() { inner() }
      outer();
    JS
    e = assert_raises(RustyRacer::RuntimeError) { @ctx.eval(src, filename: "app.js") }
    assert_includes e.message, "kaboom"
    refute_nil e.backtrace
    joined = e.backtrace.join("\n")
    # the JS frames are reconstructed into the Ruby backtrace, with our filename
    assert_includes joined, "app.js"
    assert_includes joined, "inner"
  end

  def test_multiline_error_message_does_not_leak_into_backtrace
    e = assert_raises(RustyRacer::RuntimeError) { @ctx.eval('throw new Error("line1\nline2")') }
    # every backtrace frame must look like a frame (carry a location), not a
    # stray fragment of the multi-line message
    e.backtrace.each { |f| refute_equal "line2", f }
  end

  def test_stackless_throw_has_no_host_backtrace
    # throwing a non-Error has no JS stack; the backtrace must not be backfilled
    # with host-side (Rust/pump) frames.
    e = assert_raises(RustyRacer::RuntimeError) { @ctx.eval("throw 42") }
    assert_equal [], e.backtrace
  end

  def test_eval_filename_appears_in_thrown_stack
    e = assert_raises(RustyRacer::RuntimeError) do
      @ctx.eval('throw new Error("boom")', filename: "widget.js")
    end
    assert(e.backtrace.any? { |line| line.include?("widget.js") }, "filename missing from backtrace")
  end

  def test_other_ruby_threads_progress_during_eval
    counter = 0
    t = Thread.new { loop { counter += 1; Thread.pass } }
    @ctx.eval("const until = Date.now() + 200; while (Date.now() < until) {}")
    t.kill
    t.join
    assert_operator counter, :>, 1000, "GVL not released during eval"
  end

  def test_host_namespace_injects_drain_microtasks
    ctx = RustyRacer::Isolate.new(host_namespace: "MiniRacer").context
    assert_equal "object", ctx.eval("typeof MiniRacer")
    assert_equal "function", ctx.eval("typeof MiniRacer.drainMicrotasks")
    order = ctx.eval(<<~JS)
      const seen = [];
      Promise.resolve().then(() => seen.push("microtask"));
      seen.push("before");
      MiniRacer.drainMicrotasks();
      seen.push("after");
      seen;
    JS
    assert_equal %w[before microtask after], order
  end

  def test_host_namespace_survives_reset
    ctx = RustyRacer::Isolate.new(host_namespace: "MiniRacer").context
    ctx.reset
    assert_equal "object", ctx.eval("typeof MiniRacer")
  end

  def test_no_host_namespace_by_default
    assert_equal "undefined", @ctx.eval("typeof MiniRacer")
  end

  def test_set_flags_after_init_raises
    # V8 is already initialized by setup's Context.new, so set_flags! must
    # refuse (a successful set_flags! needs a fresh process — see csim's
    # subprocess single-threaded tests).
    assert_raises(RustyRacer::PlatformAlreadyInitialized) do
      RustyRacer::Platform.set_flags!(:use_strict)
    end
  end

  def test_marshals_arrays_and_hashes
    # JS -> Ruby
    assert_equal [1, 2, 3], @ctx.eval("[1, 2, 3]")
    assert_equal({ "a" => 1, "b" => [true, "x"] }, @ctx.eval('({a: 1, b: [true, "x"]})'))
    # Ruby -> JS -> Ruby through call args + return
    @ctx.eval("function echo(x) { return x }")
    assert_equal({ "k" => [1, 2] }, @ctx.call("echo", { "k" => [1, 2] }))
  end

  def test_strict_bool_marshalling
    # regression: an Integer arg must NOT become `true` (bool::try_convert is
    # truthy; ruby_to_jsval checks the actual true/false singletons instead).
    @ctx.eval("function kind(x) { return typeof x }")
    assert_equal "number", @ctx.call("kind", 42)
    assert_equal "boolean", @ctx.call("kind", true)
    assert_equal "string", @ctx.call("kind", "hi")
  end

  def test_date_marshals_to_time
    # JS Date -> Ruby Time
    t = @ctx.eval('new Date("2021-01-02T03:04:05.000Z")')
    assert_kind_of Time, t
    assert_equal Time.utc(2021, 1, 2, 3, 4, 5).to_i, t.to_i
    # Ruby Time -> JS Date -> back, through call args
    @ctx.eval("function year(d) { return d.getUTCFullYear() }")
    assert_equal 2021, @ctx.call("year", Time.utc(2021, 6, 1))
    # round-trip identity (to the second)
    now = Time.utc(2022, 3, 4, 5, 6, 7)
    @ctx.eval("function echo(x) { return x }")
    assert_equal now.to_i, @ctx.call("echo", now).to_i
  end

  def test_bigint_marshals_to_integer_without_precision_loss
    # JS BigInt -> Ruby Integer (well beyond Float's 2**53 exact range)
    assert_equal 2**53 + 1, @ctx.eval("BigInt(2)**53n + 1n")
    assert_equal(-(2**70), @ctx.eval("-(2n**70n)"))
    big = 123456789012345678901234567890
    assert_equal big, @ctx.eval("123456789012345678901234567890n")

    # Ruby Integer -> JS: a bignum becomes a BigInt, not a lossy Number
    @ctx.eval("function isBig(x) { return typeof x === 'bigint' }")
    assert_equal true, @ctx.call("isBig", 2**80)
    @ctx.eval("function echo(x) { return x }")
    assert_equal big, @ctx.call("echo", big)
    assert_equal(-big, @ctx.call("echo", -big))

    # small ints stay JS numbers (not bigint)
    assert_equal false, @ctx.call("isBig", 42)
    # integers beyond Number's exact range (2**53) become BigInt even within
    # i64, so precision is never lost (regression guard)
    assert_equal true, @ctx.call("isBig", 2**60)
    assert_equal 2**60 + 1, @ctx.call("echo", 2**60 + 1)
    # 2**53 itself is still exactly representable -> stays a Number
    assert_equal false, @ctx.call("isBig", 2**53)
  end

  def test_large_float_stays_number_not_bigint
    # a Float must not be coerced to Integer/BigInt (strict Integer typing)
    @ctx.eval("function kind(x) { return typeof x }")
    assert_equal "number", @ctx.call("kind", 1e300)
    assert_equal 1e300, @ctx.call("echo", 1e300) if @ctx.eval("typeof echo") == "function"
    @ctx.eval("function echo2(x) { return x }")
    assert_in_delta 1e300, @ctx.call("echo2", 1e300), 0.0
  end

  def test_shared_acyclic_call_arg_not_lost
    # a shared (acyclic) substructure in a call arg must survive, not drop to null
    shared = {"v" => 1}
    @ctx.eval("function bv(x) { return x.b && x.b.v }")
    assert_equal 1, @ctx.call("bv", {"a" => shared, "b" => shared})
  end

  def test_call_preserves_arg_identity_within_one_arg
    # Function::call marshals args faithfully, so within a single arg a shared
    # object stays one object (===), not two copies.
    shared = {"v" => 1}
    @ctx.eval("function sameRef(x) { return x.a === x.b }")
    assert_equal true, @ctx.call("sameRef", {"a" => shared, "b" => shared})
  end

  def test_call_resolves_dotted_path_with_receiver
    @ctx.eval("globalThis.math = { base: 100, addBase(x) { return this.base + x } }")
    # dotted path resolves math.addBase and uses `math` as `this`
    assert_equal 105, @ctx.call("math.addBase", 5)
  end

  def test_call_passes_bigint_arg_without_loss
    @ctx.eval("function inc(x) { return x + 1n }")
    assert_equal 2**70 + 1, @ctx.call("inc", 2**70)
  end

  def test_call_void_runs_without_marshalling_return
    # call_void runs the fn for its side effect but never walks the return,
    # so a huge/cyclic result is fine and the Ruby return is nil.
    @ctx.eval("function makeCyclic() { const a = {}; a.self = a; globalThis.RAN = true; return a }")
    assert_nil @ctx.call_void("makeCyclic")
    assert_equal true, @ctx.eval("globalThis.RAN")
  end

  def test_attach_under_host_namespace
    ctx = RustyRacer::Isolate.new(host_namespace: "MiniRacer").context
    ctx.attach("MiniRacer.rubyAdd", proc { |a, b| a + b })
    assert_equal 7, ctx.eval("MiniRacer.rubyAdd(3, 4)")
    # creates intermediate objects even without a pre-existing namespace
    ctx.attach("Helpers.greet", proc { |who| "hi #{who}" })
    assert_equal "hi bob", ctx.eval('Helpers.greet("bob")')
  end

  def test_context_default_timeout
    ctx = RustyRacer::Isolate.new(timeout_ms: 50).context
    assert_raises(RustyRacer::ScriptTerminatedError) { ctx.eval("for(;;){}") }
    # context survives and a normal eval still works
    assert_equal 3, ctx.eval("1 + 2")
    # the default also applies to call
    ctx.eval("function spin() { for(;;){} }")
    assert_raises(RustyRacer::ScriptTerminatedError) { ctx.call("spin") }
  end

  def test_host_fn_invoked_from_microtask_during_checkpoint
    # csim's settle model: a Promise resolved via a host callback. The host fn
    # fires from a microtask during the checkpoint and must still route to Ruby.
    @ctx.attach("rubyVal", proc { 99 })
    @ctx.eval('globalThis.out = null; Promise.resolve().then(() => { globalThis.out = rubyVal() });')
    assert_nil @ctx.eval("globalThis.out") # not run yet (explicit policy)
    @iso.perform_microtask_checkpoint
    assert_equal 99, @ctx.eval("globalThis.out")
  end

  def test_attach_does_not_clobber_primitive_global
    @ctx.eval("globalThis.x = 42")
    assert_raises(RustyRacer::RuntimeError) { @ctx.attach("x.y", proc { 1 }) }
    assert_equal 42, @ctx.eval("globalThis.x") # untouched
  end

  def test_perform_microtask_checkpoint_drains_queue
    order = @ctx.eval(<<~JS)
      globalThis.seen = [];
      Promise.resolve().then(() => seen.push("micro"));
      seen.push("before");
      seen;
    JS
    assert_equal ["before"], order
    @iso.perform_microtask_checkpoint
    assert_equal %w[before micro], @ctx.eval("globalThis.seen")
  end

  def test_call_unknown_name_raises_not_injects
    # name is resolved as a property path, never eval'd, so a bogus/injection-y
    # name cannot execute code — it just fails to resolve to a function.
    assert_raises(RustyRacer::RuntimeError) { @ctx.call("no.such.fn") }
    assert_raises(RustyRacer::RuntimeError) { @ctx.call("(()=>42)") }
  end

  def test_js_map_marshals_to_ruby_hash
    h = @ctx.eval('new Map([["a", 1], [2, "two"], ["nested", {x: 9}]])')
    assert_kind_of Hash, h
    assert_equal 1, h["a"]
    assert_equal "two", h[2]            # non-string key preserved
    assert_equal({"x" => 9}, h["nested"])
  end

  def test_js_set_marshals_to_ruby_set
    s = @ctx.eval('new Set([1, 2, 2, 3])')
    assert_kind_of Set, s
    assert_equal Set[1, 2, 3], s
  end

  def test_ruby_set_marshals_to_js_set
    @ctx.attach("getSet", proc { Set[1, 2, 3] })
    assert_equal "object", @ctx.eval("typeof getSet()")
    assert_equal true, @ctx.eval("getSet() instanceof Set")
    assert_equal 3, @ctx.eval("getSet().size")
    assert_equal true, @ctx.eval("getSet().has(2)")
  end

  def test_ruby_set_passes_through_call_as_js_set
    # a Ruby Set passed via Context#call arrives as a real JS Set
    @ctx.eval("function hasIt(s, x) { return s instanceof Set && s.has(x) }")
    assert_equal true, @ctx.call("hasIt", Set[1, 2, 3], 2)
  end

  def test_shared_reference_preserved_js_to_ruby
    # one object referenced twice stays one object on the Ruby side
    h = @ctx.eval('const x = {v: 1}; ({a: x, b: x})')
    assert_same h["a"], h["b"]
    h["a"]["v"] = 99
    assert_equal 99, h["b"]["v"]
  end

  def test_cycle_preserved_js_to_ruby
    # a self-referential object round-trips as a Ruby cycle, not a crash/truncation
    h = @ctx.eval('const a = {name: "root"}; a.self = a; a')
    assert_equal "root", h["name"]
    assert_same h, h["self"]
    assert_same h, h["self"]["self"]["self"]
  end

  def test_cycle_preserved_ruby_to_js
    # build a cyclic Ruby Hash, hand it to JS via a host fn return, prove JS
    # sees the cycle (the ref table reconstructs identity on the V8 side).
    cyclic = {}
    cyclic["self"] = cyclic
    @ctx.attach("getCyclic", proc { cyclic })
    assert_equal true, @ctx.eval("(() => { const x = getCyclic(); return x.self === x })()")
  end

  def test_shared_reference_preserved_ruby_to_js
    shared = {"v" => 1}
    pair = {"a" => shared, "b" => shared}
    @ctx.attach("getPair", proc { pair })
    assert_equal true, @ctx.eval("(() => { const x = getPair(); return x.a === x.b })()")
  end

  def test_invalid_date_raises_not_silent_nil
    # parity with csim's des_date: a non-finite Date is a RangeError, not nil.
    assert_raises(RangeError) { @ctx.eval('new Date("not a date")') }
  end

  def test_reset_clears_globals
    @ctx.eval("globalThis.x = 41")
    assert_equal 41, @ctx.eval("globalThis.x")
    @ctx.reset
    assert_equal "undefined", @ctx.eval("typeof globalThis.x")
    assert_equal 2, @ctx.eval("1 + 1") # realm usable after reset
  end

  def test_snapshot_bakes_globals_into_a_booted_context
    snap = RustyRacer::Snapshot.new(<<~JS)
      globalThis.GREETING = "from snapshot";
      function double(x) { return x * 2 }
    JS
    assert_operator snap.size, :>, 0

    ctx = RustyRacer::Isolate.new(snapshot: snap).context
    assert_equal "from snapshot", ctx.eval("GREETING")
    assert_equal 42, ctx.eval("double(21)")

    # a context with no snapshot does not have those globals
    assert_equal "undefined", @ctx.eval("typeof GREETING")
  end

  def test_snapshot_dump_and_load_round_trip
    snap = RustyRacer::Snapshot.new('globalThis.V = 7')
    blob = snap.dump
    assert_equal Encoding::ASCII_8BIT, blob.encoding
    reloaded = RustyRacer::Snapshot.load(blob)
    assert_equal snap.size, reloaded.size
    ctx = RustyRacer::Isolate.new(snapshot: reloaded).context
    assert_equal 7, ctx.eval("V")
  end

  def test_snapshot_warmup_grows_and_still_boots
    snap = RustyRacer::Snapshot.new('globalThis.A = 1')
    before = snap.size
    snap.warmup!('function warmMe() { return A + 1 } warmMe();')
    assert_operator snap.size, :>=, before
    ctx = RustyRacer::Isolate.new(snapshot: snap).context
    assert_equal 1, ctx.eval("A")
    assert_equal 2, ctx.eval("warmMe()")
  end

  def test_snapshot_with_broken_code_raises
    assert_raises(RustyRacer::SnapshotError) do
      RustyRacer::Snapshot.new("this is not valid js ===")
    end
  end

  def test_create_realm_is_isolated_from_main_and_siblings
    a = @iso.create_context
    b = @iso.create_context
    @ctx.eval("globalThis.x = 'main'")
    a.eval("globalThis.x = 'a'")
    b.eval("globalThis.x = 'b'")
    # each realm has its own globalThis
    assert_equal "main", @ctx.eval("globalThis.x")
    assert_equal "a", a.eval("globalThis.x")
    assert_equal "b", b.eval("globalThis.x")
    # the main realm never saw the realms' globals
    assert_equal "undefined", a.eval("typeof globalThis.notThere")
  end

  def test_module_compiled_per_context_and_evaluates_in_it
    other = @iso.create_context
    m = other.compile_module("globalThis.WHERE = 'other'; export const x = 1;")
    m.instantiate { |_s, _r| nil }
    m.evaluate
    # the module ran in `other`, not the default context
    assert_equal "other", other.eval("globalThis.WHERE")
    assert_equal "undefined", @ctx.eval("typeof globalThis.WHERE")
  end

  def test_cross_context_import_is_rejected_not_aborted
    # a resolve block returning a module from a *different* context must fail
    # cleanly (V8 would CHECK-abort the process otherwise).
    other = @iso.create_context
    dep_elsewhere = other.compile_module("export const x = 1;", filename: "/dep.js")
    app = @ctx.compile_module('import {x} from "./dep.js";', filename: "/app.js")
    assert_raises(RustyRacer::RuntimeError) do
      app.instantiate { |_s, _r| dep_elsewhere } # foreign-context dep
    end
    # the isolate is still usable (no crash)
    assert_equal 2, @ctx.eval("1 + 1")
  end

  def test_realm_call_and_attach
    r = @iso.create_context
    r.eval("function mul(a, b) { return a * b }")
    assert_equal 12, r.call("mul", 3, 4)
    r.attach("rubyAdd", proc { |a, b| a + b })
    assert_equal 30, r.eval("rubyAdd(10, 20)")
    # the host fn lives only in that realm, not the main one
    assert_equal "undefined", @ctx.eval("typeof rubyAdd")
  end

  def test_realm_dispose
    r = @iso.create_context
    assert_equal false, r.disposed?
    assert_equal 5, r.eval("2 + 3")
    r.dispose
    assert_equal true, r.disposed?
    assert_raises(::RuntimeError) { r.eval("1") }
    r.dispose # idempotent
    # the parent context still works after a realm is disposed
    assert_equal 2, @ctx.eval("1 + 1")
  end

  def test_es_module_compile_instantiate_evaluate
    dep = @ctx.compile_module("export const x = 21;", filename: "/dep.js")
    app = @ctx.compile_module(
      'import {x} from "./dep.js"; export const result = x * 2; globalThis.RAN = result;',
      filename: "/app.js"
    )
    # the resolve block maps each import to an already-compiled Module
    app.instantiate do |specifier, referrer_url|
      assert_equal "/app.js", referrer_url
      specifier == "./dep.js" ? dep : nil
    end
    app.evaluate
    assert_equal 42, @ctx.eval("globalThis.RAN")
    # module namespaces expose exports
    assert_equal 42, app.namespace["result"]
    assert_equal 21, dep.namespace["x"]
  end

  def test_es_module_unresolved_import_raises
    app = @ctx.compile_module('import {x} from "./missing.js";', filename: "/app.js")
    assert_raises(RustyRacer::RuntimeError) { app.instantiate { |_spec, _ref| nil } }
  end

  def test_es_module_syntax_error_is_parse_error
    assert_raises(RustyRacer::ParseError) { @ctx.compile_module("import from", filename: "/bad.js") }
  end

  def test_es_module_namespace_before_instantiate_raises_not_aborts
    # guard against V8 CHECK-aborting the process on an un-instantiated module
    m = @ctx.compile_module("export const a = 1;")
    assert_raises(RustyRacer::RuntimeError) { m.namespace }
    assert_raises(RustyRacer::RuntimeError) { m.evaluate }
  end

  def test_es_module_top_level_throw_surfaces
    m = @ctx.compile_module('throw new Error("boom in module");', filename: "/t.js")
    m.instantiate { |_s, _r| nil }
    e = assert_raises(RustyRacer::RuntimeError) { m.evaluate }
    assert_includes e.message, "boom in module"
  end

  def test_es_module_resolver_raise_propagates
    app = @ctx.compile_module('import {x} from "./dep.js";', filename: "/app.js")
    e = assert_raises(ArgumentError) { app.instantiate { |_s, _r| raise ArgumentError, "resolver boom" } }
    assert_includes e.message, "resolver boom"
  end

  def test_es_module_resolver_wrong_type_raises
    app = @ctx.compile_module('import {x} from "./dep.js";', filename: "/app.js")
    assert_raises(TypeError) { app.instantiate { |_s, _r| 42 } }
  end

  def test_dynamic_import_resolves_to_a_loaded_module
    # pre-load the dependency (compile + instantiate + evaluate)
    dep = @ctx.compile_module("export const v = 7;", filename: "/dep.js")
    dep.instantiate { |_s, _r| nil }
    dep.evaluate
    @iso.dynamic_import_resolver = ->(specifier, _referrer) { specifier == "/dep.js" ? dep : nil }

    @ctx.eval(<<~JS, filename: "/main.js")
      globalThis.OUT = null;
      import("/dep.js").then(m => { globalThis.OUT = m.v });
    JS
    assert_nil @ctx.eval("globalThis.OUT") # pending until drained (explicit policy)
    @iso.perform_microtask_checkpoint
    assert_equal 7, @ctx.eval("globalThis.OUT")
  end

  def test_dynamic_import_without_resolver_rejects
    @ctx.eval('globalThis.ERR = null; import("/x.js").catch(e => { globalThis.ERR = String(e) });')
    @iso.perform_microtask_checkpoint
    assert_match(/import|not|resolved/i, @ctx.eval("globalThis.ERR"))
  end

  def test_module_cached_data_round_trip
    src = "export const x = 1 + 2;"
    # produce a bytecode cache
    m1 = @ctx.compile_module(src, filename: "/m.js", produce_cache: true)
    blob = m1.cached_data
    refute_nil blob
    assert_operator blob.bytesize, :>, 0
    assert_equal Encoding::ASCII_8BIT, blob.encoding

    # consume it in a fresh context: accepted (not rejected), same result
    ctx2 = RustyRacer::Isolate.new.context
    m2 = ctx2.compile_module(src, filename: "/m.js", cached_data: blob)
    assert_equal false, m2.cache_rejected?
    m2.instantiate { |_s, _r| nil }
    m2.evaluate
    assert_equal 3, m2.namespace["x"]
  end

  def test_module_cache_rejected_on_source_mismatch
    blob = @ctx.compile_module("export const x = 1;", produce_cache: true).cached_data
    # a different source invalidates the cache; V8 recompiles and flags rejected
    m = @ctx.compile_module("export const x = 999;", cached_data: blob)
    assert_equal true, m.cache_rejected?
    m.instantiate { |_s, _r| nil }
    m.evaluate
    assert_equal 999, m.namespace["x"] # still correct (recompiled from source)
  end

  def test_module_non_binary_cached_data_raises
    # a cache string that isn't ASCII-8BIT (e.g. read without 'rb') is refused
    assert_raises(EncodingError) do
      @ctx.compile_module("export const x = 1;", cached_data: "not binary".encode("UTF-8"))
    end
  end

  def test_module_without_produce_cache_has_nil_cached_data
    m = @ctx.compile_module("export const x = 1;")
    assert_nil m.cached_data
    assert_equal false, m.cache_rejected?
  end

  def test_es_module_dispose
    m = @ctx.compile_module("export const a = 1;")
    assert_equal false, m.disposed?
    m.dispose
    assert_equal true, m.disposed?
    assert_raises(::RuntimeError) { m.evaluate }
  end

  def test_call_invokes_global_function
    @ctx.eval("function mul(a, b) { return a * b }")
    assert_equal 6, @ctx.call("mul", 2, 3)
    @ctx.eval('globalThis.greet = (who) => "hi " + who')
    assert_equal "hi bob", @ctx.call("greet", "bob")
  end

  def test_host_function_roundtrip
    @ctx.attach("rubyAdd", proc { |a, b| a + b })
    assert_equal 42, @ctx.eval("rubyAdd(20, 22)")
    assert_equal "ab", @ctx.eval('rubyAdd("a", "b")')
  end

  def test_ruby_exception_in_host_fn_surfaces_and_context_survives
    @ctx.attach("rubyBoom", proc { raise ArgumentError, "no thanks" })
    out = @ctx.eval('(() => { try { rubyBoom(); return "uncaught"; } catch (e) { return "caught:" + String(e).includes("no thanks"); } })()')
    assert_equal "caught:true", out
    # audit #24: the context must not be wedged afterwards
    assert_equal 2, @ctx.eval("1 + 1")
  end

  def test_nested_ruby_js_ruby_js
    @ctx.attach("reenter", proc { @ctx.eval("6 * 7") })
    assert_equal 42, @ctx.eval("reenter()")
  end

  def test_timeout_terminates_and_recovers
    assert_raises(RustyRacer::ScriptTerminatedError) { @ctx.eval("for(;;){}", timeout_ms: 100) }
    assert_equal 4, @ctx.eval("2 + 2")
  end

  def test_late_watchdog_does_not_poison_next_request
    # audit #3: a late TerminateExecution must not leak into the next request.
    100.times do
      begin
        @ctx.eval("{ const u = Date.now() + 1; while (Date.now() < u) {} }", timeout_ms: 1)
      rescue RustyRacer::ScriptTerminatedError
        # terminated this round — fine
      end
      assert_equal 1, @ctx.eval("1")
    end
  end

  def test_stop_from_another_thread_then_usable
    stopper = Thread.new { sleep 0.05; @iso.terminate }
    assert_raises(RustyRacer::ScriptTerminatedError) { @ctx.eval("for(;;){}") }
    stopper.join
    assert_equal 6, @ctx.eval("3 + 3")
  end

  def test_dispose_racing_eval_does_not_hang
    # audit #12/#13/#26: dispose racing an in-flight eval must not hang.
    10.times do
      iso = RustyRacer::Isolate.new
      c = iso.context
      worker = Thread.new do
        c.eval("const u = Date.now() + 30; while (Date.now() < u) {}")
      rescue StandardError
        # disposed/terminated mid-run is acceptable (class varies with the
        # race); hanging is not.
      end
      sleep(rand * 0.03)
      iso.dispose
      assert worker.join(5), "worker hung"
      # post-dispose use raises the plain disposed-context guard, not a JS error
      assert_raises(::RuntimeError) { c.eval("1") }
    end
  end
end
