require "mkmf"
require "rb_sys/mkmf"

# Build V8 from source so the extension links the library-TLS V8 a cdylib needs
# (the default prebuilt librusty_v8 is initial-exec TLS and fails with
# R_X86_64_TPOFF32 under -shared). On cibuildgem's native runners this just
# works; the linux source build injects -DV8_TLS_USED_IN_LIBRARY by default.
ENV["V8_FROM_SOURCE"] = "1"

create_rust_makefile("rusty_racer/rusty_racer")
