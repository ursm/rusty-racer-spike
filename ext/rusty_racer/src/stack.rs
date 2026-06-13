// V8 stack limit + conservative-GC-scan retargeting (in-thread: V8 runs on the
// calling Ruby thread's stack — a native pthread stack, or a Ruby Fiber's
// separate mmap'd stack). Self-contained: only raw pointers, std, libc, and the
// exported V8 symbols below — no IsolateState/JsVal/marshalling. The crate uses
// discover_scan_start_field (once per isolate), set_v8_stack_limit (per op), and
// STACK_DEBUG (set at init); everything else is private to this module.

use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};

// rusty_v8 doesn't wrap the runtime `v8::Isolate::SetStackLimit(uintptr_t)`, so
// link the public V8 symbol directly (stable across V8 versions). It sets the
// lowest address V8's stack may reach before it throws RangeError.
unsafe extern "C" {
    #[link_name = "_ZN2v87Isolate13SetStackLimitEm"]
    fn v8__Isolate__SetStackLimit(isolate: *mut c_void, stack_limit: usize);
    // V8's own (exported) accessors down to the conservative-GC-scan Stack
    // object, so we can re-point its stack_start per op when V8 runs on a Ruby
    // Fiber (see set_fiber_scan_start / discover_scan_start_field). Member fns:
    // the first arg is `this`. The public v8::Isolate* IS i::Isolate*.
    #[link_name = "_ZN2v88internal7Isolate4heapEv"]
    fn v8__internal__Isolate__heap(isolate: *mut c_void) -> *mut c_void;
    #[link_name = "_ZN2v88internal4Heap5stackEv"]
    fn v8__internal__Heap__stack(heap: *mut c_void) -> *mut c_void;
    // Sets the scan stack_start to v8::base::Stack::GetStackStart() (the native
    // pthread top) — used only to positively identify the field during discovery.
    #[link_name = "_ZN2v88internal4Heap13SetStackStartEv"]
    fn v8__internal__Heap__SetStackStart(heap: *mut c_void);
    #[link_name = "_ZN2v84base5Stack13GetStackStartEv"]
    fn v8__base__Stack__GetStackStart() -> usize;
}

// Locate V8's conservative-GC-scan stack_start field
// (heap::base::Stack::current_segment_.start) so set_fiber_scan_start can
// re-point it per op. The scanner walks [SP, stack_start); on a Ruby Fiber V8's
// stack_start is still the NATIVE thread top, a different region, so the walk
// runs off the fiber's mapped top into the guard page and SEGVs (the residual
// after the limit fix). We reach the Stack via V8's exported Isolate::heap()/
// Heap::stack(); the field is the first word of Stack (current_segment_ is its
// first member, .start the first field), but we VERIFY rather than trust the
// layout: Heap::SetStackStart() writes that field to base::Stack::GetStackStart(),
// so if poking a sentinel and re-calling SetStackStart restores the value at
// offset 0, that word IS the field. Any mismatch returns 0 (override disabled —
// V8 keeps its native start, i.e. the rare pre-fix crash, NEVER corruption).
// Must run with the isolate ENTERED. `real_isolate` is the raw v8::Isolate*.
pub(crate) fn discover_scan_start_field(real_isolate: *mut c_void) -> usize {
    const SENTINEL: usize = 0xA5A5_A5A5_A5A5_A5A5;
    unsafe {
        let heap = v8__internal__Isolate__heap(real_isolate);
        if heap.is_null() {
            return 0;
        }
        let stack = v8__internal__Heap__stack(heap);
        if stack.is_null() {
            return 0;
        }
        let nt = v8__base__Stack__GetStackStart();
        if nt == 0 {
            return 0;
        }
        v8__internal__Heap__SetStackStart(heap); // start := nt
        let field = stack as *mut usize; // expected &current_segment_.start
        if field.read() != nt {
            return 0; // offset 0 isn't the field (layout changed) — disable
        }
        field.write(SENTINEL);
        v8__internal__Heap__SetStackStart(heap); // must rewrite the same word
        if field.read() != nt {
            return 0; // SetStackStart doesn't own offset 0 — disable
        }
        stack as usize
    }
}

// The native thread's stack bounds are stable per NATIVE thread, but querying
// them (pthread, which reads /proc/self/maps for the main thread on Linux) is
// far too slow per op. Cache (bottom, top) in a native-thread-local — correct
// under M:N (each native thread caches its own stack) and ~free after the first
// op on a thread. (0, 0) if it can't be queried.
thread_local! {
    static STACK_BOUNDS: std::cell::Cell<(usize, usize)> =
        const { std::cell::Cell::new((0, 0)) };
}

