# C++ comparison for the rusty_v8 spike: the same synthetic 83-module binary
# tree through mini_racer-csim's load_module_graph, fresh realm per visit
# (reset_realm), timing only the graph load — same measurement window as the
# Rust side (context creation excluded).
$LOAD_PATH.unshift File.expand_path('../../../rubyjs/mini_racer/lib', __dir__)
require 'mini_racer_csim'

N_MODULES = 83
VISITS    = 50

sources = {}
N_MODULES.times do |i|
  body = +''
  [2 * i + 1, 2 * i + 2].each do |child|
    body << "import \"./mod_#{child}.js\";\n" if child < N_MODULES
  end
  body << "globalThis.__loaded = (globalThis.__loaded || 0) + 1;\n"
  body << "export const id = #{i};\n"
  sources["/mod_#{i}.js"] = body
end

fetch = lambda do |urls|
  urls.map {|u| (s = sources[u]) ? [s, nil] : nil }
end
resolve = lambda do |edges|
  edges.map {|specifier, _referrer| specifier.sub('./', '/') }
end

ctx = MiniRacerCsim::Context.new
timings = []

VISITS.times do |visit|
  ctx.reset_realm unless visit.zero? # fresh realm per visit, outside the timer

  t = Process.clock_gettime(Process::CLOCK_MONOTONIC)
  result = ctx.load_module_graph('/mod_0.js', resolve: resolve, fetch_batch: fetch)
  timings << Process.clock_gettime(Process::CLOCK_MONOTONIC) - t

  loaded = ctx.eval('globalThis.__loaded')
  raise "bad load: #{loaded}" unless loaded == N_MODULES
  raise "bad modules: #{result[:modules].size}" unless result[:modules].size == N_MODULES
end

timings.sort!
mean = timings.sum / VISITS
puts format(
  'C++ bench: %d visits x %d modules - mean %.1fus, median %.1fus, min %.1fus',
  VISITS,
  N_MODULES,
  mean * 1e6,
  timings[VISITS / 2] * 1e6,
  timings[0] * 1e6
)
