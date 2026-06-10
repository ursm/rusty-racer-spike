# frozen_string_literal: true

require "set" # JS Set <-> Ruby Set marshalling needs the stdlib Set constant

require_relative "rusty_racer/version"

# Load the compiled extension (defines RustyRacer::Isolate etc.). rb-sys names
# the init after the crate; the .so lands at rusty_racer/rusty_racer.<dlext>.
require "rusty_racer/rusty_racer"

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
    # timeout_ms caps each eval/call (0 = no limit) against in-V8 infinite loops.
    def self.new(host_namespace: nil, snapshot: nil, timeout_ms: 0)
      _new(host_namespace, snapshot, timeout_ms)
    end

    # ->(specifier, referrer_url) { already-loaded Module } for JS import().
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
  end
end
