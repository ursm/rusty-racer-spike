# frozen_string_literal: true

require_relative "lib/rusty_racer/version"

Gem::Specification.new do |spec|
  spec.name        = "rusty_racer"
  spec.version     = RustyRacer::VERSION
  spec.authors     = ["Keita Urashima"]
  spec.email       = ["ursm@ursm.jp"]
  spec.summary     = "Spike: embed V8 in Ruby via rusty_v8 + Magnus (rb-sys)"
  spec.description = "Proof-of-concept rusty_v8/Magnus engine exploring a Rust " \
                     "rewrite of mini_racer-csim's V8 layer. Not for production."
  spec.homepage    = "https://github.com/ursm/rusty-racer-spike"
  spec.license     = "MIT"

  # Drives cibuildgem's Ruby matrix; csim targets >= 3.3.
  spec.required_ruby_version = ">= 3.3"

  spec.files = Dir["lib/**/*.rb", "ext/**/*.{rs,toml,rb,lock}", "README.md"]
  spec.require_paths = ["lib"]
  spec.extensions    = ["ext/rusty_racer/extconf.rb"]

  # rb-sys drives the cargo build from extconf; cibuildgem runs it per platform.
  spec.add_dependency "rb_sys", "~> 0.9"
end
