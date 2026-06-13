# frozen_string_literal: true

source "https://rubygems.org"

gemspec

gem "rake"
gem "rake-compiler"
gem "rb_sys", "~> 0.9"
gem "minitest", "~> 6"

# Optional integration target: rusty_racer ships an ExecJS runtime adapter
# (lib/rusty_racer/execjs.rb), exercised against ExecJS's contract in the test
# suite. NOT a runtime dependency — the adapter is only loaded when the user
# already has execjs.
gem "execjs"
