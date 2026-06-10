# Stage-2 probes: the Ruby half (Magnus + GVL + channel rendezvous).
# Run: ruby stage2/test.rb   (after cargo build --release -p rusty_racer)
require 'fileutils'

so = File.expand_path('../target/release/librusty_racer.so', __dir__)
abort "build first: cargo build --release -p rusty_racer" unless File.exist?(so)
linked = File.expand_path('../target/release/rusty_racer.so', __dir__)
FileUtils.cp(so, linked)
require linked

def probe(name)
  print "[#{name}] "
  yield
  puts 'ok'
rescue => e
  puts "FAIL: #{e.class}: #{e.message}"
  exit 1
end

ctx = RustyRacer::Context.new

probe 'eval: roundtrip' do
  raise 'int'    unless ctx.eval('1 + 1') == 2
  raise 'float'  unless ctx.eval('1.5 * 2') == 3.0
  raise 'string' unless ctx.eval('"he" + "llo"') == 'hello'
  raise 'bool'   unless ctx.eval('1 < 2') == true
  raise 'nil'    unless ctx.eval('null').nil?
end

probe 'eval: JS exception -> Ruby exception' do
  begin
    ctx.eval('throw new Error("boom")')
    raise 'did not raise'
  rescue RuntimeError => e
    raise "bad message: #{e.message}" unless e.message.include?('boom')
  end
end

probe 'GVL: other Ruby threads progress during eval' do
  counter = 0
  t = Thread.new { loop { counter += 1; Thread.pass } }
  ctx.eval('const until = Date.now() + 200; while (Date.now() < until) {}')
  t.kill
  t.join
  raise "counter did not progress (#{counter})" unless counter > 1000
end

probe 'host fn: JS -> Ruby -> JS roundtrip' do
  ctx.attach('rubyAdd', proc {|a, b| a + b })
  raise 'bad result' unless ctx.eval('rubyAdd(20, 22)') == 42
  raise 'string leg' unless ctx.eval('rubyAdd("a", "b")') == 'ab'
end

probe 'host fn: Ruby exception surfaces as JS exception, context survives' do
  ctx.attach('rubyBoom', proc { raise ArgumentError, 'no thanks' })
  caught = ctx.eval('(() => { try { rubyBoom(); return "uncaught"; } catch (e) { return "caught:" + String(e).includes("no thanks"); } })()')
  raise "bad: #{caught}" unless caught == 'caught:true'
  # audit #24's class: the context must NOT be wedged afterwards
  raise 'wedged' unless ctx.eval('1 + 1') == 2
end

probe 'nested: ruby -> js -> ruby -> js' do
  ctx.attach('reenter', proc { ctx.eval('6 * 7') })
  raise 'bad nested' unless ctx.eval('reenter()') == 42
end

probe 'timeout: terminates and recovers' do
  begin
    ctx.eval_t('for(;;){}', 100)
    raise 'did not raise'
  rescue RuntimeError => e
    raise "bad message: #{e.message}" unless e.message.include?('terminated')
  end
  raise 'poisoned' unless ctx.eval('2 + 2') == 4
end

probe 'timeout: late watchdog firing cannot poison the next request (audit #3)' do
  # Run work that takes about as long as the timeout, many times; with the
  # C++ bug a late TerminateExecution leaks across the request boundary and a
  # later innocent eval raises ScriptTerminatedError.
  150.times do
    begin
      ctx.eval_t('const until = Date.now() + 1; while (Date.now() < until) {}', 1)
    rescue RuntimeError
      # terminated this round: fine, that's the watchdog winning the race
    end
    raise 'stale termination leaked into the next request' unless ctx.eval('1') == 1
  end
end

probe 'stop: from another thread, then context still usable' do
  stopper = Thread.new { sleep 0.05; ctx.stop }
  begin
    ctx.eval('for(;;){}')
    raise 'did not raise'
  rescue RuntimeError
  end
  stopper.join
  raise 'unusable after stop' unless ctx.eval('3 + 3') == 6
end

probe 'dispose racing in-flight eval does not hang (audit #12/#13/#26)' do
  10.times do
    c = RustyRacer::Context.new
    worker = Thread.new do
      c.eval('const until = Date.now() + 30; while (Date.now() < until) {}')
    rescue RuntimeError
      # disposed/terminated mid-run is acceptable; hanging is not
    end
    sleep(rand * 0.03)
    c.dispose
    raise 'worker hung' unless worker.join(5)
    # post-dispose use raises cleanly instead of blocking forever
    begin
      c.eval('1')
      raise 'eval on disposed context did not raise'
    rescue RuntimeError
    end
  end
end

# --- microbench: rendezvous floor vs the C extension ---------------------
$LOAD_PATH.unshift File.expand_path('../../../rubyjs/mini_racer/lib', __dir__)
require 'mini_racer_csim'

N = 20_000
rust_ctx = RustyRacer::Context.new
cpp_ctx  = MiniRacerCsim::Context.new
2_000.times { rust_ctx.eval('1'); cpp_ctx.eval('1') } # warmup

t0 = Process.clock_gettime(Process::CLOCK_MONOTONIC)
N.times { rust_ctx.eval('1') }
rust_s = Process.clock_gettime(Process::CLOCK_MONOTONIC) - t0

t0 = Process.clock_gettime(Process::CLOCK_MONOTONIC)
N.times { cpp_ctx.eval('1') }
cpp_s = Process.clock_gettime(Process::CLOCK_MONOTONIC) - t0

puts format(
  "[bench] eval('1') x %d: rust %.2fus/op, c++ %.2fus/op",
  N,
  rust_s / N * 1e6,
  cpp_s / N * 1e6
)

puts 'stage2: all probes passed'
