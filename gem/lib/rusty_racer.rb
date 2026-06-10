# frozen_string_literal: true

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
  class PlatformAlreadyInitialized < Error; end

  class Context
    # csim's keyword-arg constructor over the positional Rust primitive.
    def self.new(host_namespace: nil)
      _new(host_namespace)
    end

    # csim's keyword-arg API over the positional Rust primitive. `resolve` maps
    # [[specifier, referrer], ...] -> [url|nil, ...]; `fetch_batch` maps
    # [url, ...] -> [[source, cached_data]|nil, ...]. Returns { modules: [...] }.
    def load_module_graph(entry_url, resolve:, fetch_batch:)
      _load_module_graph(entry_url, resolve, fetch_batch)
    end
  end
end
