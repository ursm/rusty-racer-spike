# frozen_string_literal: true

require "rake/testtask"
require "rb_sys/extensiontask"

GEMSPEC = Gem::Specification.load("rusty_racer.gemspec")

RbSys::ExtensionTask.new("rusty_racer", GEMSPEC) do |ext|
  ext.lib_dir = "lib/rusty_racer"
end

Rake::TestTask.new(test: :compile) do |t|
  t.test_files = FileList["test/**/*_test.rb"]
end

task default: :test
