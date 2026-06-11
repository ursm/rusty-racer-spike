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
    iso = RustyRacer::Isolate.new(microtasks: :explicit)
    ctx = iso.context
    ctx.attach("rubyVal", proc { 99 })
    ctx.eval('globalThis.out = null; Promise.resolve().then(() => { globalThis.out = rubyVal() });')
    assert_nil ctx.eval("globalThis.out") # not run yet (explicit policy)
    iso.perform_microtask_checkpoint
    assert_equal 99, ctx.eval("globalThis.out")
  end

  def test_auto_microtasks_drain_at_end_of_outermost_eval
    # the default mirrors V8's kAuto (the standard embedder contract): the
    # queue drains when the outermost eval/call completes, so promise
    # continuations are visible to the next eval without a manual checkpoint
    @ctx.eval('globalThis.x = 0; Promise.resolve().then(() => { globalThis.x = 1 });')
    assert_equal 1, @ctx.eval("globalThis.x")
  end

  def test_auto_microtasks_do_not_drain_after_nested_ops
    # a nested eval completes at call depth > 0, so it must NOT drain (same as
    # nested script entry in a browser); the queue drains when the OUTER call
    # finishes
    @ctx.attach("f", proc {
      @ctx.eval('Promise.resolve().then(() => { globalThis.n = 1 });')
      @ctx.eval("typeof globalThis.n") # still pending inside the nested window
    })
    assert_equal "undefined", @ctx.call("f")
    assert_equal 1, @ctx.eval("globalThis.n") # drained when the call returned
  end

  def test_explicit_microtasks_option_validation
    assert_raises(ArgumentError) { RustyRacer::Isolate.new(microtasks: :bogus) }
  end

  def test_auto_drain_is_covered_by_the_watchdog
    # the kAuto end-of-script drain runs inside the request's watchdog bracket,
    # so a runaway microtask continuation times out instead of running unbounded
    c = RustyRacer::Isolate.new.context
    assert_raises(RustyRacer::ScriptTerminatedError) do
      c.eval('Promise.resolve().then(() => { for(;;){} }); 42', timeout_ms: 200)
    end
  end

  def test_auto_drain_self_requeueing_microtask_terminates_not_hangs
    # a microtask that re-queues itself would spin the drain forever; the
    # explicit-checkpoint drain (not V8's kAuto, which ignores termination
    # inside Function::Call) must let the watchdog stop it
    iso = RustyRacer::Isolate.new(timeout_ms: 200)
    c = iso.context
    c.eval('function f(){ Promise.resolve().then(function spin(){ Promise.resolve().then(spin) }); return 7 }')
    assert_raises(RustyRacer::ScriptTerminatedError) { c.call('f') }
    assert_equal 2, iso.context.eval('1 + 1') # isolate still usable
  end

  def test_auto_drain_watchdog_timeout_is_not_masked_by_completion_value
    # the watchdog fires DURING the drain, after the script's completion value
    # is computed — the value must not mask the timeout
    c = RustyRacer::Isolate.new.context
    assert_raises(RustyRacer::ScriptTerminatedError) do
      c.eval('Promise.resolve().then(() => { while(true){} }); 99', timeout_ms: 200)
    end
  end

  def test_evaluate_module_honours_timeout
    # Module#evaluate (and the kAuto drain of its TLA continuation) is watchdog-
    # covered like eval/call
    iso = RustyRacer::Isolate.new(timeout_ms: 200)
    m = iso.context.compile_module('await Promise.resolve(); for(;;){}')
    m.instantiate {|_s, _r| nil }
    assert_raises(RustyRacer::ScriptTerminatedError) { m.evaluate }
    assert_equal 2, iso.context.eval('1 + 1')
  end

  def test_attach_through_runaway_setter_times_out
    # attaching writes onto globalThis, which can fire a user setter running
    # arbitrary JS; an infinite loop there must time out, not hang the thread
    iso = RustyRacer::Isolate.new(timeout_ms: 200)
    ctx = iso.context
    ctx.eval("Object.defineProperty(globalThis, 'victim', { set() { for(;;){} }, configurable: true })")
    assert_raises(RustyRacer::ScriptTerminatedError) { ctx.attach("victim", proc { 1 }) }
    assert_equal 2, ctx.eval("1 + 1") # isolate still usable
  end

  def test_attach_host_fn_called_from_setter_routes_to_ruby
    # a host fn invoked by the setter JS that runs during attach must reach
    # Ruby (REPLY_STACK pushed), not silently return undefined
    @ctx.attach("probe", proc { "ruby-saw-it" })
    @ctx.eval("Object.defineProperty(globalThis, 'victim', { set() { globalThis.captured = probe() }, configurable: true })")
    @ctx.attach("victim", proc { 1 })
    assert_equal "ruby-saw-it", @ctx.eval("globalThis.captured")
  end

  def test_attach_does_not_clobber_primitive_global
    @ctx.eval("globalThis.x = 42")
    assert_raises(RustyRacer::RuntimeError) { @ctx.attach("x.y", proc { 1 }) }
    assert_equal 42, @ctx.eval("globalThis.x") # untouched
  end

  def test_perform_microtask_checkpoint_drains_queue
    # the :explicit opt-out (V8's kExplicit): nothing drains until the
    # embedder says so
    iso = RustyRacer::Isolate.new(microtasks: :explicit)
    ctx = iso.context
    order = ctx.eval(<<~JS)
      globalThis.seen = [];
      Promise.resolve().then(() => seen.push("micro"));
      seen.push("before");
      seen;
    JS
    assert_equal ["before"], order
    assert_equal ["before"], ctx.eval("globalThis.seen") # still pending
    iso.perform_microtask_checkpoint
    assert_equal %w[before micro], ctx.eval("globalThis.seen")
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

  def test_snapshot_warmup_keeps_code_cache_but_not_heap_state
    # V8's WarmUpSnapshotDataBlob contract: the warmup code runs in a
    # THROWAWAY context (pre-compiling functions into the blob's code cache);
    # its heap mutations do NOT bake into the blob — the cold state does.
    snap = RustyRacer::Snapshot.new('globalThis.A = 1; function double(x) { return x * 2 }')
    snap.warmup!('globalThis.W = 9; double(21);')
    ctx = RustyRacer::Isolate.new(snapshot: snap).context
    assert_equal 1, ctx.eval('A')                  # cold state kept
    assert_equal 42, ctx.eval('double(21)')        # cold functions kept
    assert_equal 'undefined', ctx.eval('typeof W') # warmup heap state NOT baked
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

  def test_context_global_reaches_another_realm
    # the embedder's iframe.contentWindow: the frame realm's globalThis,
    # reachable from the parent realm (and vice versa — plain V8
    # cross-context access, no security tokens)
    iso = RustyRacer::Isolate.new(host_namespace: 'NS')
    ctx = iso.context
    frame = iso.create_context
    frame.eval('globalThis.WHO = "frame"')
    assert_equal 'frame', ctx.eval("NS.contextGlobal(#{frame.id}).WHO")
    ctx.eval('globalThis.WHO = "main"')
    assert_equal 'main', frame.eval('NS.contextGlobal(0).WHO')
    ctx.eval("NS.contextGlobal(#{frame.id}).fromParent = 42")
    assert_equal 42, frame.eval('globalThis.fromParent')
  end

  def test_context_global_unknown_id_throws
    iso = RustyRacer::Isolate.new(host_namespace: 'NS')
    e = assert_raises(RustyRacer::RuntimeError) { iso.context.eval('NS.contextGlobal(999)') }
    assert_includes e.message, 'unknown context'
  end

  def test_context_of_attributes_values_to_their_context
    iso = RustyRacer::Isolate.new(host_namespace: 'NS')
    ctx = iso.context
    frame = iso.create_context
    assert_equal 0, ctx.eval('NS.contextOf(function f() {})')
    assert_equal frame.id, frame.eval('NS.contextOf(() => 1)')
    # cross-realm: a function created in the frame, inspected from the parent
    frame.eval('globalThis.frameFn = () => 1')
    assert_equal frame.id, ctx.eval("NS.contextOf(NS.contextGlobal(#{frame.id}).frameFn)")
    # primitives have no creation context
    assert_nil ctx.eval('NS.contextOf(42)')
  end

  def test_context_of_a_reset_away_realm_is_undefined
    # a function captured before reset still carries its old realm-id stamp;
    # contextOf must report it as gone (the id no longer maps back to it), not
    # mis-attribute it to the fresh realm now holding that id
    iso = RustyRacer::Isolate.new(host_namespace: 'NS')
    ctx = iso.context
    frame = iso.create_context
    frame.eval('globalThis.oldFn = () => 1')
    ctx.eval("globalThis.captured = NS.contextGlobal(#{frame.id}).oldFn")
    assert_equal frame.id, ctx.eval('NS.contextOf(captured)')
    frame.reset
    assert_nil ctx.eval('NS.contextOf(captured)') # its realm was reset away
  end

  def test_promise_reject_handler_attributes_rejections_to_context
    iso = RustyRacer::Isolate.new(host_namespace: 'NS')
    ctx = iso.context
    frame = iso.create_context
    ctx.eval(<<~JS)
      globalThis.SEEN = [];
      NS.setPromiseRejectHandler((event, contextId, promise, reason) => {
        SEEN.push([event, contextId, String(reason)]);
      });
    JS
    ctx.eval('Promise.reject(new Error("main boom"))')
    frame.eval('Promise.reject(new Error("frame boom"))')
    seen = ctx.eval('globalThis.SEEN')
    assert_includes seen, [0, 0, 'Error: main boom']
    assert_includes seen, [0, frame.id, 'Error: frame boom']
  end

  def test_promise_reject_handler_reports_late_handler_addition
    # HTML's bookkeeping needs the revocation too: event 1 = a handler was
    # added after the reject (the promise identity links the pair)
    iso = RustyRacer::Isolate.new(host_namespace: 'NS')
    ctx = iso.context
    ctx.eval(<<~JS)
      globalThis.SEEN = [];
      NS.setPromiseRejectHandler((event, contextId, promise) => {
        globalThis.P ??= promise;
        SEEN.push([event, promise === globalThis.P]);
      });
      globalThis.p = Promise.reject(1);
    JS
    ctx.eval('globalThis.p.catch(() => {})')
    assert_equal [[0, true], [1, true]], ctx.eval('globalThis.SEEN')
  end

  def test_promise_reject_handler_cleared_when_its_context_dies
    iso = RustyRacer::Isolate.new(host_namespace: 'NS')
    ctx = iso.context
    ctx.eval('NS.setPromiseRejectHandler(() => { globalThis.BOOM = 1 })')
    ctx.reset
    # the recorder's context is gone; a rejection must simply not notify
    # (and must not crash)
    assert_equal 2, ctx.eval('Promise.reject(1); 1 + 1')
  end

  def test_promise_reject_handler_does_not_swallow_termination
    # the handler fires synchronously mid-script; a watchdog/terminate aimed at
    # the surrounding script must survive the handler's TryCatch (which only
    # exists to swallow the handler's own throws), not be absorbed by it
    iso = RustyRacer::Isolate.new(host_namespace: 'NS')
    ctx = iso.context
    ctx.eval('NS.setPromiseRejectHandler(() => { for(;;){} })')
    assert_raises(RustyRacer::ScriptTerminatedError) do
      ctx.eval('Promise.reject(1); for(;;){}', timeout_ms: 200)
    end
    assert_equal 2, ctx.eval('1 + 1') # isolate still usable
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

  def test_import_meta_url_is_the_module_filename
    m = @ctx.compile_module('globalThis.U = import.meta.url;', filename: '/app.js')
    m.instantiate {|_s, _r| nil }
    m.evaluate
    assert_equal '/app.js', @ctx.eval('globalThis.U')
  end

  def test_import_meta_url_on_dynamically_imported_module
    @iso.dynamic_import_resolver = ->(spec, _ref) {
      @ctx.compile_module('globalThis.DU = import.meta.url;', filename: spec)
    }
    t = deadline_thread {
      @ctx.eval('import("/lazy.js");')
    }
    flunk 'deadlocked' unless t.join(10)
    t.value
    @iso.perform_microtask_checkpoint
    assert_equal '/lazy.js', @ctx.eval('globalThis.DU')
  end

  def test_import_meta_url_distinct_per_module
    a = @ctx.compile_module('export const u = import.meta.url;', filename: '/a.js')
    b = @ctx.compile_module('export const u = import.meta.url;', filename: '/b.js')
    [a, b].each {|m| m.instantiate {|_s, _r| nil }; m.evaluate }
    assert_equal '/a.js', a.namespace['u']
    assert_equal '/b.js', b.namespace['u']
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
    # explicit mode keeps the import() continuation pending until drained
    iso = RustyRacer::Isolate.new(microtasks: :explicit)
    ctx = iso.context
    dep = ctx.compile_module("export const v = 7;", filename: "/dep.js")
    dep.instantiate { |_s, _r| nil }
    dep.evaluate
    iso.dynamic_import_resolver = ->(specifier, _referrer) { specifier == "/dep.js" ? dep : nil }

    ctx.eval(<<~JS, filename: "/main.js")
      globalThis.OUT = null;
      import("/dep.js").then(m => { globalThis.OUT = m.v });
    JS
    assert_nil ctx.eval("globalThis.OUT") # pending until drained (explicit policy)
    iso.perform_microtask_checkpoint
    assert_equal 7, ctx.eval("globalThis.OUT")
  end

  def test_dynamic_import_auto_links_and_evaluates_a_compiled_module
    # V8's host contract: the resolver may return a merely COMPILED module —
    # linking and evaluating are the binding's job
    @iso.dynamic_import_resolver = ->(spec, _ref) { @ctx.compile_module('export const v = 7;', filename: spec) }
    t = deadline_thread {
      @ctx.eval('globalThis.OUT = null; import("/m.js").then(m => { globalThis.OUT = m.v }, e => { globalThis.OUT = String(e) });')
    }
    flunk 'dynamic-import auto-link deadlocked' unless t.join(10)
    t.value
    assert_equal 7, @ctx.eval('globalThis.OUT')
  end

  def test_dynamic_import_auto_link_resolves_static_imports_via_the_resolver
    sources = {
      '/app.js' => 'import {x} from "/dep.js"; export const v = x + 1;',
      '/dep.js' => 'export const x = 41;'
    }
    @iso.dynamic_import_resolver = ->(spec, _ref) {
      sources[spec] && @ctx.compile_module(sources[spec], filename: spec)
    }
    t = deadline_thread {
      @ctx.eval('globalThis.OUT = null; import("/app.js").then(m => { globalThis.OUT = m.v }, e => { globalThis.OUT = String(e) });')
    }
    flunk 'static-dep auto-link deadlocked' unless t.join(10)
    t.value
    assert_equal 42, @ctx.eval('globalThis.OUT')
  end

  def test_dynamic_import_evaluation_error_rejects_the_promise
    @iso.dynamic_import_resolver = ->(spec, _ref) {
      @ctx.compile_module('throw new Error("module boom");', filename: spec)
    }
    t = deadline_thread {
      @ctx.eval('globalThis.OUT = null; import("/x.js").then(() => { globalThis.OUT = "ok" }, e => { globalThis.OUT = String(e) });')
    }
    flunk 'deadlocked' unless t.join(10)
    t.value
    assert_includes @ctx.eval('globalThis.OUT'), 'module boom'
  end

  def test_dynamic_import_top_level_await_completes
    # the import() promise is settled FROM the evaluation promise, so a
    # top-level await module hands out its namespace only once it finished
    @iso.dynamic_import_resolver = ->(spec, _ref) {
      @ctx.compile_module('await Promise.resolve(); export const v = 5;', filename: spec)
    }
    t = deadline_thread {
      @ctx.eval('globalThis.OUT = null; import("/tla.js").then(m => { globalThis.OUT = m.v }, e => { globalThis.OUT = String(e) });')
    }
    flunk 'TLA import deadlocked' unless t.join(10)
    t.value
    assert_equal 5, @ctx.eval('globalThis.OUT')
  end

  def test_dynamic_import_settle_is_immune_to_patched_promise_then
    # the binding settles import() via the native Promise::then builtin, so a
    # user-patched Promise.prototype.then cannot break the link/evaluate of an
    # imported module (the module's own side effects still happen)
    @iso.dynamic_import_resolver = ->(spec, _ref) {
      @ctx.compile_module('globalThis.SIDE = "ran"; export const v = 7;', filename: spec)
    }
    @ctx.eval('Promise.prototype.then = function(){ throw new Error("patched") }')
    @ctx.eval('globalThis.SIDE = null; import("/m.js");')
    @iso.perform_microtask_checkpoint
    assert_equal 'ran', @ctx.eval('globalThis.SIDE')
  end

  def test_dynamic_import_evaluation_timeout_terminates_not_swallowed
    # a watchdog/terminate during the imported module's evaluation must escalate
    # to the outer eval (the import callback must not absorb it), not vanish
    iso = RustyRacer::Isolate.new(timeout_ms: 200)
    ctx = iso.context
    iso.dynamic_import_resolver = ->(spec, _ref) { ctx.compile_module('for(;;){}', filename: spec) }
    assert_raises(RustyRacer::ScriptTerminatedError) { ctx.eval('import("/spin.js"); 1') }
    assert_equal 2, ctx.eval('1 + 1')
  end

  def test_dynamic_import_resolves_in_the_realm_it_actually_fired_in
    # under kAuto a microtask queued by a frame realm can run import() during
    # the drain at the end of the MAIN realm's eval — the import must resolve
    # against the realm it actually executes in (the frame), not CURRENT_CTX
    # (which still names the main realm)
    iso = RustyRacer::Isolate.new(host_namespace: 'NS')
    ctx = iso.context
    frame = iso.create_context
    iso.dynamic_import_resolver = ->(spec, _ref) {
      frame.compile_module('export const v = 99;', filename: spec)
    }
    # queue the frame's import behind a resolver the main realm will trigger
    frame.eval(<<~JS)
      globalThis.GOT = null;
      globalThis.fire = null;
      new Promise(r => { globalThis.fire = r })
        .then(() => import("/m.js"))
        .then(m => { globalThis.GOT = m.v });
    JS
    # fire it from the MAIN realm; the import() then runs in the frame during
    # the main eval's end-of-script drain, with CURRENT_CTX == 0
    ctx.eval("NS.contextGlobal(#{frame.id}).fire()")
    5.times { iso.perform_microtask_checkpoint }
    assert_equal 99, frame.eval('globalThis.GOT')
  end

  def test_dynamic_import_unresolved_static_dep_rejects
    @iso.dynamic_import_resolver = ->(spec, _ref) {
      spec == '/app.js' ? @ctx.compile_module('import {x} from "/missing.js";', filename: spec) : nil
    }
    t = deadline_thread {
      @ctx.eval('globalThis.OUT = null; import("/app.js").then(() => { globalThis.OUT = "ok" }, e => { globalThis.OUT = String(e) });')
    }
    flunk 'deadlocked' unless t.join(10)
    t.value
    refute_equal 'ok', @ctx.eval('globalThis.OUT')
    assert_kind_of String, @ctx.eval('globalThis.OUT')
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

  def test_module_status_follows_the_lifecycle
    m = @ctx.compile_module('export const x = 1;')
    assert_equal :uninstantiated, m.status
    m.instantiate {|_s, _r| nil }
    assert_equal :instantiated, m.status
    m.evaluate
    assert_equal :evaluated, m.status
  end

  def test_module_status_errored
    m = @ctx.compile_module('throw new Error("boom");')
    m.instantiate {|_s, _r| nil }
    assert_raises(RustyRacer::RuntimeError) { m.evaluate }
    assert_equal :errored, m.status
  end

  def test_top_level_await_rejection_surfaces_under_auto_drain
    # a TLA module that rejects only after the drain runs its continuation must
    # raise, not silently return nil (the evaluate() promise is pending at the
    # status check and only settles during auto_drain)
    m = @ctx.compile_module('await Promise.resolve(); throw new Error("late TLA failure");')
    m.instantiate {|_s, _r| nil }
    e = assert_raises(RustyRacer::RuntimeError) { m.evaluate }
    assert_includes e.message, 'late TLA failure'
    assert_equal :errored, m.status
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

  def test_text_string_marshals_to_js_string
    @ctx.eval('function kind(x) { return typeof x }')
    @ctx.eval('function id(x) { return x }')
    # text-tagged Strings are JS strings, and round-trip back as UTF-8 Strings
    assert_equal 'string', @ctx.call('kind', 'café')
    out = @ctx.call('id', 'café')
    assert_equal Encoding::UTF_8, out.encoding
    assert_equal 'café', out
  end

  def test_binary_string_marshals_to_uint8array_and_back
    # the encoding tag is the type: a binary (ASCII-8BIT) String crosses as a
    # JS Uint8Array, and a Uint8Array comes back as a binary String — symmetric
    @ctx.eval('function kind(x) { return x instanceof Uint8Array }')
    @ctx.eval('function len(x) { return x.length }')
    @ctx.eval('function id(x) { return x }')
    bytes = 'café'.b # 5 bytes, high bytes
    assert_equal true, @ctx.call('kind', bytes)
    assert_equal 5, @ctx.call('len', bytes) # JS sees the bytes
    out = @ctx.call('id', bytes)
    assert_equal Encoding::ASCII_8BIT, out.encoding
    assert_equal bytes, out # full round-trip, byte-for-byte
    # arbitrary bytes (not valid UTF-8) survive intact — no U+FFFD, no error
    raw = (0..255).to_a.pack('C*')
    assert_equal raw, @ctx.call('id', raw)
  end

  def test_js_array_buffer_and_views_marshal_to_binary_string
    # a bare ArrayBuffer and any typed-array/DataView view become binary Strings
    assert_equal "\x01\x02\x03\x04".b, @ctx.eval('new Uint8Array([1,2,3,4])')
    assert_equal Encoding::ASCII_8BIT, @ctx.eval('new Uint8Array([1]).buffer').encoding
    # a view copies only its window, not the whole buffer
    assert_equal "\x02\x03".b, @ctx.eval('new Uint8Array([0,1,2,3,4,5]).subarray(2,4)')
    assert_equal "\x00\x00\x80\x3f".b, @ctx.eval('new Uint8Array(new Float32Array([1.0]).buffer)')
    # a bare SharedArrayBuffer too (not just views over it) — must not silently
    # marshal as an empty Hash
    sab = @ctx.eval('const s = new SharedArrayBuffer(4); new Uint8Array(s).set([1,2,3,4]); s')
    assert_equal "\x01\x02\x03\x04".b, sab
    assert_equal Encoding::ASCII_8BIT, sab.encoding
  end

  def test_binary_symbol_value_raises_curated_encoding_error
    # a binary-encoded Symbol value can't become a JS string; the error is the
    # binding's curated EncodingError, not a raw magnus "expected utf-8" message
    @ctx.eval('function id(x) { return x }')
    sym = "\xFF\xFE".b.force_encoding('ASCII-8BIT').to_sym
    e = assert_raises(EncodingError) { @ctx.call('id', sym) }
    assert_includes e.message, 'not valid UTF-8'
  end

  def test_reset_during_microtask_checkpoint_is_refused
    # a microtask (any realm) may be live on the V8 stack during a checkpoint;
    # resetting/disposing a realm then would corrupt it, so it's refused
    @ctx.attach('killer', proc {
      begin
        @ctx.reset
        'reset-succeeded'
      rescue RustyRacer::RuntimeError => e
        "refused:#{e.message.include?('checkpoint')}"
      end
    })
    @ctx.eval('globalThis.OUT = null; Promise.resolve().then(() => { globalThis.OUT = killer() });')
    @iso.perform_microtask_checkpoint
    assert_equal 'refused:true', @ctx.eval('globalThis.OUT')
    assert_equal 2, @ctx.eval('1 + 1') # isolate still usable
  end

  def test_evaluate_already_errored_module_reports_its_error_under_tight_timeout
    # an already-errored module's re-evaluate runs no JS; even with a tiny
    # isolate timeout it must report the module's real error, not a spurious
    # ScriptTerminatedError (ran_js is false for the no-JS status arms)
    iso = RustyRacer::Isolate.new(timeout_ms: 1)
    ctx = iso.context
    m = ctx.compile_module('throw new Error("real module error");')
    m.instantiate {|_s, _r| nil }
    assert_raises(RustyRacer::RuntimeError) { m.evaluate } # first evaluate errors it
    e = assert_raises(RustyRacer::RuntimeError) { m.evaluate } # re-evaluate: no JS
    refute_kind_of RustyRacer::ScriptTerminatedError, e
    assert_includes e.message, 'real module error'
  end

  def test_shared_binary_keeps_one_identity
    # an aliased binary blob must stay ONE object across the boundary, not be
    # duplicated (like shared Arrays/Hashes) — both directions
    @ctx.eval('function sameRef(a) { return a[0] === a[1] }')
    bin = 'payload'.b
    assert_equal true, @ctx.call('sameRef', [bin, bin]) # Ruby -> JS: one Uint8Array
    # JS -> Ruby: one binary String for an aliased Uint8Array
    pair = @ctx.eval('const u = new Uint8Array([1,2,3]); [u, u]')
    assert_same pair[0], pair[1]
    pair[0] << 9 # mutating one is visible through the other (same object)
    assert_equal pair[0], pair[1]
  end

  def test_mistagged_text_as_binary_surfaces_as_uint8array
    # the loud-failure property: a text string mis-tagged binary becomes a
    # Uint8Array (so the mis-tag is detectable, not silently coerced)
    @ctx.eval('function kind(x) { return x instanceof Uint8Array }')
    assert_equal true, @ctx.call('kind', 'plain text'.b)
  end

  def test_binary_tagged_hash_key_marshals
    @ctx.eval('function keys(h) { return Object.keys(h) }')
    assert_equal ['café'], @ctx.call('keys', {'café'.b => 1})
  end

  def test_bare_symbol_marshals_to_js_string
    # symbols already crossed as hash KEYS; bare values (args, array elements,
    # hash values) get the same one-way Symbol -> String treatment
    @ctx.eval('function id(x) { return x }')
    assert_equal 'click', @ctx.call('id', :click)
    assert_equal %w[a b], @ctx.call('id', [:a, :b])
    assert_equal({'k' => 'v'}, @ctx.call('id', {k: :v}))
    @ctx.eval('function kind(x) { return typeof x }')
    assert_equal 'string', @ctx.call('kind', :sym)
  end

  def test_to_str_string_like_marshals_by_tag
    # an object delegating via to_str gets the same tag-driven treatment as
    # the String it wraps
    stringlike = Class.new {
      def initialize(s) = @s = s
      def to_str = @s
    }
    @ctx.eval('function id(x) { return x }')
    @ctx.eval('function kind(x) { return x instanceof Uint8Array }')
    # text-tagged -> JS string
    assert_equal 'café', @ctx.call('id', stringlike.new('café'))
    # binary-tagged -> Uint8Array -> binary String
    assert_equal true, @ctx.call('kind', stringlike.new('café'.b))
    assert_equal 'xy'.b, @ctx.call('id', stringlike.new('xy'.b))
  end

  def test_non_utf8_text_transcodes_or_raises
    @ctx.eval('function id(x) { return x }')
    # a non-UTF-8 TEXT encoding is transcoded to UTF-8 on the way to JS
    assert_equal 'あ', @ctx.call('id', 'あ'.encode('Shift_JIS'))
    # bytes unmappable in their declared text encoding raise loudly
    assert_raises(EncodingError) do
      @ctx.call('id', "\x82".dup.force_encoding('Shift_JIS')) # lone SJIS lead byte
    end
    # invalid bytes in a UTF-8-TAGGED String also raise, not silently U+FFFD
    assert_raises(EncodingError) do
      @ctx.call('id', "\xC3\x28".dup.force_encoding('UTF-8')) # invalid UTF-8
    end
  end

  def test_hash_key_with_broken_to_s_raises_not_empty_string
    # a to_s returning a non-String must stay a loud error: silently mapping
    # the key to "" would collide/clobber other keys
    weird = Class.new { def to_s = 42 }
    @ctx.eval('function id(x) { return x }')
    assert_raises(TypeError) { @ctx.call('id', {weird.new => 1}) }
  end

  def test_reset_releases_attached_proc_roots
    ref = attach_capturing_proc(@ctx)
    @ctx.reset
    GC.start
    GC.start
    refute ref.weakref_alive?, 'reset left the attached proc (and its captures) GC-rooted'
  end

  def test_dispose_releases_attached_proc_roots
    iso = RustyRacer::Isolate.new
    ctx = iso.context
    ref = attach_capturing_proc(ctx)
    iso.dispose
    GC.start
    GC.start
    refute ref.weakref_alive?, 'dispose left the attached proc (and its captures) GC-rooted'
  end

  def test_attached_proc_survives_gc
    # the attached lambdas below have NO other Ruby reference: the extension
    # itself must root them (rb_gc_register_address via RootedProc) or GC
    # collects them mid-suite and call_proc SEGVs
    @ctx.attach('churn', ->(i) { GC.start if (i % 25).zero?; 'x' * 100 + i.to_s })
    @ctx.attach('nestedOp', ->(s) {
      scr = @ctx.compile("1+#{s.to_i}", filename: 'n.js')
      v = scr.run
      scr.dispose
      v
    })
    total = @ctx.eval(<<~JS, timeout_ms: 30_000)
      let a = 0;
      for (let i = 0; i < 500; i++) {
        a += churn(i).length;
        if (i % 100 === 0) a += nestedOp(String(i));
      }
      a;
    JS
    assert_kind_of Integer, total
  end

  def test_attached_proc_survives_gc_compact
    skip 'GC.compact unavailable' unless GC.respond_to?(:compact)
    # rooting pins the proc, so compaction cannot move it out from under the
    # raw VALUE copies the extension holds
    @ctx.attach('f', -> { 'alive' })
    assert_equal 'alive', @ctx.eval('f()')
    GC.compact
    assert_equal 'alive', @ctx.eval('f()')
  end

  def test_dynamic_import_resolver_survives_gc_compact
    skip 'GC.compact unavailable' unless GC.respond_to?(:compact)
    dep = @ctx.compile_module('export const v = 7;', filename: '/dep.js')
    dep.instantiate {|_s, _r| nil }
    dep.evaluate
    @iso.dynamic_import_resolver = ->(specifier, _referrer) { specifier == '/dep.js' ? dep : nil }
    GC.compact
    @ctx.eval('globalThis.OUT = null; import("/dep.js").then(m => { globalThis.OUT = m.v });')
    @iso.perform_microtask_checkpoint
    assert_equal 7, @ctx.eval('globalThis.OUT')
  end

  # --- re-entrancy matrix: while a host proc runs, the V8 thread is parked in
  # that callback awaiting the answer and is NOT reading the main queue, so
  # EVERY op issued from inside the proc must be serviced by the suspended
  # frame (service_request) — anything less deadlocks the rendezvous. Each op
  # runs on a worker thread with a deadline so a regression fails fast instead
  # of hanging the suite (the wedged isolate is abandoned).

  def test_nested_compile_and_run_inside_host_fn
    @ctx.attach('f', proc { @ctx.compile('6 * 7', filename: '/n.js').run })
    assert_equal 42, call_with_deadline(@ctx, 'f')
  end

  def test_nested_module_pipeline_inside_host_fn
    @ctx.attach('f', proc {
      m = @ctx.compile_module('export const x = 41; globalThis.MX = x + 1;', filename: '/m.js')
      m.instantiate {|_s, _r| nil }
      m.evaluate
      @ctx.eval('globalThis.MX')
    })
    assert_equal 42, call_with_deadline(@ctx, 'f')
  end

  def test_nested_microtask_checkpoint_inside_host_fn
    @ctx.attach('f', proc { @iso.perform_microtask_checkpoint; 1 })
    assert_equal 1, call_with_deadline(@ctx, 'f')
  end

  def test_nested_microtask_checkpoint_drains_queue_mid_script
    # csim's __csim_yield: a host fn checkpoints BETWEEN listeners, with JS
    # still on the stack — the queued microtasks must actually run, not no-op
    @ctx.attach('yieldNow', proc { @iso.perform_microtask_checkpoint; @ctx.eval('globalThis.SEEN') })
    @ctx.eval('globalThis.SEEN = null; function g() { Promise.resolve().then(() => { globalThis.SEEN = "micro" }); return yieldNow(); }')
    assert_equal 'micro', call_with_deadline(@ctx, 'g')
  end

  def test_nested_create_context_inside_host_fn
    @ctx.attach('f', proc { @iso.create_context.eval('20 + 22') })
    assert_equal 42, call_with_deadline(@ctx, 'f')
  end

  def test_nested_attach_inside_host_fn
    @ctx.attach('f', proc {
      @ctx.attach('g', proc { 42 })
      @ctx.eval('g()')
    })
    assert_equal 42, call_with_deadline(@ctx, 'f')
  end

  def test_nested_cross_realm_eval_inside_host_fn
    # a nested op carries its own context id, so a host fn on the main realm
    # can evaluate in ANOTHER realm (previously it silently ran in the
    # suspended frame's realm)
    realm = @iso.create_context
    realm.eval('globalThis.WHO = "realm"')
    @ctx.attach('f', proc { realm.eval('globalThis.WHO') })
    assert_equal 'realm', call_with_deadline(@ctx, 'f')
  end

  def test_nested_eval_timeout_terminates_and_escalates
    # nested ops get the watchdog too: a runaway nested eval times out instead
    # of wedging. V8's terminate flag is isolate-global and a nested frame
    # never cancels it (that could erase an Isolate#terminate aimed at the
    # suspended outer JS), so the termination ESCALATES: the proc sees the
    # nested ScriptTerminatedError, and the outer call is terminated as it
    # resumes. The isolate stays usable afterwards.
    caught = false
    @ctx.attach('f', proc {
      begin
        @ctx.eval('for(;;){}', timeout_ms: 100)
        'no-timeout'
      rescue RustyRacer::ScriptTerminatedError
        caught = true
        'terminated'
      end
    })
    t = deadline_thread { @ctx.call('f') }
    assert_raises(RustyRacer::ScriptTerminatedError) {
      flunk 'nested timeout deadlocked' unless t.join(10)
      t.value
    }
    assert caught, 'the proc did not observe the nested termination'
    assert_equal 2, @ctx.eval('1 + 1')
  end

  def test_outer_timeout_survives_a_nested_op
    # The watchdog tracks every armed op's deadline independently (a LIFO stack),
    # not one shared slot. A nested op (host fn -> nested eval) arms and disarms
    # the watchdog while the OUTER op is suspended; that must NOT clear the
    # outer op's own deadline. With a single slot the nested disarm would leave
    # the outer `for(;;){}` below unwatched and it would run unbounded.
    @ctx.attach('quick', proc { @ctx.eval('1 + 1', timeout_ms: 100) })
    t = deadline_thread { @ctx.eval('quick(); for(;;){}', timeout_ms: 200) }
    assert_raises(RustyRacer::ScriptTerminatedError) {
      flunk 'outer timeout was lost after the nested op returned' unless t.join(10)
      t.value
    }
    assert_equal 2, @ctx.eval('1 + 1')
  end

  def test_terminate_during_host_proc_is_not_erased_by_nested_watchdog
    # an Isolate#terminate aimed at the suspended outer JS must survive a
    # nested watchdog firing in the same window (a nested cancel of the
    # isolate-global flag would let the outer loop run unbounded)
    @ctx.attach('slow', proc { sleep 0.3; 1 })
    @ctx.attach('f', proc {
      begin
        # nested watchdog fires at ~100ms while the nested JS is parked in
        # `slow`; the outer terminate lands at ~200ms, before `slow` returns
        @ctx.eval('slow(); for(;;){}', timeout_ms: 100)
      rescue RustyRacer::ScriptTerminatedError
        # expected
      end
      1
    })
    stopper = Thread.new { sleep 0.2; @iso.terminate }
    t = deadline_thread { @ctx.eval('f(); for(;;){}') }
    assert_raises(RustyRacer::ScriptTerminatedError) {
      flunk 'outer loop ran unbounded (the terminate was erased)' unless t.join(10)
      t.value
    }
    stopper.join
    assert_equal 2, @ctx.eval('1 + 1')
  end

  def test_nested_op_targets_its_own_isolate
    # a proc of isolate A calling into isolate B must reach B's V8 thread (B
    # is idle), not be misrouted into A's suspended frame and run in A's realm
    iso_b = RustyRacer::Isolate.new
    ctx_b = iso_b.context
    @ctx.eval('globalThis.who = "A"')
    ctx_b.eval('globalThis.who = "B"')
    @ctx.attach('f', proc { ctx_b.eval('globalThis.who') })
    assert_equal 'B', call_with_deadline(@ctx, 'f')
  end

  def test_nested_instantiate_during_instantiate_raises_not_crashes
    # V8 module instantiation is not re-entrant: a resolve block instantiating
    # another module mid-instantiate walks the half-built graph and SEGVs, so
    # it must be refused with a clean error (the block may still COMPILE
    # lazily — the outer instantiate links the dep)
    app = @ctx.compile_module('import {x} from "/dep.js"; globalThis.X = x;', filename: '/app.js')
    t = deadline_thread {
      app.instantiate {|spec, _ref|
        dep = @ctx.compile_module('export const x = 1;', filename: spec)
        dep.instantiate {|_s, _r| nil } # re-entrant: must raise, not SEGV
        dep
      }
    }
    e = assert_raises(RustyRacer::RuntimeError) {
      flunk 'nested instantiate deadlocked' unless t.join(10)
      t.value
    }
    assert_includes e.message, 'not re-entrant'
    assert_equal 2, @ctx.eval('1 + 1')
  end

  def test_nested_reset_of_suspended_realm_is_refused
    # resetting the realm whose request is suspended on the V8 stack would
    # drop its in-flight modules and swap the v8::Context behind the same id
    # (defeating the cross-context import guards), so it is refused
    @ctx.attach('f', proc {
      @ctx.reset
      'reset-succeeded'
    })
    t = deadline_thread { @ctx.call('f') }
    e = assert_raises(RustyRacer::RuntimeError) {
      flunk 'nested reset deadlocked' unless t.join(10)
      t.value
    }
    assert_includes e.message, 'suspended'
    assert_equal 2, @ctx.eval('1 + 1')
    # resetting OUTSIDE a suspended frame still works
    @ctx.reset
    assert_equal 2, @ctx.eval('1 + 1')
  end

  def test_nested_reset_of_idle_realm_is_allowed
    # only the realms on the V8 stack are protected; a host fn may freely
    # reset a DIFFERENT, idle realm
    realm = @iso.create_context
    realm.eval('globalThis.x = 1')
    @ctx.attach('f', proc {
      realm.reset
      realm.eval('typeof globalThis.x')
    })
    assert_equal 'undefined', call_with_deadline(@ctx, 'f')
  end

  def test_lazy_compile_inside_instantiate_resolver
    # the instantiate resolve block may compile dependencies lazily — the
    # suspended InstantiateModule frame services the nested compile
    app = @ctx.compile_module('import {x} from "./dep.js"; globalThis.LAZY = x;', filename: '/app.js')
    t = deadline_thread {
      app.instantiate {|specifier, _referrer|
        @ctx.compile_module('export const x = 42;', filename: specifier)
      }
      app.evaluate
    }
    flunk 'lazy compile in instantiate resolver deadlocked' unless t.join(10)
    t.value
    assert_equal 42, @ctx.eval('globalThis.LAZY')
  end

  def test_lazy_load_inside_dynamic_import_resolver
    # the dynamic-import resolver may compile + instantiate + evaluate the
    # module on demand (the suspended import() frame services all three)
    @iso.dynamic_import_resolver = lambda {|specifier, _referrer|
      m = @ctx.compile_module('export const v = 7;', filename: specifier)
      m.instantiate {|_s, _r| nil }
      m.evaluate
      m
    }
    t = deadline_thread {
      @ctx.eval('globalThis.OUT = null; import("/lazy.js").then(m => { globalThis.OUT = m.v });')
      @iso.perform_microtask_checkpoint
    }
    flunk 'lazy dynamic import deadlocked' unless t.join(10)
    t.value
    assert_equal 7, @ctx.eval('globalThis.OUT')
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

  def test_idle_terminate_does_not_poison_the_next_request
    # Isolate#terminate fired while the V8 thread is idle (no JS running) sets
    # the global terminate flag but no watchdog; the next eval must clear it and
    # run normally, not abort spuriously
    @ctx.eval("1") # warm up
    @iso.terminate # nothing is running
    results = (101..105).map {|n| @ctx.eval(n.to_s) rescue :terminated }
    assert_equal [101, 102, 103, 104, 105], results
  end

  def test_realm_disposed_error_is_not_turned_into_terminated
    # a watchdog is armed even for a disposed realm (which runs no JS); a raced
    # firing must not mask the real "disposed" error as ScriptTerminatedError
    iso = RustyRacer::Isolate.new(timeout_ms: 1)
    r = iso.create_context
    r.dispose
    50.times do
      e = assert_raises(::RuntimeError) { r.eval("1 + 1") }
      refute_kind_of RustyRacer::ScriptTerminatedError, e
    end
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

  private

  # Invoke ctx.call(name) on a worker thread with a deadline, so a re-entrancy
  # deadlock fails the test instead of hanging the suite.
  def call_with_deadline(ctx, name)
    t = deadline_thread { ctx.call(name) }
    flunk "#{name} deadlocked (nested op not serviced re-entrantly)" unless t.join(10)
    t.value
  end

  # A worker thread whose (sometimes intentional) exception is re-raised at
  # join/value rather than spammed to stderr.
  def deadline_thread(&block)
    t = Thread.new(&block)
    t.report_on_exception = false
    t
  end

  # Attach a proc capturing a fresh object and return a WeakRef to the
  # capture. Runs on a throwaway thread so no stack residue on the test
  # thread keeps the object conservatively alive — after the attach, the
  # extension's GC root must be the ONLY thing keeping it.
  def attach_capturing_proc(ctx)
    require 'weakref'
    Thread.new {
      captured = Object.new
      ctx.attach('f', proc { captured.object_id })
      WeakRef.new(captured)
    }.value
  end
end