fn native_stack_bounds_cached() -> (usize, usize) {
    STACK_BOUNDS.with(|c| {
        let cached = c.get();
        if cached.0 != 0 {
            return cached;
        }
        let bounds = native_stack_bounds();
        c.set(bounds);
        bounds
    })
}

// (bottom, top) of the CURRENT native thread's stack via pthread (uncached —
// callers go through native_stack_bounds_cached). The stack grows DOWN from top
// toward bottom. (0, 0) if it can't be queried. NB: this is the NATIVE thread's
// pthread stack; a Ruby Fiber runs on a separate mmap'd stack invisible here.
#[cfg(target_os = "linux")]
fn native_stack_bounds() -> (usize, usize) {
    unsafe {
        let mut attr: libc::pthread_attr_t = std::mem::zeroed();
        if libc::pthread_getattr_np(libc::pthread_self(), &mut attr) != 0 {
            return (0, 0);
        }
        let mut addr: *mut c_void = null_mut();
        let mut size: libc::size_t = 0;
        let rc = libc::pthread_attr_getstack(&attr, &mut addr, &mut size);
        libc::pthread_attr_destroy(&mut attr);
        if rc != 0 {
            return (0, 0);
        }
        (addr as usize, addr as usize + size)
    }
}

#[cfg(target_os = "macos")]
fn native_stack_bounds() -> (usize, usize) {
    unsafe {
        let top = libc::pthread_get_stackaddr_np(libc::pthread_self()) as usize;
        let size = libc::pthread_get_stacksize_np(libc::pthread_self());
        (top.saturating_sub(size), top)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn native_stack_bounds() -> (usize, usize) {
    (0, 0)
}

// Lower bound (and upper, for caching) of the memory region containing `addr`
// — i.e. the BOTTOM of the stack `addr` is on. Used for a Ruby Fiber, whose
// mmap'd stack pthread can't see: V8's limit must sit ABOVE this bottom or a
// deep fiber recursion overflows the real stack and SEGVs the unmapped guard.
// Cached per native thread keyed by the region (parsing /proc/self/maps is
// slow): reused while successive ops stay on the same fiber. (0, 0) if unknown.
thread_local! {
    static FIBER_REGION: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
}

fn current_region_bounds_cached(addr: usize) -> (usize, usize) {
    FIBER_REGION.with(|c| {
        let (lo, hi) = c.get();
        if lo != 0 && addr >= lo && addr < hi {
            return (lo, hi);
        }
        let bounds = query_region_bounds(addr);
        if bounds.0 != 0 {
            c.set(bounds);
        }
        bounds
    })
}

// The [start, end) of the /proc/self/maps mapping containing `addr`. Linux only;
// (0, 0) elsewhere (and the caller falls back). Reads the file fresh — slow, so
// only called on a cache miss (a new fiber).
#[cfg(target_os = "linux")]
fn query_region_bounds(addr: usize) -> (usize, usize) {
    use std::io::Read;
    let mut buf = String::new();
    if std::fs::File::open("/proc/self/maps")
        .and_then(|mut f| f.read_to_string(&mut buf))
        .is_err()
    {
        return (0, 0);
    }
    for line in buf.lines() {
        // e.g. "7f6a...000-7f6a...000 rw-p 00000000 00:00 0 ..."
        let Some((range, _)) = line.split_once(' ') else {
            continue;
        };
        let Some((lo, hi)) = range.split_once('-') else {
            continue;
        };
        if let (Ok(lo), Ok(hi)) = (
            usize::from_str_radix(lo, 16),
            usize::from_str_radix(hi, 16),
        ) {
            if addr >= lo && addr < hi {
                return (lo, hi);
            }
        }
    }
    (0, 0)
}

#[cfg(not(target_os = "linux"))]
fn query_region_bounds(_addr: usize) -> (usize, usize) {
    (0, 0)
}

// Set from RUSTY_RACER_STACK_DEBUG at init; gates the per-op stack diagnostics.
pub(crate) static STACK_DEBUG: AtomicBool = AtomicBool::new(false);

// Re-point V8's stack limit at the CURRENT stack each op. In-thread V8 runs
// wherever the Ruby code is: usually the native thread's pthread stack, but also
// a Ruby Fiber's separate mmap'd stack (Capybara::Result is an Enumerator) that
// pthread can't see. The limit MUST sit between the current SP and the real
// bottom of whatever stack we're on:
//   * Too high (above SP) and V8 declares a FALSE overflow on entry.
//   * Too low (below the real bottom) and a deep recursion grows past the
//     mapped stack and SEGVs the unmapped guard page below it.
// So detect the stack by comparing the SP to the cached native bounds: on the
// native stack, anchor to its pthread bottom; on a fiber, find the bottom of the
// /proc/self/maps region holding the SP (the fiber's real bottom — anchoring to
// SP minus a fixed guard punched through the bottom of Avo's small/deep Capybara
// fibers and SEGV'd). Must be called with the isolate ENTERED. `real_isolate` is
// the raw v8::Isolate* read out of iso_ptr.
//
// On a fiber it ALSO re-points V8's conservative-GC-scan stack_start (via
// scan_start_field, discovered once per isolate) to `stack_top`: Enter just set
// it to the native top, but the scanner walks [marker, stack_start), so a native
// start runs the scan off the fiber's mapped stack into unmapped memory and
// SEGVs (Avo's Capybara filter chain). scan_start_field is 0 when discovery
// failed (override disabled).
//
// LIMITATION (worker-thread fibers): the GC and a thrown exception ALSO
// `CHECK(IsOnCentralStack(SP))`, which tests the SP against
// `base::Stack::GetStackStart()` — the pthread top, cached per native thread,
// with no API to retarget — NOT the scan start we re-point above. A fiber mmap'd
// ABOVE that top (the common case on a NON-main native thread, whose stack sits
// below later fiber mmaps) fails the CHECK, so V8 aborts on the next GC or throw.
// We can fix the scan (the SEGV) but not that CHECK. On the main thread the
// process stack is the highest address, so every fiber is below it and both the
// scan and the CHECK are safe — the Capybara/Avo case. See README.
pub(crate) fn set_v8_stack_limit(real_isolate: *mut c_void, scan_start_field: usize, stack_top: usize) {
    let sp_marker = 0u8;
    let sp = &sp_marker as *const u8 as usize;
    let (nbottom, ntop) = native_stack_bounds_cached();
    let on_native = nbottom != 0 && sp > nbottom && sp <= ntop;
    // Reserve below the limit for V8's own RangeError-throw frames.
    const NATIVE_GUARD: usize = 128 * 1024;
    // V8 throws when SP descends to the limit, then needs some real stack BELOW
    // it to build the RangeError (and V8 itself allows growing a little past the
    // limit — its overflow slack). On a fiber that reserve must NOT cross the
    // fiber's real bottom (the mapping below it is an unmapped guard -> SEGV), so
    // keep it comfortably above V8's slack.
    const FIBER_RESERVE: usize = 64 * 1024;
    let mut region = (0usize, 0usize);
    let limit = if on_native {
        nbottom + NATIVE_GUARD
    } else {
        // Anchor to the FIBER's real bottom (the /proc/self/maps region holding
        // the SP), not the SP: SP - fixed_guard can punch through the bottom of a
        // small/deep fiber stack and SEGV (Avo's deep Capybara filter chain).
        // Reserve FIBER_RESERVE above the bottom for the throw, but keep the
        // limit below the SP so we don't false-overflow; on a nearly-full fiber
        // that clamps the headroom down (an early but CLEAN RangeError).
        region = current_region_bounds_cached(sp);
        if region.0 != 0 {
            (region.0 + FIBER_RESERVE).min(sp.saturating_sub(8 * 1024))
        } else {
            sp.saturating_sub(64 * 1024) // region unknown (non-linux) — best effort
        }
    };
    if limit == 0 {
        return; // couldn't determine a sane limit — leave V8's default
    }
    unsafe { v8__Isolate__SetStackLimit(real_isolate, limit) };
    // On a fiber, re-point V8's conservative-GC-scan stack_start to `stack_top`
    // — a live address captured by the caller ABOVE every V8 frame of this op.
    // Enter() set the start to the NATIVE top (a different region); the scanner
    // walks [marker, start), so a native start runs it off the fiber's mapped
    // top into unmapped memory and SEGVs. Anchoring to stack_top keeps the whole
    // scan range between two real stack pointers (marker..stack_top), so it's
    // guaranteed mapped, and every V8 root (all below stack_top) is still found.
    // (We can't use the /proc/maps region top here: that mapping isn't reliably
    // contiguous, so the scan could still hit a hole below it.)
    if !on_native && stack_top != 0 && scan_start_field != 0 {
        unsafe { (scan_start_field as *mut usize).write(stack_top) };
    }
    // Opt-in diagnostics (RUSTY_RACER_STACK_DEBUG): the SP vs the native stack
    // [nbottom, ntop], the fiber region (if any), the per-op limit, and whether
    // the SP is above the limit. A crash with sp_above_limit=false means the
    // limit is wrong for the current stack.
    if STACK_DEBUG.load(Ordering::Relaxed) {
        eprintln!(
            "[rusty stack] sp={sp:#x} nbottom={nbottom:#x} ntop={ntop:#x} \
             region=[{:#x},{:#x}) limit={limit:#x} fiber={} sp_above_limit={} \
             fiber_above_native={}",
            region.0,
            region.1,
            !on_native,
            sp > limit,
            !on_native && nbottom != 0 && sp > ntop,
        );
    }
}
