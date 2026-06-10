# frozen_string_literal: true

require "minitest/autorun"
require "rusty_racer"

# The stage-2 probes as a suite cibuildgem runs natively on each platform —
# proving the from-source V8 build links and runs, not just compiles. Mapped to
# the mini_racer-csim audit's hang classes where relevant.
class RustyRacerTest < Minitest::Test
  def setup
    @ctx = RustyRacer::Context.new
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

  def test_other_ruby_threads_progress_during_eval
    counter = 0
    t = Thread.new { loop { counter += 1; Thread.pass } }
    @ctx.eval("const until = Date.now() + 200; while (Date.now() < until) {}")
    t.kill
    t.join
    assert_operator counter, :>, 1000, "GVL not released during eval"
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
    assert_raises(RustyRacer::ScriptTerminatedError) { @ctx.eval_t("for(;;){}", 100) }
    assert_equal 4, @ctx.eval("2 + 2")
  end

  def test_late_watchdog_does_not_poison_next_request
    # audit #3: a late TerminateExecution must not leak into the next request.
    100.times do
      begin
        @ctx.eval_t("const u = Date.now() + 1; while (Date.now() < u) {}", 1)
      rescue RustyRacer::ScriptTerminatedError
        # terminated this round — fine
      end
      assert_equal 1, @ctx.eval("1")
    end
  end

  def test_stop_from_another_thread_then_usable
    stopper = Thread.new { sleep 0.05; @ctx.stop }
    assert_raises(RustyRacer::ScriptTerminatedError) { @ctx.eval("for(;;){}") }
    stopper.join
    assert_equal 6, @ctx.eval("3 + 3")
  end

  def test_dispose_racing_eval_does_not_hang
    # audit #12/#13/#26: dispose racing an in-flight eval must not hang.
    10.times do
      c = RustyRacer::Context.new
      worker = Thread.new do
        c.eval("const u = Date.now() + 30; while (Date.now() < u) {}")
      rescue StandardError
        # disposed/terminated mid-run is acceptable (class varies with the
        # race); hanging is not.
      end
      sleep(rand * 0.03)
      c.dispose
      assert worker.join(5), "worker hung"
      # post-dispose use raises the plain disposed-context guard, not a JS error
      assert_raises(::RuntimeError) { c.eval("1") }
    end
  end
end
