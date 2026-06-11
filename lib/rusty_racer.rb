# frozen_string_literal: true

require "set" # JS Set <-> Ruby Set marshalling needs the stdlib Set constant

require_relative "rusty_racer/version"

# Load the compiled extension (defines RustyRacer::Isolate etc.). rb-sys names
# the init after the crate. A precompiled ("fat") gem ships one binary per Ruby
# minor version under rusty_racer/<major.minor>/ (the .so is ABI-specific — a
# 3.3 build malfunctions on 4.0), so prefer the version-specific path; the
# source gem and a local `rake compile` produce a single rusty_racer/rusty_racer
# instead, which the fallback loads.
begin
  RUBY_VERSION =~ /(\d+\.\d+)/
  require_relative "rusty_racer/#{Regexp.last_match(1)}/rusty_racer"
rescue LoadError
  require "rusty_racer/rusty_racer"
end

module RustyRacer
  # JS exceptions map to these (see err_class on the Rust side).
  class Error < StandardError; end
  class EvalError < Error; end
  class ParseError < EvalError; end
  class RuntimeError < EvalError; end
  class ScriptTerminatedError < EvalError; end
  class SnapshotError < Error; end
  class PlatformAlreadyInitialized < Error; end

  # A V8 isolate. Owns the VM and its lifecycle; hands out Contexts to run JS in.
  class Isolate
    # Keyword-arg constructor over the positional Rust primitive. A snapshot
    # (RustyRacer::Snapshot) boots the isolate with its baked-in state;
    # timeout_ms caps each eval/call (0 = no limit) against in-V8 infinite
    # loops. microtasks mirrors V8's kAuto/kExplicit: :auto (default) drains
    # the microtask queue when the outermost eval/call/run/evaluate completes
    # (the standard embedder contract); :explicit drains only on
    # #perform_microtask_checkpoint.
    def self.new(host_namespace: nil, snapshot: nil, timeout_ms: 0, microtasks: :auto)
      unless %i[auto explicit].include?(microtasks)
        raise ArgumentError, "microtasks must be :auto or :explicit, got #{microtasks.inspect}"
      end

      _new(host_namespace, snapshot, timeout_ms, microtasks == :explicit)
    end

    # ->(specifier, referrer_url, context) { Module } for JS import(). |context|
    # is the realm import() actually fired in (the Context, not just its id), so
    # an import() from an extra realm resolves/compiles in THAT realm rather than
    # the main one — return e.g. context.compile_module(src, filename: specifier).
    # The block may return a merely compiled Module: linking and evaluation are
    # the binding's job (V8's host contract), and static imports met while linking
    # resolve through this same block (also with the realm as the 3rd arg).
    # (Module#instantiate's own resolve block keeps its 2-arg form.)
    # Held in an ivar so the proc stays alive for the isolate's lifetime (the
    # native side only keeps a weak handle).
    def dynamic_import_resolver=(resolver)
      @dynamic_import_resolver = resolver
      _set_dynamic_import_resolver(resolver)
    end
  end

  # A v8::Context (realm) handed out by an Isolate: where JS actually runs.
  class Context
    # `timeout_ms` (0 = the isolate default) caps this eval; `filename` names the
    # script in stack traces and parse-error locations.
    def eval(source, timeout_ms: 0, filename: '<eval>')
      _eval(source, timeout_ms, filename)
    end

    # Compile a classic <script>; returns a RustyRacer::Script to #run.
    # cached_data:/produce_cache: are the bytecode cache (see #compile_module).
    def compile(source, filename: '<compile>', cached_data: nil, produce_cache: false)
      _compile(source, filename, cached_data, produce_cache)
    end

    # Compile an ES module; returns a RustyRacer::Module to instantiate/evaluate.
    # cached_data: a binary bytecode cache to consume (skip reparse); the result
    # reports #cache_rejected? if stale. produce_cache: collect a fresh cache,
    # readable via Module#cached_data for cross-process reuse.
    def compile_module(source, filename: '<compile_module>', cached_data: nil, produce_cache: false)
      _compile_module(source, filename, cached_data, produce_cache)
    end
  end

  class Module
    # instantiate { |specifier, referrer_url| dependency_module } — the block
    # resolves each import to an already-compiled Module. Returns self.
    def instantiate(&block)
      raise ArgumentError, 'instantiate requires a resolver block' unless block

      _instantiate(block)
      self
    end

    # The V8 module status: :uninstantiated, :instantiating, :instantiated,
    # :evaluating, :evaluated or :errored.
    def status
      _status.to_sym
    end
  end
end
