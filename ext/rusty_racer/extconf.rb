require "mkmf"
require "rb_sys/mkmf"

# NOTE: V8 must be built from source so the extension links the library-TLS V8
# a cdylib needs (the prebuilt librusty_v8 is initial-exec TLS -> R_X86_64_TPOFF32
# under -shared). V8_FROM_SOURCE has to be in the *process environment* of the
# `cargo` invocation, which make spawns separately from this extconf — so it is
# set in the CI workflow env (and must be exported in the shell for local
# `rake compile`), NOT here, where it would not propagate.

create_rust_makefile("rusty_racer/rusty_racer")
