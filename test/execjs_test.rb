# frozen_string_literal: true

require 'minitest/autorun'

# execjs is a development-only dependency (the adapter never loads it at runtime),
# so skip this suite where it isn't installed rather than erroring.
begin
  require 'rusty_racer/execjs'
  EXECJS_AVAILABLE = true
rescue LoadError
  EXECJS_AVAILABLE = false
end

# Mirrors ExecJS's own runtime contract suite (rails/execjs test/test_execjs.rb),
# trimmed to the runtime-agnostic cases (the fixture-driven babel/coffee/uglify
# integrations and the ExternalRuntime probes are out of scope). Proves the
# adapter behaves like any other ExecJS runtime, so any ExecJS consumer can run
# on rusty_racer unchanged.
class ExecJSTest < Minitest::Test
  def setup
    skip 'execjs not installed' unless EXECJS_AVAILABLE
    @runtime = RustyRacer::ExecJSRuntime.new
  end

  def test_available
    assert @runtime.available?
    assert_kind_of String, @runtime.name
  end

  # --- value marshalling (ExecJS JSON semantics) ---------------------------

  VALUE_CASES = {
    'function() {}'             => nil,
    '0'                         => 0,
    'null'                      => nil,
    'undefined'                 => nil,
    'true'                      => true,
    'false'                     => false,
    '[1, 2]'                    => [1, 2],
    '[1, function() {}]'        => [1, nil],
    "'hello'"                   => 'hello',
    "'red yellow blue'.split(' ')" => %w[red yellow blue],
    '{a:1,b:2}'                 => {'a' => 1, 'b' => 2},
    '{a:true,b:function (){}}'  => {'a' => true},
    "'café'"                    => 'café',
    '"☃"'                       => '☃',
    '"\\u2603"'                  => '☃',
    "'\u{1f604}'"               => "\u{1f604}", # smiling emoji
    "'\u{1f1fa}\u{1f1f8}'"      => "\u{1f1fa}\u{1f1f8}", # US flag
    '"\\\\"'                    => '\\'
  }.freeze

  VALUE_CASES.each_with_index do |(input, output), i|
    define_method("test_exec_value_#{i}") do
      assert_value output, @runtime.exec("return #{input}")
    end

    define_method("test_eval_value_#{i}") do
      assert_value output, @runtime.eval(input)
    end

    define_method("test_compile_eval_value_#{i}") do
      assert_value output, @runtime.compile("var a = #{input};").eval('a')
    end

    define_method("test_compile_call_value_#{i}") do
      assert_value output, @runtime.compile("function a() { return #{input}; }").call('a')
    end
  end

  # JSON round-trip of Ruby values passed in as call args and back out.
  JSON_VALUES = [
    nil, true, false, 1, 3.14, 'hello', '\\', 'café', '☃',
    "\u{1f604}", "\u{1f1fa}\u{1f1f8}",
    [1, 2, 3], [1, [2, 3]], [1, [2, [3]]], %w[red yellow blue],
    {'a' => 1, 'b' => 2}, {'a' => 1, 'b' => [2, 3]}, {'a' => true}
  ].freeze

  JSON_VALUES.each_with_index do |value, i|
    define_method("test_call_roundtrip_#{i}") do
      context = @runtime.compile('function id(obj) { return obj; }')
      assert_value value, context.call('id', value)
    end

    define_method("test_stringify_value_#{i}") do
      context = @runtime.compile('function json(obj) { return JSON.stringify(obj); }')
      assert_value JSON.generate(value, quirks_mode: true), context.call('json', value)
    end
  end

  # --- call --------------------------------------------------------------

  def test_call_simple
    context = @runtime.compile('id = function(v) { return v; }')
    assert_equal 'bar', context.call('id', 'bar')
  end

  def test_call_nested_path
    context = @runtime.compile('a = {}; a.b = {}; a.b.id = function(v) { return v; }')
    assert_equal 'bar', context.call('a.b.id', 'bar')
  end

  def test_call_function_literal
    context = @runtime.compile('')
    assert_equal 2, context.call('function(a, b) { return a + b }', 1, 1)

    context = @runtime.compile('foo = 1')
    assert_equal 2, context.call('(function(bar) { return foo + bar })', 1)
  end

  def test_call_this_is_global
    context = @runtime.compile(<<~JS)
      name = 123;
      function Person(name) { this.name = name; }
      Person.prototype.getThis = function() { return this.name; }
    JS
    assert_equal 123, context.call('(new Person("Bob")).getThis')
  end

  def test_call_missing_function
    context = @runtime.compile('')
    assert_raises ExecJS::ProgramError do
      context.call('missing')
    end
  end

  def test_symbol_args
    context = @runtime.compile('function echo(test) { return test; }')
    assert_equal 'symbol', context.call('echo', :symbol)
    assert_equal ['symbol'], context.call('echo', [:symbol])
    assert_equal({'key' => 'value'}, context.call('echo', {key: :value}))
  end

  # --- exec / eval edges -------------------------------------------------

  def test_eval_blank
    assert_nil @runtime.eval('')
    assert_nil @runtime.eval('  ')
  end

  def test_exec_return
    assert_nil @runtime.exec('return')
    assert_nil @runtime.exec('1')
    assert_equal 1, @runtime.exec('return 1')
  end

  def test_trailing_line_comment
    # A source that closes with a `//` comment must not swallow the wrapper the
    # adapter builds around it (stricter than the bare ExecJS string-wrapping).
    assert_equal 2, @runtime.eval('1 + 1 // last line comment')
    assert_equal 3, @runtime.exec('return 1 + 2 // done')
    context = @runtime.compile('add = function(a, b) { return a + b }')
    assert_equal 5, context.call('add // the adder', 2, 3)
  end

  def test_compile_named_and_anonymous
    context = @runtime.compile('foo = function() { return "bar"; }')
    assert_equal 'bar', context.exec('return foo()')
    assert_equal 'bar', context.eval('foo()')
    assert_equal 'bar', context.call('foo')

    context = @runtime.compile('function foo() { return "bar"; }')
    assert_equal 'bar', context.call('foo')
  end

  # --- environment is bare (no Node/browser globals) ---------------------

  def test_this_is_global_scope
    assert_equal true, @runtime.eval('this === (function() { return this })()')
  end

  def test_no_ambient_globals
    %w[
      self global process module exports require console
      setTimeout setInterval clearTimeout clearInterval setImmediate clearImmediate
    ].each do |name|
      assert @runtime.eval("typeof #{name} == 'undefined'"), "expected #{name} to be undefined"
      refute @runtime.eval("'#{name}' in this"), "expected #{name} not on global"
    end
  end

  def test_additional_options_are_accepted
    # ExecJS passes arbitrary option keys through exec/eval/compile; the adapter
    # must accept and ignore them.
    assert_equal true, @runtime.eval('true', foo: true)
    assert_equal true, @runtime.exec('return true', foo: true)
    context = @runtime.compile('foo = true', foo: true)
    assert_equal true, context.eval('foo', foo: true)
    assert_equal true, context.exec('return foo', foo: true)
  end

  # --- encoding ----------------------------------------------------------

  def test_result_encoding
    utf8 = Encoding.find('UTF-8')
    assert_equal utf8, @runtime.exec("return 'hello'").encoding
    assert_equal utf8, @runtime.eval("'☃'").encoding

    result = @runtime.eval("'hello'".encode('US-ASCII'))
    assert_equal 'hello', result
    assert_equal utf8, result.encoding
  end

  def test_binary_source_rejected
    assert_raises Encoding::UndefinedConversionError do
      @runtime.eval("\xde\xad\xbe\xef".dup.force_encoding('BINARY'))
    end
  end

  def test_surrogate_pairs
    str = "\u{1f604}"
    assert_equal 2, @runtime.eval("'#{str}'.length")
    assert_equal str, @runtime.eval("'#{str}'")
  end

  def test_large_return_value
    string = @runtime.eval('(new Array(100001)).join("abcdef")')
    assert_equal 600_000, string.size
  end

  # --- error mapping -----------------------------------------------------

  def test_syntax_error_is_runtime_error
    %w[exec eval].each do |m|
      err = assert_raises(ExecJS::RuntimeError) { @runtime.public_send(m, ')') }
      assert_includes err.backtrace.join("\n"), '(execjs):'
    end
    err = assert_raises(ExecJS::RuntimeError) { @runtime.compile(')') }
    assert_includes err.backtrace.first, '(execjs):'
  end

  def test_thrown_error_is_program_error
    err = assert_raises(ExecJS::ProgramError) { @runtime.exec("throw new Error('hello')") }
    assert_includes err.backtrace.join("\n"), '(execjs):'

    assert_raises(ExecJS::ProgramError) { @runtime.eval("(function(){ throw new Error('x') })()") }
    assert_raises(ExecJS::ProgramError) { @runtime.compile("throw new Error('hello')") }
    assert_raises(ExecJS::ProgramError) { @runtime.exec("throw 'hello'") }
  end

  # --- usable through the ExecJS facade ----------------------------------

  def test_via_execjs_facade
    # ExecJS.runtime raises if no runtime was autodetected — fine, we set our own.
    previous = ExecJS.runtime rescue nil
    ExecJS.runtime = @runtime
    assert_equal 2, ExecJS.eval('1 + 1')
    assert_equal 3, ExecJS.exec('return 1 + 2')
    assert_equal 'HELLO', ExecJS.compile('function up(s){return s.toUpperCase()}').call('up', 'hello')
  ensure
    ExecJS.runtime = previous if previous
  end

  private

  def assert_value(expected, actual)
    if expected.nil?
      assert_nil actual
    else
      assert_equal expected, actual
    end
  end
end
