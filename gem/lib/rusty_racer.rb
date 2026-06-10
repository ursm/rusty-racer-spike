# frozen_string_literal: true

require "set" # JS Set <-> Ruby Set marshalling needs the stdlib Set constant

require_relative "rusty_racer/version"

# Load the compiled extension (defines RustyRacer::Context). rb-sys names the
# init after the crate; the .so lands at rusty_racer/rusty_racer.<dlext>.
require "rusty_racer/rusty_racer"

module RustyRacer
  # The error hierarchy csim's V8 adapter rescues. The extension maps VM
  # failures to these (see err_class in the Rust side).
  class Error < StandardError; end
  class EvalError < Error; end
  class ParseError < EvalError; end
  class RuntimeError < EvalError; end
  class ScriptTerminatedError < EvalError; end
  class SnapshotError < Error; end
  class PlatformAlreadyInitialized < Error; end

  class Context
    # Keyword-arg constructor over the positional Rust primitive. A snapshot
    # (RustyRacer::Snapshot) boots the isolate with its baked-in state;
    # timeout_ms caps each eval/call (0 = no limit) against in-V8 infinite loops.
    def self.new(host_namespace: nil, snapshot: nil, timeout_ms: 0)
      _new(host_namespace, snapshot, timeout_ms)
    end

    # `filename` names the script in stack traces and parse-error locations.
    def eval(source, filename: '<eval>')
      _eval(source, filename)
    end

    # Evaluate with a millisecond timeout (raises ScriptTerminatedError).
    def eval_t(source, timeout_ms, filename: '<eval>')
      _eval_t(source, filename, timeout_ms)
    end

    # Keyword-arg API over the positional Rust primitive. `resolve` maps
    # [[specifier, referrer], ...] -> [url|nil, ...]; `fetch_batch` maps
    # [url, ...] -> [[source, cached_data]|nil, ...]. Returns { modules: [...] }.
    def load_module_graph(entry_url, resolve:, fetch_batch:)
      _load_module_graph(entry_url, resolve, fetch_batch)
    end
  end

  class Realm
    def eval(source, filename: '<eval>')
      _eval(source, filename)
    end
  end
end
