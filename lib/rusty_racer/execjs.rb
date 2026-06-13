# frozen_string_literal: true

# An ExecJS runtime backed by rusty_racer, so any ExecJS consumer (asset
# pipelines, CoffeeScript/Babel/Uglify wrappers, …) can run on V8-in-Ruby with
# no code change: `require "rusty_racer/execjs"` then
#
#   ExecJS.runtime = RustyRacer::ExecJSRuntime.new
#
# This file is OPTIONAL — rusty_racer never loads it itself, so execjs stays a
# non-dependency. Requiring it declares you want the integration (and so have
# execjs installed).
#
# Values cross the boundary with ExecJS's JSON semantics, not rusty_racer's
# richer native marshalling: every result is taken through `JSON.stringify` on
# the V8 side and parsed back. That is exactly the contract ExecJS's external
# runtimes (Node, …) provide — functions and `undefined` drop out
# (`[1, function(){}]` => `[1, nil]`, `{a:1, f(){}}` => `{"a"=>1}`), Dates become
# ISO strings — so a consumer sees identical results whatever runtime it picked.

require 'json'
require 'execjs'
require 'rusty_racer'

module RustyRacer
  class ExecJSRuntime < ExecJS::Runtime
    class Context < ExecJS::Runtime::Context
      # `filename` for every JS run, so a thrown error's stack (which rusty_racer
      # surfaces as the Ruby backtrace) reads "(execjs):line:col" — ExecJS's test
      # suite asserts the backtrace mentions "(execjs):".
      LOCATION = '(execjs)'

      def initialize(_runtime, source = '', _options = {})
        @isolate = RustyRacer::Isolate.new
        @context = @isolate.context
        # ExecJS guarantees a bare global (no browser/Node ambient): V8 installs a
        # default `console`, so drop it to match the contract (consumers attach
        # their own if needed), exactly as the mini_racer runtime does.
        @context.eval('delete globalThis.console')
        source = encode(source)
        translate { @context.eval(source, filename: LOCATION) } if /\S/.match?(source)
      end

      # Run statements in a function body and return what they `return` (nil when
      # nothing is returned), per ExecJS. The trailing newline before `}` ends any
      # `//` line comment the source closes with, so it can't eat the wrapper.
      def exec(source, _options = {})
        source = encode(source)
        eval("(function(){#{source}\n})()") if /\S/.match?(source)
      end

      # Evaluate an expression and return its (JSON-projected) value. Blank source
      # is nil. The expression is parenthesised so a leading `{` reads as an
      # object literal (and a trailing `//` comment can't swallow the closing
      # parens — hence the newline), then routed through JSON.stringify for ExecJS
      # semantics.
      def eval(source, _options = {})
        source = encode(source)
        return unless /\S/.match?(source)

        json = translate { @context.eval("JSON.stringify((#{source}\n))", filename: LOCATION) }
        # JSON.stringify yields `undefined` (-> nil here) for a function/undefined
        # result; otherwise a JSON string to parse back.
        json.nil? ? nil : JSON.parse(json)
      end

      # Evaluate `identifier` to a function and call it with the global object as
      # `this` and the (JSON-marshalled) args. `identifier` is arbitrary JS — a
      # name path ("a.b.fn"), a member expression, or a function literal — so it
      # is applied rather than looked up. The newline guards a trailing comment as
      # in eval/exec.
      def call(identifier, *args)
        eval("(#{encode(identifier)}\n).apply(this, #{JSON.generate(args)})")
      end

      private

      # ExecJS speaks UTF-8. Encoding here both normalises the source and turns a
      # genuinely binary input into the Encoding::UndefinedConversionError ExecJS
      # expects, rather than feeding mojibake to V8.
      def encode(source)
        source.encode(Encoding::UTF_8)
      end

      # Map rusty_racer's exception family onto ExecJS's: a compile/syntax failure
      # is an ExecJS::RuntimeError, a thrown-at-runtime error (or a terminated
      # script) is an ExecJS::ProgramError. ExecJS asserts the backtrace mentions
      # "(execjs):". A thrown error already carries the JS stack tagged with our
      # LOCATION filename; a parse error has only the Ruby call stack, so give it
      # a synthetic "(execjs):1" frame instead of leaking rusty internals.
      def translate
        yield
      rescue RustyRacer::ParseError => e
        raise wrap(ExecJS::RuntimeError, e.message, ["#{LOCATION}:1"])
      rescue RustyRacer::EvalError => e
        backtrace = Array(e.backtrace)
        backtrace = ["#{LOCATION}:1"] if backtrace.empty?
        raise wrap(ExecJS::ProgramError, e.message, backtrace)
      end

      def wrap(klass, message, backtrace)
        ex = klass.new(message)
        ex.set_backtrace(backtrace)
        ex
      end
    end

    def name
      'RustyRacer (V8)'
    end

    def available?
      require 'rusty_racer'
      true
    rescue LoadError
      false
    end
  end
end
