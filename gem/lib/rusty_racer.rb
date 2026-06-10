# frozen_string_literal: true

require_relative "rusty_racer/version"

# Load the compiled extension (defines RustyRacer::Context). rb-sys names the
# init after the crate; the .so lands at rusty_racer/rusty_racer.<dlext>.
require "rusty_racer/rusty_racer"

module RustyRacer
  # The error hierarchy csim's V8 adapter rescues. The extension currently
  # raises plain RuntimeError; mapping VmError -> these specific classes is the
  # next API increment (stage "A"). Declared here so the namespace is stable.
  class Error < StandardError; end
  class EvalError < Error; end
  class ParseError < EvalError; end
  class RuntimeError < EvalError; end
  class ScriptTerminatedError < EvalError; end
end
