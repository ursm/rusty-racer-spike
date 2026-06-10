# frozen_string_literal: true

require_relative "lib/rusty_racer/version"

Gem::Specification.new do |spec|
  spec.name        = "rusty_racer"
  spec.version     = RustyRacer::VERSION
  spec.authors     = ["Keita Urashima"]
  spec.email       = ["ursm@ursm.jp"]
  spec.summary     = "Embed V8 in Ruby via rusty_v8 + Magnus (rb-sys)"
  spec.description = "A V8 engine for Ruby built on rusty_v8 and Magnus: eval, " \
                     "host functions, ES modules, snapshots, realms, bytecode " \
                     "cache, and faithful value marshalling."
  spec.homepage    = "https://github.com/ursm/rusty-racer-spike"
  spec.license     = "MIT"

  spec.required_ruby_version = ">= 3.3"

  # Scoped to source (NOT ext/**) so a local ext/rusty_racer/target/ build dir
  # is never swept into the packaged gem — Dir globs ignore .gitignore.
  spec.files = Dir[
    "lib/**/*.rb",
    "ext/rusty_racer/src/**/*.rs",
    "ext/rusty_racer/extconf.rb",
    "ext/rusty_racer/Cargo.{toml,lock}",
    "Cargo.toml",
    "README.md",
  ]
  spec.require_paths = ["lib"]
  spec.extensions    = ["ext/rusty_racer/extconf.rb"]

  # rb-sys drives the cargo build from extconf; cibuildgem runs it per platform.
  spec.add_dependency "rb_sys", "~> 0.9"
end
