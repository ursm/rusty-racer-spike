// The Ruby half. A Magnus extension running V8 IN-THREAD: each Isolate runs on
// the Ruby thread that created it (Core::run opens a scope and calls
// service_request inline, releasing the GVL around the JS), with no dedicated V8
// thread and no request channel — the thread-hop cost that model paid per op is
// gone (see the inthread-perf migration). An op's reply is service_request's
// return value, not a channel message.
//
// THREAD-BINDING is the load-bearing constraint. rusty_v8 v150 makes
// OwnedIsolate !Send and binds no v8::Locker, and V8's enter/exit + HandleScope
// are native-thread-bound — yet Magnus wrappers must be Send (Ruby objects
// migrate threads) and Ruby 4.0's M:N scheduler moves a Ruby thread across
// native threads. The binding reconciles these:
//   - The isolate is bound to its owner RUBY thread (rb_thread_current; a native
//     ThreadId is unstable under M:N). Every op asserts owner == current and
//     raises otherwise — a foreign-thread use can't concurrently touch a
//     !Locker isolate.
//   - ALL V8 access for one op (enter -> scope -> JS -> scope-drop -> exit) runs
//     inside ONE without_gvl, hence on one native thread with no GVL boundary
//     mid-op (the OwnedIsolate is entered per top-level op and exited between
//     ops, so several isolates coexist on one thread in any dispose order).
//   - The OwnedIsolate lives boxed in a global registry (ISOLATES); Core keeps a
//     stable raw ptr into it. !Send is contained behind the owner-thread assert.
//
// Host callbacks and module resolvers run INLINE: with_gvl reacquires the GVL to
// run the Ruby proc, then returns into JS; a nested op the proc issues just
// recurses into Core::run (depth > 0, callback_scope!) — re-entrancy is the Rust
// call stack, not a channel round-trip. A Ruby exception is a magnus Err value
// (no longjmp through V8 frames), re-thrown JS-side; an instantiate resolver's
// exception is re-raised with its original class. The watchdog (a per-isolate
// thread firing TerminateExecution via the Send IsolateHandle) stays; the
// OUTERMOST op cancels a stale terminate so it can't poison the next op, while a
// nested op's cancel never erases a termination aimed at the suspended outer JS.
//
// Attached procs and the dynamic-import resolver are GC-rooted via
// rb_gc_register_address (see RootedProc): marked, so the extension may hold the
// only reference, and pinned, so GC.compact cannot move them behind the
// extension's back.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once, Weak};

use magnus::block::Proc;
use magnus::value::{BoxValue, ReprValue};
use magnus::{
    function, method, prelude::*, Error, Exception, ExceptionClass, RHash, Ruby, TryConvert, Value,
};

mod marshal;
use marshal::{js_to_jsval, jsval_to_js, jsval_to_ruby, ruby_to_jsval, JsVal};
mod ops;
use ops::{run_source, service_request, Compiled, Request, VmReply};
mod stack;
use stack::{discover_scan_start_field, set_v8_stack_limit, STACK_DEBUG};
mod watchdog;
use watchdog::{
    arm_watchdog, disarm_watchdog, run_js_bracketed, watchdog_loop, WatchdogShared, WATCHDOG_DEBUG,
};

// A Ruby Proc rooted for as long as the Core holds it. BoxValue registers a
// stable heap address with rb_gc_register_address, which both MARKS the proc
// (the extension may hold the only reference — e.g. attach("f", -> {...}))
// and PINS it (GC.compact must not move it: the copies living in Rust are
// invisible to the compactor and would go stale).
//
// SAFETY of the manual Send: the !Send contents (and BoxValue's drop, which
// calls rb_gc_unregister_address) only run under the GVL. Two conventions
// keep that true — breaking either is silent UB, so don't:
//   - Arc<Core> lives ONLY in the four TypedData wrappers (Isolate, Context,
//     Module, Script). Never clone it into the V8/watchdog threads, or the
//     last drop could run off a Ruby thread.
//   - the wrappers must not set free_immediately: their dfree (and so Core's
//     drop) has to run outside the GC sweep, where Ruby APIs are forbidden.
struct RootedProc(BoxValue<Proc>);
unsafe impl Send for RootedProc {}

impl RootedProc {
    fn get(&self) -> Proc {
        *self.0
    }
}

// The owner Ruby thread, GC-ROOTED for the isolate's life. The owner check
// compares raw Thread VALUEs (Core.owner, a usize); a Thread object is itself
// GC-managed, so without this its slot could be freed after the thread dies and
// REUSED by a new thread — a false owner match that would silently drive a
// !Locker isolate from the wrong native thread. Rooting pins the VALUE so its
// address can't alias another thread while any wrapper is alive. Send/Sync for
// the same reason as RootedProc (only the owner thread ever touches Ruby state).
struct RootedThread(#[allow(dead_code)] BoxValue<Value>);
unsafe impl Send for RootedThread {}
unsafe impl Sync for RootedThread {}

// The stable raw pointer to a Core's in-thread V8 isolate, stashed in Core so
// the runner can open a scope (and the watchdog can be addressed) without
// borrowing the OwnedIsolate out of the ISOLATES registry. Send + Sync for the
// same reason as RootedProc: the pointer is only ever DEREFERENCED on the
// owner thread (every op asserts owner == current), so moving the wrapping
// Core across Ruby threads (GC) never touches V8 off-thread.
struct IsoPtr(*mut v8::Isolate);
unsafe impl Send for IsoPtr {}
unsafe impl Sync for IsoPtr {}

// Reach the IsolateState parked in a scope's (or isolate's) embedder slot — a
// macro, not a fn, so it works on any scope type AND on a bare `&mut Isolate`,
// all of which expose get_slot_mut (a generic fn can't express that over the
// PinScope alias). Borrows the receiver mutably, so use it in SHORT bursts,
// never held across a JS run (a re-entrant host callback must be able to borrow
// it again). Panics if absent — every isolate the binding makes installs one.
// (Defined up here because host_fn_callback, earlier in the file than the
// IsolateState struct, uses it; macro_rules! is textually ordered.)
macro_rules! istate {
    ($scope:expr) => {
        $scope
            .get_slot_mut::<IsolateState>()
            .expect("IsolateState missing from isolate slot")
    };
}
pub(crate) use istate;

// One attach()'d host fn: the realm it was attached into — so resetting or
// disposing that realm can release the GC root — and the rooted proc itself
// (None once released; the slot index stays valid as a host_fn_id).
struct ProcSlot {
    context_id: i32,
    proc: Option<RootedProc>,
}

// The isolate's host-fn registry, indexed by host_fn_id. `free` lists slots
// emptied by release_context_procs so a later attach reuses them instead of
// growing `slots` forever — a long-lived process that re-navigates iframe
// realms would otherwise leak one slot per attach. Reuse is safe because a slot
// is only freed when its realm (and the V8 functions that carried its id) is
// gone, so no stale V8 external can still call a recycled id.
#[derive(Default)]
struct ProcTable {
    slots: Vec<ProcSlot>,
    free: Vec<usize>,
}

impl ProcTable {
    // Take a free slot if one exists, else grow. Returns the host_fn_id.
    fn alloc(&mut self, slot: ProcSlot) -> usize {
        if let Some(id) = self.free.pop() {
            self.slots[id] = slot;
            id
        } else {
            self.slots.push(slot);
            self.slots.len() - 1
        }
    }

    // Release every live proc attached into |context_id| (its realm is gone),
    // returning each slot to the free list. Idempotent: an already-released slot
    // has proc == None and is skipped, so it can't be double-freed.
    fn release(&mut self, context_id: i32) {
        for (id, slot) in self.slots.iter_mut().enumerate() {
            if slot.context_id == context_id && slot.proc.is_some() {
                slot.proc = None;
                self.free.push(id);
            }
        }
    }
}

// Look up a RustyRacer::<name> exception class at raise time. The classes are
// defined in lib/rusty_racer.rb (loaded after this extension), so they exist
// by the time any eval can raise. Falls back to Ruby's RuntimeError.
fn err_class(ruby: &Ruby, name: &str) -> ExceptionClass {
    ruby.class_object()
        .const_get::<_, magnus::RModule>("RustyRacer")
        .and_then(|m| m.const_get::<_, ExceptionClass>(name))
        .unwrap_or_else(|_| ruby.exception_runtime_error())
}

#[derive(Debug)]
enum VmError {
    Parse(String),   // compile-time failure -> RustyRacer::ParseError
    Runtime(String), // internal failure (no JS stack) -> RustyRacer::RuntimeError
    // A thrown JS exception: its message plus the JS stack frames, which become
    // the Ruby exception's backtrace -> RustyRacer::RuntimeError.
    JsError {
        message: String,
        backtrace: Vec<String>,
    },
    Terminated,  // watchdog/stop -> RustyRacer::ScriptTerminatedError
    OutOfMemory, // memory_limit hit -> RustyRacer::V8OutOfMemoryError
}

// V8's near-heap-limit callback (registered on EVERY isolate). V8 calls this,
// synchronously on the owner thread, when a GC still leaves the heap about to
// exceed the current ceiling — i.e. the script is running away on memory. That
// ceiling is the configured memory_limit when one was set, otherwise V8's own
// platform-derived default (the default-protection path), so a runaway raises a
// catchable error either way instead of aborting the process. `data` is the
// isolate ptr we registered (Core.iso_ptr). We flag the isolate and terminate the
// running JS so it unwinds with that catchable error. The return value becomes
// V8's new ceiling: hand it a DOUBLED limit so the unwind itself (and any pending
// finalizers) has room to allocate without tripping a hard OOM abort mid-unwind.
// Core::run, once the op has unwound, forces a GC to reclaim and resets the
// ceiling — see the OOM recovery there. The bump is a no-op-after-the-fact:
// doubling here, GC + reset after, so the limit keeps protecting later ops.
unsafe extern "C" fn near_heap_limit_cb(data: *mut c_void, current_heap_limit: usize, initial: usize) -> usize {
    let isolate = unsafe { &mut *(data as *mut v8::Isolate) };
    // get_slot_mut (not istate!): this runs as an extern "C" callback from V8's
    // C++ allocator, where a panic would unwind across the FFI boundary. The slot
    // is always present once an op can run (set in Isolate::new before any JS), but
    // skip flagging rather than .expect()-panic in the impossible absent case.
    // The closure releases the &mut borrow before terminate_execution (&self).
    // Also record V8's `initial` ceiling so recovery can restore it when no
    // explicit memory_limit was set (see oom_initial_limit).
    let flagged = isolate
        .get_slot_mut::<IsolateState>()
        .map(|s| {
            s.oom_fired = true;
            s.oom_initial_limit = initial;
        })
        .is_some();
    if flagged {
        isolate.terminate_execution();
    }
    current_heap_limit.saturating_mul(2)
}

// After an OOM the running op's outcome is a bare Terminated (the terminate the
// callback fired). Swap it for OutOfMemory so it surfaces as V8OutOfMemoryError,
// preserving the reply's variant (the caller dispatches on it). Only the error
// arm changes; a Terminated from a real timeout/stop never reaches here because
// this runs only when oom_fired was set.
fn relabel_oom(reply: VmReply) -> VmReply {
    fn fix<T>(r: Result<T, VmError>) -> Result<T, VmError> {
        match r {
            Err(VmError::Terminated) => Err(VmError::OutOfMemory),
            other => other,
        }
    }
    match reply {
        VmReply::Done(r) => VmReply::Done(fix(r)),
        VmReply::ModuleCompiled(r) => VmReply::ModuleCompiled(fix(r)),
        VmReply::ScriptCompiled(r) => VmReply::ScriptCompiled(fix(r)),
        VmReply::CodeCache(r) => VmReply::CodeCache(fix(r)),
    }
}

// ---------------------------------------------------------------------------
// GVL plumbing — the unsafe boundary of the gem (two trampolines).
// ---------------------------------------------------------------------------
fn without_gvl<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    struct Job<F, R> {
        f: Option<F>,
        r: Option<R>,
    }
    unsafe extern "C" fn run<F: FnOnce() -> R, R>(data: *mut c_void) -> *mut c_void {
        let job = unsafe { &mut *(data as *mut Job<F, R>) };
        job.r = Some((job.f.take().unwrap())());
        null_mut()
    }
    let mut job = Job::<F, R> { f: Some(f), r: None };
    unsafe {
        rb_sys::rb_thread_call_without_gvl(
            Some(run::<F, R>),
            &mut job as *mut _ as *mut c_void,
            None, // spike: not interruptible by Thread#kill while waiting
            null_mut(),
        );
    }
    job.r.unwrap()
}

// The inverse trampoline: REACQUIRE the GVL to run |f| (a Ruby callback), then
// release it again. Called from inside a V8 host callback / module resolver,
// which runs GVL-released under the in-thread runner's `without_gvl`; the proc
// it invokes needs the GVL held. Nested without_gvl/with_gvl (an op issued by
// the proc re-enters the runner, releasing the GVL again) is sound. |f| must
// NOT let a Ruby exception escape as a longjmp — the callers convert proc
// errors to a Result value and throw on the JS side instead.
fn with_gvl<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    struct Job<F, R> {
        f: Option<F>,
        r: Option<R>,
    }
    unsafe extern "C" fn run<F: FnOnce() -> R, R>(data: *mut c_void) -> *mut c_void {
        let job = unsafe { &mut *(data as *mut Job<F, R>) };
        job.r = Some((job.f.take().unwrap())());
        null_mut()
    }
    let mut job = Job::<F, R> { f: Some(f), r: None };
    unsafe {
        rb_sys::rb_thread_call_with_gvl(Some(run::<F, R>), &mut job as *mut _ as *mut c_void);
    }
    job.r.unwrap()
}

// Identity of the calling RUBY thread (its Thread VALUE, stable for the thread's
// life) — used to bind an isolate to its owner thread. Unlike a native ThreadId,
// this survives Ruby's M:N scheduler moving the thread between native threads.
// MUST be called with the GVL held.
fn current_ruby_thread() -> usize {
    unsafe { rb_sys::rb_thread_current() as usize }
}

// Reduce a magnus Error to a single Exception INSTANCE so it can be GC-rooted and
// re-raised later WITH ITS ORIGINAL CLASS. A Ruby proc's raise is already an
// instance; an Error::new(class, msg) from our own code becomes an instance of
// that class carrying the message. Must be called with the GVL held.
fn error_to_exception(e: &Error) -> Option<Exception> {
    let v = e.value()?;
    if let Ok(exc) = Exception::try_convert(v) {
        return Some(exc);
    }
    if let Ok(class) = ExceptionClass::try_convert(v) {
        return class.new_instance((e.to_string(),)).ok();
    }
    None
}

// JS called a host function. We are on the owner thread with the GVL RELEASED
// (the runner's without_gvl). Reacquire the GVL via with_gvl, run the Ruby proc
// inline, and set the JS return — no channel, no other thread. A VM op the proc
// issues just re-enters Core::run (at depth > 0), so re-entrancy is the plain
// Rust call stack. A Ruby exception is returned as Err(String) (never a longjmp
// through V8 frames) and re-thrown on the JS side.
fn host_fn_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let host_fn_id = match v8::Local::<v8::External>::try_from(args.data()) {
        Ok(e) => e.value() as usize,
        Err(_) => return,
    };
    let mut js_args = Vec::with_capacity(args.length() as usize);
    for i in 0..args.length() {
        js_args.push(js_to_jsval(scope, args.get(i)));
    }
    // Reach the owning Core through the slot back-pointer (the callback holds
    // only a scope). Null only before wiring, which is before any JS can run.
    let core_ptr = istate!(scope).core_ptr;
    if core_ptr.is_null() {
        throw_js_error(scope, "host function has no owner");
        return;
    }
    let result: Result<JsVal, String> = with_gvl(|| {
        let ruby = Ruby::get().unwrap();
        let core = unsafe { &*core_ptr };
        // rb_protect so a Ruby exception raised during arg/return marshalling
        // (ary_new_capa / jsval_to_ruby / ruby_to_jsval can raise on OOM etc.)
        // is CAUGHT here instead of longjmp-ing through V8's C++ frames. The
        // proc's own raise is already a magnus Err; this covers the rest.
        use magnus::rb_sys::AsRawValue;
        let mut out: Option<Result<JsVal, String>> = None;
        match magnus::rb_sys::protect(|| {
            out = Some(core.call_proc(&ruby, host_fn_id, &js_args));
            ruby.qnil().as_raw()
        }) {
            Ok(_) => out.unwrap_or_else(|| Err("host function did not complete".into())),
            Err(e) => Err(format!("{e}")),
        }
    });
    match result {
        Ok(val) => {
            let v = jsval_to_js(scope, val);
            rv.set(v);
        }
        // The proc raised (or marshalling failed): surface as a JS exception.
        Err(message) => throw_js_error(scope, &message),
    }
}

fn throw_js_error(scope: &mut v8::PinScope<'_, '_>, message: &str) {
    if let Some(msg) = v8::String::new(scope, message) {
        let exception = v8::Exception::error(scope, msg);
        scope.throw_exception(exception);
    }
}

// Reject |resolver| with a fresh Error(|message|) — the resolver-promise twin
// of throw_js_error, shared by the dynamic-import paths.
fn reject_with_error(
    scope: &mut v8::PinScope<'_, '_>,
    resolver: v8::Local<v8::PromiseResolver>,
    message: &str,
) {
    if let Some(s) = v8::String::new(scope, message) {
        let e = v8::Exception::error(scope, s);
        resolver.reject(scope, e);
    }
}

// A ScriptOrigin naming the script |filename|, so stack traces and parse-error
// locations report a meaningful resource name instead of being anonymous.
fn script_origin<'s>(scope: &v8::PinScope<'s, '_>, filename: &str) -> v8::ScriptOrigin<'s> {
    let name = v8::String::new(scope, filename).unwrap_or_else(|| v8::String::empty(scope));
    v8::ScriptOrigin::new(
        scope,
        name.into(),
        0,
        0,
        false,
        -1,
        None,
        false,
        false,
        /*is_module*/ false,
        None,
    )
}

// Turn V8's Error.stack text into Ruby-backtrace lines. The first line is the
// "ErrorType: message" header (dropped); each "  at NAME (LOC)" frame becomes
// "LOC:in 'NAME'", and a bare "  at LOC" frame becomes "LOC".
fn parse_js_stack(stack: &str) -> Vec<String> {
    stack
        .lines()
        .filter_map(|line| {
            // Only "at ..." lines are frames; this also skips the header, which
            // is the "ErrorType: message" line(s) — and the message may itself
            // span multiple lines, so a blind skip(1) would leak it as a frame.
            let frame = line.trim().strip_prefix("at ")?;
            if frame.is_empty() {
                return None;
            }
            // "NAME (LOC)" -> "LOC:in 'NAME'". Split on the FIRST " (" so a LOC
            // path that itself contains parentheses stays intact.
            if frame.ends_with(')') {
                if let Some(open) = frame.find(" (") {
                    let name = &frame[..open];
                    let loc = &frame[open + 2..frame.len() - 1];
                    return Some(format!("{loc}:in '{name}'"));
                }
            }
            Some(frame.to_string())
        })
        .collect()
}

// Capture a thrown exception as a JsError: its message + JS stack frames. The
// |exception| and |fallback_stack| Locals are read by the caller (where the
// scope is still a TryCatch); here we only need plain scope access, so this
// takes a PinScope. Prefers the Error's own .stack, then the TryCatch trace.
fn capture_js_error(
    scope: &mut v8::PinScope<'_, '_>,
    exception: Option<v8::Local<v8::Value>>,
    fallback_stack: Option<v8::Local<v8::Value>>,
) -> VmError {
    let message = exception
        .map(|e| e.to_rust_string_lossy(scope))
        .unwrap_or_else(|| "unexpected failure".to_string());
    let mut stack_str = None;
    if let Some(e) = exception {
        if let Some(obj) = e.to_object(scope) {
            if let Some(key) = v8::String::new(scope, "stack") {
                if let Some(s) = obj.get(scope, key.into()) {
                    if s.is_string() {
                        stack_str = Some(s.to_rust_string_lossy(scope));
                    }
                }
            }
        }
    }
    if stack_str.is_none() {
        if let Some(s) = fallback_stack {
            stack_str = Some(s.to_rust_string_lossy(scope));
        }
    }
    let backtrace = stack_str
        .map(|s| parse_js_stack(s.as_str()))
        .unwrap_or_default();
    VmError::JsError { message, backtrace }
}

// ---------------------------------------------------------------------------
// ES modules: V8's raw compile/instantiate/evaluate steps, with the embedder
// owning the url->Module registry (MODULES) and the resolve policy.
// ---------------------------------------------------------------------------
fn module_origin<'s>(scope: &v8::PinScope<'s, '_>, url: &str) -> v8::ScriptOrigin<'s> {
    let name = v8::String::new(scope, url).unwrap();
    v8::ScriptOrigin::new(
        scope, name.into(), 0, 0, false, -1, None, false, false, /*is_module*/ true, None,
    )
}

// Registry for the thin compile_module/instantiate API: each compiled module is
// addressed by an id, with its url kept for the resolve round-trip and a
// hash bucket to map a referrer Local<Module> back to its id.
#[derive(Default)]
struct ModuleReg {
    // id -> (module, url, owning context id). The context id is needed because
    // a module is bound to the v8::Context it was compiled in.
    by_id: HashMap<i32, (v8::Global<v8::Module>, String, i32)>,
    by_hash: HashMap<i32, Vec<(v8::Global<v8::Module>, i32)>>,
    next_id: i32,
}

// Classic compiled scripts: id -> (unbound script, owning context id). An
// UnboundScript is context-independent, but we run it in the context it was
// compiled in (and reset/dispose of that context drops it).
#[derive(Default)]
struct ScriptReg {
    by_id: HashMap<i32, (v8::Global<v8::UnboundScript>, i32)>,
    next_id: i32,
}

// The V8 thread's realm registry: the main context (id 0, swappable by reset),
// the extra realms from create_context, and the host-namespace name to
// re-install on fresh realms. Thread-local (like MODULES/SCRIPTS) so
// service_request can run from BOTH the main request loop and the nested wait
// loops (host callbacks / module resolvers), which only have a scope in hand.
#[derive(Default)]
struct V8State {
    main_context: Option<v8::Global<v8::Context>>,
    contexts: HashMap<i32, v8::Global<v8::Context>>,
    next_context_id: i32,
    host_namespace: Option<String>,
    // One security token shared by every realm of this isolate: the
    // embedder's frames are same-origin, so cross-context access (e.g.
    // NS.contextGlobal) must pass V8's access checks, which compare the
    // contexts' tokens by identity.
    security_token: Option<v8::Global<v8::Value>>,
    // The isolate-wide JS recorder registered via NS.setPromiseRejectHandler,
    // tagged with the context id it was created in (cleared when that context
    // dies — the function would be unusable). The V8 promise-reject callback
    // forwards (event, contextId, promise, reason) to it; the embedder builds
    // HTML's unhandled-rejection bookkeeping on top.
    promise_reject_handler: Option<(i32, v8::Global<v8::Function>)>,
}

// Context embedder-data slot holding the realm id (an Integer), stamped by
// new_realm so id_of_context is O(1). Slot 0 is the embedder's own first slot
// (the binding adds INTERNAL_SLOT_COUNT); nothing else here uses embedder data.
const REALM_ID_SLOT: i32 = 0;

// (STATE/MODULES/SCRIPTS/ACTIVE_REALMS/INSTANTIATING/WATCHDOG_FIRED/
// AUTO_MICROTASKS/DRAINING moved into IsolateState in the isolate slot, reached
// via istate!(scope). Their invariants are documented on IsolateState's fields.)

// ---------------------------------------------------------------------------
// In-thread execution (replaces the dedicated-V8-thread + channel model)
// ---------------------------------------------------------------------------
//
// Everything the old design kept in per-V8-thread thread_locals (above) now
// lives in ONE struct stored in the isolate's embedder slot
// (isolate.set_slot/get_slot). Any function holding a scope reaches it via
// `istate(scope)`, so it's automatically per-isolate with no thread-local
// keying. Accessed in SHORT bursts (never held across a JS run) so a re-entrant
// host callback can borrow it again — same discipline the old thread_locals had.
struct IsolateState {
    realms: V8State,
    modules: ModuleReg,
    scripts: ScriptReg,
    active_realms: Vec<i32>,
    instantiating: bool,
    watchdog_fired: bool,
    auto_microtasks: bool,
    draining: bool,
    // Back-pointer to the owning Core, so a V8 callback (host fn / module
    // resolver) — which holds only a scope — can reach Core.procs and
    // Core.dynamic_import_resolver. Set once, after the Arc<Core> is built;
    // valid for the whole isolate life (Core outlives the slot — the slot is
    // dropped during isolate disposal, which a live wrapper triggers). Null
    // only in the brief window before it is wired up.
    core_ptr: *const Core,
    // Module#instantiate's resolve BLOCK, parked here for the duration of an
    // InstantiateModule op so resolve_imported (a V8 callback) can find it
    // (the old design passed it to pump). GC-rooted; cleared after the op.
    // ..._err carries the resolver's own raised exception (GC-rooted) so the
    // instantiate request can re-raise it WITH ITS ORIGINAL CLASS, instead of a
    // generic "failed to link" (preserving the old pump behaviour).
    instantiate_resolve: Option<RootedProc>,
    instantiate_resolve_err: Option<BoxValue<Exception>>,
    watchdog: Arc<WatchdogShared>,
    // Set by near_heap_limit_cb when the heap ceiling is hit: it terminates the
    // running JS, and Core::run reads this after the op to relabel the terminate as
    // OutOfMemory and recover the heap (GC + reset the ceiling).
    // Plain bool (no atomic): the callback fires synchronously on the owner thread,
    // never concurrently with the bracket that reads it.
    oom_fired: bool,
    // V8's original heap ceiling, captured from the callback's `initial` argument
    // when it fires. Recovery resets the ceiling to memory_limit when one was set,
    // but with no explicit limit (the default-protection path) the ceiling IS V8's
    // platform-derived default, whose value we don't otherwise know — so we restore
    // to this captured initial instead. 0 until the callback has fired at least once.
    oom_initial_limit: usize,
}

impl IsolateState {
    fn new(host_namespace: Option<String>, auto_microtasks: bool) -> Self {
        IsolateState {
            realms: V8State {
                host_namespace,
                next_context_id: 1,
                ..V8State::default()
            },
            modules: ModuleReg::default(),
            scripts: ScriptReg::default(),
            active_realms: Vec::new(),
            instantiating: false,
            watchdog_fired: false,
            auto_microtasks,
            draining: false,
            core_ptr: std::ptr::null(),
            instantiate_resolve: None,
            instantiate_resolve_err: None,
            watchdog: WatchdogShared::new(),
            oom_fired: false,
            oom_initial_limit: 0,
        }
    }
}

// The live OwnedIsolates, keyed by id. A GLOBAL (not a thread_local): Ruby's M:N
// scheduler moves a Ruby thread across native threads, so a native-thread-local
// registry would "lose" an isolate when its owner migrated. The registry is only
// touched at create (insert) and dispose (remove) — never per op (the runner
// uses Core.iso_ptr) — so the global Mutex is uncontended on the hot path.
//
// OwnedIsolate is !Send, so it rides in a SendIso newtype: sound because the V8
// isolate is only ever ENTERED/used on its owner Ruby thread (asserted per op);
// the registry merely owns the box for its lifetime and moves only the pointer.
// BOXED so the OwnedIsolate has a STABLE address — Core stashes a raw *mut
// Isolate pointing INTO it (at the cxx_isolate NonNull), which a move would
// dangle. As a `static` the map is never dropped at process exit, so a leaked
// (never-disposed) isolate just leaks instead of aborting V8's drop assert.
// The box is held purely for ownership (its Drop disposes V8) and is reached at
// runtime through Core.iso_ptr, never by reading this field — hence allow(dead).
#[allow(dead_code)]
struct SendIso(Box<v8::OwnedIsolate>);
unsafe impl Send for SendIso {}

static ISOLATES: std::sync::OnceLock<Mutex<HashMap<u32, SendIso>>> = std::sync::OnceLock::new();

fn isolates() -> &'static Mutex<HashMap<u32, SendIso>> {
    ISOLATES.get_or_init(|| Mutex::new(HashMap::new()))
}

static NEXT_ISOLATE_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

// Count of isolates that could not be disposed because their last wrapper was
// GC-dropped off the owner thread (see Drop for Core) — they leak the OwnedIsolate
// (and its watchdog thread) until process exit. Exposed as
// RustyRacer.leaked_isolate_count so a workload that churns owner threads can
// observe the leak instead of seeing only mystery RSS growth. The cure is to
// dispose isolates explicitly on their owner thread before that thread exits.
static LEAKED_ISOLATES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

// Run a microtask checkpoint with DRAINING set (nesting-safe via save/restore),
// so a nested Reset/DisposeContext issued by a drained microtask is refused.
fn checkpoint_draining(scope: &mut v8::PinScope<'_, '_>) {
    let prev = istate!(scope).draining;
    istate!(scope).draining = true;
    scope.perform_microtask_checkpoint();
    istate!(scope).draining = prev;
}

// The kAuto end-of-script microtask drain, done by the binding: only at the
// OUTERMOST request (nested ops run at V8 call depth > 0 and must not drain),
// only in :auto mode. Called inside the request's watchdog bracket and in the
// request's ContextScope, so a runaway continuation is time-capped and
// terminable.
// The kAuto end-of-script microtask drain (only at the outermost request, only
// in :auto mode). Skipped if JS is already terminating (a checkpoint under an
// active termination is pointless). A runaway drained continuation is caught by
// the watchdog, whose firing the caller maps to Terminated — see the |ran_js|
// override in each JS-running arm.
fn auto_drain(scope: &mut v8::PinScope<'_, '_>, outermost: bool) {
    let auto = istate!(scope).auto_microtasks;
    if outermost && auto && !scope.is_execution_terminating() {
        checkpoint_draining(scope);
    }
}

// Drop every module AND script compiled in `context_id` (its v8::Context is
// going away — on reset or dispose — so those handles are now dead).
fn drop_context_artifacts(state: &mut IsolateState, context_id: i32) {
    let m = &mut state.modules;
    let dead: Vec<i32> = m
        .by_id
        .iter()
        .filter(|(_, (_, _, cid))| *cid == context_id)
        .map(|(id, _)| *id)
        .collect();
    for id in dead {
        m.by_id.remove(&id);
        for bucket in m.by_hash.values_mut() {
            bucket.retain(|(_, mid)| *mid != id);
        }
    }
    state.scripts.by_id.retain(|_, (_, cid)| *cid != context_id);
    // A promise-reject recorder created in this context is unusable now.
    if state
        .realms
        .promise_reject_handler
        .as_ref()
        .is_some_and(|(cid, _)| *cid == context_id)
    {
        state.realms.promise_reject_handler = None;
    }
}

// A script's (unbound handle, owning context id), for running it in that context.
fn script_handle(state: &IsolateState, script_id: i32) -> Option<(v8::Global<v8::UnboundScript>, i32)> {
    state
        .scripts
        .by_id
        .get(&script_id)
        .map(|(g, cid)| (g.clone(), *cid))
}

// A registered module's url (the filename it was compiled with), by identity
// against MODULES — the reverse of the id lookup. Used to give a referrer its
// url for the resolve round-trip and to fill import.meta.url.
fn module_url(scope: &mut v8::PinScope<'_, '_>, module: v8::Local<v8::Module>) -> Option<String> {
    let hash = module.get_identity_hash().get();
    // Snapshot the hash bucket (cloned globals) so the scope is free for the
    // Local comparison below: istate! borrows the scope and v8::Local::new
    // needs it too, so they can't overlap in one expression.
    let bucket = istate!(scope).modules.by_hash.get(&hash)?.clone();
    let id = bucket
        .iter()
        .find(|(g, _)| v8::Local::new(&*scope, g) == module)
        .map(|(_, id)| *id)?;
    istate!(scope)
        .modules
        .by_id
        .get(&id)
        .map(|(_, u, _)| u.clone())
}

// V8 calls this the first time a module reads import.meta. Fills in
// import.meta.url with the module's compile-time url (filename); other
// properties are left to the module system / embedder.
unsafe extern "C" fn import_meta_cb(
    context: v8::Local<v8::Context>,
    module: v8::Local<v8::Module>,
    meta: v8::Local<v8::Object>,
) {
    v8::callback_scope!(unsafe scope, context);
    let Some(url) = module_url(scope, module) else {
        return;
    };
    if let (Some(key), Some(val)) = (v8::String::new(scope, "url"), v8::String::new(scope, &url)) {
        meta.create_data_property(scope, key.into(), val.into());
    }
}

// V8 calls this per import edge during InstantiateModule (and while
// finish_dynamic_import links a dynamically-imported module). Maps the referrer
// to its url and calls the Ruby resolver INLINE (reacquiring the GVL): the
// static instantiate block parked in the slot for this op, or — when none is
// parked (a dynamic import's auto-link) — the Context's dynamic_import_resolver.
fn resolve_imported<'s>(
    context: v8::Local<'s, v8::Context>,
    specifier: v8::Local<'s, v8::String>,
    _import_attributes: v8::Local<'s, v8::FixedArray>,
    referrer: v8::Local<'s, v8::Module>,
) -> Option<v8::Local<'s, v8::Module>> {
    v8::callback_scope!(unsafe scope, context);
    let spec = specifier.to_rust_string_lossy(scope);
    let ref_url = module_url(scope, referrer)?;
    // The realm being linked (this callback's own context). Computed up front so
    // it both rides along to the dynamic_import_resolver AND backs the
    // foreign-context check below; for a real module context it is always Some.
    let here = id_of_context(scope, context);
    let core_ptr = istate!(scope).core_ptr;
    if core_ptr.is_null() {
        return None;
    }
    // The static instantiate block parked for THIS op (Some) vs a dynamic
    // import's auto-link (None -> dynamic_import_resolver, with the initiating
    // realm so it can resolve per-realm).
    let instantiate = istate!(scope).instantiate_resolve.as_ref().map(|r| r.get());
    let dep_id = match instantiate {
        Some(resolve) => {
            match with_gvl(|| {
                resolve_module_via_ruby(unsafe { &*core_ptr }, resolve, &spec, &ref_url, None)
            }) {
                Ok(id) => id,
                // Stash the resolver's own raised exception (GC-rooted) so the
                // InstantiateModule op can re-raise it with its original class
                // instead of a generic "failed to link".
                Err(e) => {
                    if let Some(exc) = error_to_exception(&e) {
                        istate!(scope).instantiate_resolve_err = Some(BoxValue::new(exc));
                    }
                    None
                }
            }
        }
        None => {
            let resolver = unsafe { &*core_ptr }
                .dynamic_import_resolver
                .lock()
                .unwrap()
                .as_ref()
                .map(|r| r.get());
            match resolver {
                Some(p) => with_gvl(|| {
                    resolve_module_via_ruby(
                        unsafe { &*core_ptr },
                        p,
                        &spec,
                        &ref_url,
                        Some(here.unwrap_or(0)),
                    )
                })
                .unwrap_or(None),
                None => None,
            }
        }
    };
    let dep_id = dep_id?;
    // The dep must live in the context actually being linked — the auto-link of
    // a dynamic import runs in whatever realm import() fired in, which kAuto can
    // detach from the request that started it. A foreign-context module would
    // V8-CHECK-abort.
    let g = {
        let (g, _, cid) = istate!(scope).modules.by_id.get(&dep_id)?;
        if Some(*cid) != here {
            return None;
        }
        g.clone()
    };
    Some(v8::Local::new(scope, &g))
}

// V8 calls this for a JS `import(specifier)`. Returns a Promise fulfilled with
// the resolved module's namespace (or rejected). Calls the Context's
// dynamic_import_resolver INLINE (reacquiring the GVL). The resolver may return
// a merely COMPILED module: per V8's host contract, link + evaluate happen here
// (finish_dynamic_import).
fn dynamic_import_cb<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    _host_defined_options: v8::Local<'s, v8::Data>,
    resource_name: v8::Local<'s, v8::Value>,
    specifier: v8::Local<'s, v8::String>,
    _import_attributes: v8::Local<'s, v8::FixedArray>,
) -> Option<v8::Local<'s, v8::Promise>> {
    let resolver = v8::PromiseResolver::new(scope)?;
    let promise = resolver.get_promise(scope);
    let reject = |scope: &mut v8::PinScope<'s, '_>, msg: &str| {
        reject_with_error(scope, resolver, msg);
    };
    let spec = specifier.to_rust_string_lossy(scope);
    let referrer = resource_name.to_rust_string_lossy(scope);
    // The realm import() fired in — handed to the resolver as a Context so it can
    // resolve/compile the module in the right realm (e.g. an iframe's), not the
    // main one.
    let initiating = id_of_context(scope, scope.get_current_context()).unwrap_or(0);
    let core_ptr = istate!(scope).core_ptr;
    if core_ptr.is_null() {
        reject(scope, "dynamic import has no owner");
        return Some(promise);
    }
    let resolver_proc = unsafe { &*core_ptr }
        .dynamic_import_resolver
        .lock()
        .unwrap()
        .as_ref()
        .map(|r| r.get());
    let id = match resolver_proc {
        // A raising resolver only fails the import() (it rejects generically);
        // it must NOT abort the surrounding eval, so swallow the Err here.
        Some(p) => with_gvl(|| {
            resolve_module_via_ruby(unsafe { &*core_ptr }, p, &spec, &referrer, Some(initiating))
        })
        .unwrap_or(None),
        None => None,
    };
    match id {
        Some(id) => {
            // The resolved module must live in the context import() ACTUALLY
            // ran in — a foreign-context module would V8-CHECK-abort. Use the
            // scope's current context, not the request's: under kAuto a
            // microtask queued by realm B can run import() during the drain at
            // the end of realm A's request, so the running realm is the truth.
            let current = scope.get_current_context();
            let here = id_of_context(scope, current);
            let g = istate!(scope)
                .modules
                .by_id
                .get(&id)
                .filter(|(_, _, cid)| Some(*cid) == here)
                .map(|(g, _, _)| g.clone());
            match g {
                Some(g) => {
                    let module = v8::Local::new(scope, &g);
                    finish_dynamic_import(scope, module, resolver);
                }
                None => reject(scope, "resolved module not found"),
            }
        }
        None => reject(scope, "import() was not resolved to a module"),
    }
    Some(promise)
}

// The on-fulfilled handler for finish_dynamic_import's native then: ignores
// the evaluation promise's fulfilment value and returns the module namespace
// captured as the function's data, so the import() promise fulfils with the
// namespace once evaluation (incl. top-level await) completes.
fn dyn_import_namespace_cb(
    _scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    rv.set(args.data());
}

// Complete a dynamic import with the module the resolver named. V8's
// HostImportModuleDynamicallyCallback contract makes load -> link -> evaluate
// the HOST's responsibility, so a freshly compiled (unevaluated) module is
// linked and evaluated right here; static imports met while linking resolve
// through the same dynamic_import_resolver (pump's ResolveModule fallback).
// The import() promise is settled FROM the evaluation promise, which handles
// sync completion, a thrown error, and top-level await uniformly — and since
// V8's Module::Evaluate is idempotent (an Evaluated module returns its
// existing top-level promise), the registry-hit case costs nothing extra.
fn finish_dynamic_import(
    scope: &mut v8::PinScope<'_, '_>,
    module: v8::Local<v8::Module>,
    resolver: v8::Local<v8::PromiseResolver>,
) {
    v8::tc_scope!(let tc, scope);
    if module.get_status() == v8::ModuleStatus::Uninstantiated {
        // Same no-re-entrancy rule as Request::InstantiateModule: linking
        // while another link is on the stack walks V8's half-built graph.
        if istate!(tc).instantiating {
            reject_with_error(
                tc,
                resolver,
                "cannot link a dynamic import while another module is instantiating",
            );
            return;
        }
        istate!(tc).instantiating = true;
        let linked = module.instantiate_module(tc, resolve_imported);
        istate!(tc).instantiating = false;
        if linked != Some(true) {
            // A watchdog/terminate that landed during linking must escalate to
            // the outer request (this nested frame must not absorb it), so
            // leave the promise pending and return with the flag still set.
            if tc.has_terminated() {
                return;
            }
            match tc.exception() {
                Some(exc) => {
                    resolver.reject(tc, exc);
                }
                None => reject_with_error(tc, resolver, "failed to link dynamically imported module"),
            }
            return;
        }
    }
    match module.get_status() {
        // A module that threw during evaluation rejects with its own
        // exception, not a stale namespace.
        v8::ModuleStatus::Errored => {
            let exc = module.get_exception();
            resolver.reject(tc, exc);
        }
        v8::ModuleStatus::Instantiated | v8::ModuleStatus::Evaluated => {
            match module.evaluate(tc) {
                Some(value) => {
                    let Ok(eval_promise) = v8::Local::<v8::Promise>::try_from(value) else {
                        reject_with_error(tc, resolver, "module evaluation did not yield a promise");
                        return;
                    };
                    // Settle the import() promise from the evaluation promise
                    // via the NATIVE Promise::then (a V8 builtin, immune to a
                    // user-patched Promise.prototype.then): on fulfilment hand
                    // back the namespace, on rejection adopt the same reason.
                    let ns = module.get_module_namespace();
                    let fulfill = v8::Function::builder(dyn_import_namespace_cb)
                        .data(ns)
                        .build(tc);
                    match fulfill.and_then(|f| eval_promise.then(tc, f)) {
                        Some(chained) => {
                            resolver.resolve(tc, chained.into());
                        }
                        // Termination during then escalates; otherwise fail.
                        None if tc.has_terminated() => {}
                        None => reject_with_error(tc, resolver, "failed to settle dynamic import"),
                    }
                }
                // Termination escalates to the outer request (leave pending).
                None if tc.has_terminated() => {}
                None => match tc.exception() {
                    Some(exc) => {
                        resolver.reject(tc, exc);
                    }
                    None => reject_with_error(
                        tc,
                        resolver,
                        "dynamically imported module failed to evaluate",
                    ),
                },
            }
        }
        // Mid-link/mid-evaluation on this very stack (a cycle back into an
        // in-flight module): refuse cleanly rather than corrupt the walk.
        _ => reject_with_error(
            tc,
            resolver,
            "dynamically imported module is busy (instantiating/evaluating)",
        ),
    }
}

// The id of |context|, read O(1) from the realm-id stamped in by new_realm.
// None when the context is not a LIVE realm of this isolate — a context reset
// away still carries its old stamp, so confirm the id currently maps back to
// this very context before trusting it.
fn id_of_context(scope: &mut v8::PinScope<'_, '_>, context: v8::Local<v8::Context>) -> Option<i32> {
    let id = context
        .get_embedder_data(scope, REALM_ID_SLOT)
        .and_then(|v| v.int32_value(scope))?;
    let current = context_for(istate!(scope), id);
    let live = current.is_some_and(|g| v8::Local::new(scope, &g) == context);
    live.then_some(id)
}

// Pick the Global context for a realm id: 0 = main, N = an extra realm (None
// if it was disposed or never existed). Clones the Global (cheap, refcounted)
// so no STATE borrow is held while the caller runs JS.
fn context_for(state: &IsolateState, context_id: i32) -> Option<v8::Global<v8::Context>> {
    if context_id == 0 {
        state.realms.main_context.clone()
    } else {
        state.realms.contexts.get(&context_id).cloned()
    }
}

// A module's (handle, owning context id), for running its ops in the right
// v8::Context.
fn module_handle(state: &IsolateState, module_id: i32) -> Option<(v8::Global<v8::Module>, i32)> {
    state
        .modules
        .by_id
        .get(&module_id)
        .map(|(g, _, cid)| (g.clone(), *cid))
}

// ---------------------------------------------------------------------------
// Ruby side
// ---------------------------------------------------------------------------
struct Shared {
    handle: v8::IsolateHandle,
    disposed: bool,
}

// The in-thread V8 isolate's control block. The isolate runs ON the Ruby thread
// that created it (its `owner`): there is no dedicated V8 thread and no request
// channel — `Core::run` opens a scope on the isolate (via `iso_ptr`) and runs
// the op inline, releasing the GVL around the JS. A Context and all the Realms
// it spawns share ONE Core via Arc, so any of them can issue ops and they all
// see the same attached procs and dispose state. The isolate is THREAD-BOUND:
// every op asserts owner == current thread and raises otherwise (a foreign-thread
// use would SEGV deep in V8 — rusty_v8 exposes no v8::Locker).
struct Core {
    // Weak self-handle so a &Core method can mint an Arc<Core> again (built via
    // Arc::new_cyclic). Needed to hand a fresh Context wrapper to the dynamic
    // import resolver — Context owns an Arc<Core> and &self can't recover it.
    me: Weak<Core>,
    shared: Mutex<Shared>,
    // The id this isolate is keyed under in the owner thread's ISOLATES registry,
    // and the owning RUBY thread (its Thread VALUE — see current_ruby_thread).
    // Every op checks `owner == current_ruby_thread()`. `_owner_root` GC-roots
    // that Thread VALUE so its address can't be reused by a later thread (a false
    // owner match — see RootedThread).
    iso_id: u32,
    owner: usize,
    _owner_root: RootedThread,
    // Stable raw ptr to the OwnedIsolate's V8 isolate (it lives in ISOLATES). The
    // runner opens its scope from this without borrowing ISOLATES across the run,
    // so a re-entrant op can't double-borrow the registry. Dereferenced only on
    // the owner thread.
    iso_ptr: IsoPtr,
    // Address of V8's conservative-GC-scan stack_start field (see
    // discover_scan_start_field), or 0 if discovery failed. set_v8_stack_limit
    // writes the fiber region top here per fiber op so a GC scan stays mapped.
    // Set once at creation, then read-only; AtomicUsize for shared &Core access.
    scan_start_field: std::sync::atomic::AtomicUsize,
    // Re-entry depth for THIS isolate, readable without a scope (the runner needs
    // it to choose the scope kind before any scope exists): 0 = top-level op
    // (open a fresh HandleScope from iso_ptr); >0 = a host callback is on the V8
    // stack (bootstrap via callback_scope! onto the ambient scope). Bumped around
    // each `run`.
    depth: std::sync::atomic::AtomicU32,
    // host_fn_id indexes ProcTable.slots. Mutex (uncontended — single owner
    // thread) so host_fn_callback can reach it through Core (via the slot's
    // core_ptr) while a &Core method also holds it. Each proc is GC-rooted while
    // live — see RootedProc/ProcSlot; reset/dispose releases roots, recycles slots.
    procs: Mutex<ProcTable>,
    // Default per-eval/call timeout (ms); 0 = none. eval(timeout_ms:)'s explicit
    // value overrides it. Guards against an in-V8 infinite loop without a watchdog.
    default_timeout_ms: u64,
    // Per-isolate heap ceiling (bytes); 0 = V8's default ceiling. When set, the
    // isolate is created with this as V8's max heap; near_heap_limit_cb is registered
    // either way (against this ceiling when set, V8's default otherwise), so a runaway
    // is always catchable rather than a process abort. Core::run's OOM recovery resets
    // the ceiling after each OOM (to this when set, else V8's captured default — see
    // oom_initial_limit). Space-axis twin of default_timeout_ms.
    memory_limit: usize,
    // Set by Context#dynamic_import_resolver=; called for a JS import() to map
    // (specifier, referrer) to an already-loaded Module. GC-rooted like procs.
    dynamic_import_resolver: Mutex<Option<RootedProc>>,
    // The watchdog (armed per timed op, fires TerminateExecution via the handle)
    // and its thread's join handle, held here so dispose/Drop — which run on the
    // owner thread with no scope — can stop and join it before the isolate drops.
    watchdog: Arc<WatchdogShared>,
    watchdog_join: Mutex<Option<std::thread::JoinHandle<()>>>,
}

// The V8 isolate (one per Isolate): lifecycle + the isolate-level ops
// (terminate, microtask checkpoint, dynamic import). eval/call/etc. live on
// Context, which an Isolate hands out (a v8::Context).
#[magnus::wrap(class = "RustyRacer::Isolate")]
struct Isolate {
    core: Arc<Core>,
}

// A v8::Context (a realm): the default one (id 0, via Isolate#context) or an
// extra one (id >= 1, via Isolate#create_context). eval/call/attach/
// compile_module run here. Its own `disposed` is per-context; the Core's is
// isolate-level.
#[magnus::wrap(class = "RustyRacer::Context")]
struct Context {
    core: Arc<Core>,
    id: i32,
    disposed: AtomicBool,
}

// A V8 startup blob. Built by running code in a throwaway isolate; consumed by
// Context.new(snapshot:). warmup! re-snapshots with extra code to pre-compile.
#[magnus::wrap(class = "RustyRacer::Snapshot")]
struct Snapshot {
    blob: RefCell<Vec<u8>>,
}

// Context#compile_module result: a handle to a V8 module (by id).
#[magnus::wrap(class = "RustyRacer::Module")]
struct JsModule {
    core: Arc<Core>,
    module_id: i32,
    disposed: AtomicBool,
    // Bytecode cache produced at compile (produce_cache:), and whether a
    // supplied cache was rejected — exposed as #cached_data / #cache_rejected?.
    cached_data: Option<Vec<u8>>,
    cache_rejected: bool,
}

// Context#compile result: a handle to a classic compiled script (by id).
#[magnus::wrap(class = "RustyRacer::Script")]
struct Script {
    core: Arc<Core>,
    script_id: i32,
    disposed: AtomicBool,
    cached_data: Option<Vec<u8>>,
    cache_rejected: bool,
}

// Set true once V8 is initialized; Platform.set_flags! refuses after that
// (flags must be set before V8::initialize), like mini_racer's
// PlatformAlreadyInitialized.
static V8_INITED: AtomicBool = AtomicBool::new(false);

fn init_v8() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        STACK_DEBUG.store(
            std::env::var_os("RUSTY_RACER_STACK_DEBUG").is_some(),
            Ordering::Relaxed,
        );
        WATCHDOG_DEBUG.store(
            std::env::var_os("RUSTY_RACER_WATCHDOG_DEBUG").is_some(),
            Ordering::Relaxed,
        );
        let platform = v8::new_default_platform(0, false).make_shared();
        v8::V8::initialize_platform(platform);
        v8::V8::initialize();
        V8_INITED.store(true, Ordering::SeqCst);
    });
}

// RustyRacer.cached_data_version_tag -> Integer (V8's CachedData version tag).
fn cached_data_version_tag() -> u32 {
    v8::script_compiler::cached_data_version_tag()
}

// RustyRacer::Platform.set_flags!(*flags, **kwargs): symbol/string -> --flag,
// hash entry -> --key=value. Must run before the first Isolate.new.
fn platform_set_flags(args: &[Value]) -> Result<(), Error> {
    let ruby = Ruby::get().unwrap();
    if V8_INITED.load(Ordering::SeqCst) {
        return Err(Error::new(
            err_class(&ruby, "PlatformAlreadyInitialized"),
            "the V8 platform is already initialized; set flags before the first Isolate.new",
        ));
    }
    let mut flags = String::new();
    for a in args {
        if let Ok(h) = RHash::try_convert(*a) {
            h.foreach(|k: Value, v: Value| {
                let ks = k.funcall::<_, _, String>("to_s", ())?;
                // A nil value means a bare boolean flag (--key), not --key=.
                if v.is_nil() {
                    flags.push_str(&format!(" --{ks}"));
                } else {
                    let vs = v.funcall::<_, _, String>("to_s", ())?;
                    flags.push_str(&format!(" --{ks}={vs}"));
                }
                Ok(magnus::r_hash::ForEach::Continue)
            })?;
        } else {
            let s = a.funcall::<_, _, String>("to_s", ())?;
            flags.push_str(&format!(" --{s}"));
        }
    }
    v8::V8::set_flags_from_string(flags.trim());
    Ok(())
}

// A globalThis.<host_namespace> member: drain the microtask queue inline.
// Self-contained native callback (no Ruby roundtrip).
fn drain_microtasks(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    scope.perform_microtask_checkpoint();
}

// NS.contextGlobal(id) -> the globalThis of context |id|. Cross-context
// access within the isolate is plain V8 (no security tokens configured), so
// this is the embedder's `iframe.contentWindow`: the frame realm's global,
// reachable from the parent realm. Throws on an unknown/disposed id.
fn context_global(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(id) = args.get(0).int32_value(scope) else {
        throw_js_error(scope, "contextGlobal expects a context id");
        return;
    };
    let realm = context_for(istate!(scope), id);
    match realm {
        Some(g) => {
            let context = v8::Local::new(scope, &g);
            rv.set(context.global(scope).into());
        }
        None => throw_js_error(scope, &format!("unknown context {id}")),
    }
}

// NS.contextOf(value) -> the id of the context |value| was created in, or
// undefined for primitives and for objects whose context is no longer a live
// realm (e.g. it was reset away). Lets the embedder attribute a function or
// object to its frame.
fn context_of(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Ok(obj) = v8::Local::<v8::Object>::try_from(args.get(0)) else {
        return; // primitive -> undefined
    };
    let Some(context) = obj.get_creation_context(scope) else {
        return;
    };
    if let Some(id) = id_of_context(scope, context) {
        rv.set(v8::Integer::new(scope, id).into());
    }
}

// NS.setPromiseRejectHandler(fn | null): register (or clear) the isolate-wide
// recorder that promise_reject_cb forwards V8's reject notifications to.
fn set_promise_reject_handler(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    match v8::Local::<v8::Function>::try_from(args.get(0)) {
        Ok(f) => {
            let cid = f
                .get_creation_context(scope)
                .and_then(|cx| id_of_context(scope, cx))
                .unwrap_or(0);
            let g = v8::Global::new(scope, f);
            istate!(scope).realms.promise_reject_handler = Some((cid, g));
        }
        Err(_) => {
            istate!(scope).realms.promise_reject_handler = None;
        }
    }
}

// V8 calls this synchronously on promise rejections with no handler (and the
// later revocations when a handler IS added). Forwards
// (event, contextId, promise, reason) to the registered JS recorder — the
// contextId being the PROMISE's creation context, which is how the embedder
// attributes an unhandledrejection event to the right frame. Events mirror
// v8::PromiseRejectEvent: 0 = rejected with no handler, 1 = handler added
// after reject, 2 = reject after resolved, 3 = resolve after resolved.
unsafe extern "C" fn promise_reject_cb(message: v8::PromiseRejectMessage) {
    v8::callback_scope!(unsafe scope, &message);
    let handler = istate!(scope)
        .realms
        .promise_reject_handler
        .as_ref()
        .map(|(_, g)| g.clone());
    let Some(handler) = handler else { return };
    let promise = message.get_promise();
    let event = match message.get_event() {
        v8::PromiseRejectEvent::PromiseRejectWithNoHandler => 0,
        v8::PromiseRejectEvent::PromiseHandlerAddedAfterReject => 1,
        v8::PromiseRejectEvent::PromiseRejectAfterResolved => 2,
        v8::PromiseRejectEvent::PromiseResolveAfterResolved => 3,
    };
    let context_id = promise
        .get_creation_context(scope)
        .and_then(|cx| id_of_context(scope, cx));
    let handler = v8::Local::new(scope, &handler);
    // Run the recorder in ITS OWN context (it may differ from the rejecting
    // promise's).
    let Some(handler_context) = handler.get_creation_context(scope) else {
        return;
    };
    let scope = &mut v8::ContextScope::new(scope, handler_context);
    let event_arg: v8::Local<v8::Value> = v8::Integer::new(scope, event).into();
    let context_arg: v8::Local<v8::Value> = match context_id {
        Some(id) => v8::Integer::new(scope, id).into(),
        None => v8::undefined(scope).into(),
    };
    let reason: v8::Local<v8::Value> = message
        .get_value()
        .unwrap_or_else(|| v8::undefined(scope).into());
    let recv: v8::Local<v8::Value> = v8::undefined(scope).into();
    // The recorder must never break the script that happened to reject a
    // promise — swallow anything it THROWS. But a TerminateExecution (watchdog
    // timeout or Isolate#terminate, aimed at the surrounding script) is not an
    // ordinary throw: the TryCatch absorbs it too, so re-assert it after the
    // call, or the terminated outer script would resume unbounded.
    let terminated = {
        v8::tc_scope!(let tc, scope);
        let _ = handler.call(tc, recv, &[event_arg, context_arg, promise.into(), reason]);
        tc.has_terminated()
    };
    if terminated {
        scope.terminate_execution();
    }
}

// Build a fresh v8::Context and install the host namespace (from STATE) into
// it — the single definition of "a realm of this isolate", shared by boot,
// reset and create_context so realms can't drift apart.
fn new_realm(scope: &mut v8::PinScope<'_, '_, ()>, id: i32) -> v8::Global<v8::Context> {
    let fresh = {
        let context = v8::Context::new(scope, Default::default());
        v8::Global::new(scope, context)
    };
    // Stamp the realm id into the context so id_of_context is O(1) (it would
    // otherwise scan every realm on every promise rejection / contextOf call).
    {
        let context = v8::Local::new(scope, &fresh);
        let id_val: v8::Local<v8::Value> = v8::Integer::new(scope, id).into();
        context.set_embedder_data(REALM_ID_SLOT, id_val);
    }
    // DESIGN DECISION: every realm of an isolate shares ONE security token, so
    // they are all mutually same-origin — the model is "a group of same-origin
    // frames sharing one heap", and NS.contextGlobal gives full cross-realm
    // access exactly as same-origin `iframe.contentWindow` does. (By default V8
    // gives each context a distinct token, which would make every cross-realm
    // access fail its check.)
    //
    // This is the right model for an embedder that treats all frames as one
    // trust domain. It is NOT a security boundary between realms: same-isolate
    // V8 contexts never are, and here it is deliberately wide open.
    //
    // To distinguish origins (real cross-origin iframes), this is the extension
    // point: give each realm a token derived from its origin (e.g. a per-origin
    // String) instead of the shared one, so cross-origin access fails the check.
    // Note: V8's token only does same-origin(full) vs cross-origin(deny). HTML's
    // cross-origin allowlist (location/postMessage/...) needs AccessCheckCallback,
    // which rusty_v8 v150 does not expose — that would need new FFI.
    {
        let context = v8::Local::new(scope, &fresh);
        let token = istate!(scope).realms.security_token.clone();
        let token: v8::Local<v8::Value> = match token {
            Some(t) => v8::Local::new(scope, &t),
            None => {
                let t: v8::Local<v8::Value> = v8::String::new(scope, "rusty_racer")
                    .map(|s| s.into())
                    .unwrap_or_else(|| v8::undefined(scope).into());
                let g = v8::Global::new(scope, t);
                istate!(scope).realms.security_token = Some(g);
                t
            }
        };
        context.set_security_token(token);
    }
    let host_namespace = istate!(scope).realms.host_namespace.clone();
    if let Some(name) = host_namespace {
        install_host_namespace(scope, &fresh, &name);
    }
    fresh
}

// Inject globalThis.<name> = { drainMicrotasks } into a context. Re-run on
// reset_realm so the fresh realm keeps the namespace. Takes a scope (not the
// isolate) so service_request can call it from nested servicing too.
fn install_host_namespace(
    scope: &mut v8::PinScope<'_, '_, ()>,
    ctx: &v8::Global<v8::Context>,
    name: &str,
) {
    v8::scope!(let scope, &mut *scope);
    let context = v8::Local::new(scope, ctx);
    let scope = &mut v8::ContextScope::new(scope, context);
    let ns = v8::Object::new(scope);
    let members: [(&str, Option<v8::Local<v8::Function>>); 4] = [
        ("drainMicrotasks", v8::Function::new(scope, drain_microtasks)),
        ("contextGlobal", v8::Function::new(scope, context_global)),
        ("contextOf", v8::Function::new(scope, context_of)),
        (
            "setPromiseRejectHandler",
            v8::Function::new(scope, set_promise_reject_handler),
        ),
    ];
    for (member, function) in members {
        if let (Some(f), Some(k)) = (function, v8::String::new(scope, member)) {
            ns.set(scope, k.into(), f.into());
        }
    }
    if let Some(key) = v8::String::new(scope, name) {
        let global = context.global(scope);
        global.set(scope, key.into(), ns.into());
    }
}

// Set |function| at the dotted property path |name| on the global, creating
// intermediate plain objects as needed — so attach("MiniRacer.foo", ...) lands
// under the namespace whether or not globalThis.MiniRacer already exists, while
// a bare "foo" still attaches on the global.
fn attach_at_path(
    scope: &mut v8::PinScope<'_, '_>,
    context: v8::Local<v8::Context>,
    name: &str,
    function: v8::Local<v8::Function>,
) -> Result<JsVal, VmError> {
    let mut parts: Vec<&str> = name.split('.').collect();
    let leaf = parts
        .pop()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| VmError::Runtime(format!("invalid attach name `{name}`")))?;
    let mut holder = context.global(scope);
    for part in parts {
        if part.is_empty() {
            return Err(VmError::Runtime(format!("invalid attach name `{name}`")));
        }
        let key =
            v8::String::new(scope, part).ok_or_else(|| VmError::Runtime("name too large".into()))?;
        holder = match holder.get(scope, key.into()) {
            Some(v) if v.is_object() => v8::Local::<v8::Object>::try_from(v).unwrap(),
            // Don't clobber an existing non-object (e.g. a primitive global that
            // collides with the namespace name) — fail loudly instead.
            Some(v) if !v.is_undefined() && !v.is_null() => {
                return Err(VmError::Runtime(format!(
                    "`{name}`: `{part}` exists and is not an object"
                )));
            }
            _ => {
                let obj = v8::Object::new(scope);
                if holder.set(scope, key.into(), obj.into()) != Some(true) {
                    return Err(VmError::Runtime(format!("`{name}`: cannot create `{part}`")));
                }
                obj
            }
        };
    }
    let key =
        v8::String::new(scope, leaf).ok_or_else(|| VmError::Runtime("name too large".into()))?;
    if holder.set(scope, key.into(), function.into()) != Some(true) {
        return Err(VmError::Runtime(format!(
            "`{name}`: target is not writable/extensible"
        )));
    }
    Ok(JsVal::Undefined)
}

// Build a startup blob by running |code| in a throwaway isolate and snapshotting
// a default context. Runs entirely on the calling (Ruby) thread: the
// OwnedIsolate is a local, never stored in a Send wrapper, so the !Send dedicated
// -thread rule doesn't apply. |base| warms an existing blob further.
//
// |warmup| selects V8's WarmUpSnapshotDataBlob contract: |code| runs in a
// THROWAWAY context — the point is filling the isolate's compilation cache —
// and a FRESH context becomes the blob's default, so no heap state from the
// warmup run is baked in (only the compiled code survives, via
// FunctionCodeHandling::Keep). Without |warmup|, the context |code| ran in IS
// the default: Snapshot.new deliberately bakes its globals.
//
// NB: unlike Eval there is no watchdog here and the GVL is held throughout, so
// |code| must be trusted setup — an infinite loop would freeze the whole Ruby
// VM. Snapshot/warmup code is author-controlled, so that's an accepted tradeoff.
fn build_snapshot(code: &str, base: Option<Vec<u8>>, warmup: bool) -> Result<Vec<u8>, String> {
    init_v8();
    let mut creator = match base {
        Some(bytes) => v8::Isolate::snapshot_creator_from_existing_snapshot(
            v8::StartupData::from(bytes),
            None,
            None,
        ),
        None => v8::Isolate::snapshot_creator(None, None),
    };
    let mut err: Option<String> = None;
    {
        v8::scope!(let scope, &mut creator);
        let context = v8::Context::new(scope, Default::default());
        {
            let cscope = &mut v8::ContextScope::new(scope, context);
            if !code.is_empty() {
                if let Err(e) = run_source(cscope, code, if warmup { "<warmup>" } else { "<snapshot>" }) {
                    err = Some(match e {
                        VmError::Parse(m) | VmError::Runtime(m) => m,
                        VmError::JsError { message, .. } => message,
                        VmError::Terminated => "snapshot code was terminated".to_string(),
                        // Unreachable: the snapshot-creator is a separate isolate
                        // that never registers near_heap_limit_cb.
                        VmError::OutOfMemory => "snapshot code ran out of memory".to_string(),
                    });
                }
            }
        }
        // Mark the context to deserialize on boot (after the ContextScope is
        // dropped, like denoland/rusty_v8's snapshot path): the one |code|
        // mutated for a plain snapshot, a fresh one for a warmup.
        if warmup {
            let fresh = v8::Context::new(scope, Default::default());
            scope.set_default_context(fresh);
        } else {
            scope.set_default_context(context);
        }
    }
    // create_blob MUST run before the creator is dropped (rusty_v8 panics
    // otherwise), even when the user code failed — so consume it first, then
    // surface the error.
    let blob = creator.create_blob(v8::FunctionCodeHandling::Keep);
    if let Some(e) = err {
        return Err(e);
    }
    blob.map(|d| d.to_vec())
        .ok_or_else(|| "snapshot creation failed".to_string())
}

impl Isolate {
    // Build the isolate ON THIS (the calling Ruby) thread — no dedicated V8
    // thread. The OwnedIsolate moves into the owner thread's ISOLATES registry;
    // Core keeps its id + a stable raw ptr so `run` can open scopes on it. The
    // isolate is thread-bound from here on (every op asserts the owner thread).
    fn new(
        _ruby: &Ruby,
        host_namespace: Option<String>,
        snapshot: Option<magnus::typed_data::Obj<Snapshot>>,
        timeout_ms: u64,
        memory_limit: usize,
        explicit_microtasks: bool,
    ) -> Result<Self, Error> {
        init_v8();
        // A snapshot blob bakes globalThis state in: the first Context::new (in
        // new_realm below) deserializes that default context for free.
        let snapshot_bytes = snapshot.map(|s| s.blob.borrow().clone());
        let mut create_params = match snapshot_bytes {
            Some(bytes) => v8::CreateParams::default().snapshot_blob(v8::StartupData::from(bytes)),
            None => Default::default(),
        };
        // Cap V8's heap at the configured limit so its near-heap-limit callback
        // fires as the script approaches it (initial 0 = V8's default initial heap).
        // With no explicit limit the callback is still registered below, against
        // V8's own default ceiling, so a runaway raises instead of aborting.
        if memory_limit > 0 {
            create_params = create_params.heap_limits(0, memory_limit);
        }
        let mut isolate = v8::Isolate::new(create_params);
        // Always Explicit at the V8 level; the binding performs the kAuto
        // end-of-script drain itself (auto_drain) so it stays inside the
        // request's watchdog bracket and honours TerminateExecution. The
        // :auto/:explicit distinction lives in auto_microtasks (read by
        // auto_drain).
        isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);
        // JS import() routes here; rejects unless a dynamic_import_resolver is set.
        isolate.set_host_import_module_dynamically_callback(dynamic_import_cb);
        // Fills import.meta.url (the module's compile-time filename) on first access.
        isolate.set_host_initialize_import_meta_object_callback(import_meta_cb);
        // Unhandled-rejection notifications route to the JS recorder, if one was
        // registered via NS.setPromiseRejectHandler (no-op otherwise).
        isolate.set_promise_reject_callback(promise_reject_cb);
        let handle = isolate.thread_safe_handle();
        // Per-isolate state lives in the isolate's embedder slot (reached anywhere
        // via istate!(scope)). Seed it; keep a clone of the watchdog Arc so
        // dispose/Drop (which have no scope) can stop + join the thread.
        let state = IsolateState::new(host_namespace, !explicit_microtasks);
        let watchdog = Arc::clone(&state.watchdog);
        isolate.set_slot(state);
        let watchdog_join = {
            let shared = Arc::clone(&watchdog);
            let handle = isolate.thread_safe_handle();
            std::thread::spawn(move || watchdog_loop(shared, handle))
        };
        // Boot the main realm (id 0) into the slot. new_realm reads the host
        // namespace from the slot (seeded above).
        {
            v8::scope!(let scope, &mut isolate);
            let main_context = new_realm(scope, 0);
            istate!(scope).realms.main_context = Some(main_context);
        }
        // Box the OwnedIsolate so it has a STABLE address, then capture a raw ptr
        // INTO the box (a `&mut Isolate` is `&mut NonNull<RealIsolate>`, pointing
        // at the cxx_isolate field — so it must not move). Moving the Box into the
        // registry moves only the 8-byte pointer; the boxed OwnedIsolate stays put.
        let mut boxed = Box::new(isolate);
        let iso_ptr = IsoPtr(&mut **boxed as *mut v8::Isolate);
        // Arm the heap-limit callback now that iso_ptr is stable: the callback's
        // data IS this ptr (it reads the slot's oom_fired and terminates through
        // it), and Core::run resets the ceiling through the same ptr on recovery.
        // Registered unconditionally — with memory_limit it guards that ceiling,
        // without one it guards V8's default ceiling (catchable, not a process abort).
        boxed.add_near_heap_limit_callback(near_heap_limit_cb, iso_ptr.0 as *mut c_void);
        let iso_id = NEXT_ISOLATE_ID.fetch_add(1, Ordering::SeqCst);
        isolates().lock().unwrap().insert(iso_id, SendIso(boxed));
        // Root the owner Thread VALUE so its address can't be reused while this
        // isolate lives (see RootedThread); the raw VALUE backs the fast per-op
        // owner check.
        use magnus::rb_sys::{AsRawValue, FromRawValue};
        let owner_thread = unsafe { Value::from_raw(rb_sys::rb_thread_current()) };
        let core = Arc::new_cyclic(|me| Core {
            me: me.clone(),
            shared: Mutex::new(Shared { handle, disposed: false }),
            iso_id,
            owner: owner_thread.as_raw() as usize,
            _owner_root: RootedThread(BoxValue::new(owner_thread)),
            iso_ptr,
            scan_start_field: std::sync::atomic::AtomicUsize::new(0),
            depth: std::sync::atomic::AtomicU32::new(0),
            procs: Mutex::new(ProcTable::default()),
            default_timeout_ms: timeout_ms,
            memory_limit,
            dynamic_import_resolver: Mutex::new(None),
            watchdog,
            watchdog_join: Mutex::new(Some(watchdog_join)),
        });
        // Wire the slot's back-pointer now that the Arc exists, so a V8 callback
        // (host fn / module resolver), which holds only a scope, can reach Core.
        // Pure slot access — no V8 handles — so reach it through the raw ptr.
        istate!(unsafe { &mut *core.iso_ptr.0 }).core_ptr = Arc::as_ptr(&core);
        // Discover V8's conservative-GC-scan stack_start field once now, while the
        // isolate is still ENTERED (so Heap/Stack are reachable and Enter has set
        // the field to the native top, which the discovery verifies against). 0 if
        // it can't be confirmed — the fiber scan-start override then stays off.
        {
            let real_isolate = unsafe { *(core.iso_ptr.0 as *const *mut c_void) };
            core.scan_start_field
                .store(discover_scan_start_field(real_isolate), Ordering::Relaxed);
        }
        // v8::Isolate::new ENTERED the isolate (and the boot above needed it).
        // EXIT it now: with several isolates created/disposed on one thread in
        // any order, keeping each entered for life would break V8's LIFO
        // enter/exit stack (an out-of-order drop aborts). Instead each op enters
        // around its run (Core::run) and teardown re-enters just before drop.
        unsafe { (*core.iso_ptr.0).exit() };
        Ok(Isolate { core })
    }
}

impl Core {
    // Run ONE op in-thread on the owner isolate and return its terminal reply.
    // Opens a scope on the isolate — a fresh HandleScope at the top level, or,
    // when re-entered from inside a host callback (depth > 0), a CallbackScope
    // onto the ambient one — and runs `service_request` with the GVL RELEASED,
    // so other Ruby threads proceed and a host callback can reacquire the GVL
    // (with_gvl) to run its proc. The reply is service_request's return value
    // (no channel). Asserts the owner thread (a foreign-thread use would SEGV).
    // Refuse an op that must not touch the isolate: from a foreign Ruby thread
    // (M:N moves a Ruby thread across native threads, so we bind by the RUBY
    // thread, not a native ThreadId — a different Ruby thread means concurrent
    // use of a !Locker isolate) or after disposal. Callers that reach the
    // isolate (slot or scope) MUST pass this first.
    fn ensure_owner_and_live(&self, ruby: &Ruby) -> Result<(), Error> {
        if current_ruby_thread() != self.owner {
            return Err(Error::new(
                err_class(ruby, "WrongThreadError"),
                "isolate used from a thread other than the one that created it \
                 (an isolate is thread-confined; only #terminate is thread-safe)",
            ));
        }
        if self.shared.lock().unwrap().disposed {
            return Err(Error::new(ruby.exception_runtime_error(), "disposed context"));
        }
        Ok(())
    }

    fn run(&self, ruby: &Ruby, request: Request) -> Result<VmReply, Error> {
        self.ensure_owner_and_live(ruby)?;
        let iso = self.iso_ptr.0;
        let depth = self.depth.fetch_add(1, Ordering::SeqCst);
        // EVERYTHING that touches V8 — enter, the scope, the JS run, the scope
        // drop, exit — happens inside ONE without_gvl, hence on ONE native thread
        // with no GVL boundary in between: M:N could otherwise migrate us to a
        // different native thread on GVL re-acquire, and V8's enter/exit and
        // HandleScope are native-thread-bound. Between ops the isolate is exited,
        // so the next op (possibly on another native thread) just re-enters.
        //
        // service_request runs under catch_unwind: a Rust panic (a bug — a bad
        // unwrap, an OOM in marshalling) would otherwise unwind through the
        // without_gvl extern "C" trampoline and ABORT the whole Ruby VM. Catching
        // it here keeps enter/exit + depth balanced (None below) and lets run()
        // poison the isolate and raise instead of crashing the process.
        use std::panic::AssertUnwindSafe;
        let reply: Option<VmReply> = without_gvl(|| {
            if depth == 0 {
                unsafe { (*iso).enter() };
                // In-thread: V8 runs on THIS native thread's stack. Re-point its
                // stack limit at this thread's bottom before running JS — the
                // create-time limit is fixed at a shallow (or other-thread)
                // frame, so a deeper entry would false-overflow (and the bad
                // throw trips V8's IsOnCentralStack CHECK -> fatal). iso_ptr is
                // *mut v8::Isolate = *mut NonNull<RealIsolate>; the raw
                // v8::Isolate* the C++ method wants is that NonNull's value.
                let real_isolate = unsafe { *(iso as *const *mut c_void) };
                // A live address ABOVE every V8 frame of this op (the scope and
                // service_request below run in deeper frames). On a fiber it
                // becomes V8's conservative-GC-scan stack_start so the scan stays
                // within the live, mapped stack — see set_v8_stack_limit.
                let stack_top_marker = 0u8;
                // Word-align (down): V8 CHECKs the scan start is pointer-aligned.
                // Down stays above all V8 frames (they're a full frame below).
                let stack_top = (&stack_top_marker as *const u8 as usize) & !(size_of::<usize>() - 1);
                set_v8_stack_limit(
                    real_isolate,
                    self.scan_start_field.load(Ordering::Relaxed),
                    stack_top,
                );
                let mut reply = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    v8::scope!(let scope, unsafe { &mut *iso });
                    service_request(scope, request, true)
                }))
                .ok();
                // OOM recovery. The near-heap-limit callback bumped the ceiling and
                // terminated the op so it could unwind; the scope is closed and JS has
                // stopped now. Reclaim the runaway allocation (a forced GC) and reset
                // the ceiling so the limit keeps protecting later ops, then relabel the
                // terminate as OutOfMemory. watchdog_fired stays false for an OOM, so
                // the request's end-sweep left the terminate flag set — cancel it here.
                if std::mem::take(&mut istate!(unsafe { &mut *iso }).oom_fired) {
                    let iso_ref = unsafe { &mut *iso };
                    // The ceiling to restore: the configured memory_limit when one was
                    // set, otherwise V8's default ceiling, which the callback captured
                    // in oom_initial_limit (we don't otherwise know its value).
                    let restore_to = if self.memory_limit > 0 {
                        self.memory_limit
                    } else {
                        istate!(iso_ref).oom_initial_limit
                    };
                    // Reclaim the runaway allocation, then reset the ceiling from the
                    // doubled bump back to restore_to (V8 clamps it no lower than the
                    // live heap — a genuinely-retained set above the limit necessarily
                    // loosens it, inherent to recovering the isolate rather than
                    // discarding it), and re-arm the callback for the next op.
                    iso_ref.low_memory_notification();
                    iso_ref.remove_near_heap_limit_callback(near_heap_limit_cb, restore_to);
                    iso_ref.add_near_heap_limit_callback(near_heap_limit_cb, iso as *mut c_void);
                    // Clear the terminate the OOM set (the request end-sweep skips it —
                    // that only sweeps watchdog_fired). Do this AFTER the GC: the forced
                    // GC above runs with the callback still armed, so a still-huge live
                    // set can re-fire it mid-GC, re-setting both terminate and oom_fired;
                    // clearing both here keeps either from leaking into the next op.
                    iso_ref.cancel_terminate_execution();
                    istate!(iso_ref).oom_fired = false;
                    reply = reply.map(relabel_oom);
                }
                unsafe { (*iso).exit() };
                reply
            } else {
                // Re-entrant (a host callback, having reacquired the GVL to run a
                // proc that issued this op, is on the V8 stack): the isolate is
                // already entered by the depth-0 op on THIS native thread, so
                // bootstrap onto the ambient HandleScope rather than re-enter.
                // The stack limit + scan-start set at depth 0 are NOT re-pointed
                // here: reentry runs in DEEPER frames of the SAME stack, so the
                // depth-0 values still bound it correctly. The one exception is a
                // host callback that SWITCHES stacks — e.g. resumes a Ruby Fiber
                // that itself evals — where the depth-0 (native) settings are
                // stale for the fiber; that nested-fiber-under-callback case is an
                // unsupported edge (the realistic fiber path is a depth-0 eval).
                std::panic::catch_unwind(AssertUnwindSafe(|| {
                    v8::callback_scope!(unsafe scope, unsafe { &mut *iso });
                    service_request(scope, request, false)
                }))
                .ok()
            }
        });
        self.depth.fetch_sub(1, Ordering::SeqCst);
        match reply {
            Some(reply) => Ok(reply),
            None => {
                // The op panicked: V8 may be left inconsistent, so POISON the
                // isolate (every later op refuses) rather than risk using it.
                self.shared.lock().unwrap().disposed = true;
                Err(Error::new(
                    ruby.exception_runtime_error(),
                    "internal error: operation panicked; the isolate has been disposed",
                ))
            }
        }
    }

    // Map a terminal reply to a Ruby value (the common eval/call/run shape).
    fn reply_value(ruby: &Ruby, reply: VmReply) -> Result<Value, Error> {
        match reply {
            VmReply::Done(Ok(val)) => jsval_to_ruby(ruby, &val),
            VmReply::Done(Err(e)) => Err(vm_err(ruby, e)),
            _ => Err(Error::new(
                ruby.exception_runtime_error(),
                "internal: unexpected reply kind",
            )),
        }
    }

    fn call_proc(
        &self,
        ruby: &Ruby,
        host_fn_id: usize,
        args: &[JsVal],
    ) -> Result<JsVal, String> {
        let proc = {
            let procs = self.procs.lock().unwrap();
            procs
                .slots
                .get(host_fn_id)
                .and_then(|slot| slot.proc.as_ref())
                .ok_or("unknown host function")?
                .get()
        };
        // Marshal into a Ruby Array, NOT a Vec<Value>: bare Values in a heap Vec
        // are hidden from Ruby's GC mark phase (magnus's own RArray::to_vec doc
        // spells this out). With several args, once arg N is parked in the Vec
        // while arg N+1's marshalling allocates — jsval_to_ruby builds Strings/
        // Arrays/Hashes — a GC there sweeps the still-referenced arg N, and the
        // proc then runs with a dangling VALUE that corrupts the heap and crashes
        // later (seen as a rare host-callback SEGV with an all-libruby C trace).
        // An RArray held as a live local keeps every element marked throughout.
        let ruby_args = ruby.ary_new_capa(args.len());
        for v in args {
            ruby_args
                .push(jsval_to_ruby(ruby, v).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
        }
        // SAFETY: ruby_args is a live local (so GC keeps it and its elements) and
        // is not mutated while the slice is borrowed — as_slice's contract. A VM
        // op the proc issues re-enters Core::run (depth > 0) directly — no nested
        // frame bookkeeping is needed any more (the call stack IS the nesting).
        let result: Result<Value, Error> = proc.call(unsafe { ruby_args.as_slice() });
        let value = result.map_err(|e| e.to_string())?;
        ruby_to_jsval(value).map_err(|e| e.to_string())
    }

    // Context#call (and call_void). Resolves a dotted function path
    // on globalThis and invokes it via v8::Function::call. |void| skips
    // marshalling the return for fire-and-forget calls.
    fn call(&self, ruby: &Ruby, context_id: i32, args: &[Value], void: bool) -> Result<Value, Error> {
        let Some((name, call_args)) = args.split_first() else {
            return Err(Error::new(
                ruby.exception_arg_error(),
                "call requires a function name",
            ));
        };
        let name = String::try_convert(*name)?;
        let jsargs: Vec<JsVal> = call_args
            .iter()
            .map(|v| ruby_to_jsval(*v))
            .collect::<Result<_, _>>()?;

        let reply = self.run(ruby, Request::Call {
            context_id,
            name,
            args: jsargs,
            void,
            timeout_ms: self.default_timeout_ms,
        })?;
        Self::reply_value(ruby, reply)
    }

    fn drain_microtasks(&self, ruby: &Ruby) -> Result<Value, Error> {
        let reply = self.run(ruby, Request::DrainMicrotasks {
            timeout_ms: self.default_timeout_ms,
        })?;
        Self::reply_value(ruby, reply)
    }

    fn eval_t(
        &self,
        ruby: &Ruby,
        context_id: i32,
        source: String,
        filename: String,
        timeout_ms: u64,
    ) -> Result<Value, Error> {
        let reply = self.run(ruby, Request::Eval {
            context_id,
            source,
            filename,
            timeout_ms,
        })?;
        Self::reply_value(ruby, reply)
    }

    fn attach(&self, ruby: &Ruby, context_id: i32, name: String, proc: Proc) -> Result<Value, Error> {
        let host_fn_id = self.procs.lock().unwrap().alloc(ProcSlot {
            context_id,
            proc: Some(RootedProc(BoxValue::new(proc))),
        });
        let reply = self.run(ruby, Request::Attach {
            context_id,
            name,
            host_fn_id,
            timeout_ms: self.default_timeout_ms,
        })?;
        Self::reply_value(ruby, reply)
    }

    // attach_many: install several host fns in ONE round-trip to the V8 thread
    // (a fresh realm needs ~dozens; one rendezvous instead of one per fn). Slots
    // are allocated up front so each carries a stable host_fn_id; entries are
    // applied IN ORDER and a build/attach failure aborts the batch with that
    // (name-tagged) error WITHOUT rolling back earlier entries (see the
    // AttachMany arm). On that error path the unused slots are reclaimed at the
    // next reset/dispose of the realm, like single attach.
    fn attach_many(&self, ruby: &Ruby, context_id: i32, entries: Vec<(String, Proc)>) -> Result<Value, Error> {
        if entries.is_empty() {
            return Ok(ruby.qnil().as_value()); // nothing to install, skip the round-trip
        }
        let named_ids: Vec<(String, usize)> = {
            let mut procs = self.procs.lock().unwrap();
            entries
                .into_iter()
                .map(|(name, proc)| {
                    let id = procs.alloc(ProcSlot {
                        context_id,
                        proc: Some(RootedProc(BoxValue::new(proc))),
                    });
                    (name, id)
                })
                .collect()
        };
        let reply = self.run(ruby, Request::AttachMany {
            context_id,
            entries: named_ids,
            timeout_ms: self.default_timeout_ms,
        })?;
        Self::reply_value(ruby, reply)
    }

    // Release the GC roots of the procs attached into |context_id| — its
    // realm is gone (reset or disposed), so the V8-side functions that
    // referenced them are unreachable. Runs on a Ruby thread (a RootedProc
    // drop unregisters its GC address). The slots stay: host_fn_ids of other
    // realms are indices into the same Vec.
    fn release_context_procs(&self, context_id: i32) {
        self.procs.lock().unwrap().release(context_id);
    }

    fn reset(&self, ruby: &Ruby, context_id: i32) -> Result<Value, Error> {
        let reply = self.run(ruby, Request::Reset { context_id })?;
        let out = Self::reply_value(ruby, reply)?;
        // Only on success — a refused reset (unknown/suspended realm) keeps
        // its attached fns callable.
        self.release_context_procs(context_id);
        Ok(out)
    }

    // Build a new context; returns its id (replied as an Int).
    fn create_context(&self, ruby: &Ruby) -> Result<i32, Error> {
        let reply = self.run(ruby, Request::CreateContext)?;
        let id = Self::reply_value(ruby, reply)?;
        i32::try_convert(id)
    }

    fn dispose_context(&self, ruby: &Ruby, context_id: i32) -> Result<(), Error> {
        let reply = self.run(ruby, Request::DisposeContext { context_id })?;
        Self::reply_value(ruby, reply)?;
        self.release_context_procs(context_id);
        Ok(())
    }

    // Thin ESM primitives. compile_module returns the new module's id.
    #[allow(clippy::too_many_arguments)]
    fn compile_module(
        &self,
        ruby: &Ruby,
        context_id: i32,
        source: String,
        filename: String,
        cached_data: Option<Vec<u8>>,
        produce_cache: bool,
        eager: bool,
    ) -> Result<Compiled, Error> {
        let reply = self.run(ruby, Request::CompileModule {
            context_id,
            source,
            filename,
            cached_data,
            produce_cache,
            eager,
        })?;
        match reply {
            VmReply::ModuleCompiled(Ok(cm)) => Ok(cm),
            VmReply::ModuleCompiled(Err(e)) => Err(vm_err(ruby, e)),
            _ => Err(Error::new(
                ruby.exception_runtime_error(),
                "internal: unexpected compile reply",
            )),
        }
    }

    // Atomically swap the slot's parked instantiate resolve block + stashed
    // resolver error, returning the previous pair. instantiate_module SAVES the
    // outer pair and RESTORES it afterwards, so a re-entrant instantiate (issued
    // from inside a resolve block) can't clobber the outer op's parked resolver.
    // Pure slot access (no V8 handles), reached through the raw isolate ptr — so
    // the caller MUST have passed ensure_owner_and_live first (iso_ptr would
    // otherwise dangle after dispose).
    #[allow(clippy::type_complexity)]
    fn swap_instantiate(
        &self,
        resolve: Option<RootedProc>,
        err: Option<BoxValue<Exception>>,
    ) -> (Option<RootedProc>, Option<BoxValue<Exception>>) {
        let st = istate!(unsafe { &mut *self.iso_ptr.0 });
        (
            std::mem::replace(&mut st.instantiate_resolve, resolve),
            std::mem::replace(&mut st.instantiate_resolve_err, err),
        )
    }

    // instantiate parks the resolve block in the slot so resolve_imported can ask
    // it per import edge (it may compile a dependency lazily — a re-entrant op
    // that just recurses into run). A resolver that RAISED is re-raised here with
    // its original class.
    fn instantiate_module(&self, ruby: &Ruby, module_id: i32, resolve: Proc) -> Result<Value, Error> {
        // Guard BEFORE touching the slot via iso_ptr: a foreign-thread or
        // post-dispose call must be refused, not deref a freed/foreign isolate.
        self.ensure_owner_and_live(ruby)?;
        // Park ours, saving the outer op's pair to restore after (re-entrant
        // instantiate safety).
        let (saved_resolve, saved_err) =
            self.swap_instantiate(Some(RootedProc(BoxValue::new(resolve))), None);
        let reply = self.run(ruby, Request::InstantiateModule { module_id });
        // Reclaim THIS op's resolver error and restore the outer op's pair.
        let (_, resolver_err) = self.swap_instantiate(saved_resolve, saved_err);
        if let Some(exc) = resolver_err {
            return Err(Error::from(*exc));
        }
        Self::reply_value(ruby, reply?)
    }

    fn evaluate_module(&self, ruby: &Ruby, module_id: i32) -> Result<Value, Error> {
        let reply = self.run(ruby, Request::EvaluateModule {
            module_id,
            timeout_ms: self.default_timeout_ms,
        })?;
        Self::reply_value(ruby, reply)
    }

    fn module_namespace(&self, ruby: &Ruby, module_id: i32) -> Result<Value, Error> {
        let reply = self.run(ruby, Request::ModuleNamespace { module_id })?;
        Self::reply_value(ruby, reply)
    }

    fn module_status(&self, ruby: &Ruby, module_id: i32) -> Result<Value, Error> {
        let reply = self.run(ruby, Request::ModuleStatus { module_id })?;
        Self::reply_value(ruby, reply)
    }

    fn dispose_module(&self, ruby: &Ruby, module_id: i32) -> Result<(), Error> {
        let reply = self.run(ruby, Request::DisposeModule { module_id })?;
        Self::reply_value(ruby, reply).map(|_| ())
    }

    // Classic script: compile, run, dispose.
    #[allow(clippy::too_many_arguments)]
    fn compile_script(
        &self,
        ruby: &Ruby,
        context_id: i32,
        source: String,
        filename: String,
        cached_data: Option<Vec<u8>>,
        produce_cache: bool,
        eager: bool,
    ) -> Result<Compiled, Error> {
        let reply = self.run(ruby, Request::CompileScript {
            context_id,
            source,
            filename,
            cached_data,
            produce_cache,
            eager,
        })?;
        match reply {
            VmReply::ScriptCompiled(Ok(cs)) => Ok(cs),
            VmReply::ScriptCompiled(Err(e)) => Err(vm_err(ruby, e)),
            _ => Err(Error::new(
                ruby.exception_runtime_error(),
                "internal: unexpected compile reply",
            )),
        }
    }

    fn run_script(&self, ruby: &Ruby, script_id: i32) -> Result<Value, Error> {
        let reply = self.run(ruby, Request::RunScript {
            script_id,
            timeout_ms: self.default_timeout_ms,
        })?;
        Self::reply_value(ruby, reply)
    }

    fn dispose_script(&self, ruby: &Ruby, script_id: i32) -> Result<(), Error> {
        let reply = self.run(ruby, Request::DisposeScript { script_id })?;
        Self::reply_value(ruby, reply).map(|_| ())
    }

    // Serialize a fresh bytecode cache from a compiled handle's current state
    // (Script#/Module#create_code_cache). None = V8 couldn't produce one (or the
    // realm is gone).
    fn script_code_cache(&self, ruby: &Ruby, script_id: i32) -> Result<Option<Vec<u8>>, Error> {
        let reply = self.run(ruby, Request::ScriptCodeCache { script_id })?;
        code_cache_from_reply(ruby, reply)
    }

    fn module_code_cache(&self, ruby: &Ruby, module_id: i32) -> Result<Option<Vec<u8>>, Error> {
        let reply = self.run(ruby, Request::ModuleCodeCache { module_id })?;
        code_cache_from_reply(ruby, reply)
    }

    fn set_dynamic_import_resolver(&self, proc: Proc) {
        // The old RootedProc (if any) drops here, unregistering its address —
        // we are on a Ruby thread, so that's GVL-safe.
        *self.dynamic_import_resolver.lock().unwrap() = Some(RootedProc(BoxValue::new(proc)));
    }

    // Terminate whatever is running. IsolateHandle is Send + refcounted —
    // safe at ANY time, even racing disposal (audit #63 without a stop_mtx).
    fn terminate(&self) {
        let shared = self.shared.lock().unwrap();
        shared.handle.terminate_execution();
    }

    fn is_disposed(&self) -> bool {
        self.shared.lock().unwrap().disposed
    }

    // Owner-thread isolate teardown: stop + join the watchdog FIRST (so no late
    // TerminateExecution can land on the isolate while we clear and drop it),
    // then enter the isolate, release GC roots + the slot's Globals (every
    // v8::Global must die before the isolate, and dropping one needs the isolate
    // entered), then drop the OwnedIsolate (which disposes V8). Caller has set
    // `disposed`.
    fn teardown(&self) {
        // Stop + join the watchdog before we touch the isolate, so its handle
        // can't fire a terminate into an isolate we're mid-disposing.
        self.watchdog.request_shutdown();
        if let Some(join) = self.watchdog_join.lock().unwrap().take() {
            let _ = join.join();
        }
        // ENTER the isolate so it is the current one: dropping v8::Globals needs
        // it entered, and OwnedIsolate's Drop asserts `self == GetCurrent()`
        // (then exits). Between ops the isolate is exited, so we must enter here.
        unsafe { (*self.iso_ptr.0).enter() };
        {
            let mut procs = self.procs.lock().unwrap();
            procs.slots.clear();
            procs.free.clear();
        }
        *self.dynamic_import_resolver.lock().unwrap() = None;
        {
            let st = istate!(unsafe { &mut *self.iso_ptr.0 });
            st.realms = V8State::default();
            st.modules = ModuleReg::default();
            st.scripts = ScriptReg::default();
            st.instantiate_resolve = None;
        }
        // Unregister the near-heap-limit callback before disposal: dropping the box
        // runs V8's teardown GC, which could otherwise re-invoke near_heap_limit_cb
        // (touching the just-reset slot of an isolate being destroyed). The watchdog
        // is already stopped above; this closes the matching space-axis hole.
        // Registered unconditionally (default protection), so always remove it.
        unsafe { &mut *self.iso_ptr.0 }.remove_near_heap_limit_callback(near_heap_limit_cb, 0);
        // Remove (and drop) the OwnedIsolate — V8 disposal runs here — AFTER the
        // watchdog joined and the Globals were cleared, while the isolate is
        // entered (above). Drop outside the lock so V8 teardown can't deadlock on
        // the registry.
        let removed = isolates().lock().unwrap().remove(&self.iso_id);
        drop(removed);
    }

    fn dispose(&self, ruby: &Ruby) -> Result<(), Error> {
        if current_ruby_thread() != self.owner {
            return Err(Error::new(
                err_class(ruby, "WrongThreadError"),
                "dispose must run on the isolate's owner thread",
            ));
        }
        {
            let mut shared = self.shared.lock().unwrap();
            if shared.disposed {
                return Ok(());
            }
            // A dispose racing a live op on this isolate (depth > 0 = a host
            // callback is on the V8 stack) would tear the isolate down mid-run
            // and SEGV — refuse it, leaving the isolate usable.
            if self.depth.load(Ordering::SeqCst) != 0 {
                return Err(Error::new(
                    ruby.exception_runtime_error(),
                    "RustyRacer: cannot dispose an isolate from within a running op or host callback",
                ));
            }
            shared.disposed = true;
        }
        self.teardown();
        Ok(())
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        // Explicit dispose already tore the isolate down — nothing to do.
        if self.shared.lock().unwrap().disposed {
            return;
        }
        if current_ruby_thread() == self.owner {
            // Last wrapper dropped on the owner thread: full teardown. depth is 0
            // (a running op holds a wrapper alive, so the last drop can't race
            // one), and Drop can't raise — so just tear down.
            self.shared.lock().unwrap().disposed = true;
            self.teardown();
        } else {
            // Foreign-thread GC drop: a thread-bound isolate CANNOT be disposed
            // off its owner thread (that would SEGV) and Drop CANNOT raise — so
            // LEAK the OwnedIsolate (it stays in the owner thread's ISOLATES until
            // the process exits) and only signal the watchdog to stop. Disposing
            // explicitly on the owner thread before the last wrapper drops avoids
            // this leak; the counter makes it observable (RustyRacer.leaked_isolate_count).
            LEAKED_ISOLATES.fetch_add(1, Ordering::Relaxed);
            self.watchdog.request_shutdown();
        }
    }
}

// RustyRacer.live_isolate_count -> Integer: isolates currently in the registry
// (created, not yet disposed). RustyRacer.leaked_isolate_count -> Integer:
// isolates that could not be disposed because their last wrapper was dropped off
// the owner thread (see Drop) — a workload that churns owner threads should keep
// this at 0 by disposing on the owner thread.
fn live_isolate_count() -> usize {
    isolates().lock().unwrap().len()
}

fn leaked_isolate_count() -> usize {
    LEAKED_ISOLATES.load(Ordering::Relaxed)
}

// Thin magnus-method wrappers.
// Isolate = the VM and its isolate-level operations; it hands out Contexts.
impl Isolate {
    // The default context (id 0), which lives for the isolate's lifetime.
    fn context(rb_self: &Self) -> Context {
        Context {
            core: rb_self.core.clone(),
            id: 0,
            disposed: AtomicBool::new(false),
        }
    }
    // A fresh v8::Context (id >= 1) — a realm sharing this isolate's heap.
    fn create_context(ruby: &Ruby, rb_self: &Self) -> Result<Context, Error> {
        let id = rb_self.core.create_context(ruby)?;
        Ok(Context {
            core: rb_self.core.clone(),
            id,
            disposed: AtomicBool::new(false),
        })
    }
    // Terminate whatever JS is running (safe from any thread; idle = no-op).
    fn terminate(&self) {
        self.core.terminate();
    }
    fn perform_microtask_checkpoint(ruby: &Ruby, rb_self: &Self) -> Result<Value, Error> {
        rb_self.core.drain_microtasks(ruby)
    }
    // dynamic_import_resolver = ->(specifier, referrer_url) { module } for import().
    fn set_dynamic_import_resolver(rb_self: &Self, proc: Proc) {
        rb_self.core.set_dynamic_import_resolver(proc);
    }
    fn dispose(ruby: &Ruby, rb_self: &Self) -> Result<(), Error> {
        rb_self.core.dispose(ruby)
    }
    fn disposed(&self) -> bool {
        self.core.is_disposed()
    }
}

// Context = a v8::Context (realm): eval/call/attach/compile_module run here.
impl Context {
    // Stable id within the isolate (0 = the default context). Lets an embedder
    // track which realm a Context is.
    fn id(&self) -> i32 {
        self.id
    }
    fn check_live(&self, ruby: &Ruby) -> Result<(), Error> {
        // id 0's lifetime is the isolate's; extras also track their own dispose.
        if self.disposed.load(Ordering::SeqCst) || self.core.is_disposed() {
            return Err(Error::new(ruby.exception_runtime_error(), "disposed context"));
        }
        Ok(())
    }
    // timeout_ms 0 = use the isolate's default; an explicit value overrides it.
    fn eval(
        ruby: &Ruby,
        rb_self: &Self,
        source: String,
        timeout_ms: u64,
        filename: String,
    ) -> Result<Value, Error> {
        rb_self.check_live(ruby)?;
        let timeout = if timeout_ms == 0 {
            rb_self.core.default_timeout_ms
        } else {
            timeout_ms
        };
        rb_self.core.eval_t(ruby, rb_self.id, source, filename, timeout)
    }
    fn call(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<Value, Error> {
        rb_self.check_live(ruby)?;
        rb_self.core.call(ruby, rb_self.id, args, false)
    }
    fn call_void(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<Value, Error> {
        rb_self.check_live(ruby)?;
        rb_self.core.call(ruby, rb_self.id, args, true)
    }
    fn attach(ruby: &Ruby, rb_self: &Self, name: String, proc: Proc) -> Result<Value, Error> {
        rb_self.check_live(ruby)?;
        rb_self.core.attach(ruby, rb_self.id, name, proc)
    }
    // attach_many({ "name" => proc, ... }): install every host fn in one
    // round-trip to the V8 thread (vs one per attach). Applied in the hash's
    // insertion order; if one name fails, the names before it stay attached (not
    // transactional). Keys must be Strings and values Procs.
    fn attach_many(ruby: &Ruby, rb_self: &Self, table: RHash) -> Result<Value, Error> {
        rb_self.check_live(ruby)?;
        let mut entries: Vec<(String, Proc)> = Vec::new();
        table.foreach(|name: String, proc: Proc| {
            entries.push((name, proc));
            Ok(magnus::r_hash::ForEach::Continue)
        })?;
        rb_self.core.attach_many(ruby, rb_self.id, entries)
    }
    // Swap this context's globals for a fresh realm (csim's per-visit reset).
    fn reset(ruby: &Ruby, rb_self: &Self) -> Result<Value, Error> {
        rb_self.check_live(ruby)?;
        rb_self.core.reset(ruby, rb_self.id)
    }
    fn compile_module(
        ruby: &Ruby,
        rb_self: &Self,
        source: String,
        filename: String,
        cached_data: Option<magnus::RString>,
        produce_cache: bool,
        eager: bool,
    ) -> Result<JsModule, Error> {
        rb_self.check_live(ruby)?;
        let cache_in = binary_bytes(ruby, cached_data)?;
        let cm = rb_self
            .core
            .compile_module(ruby, rb_self.id, source, filename, cache_in, produce_cache, eager)?;
        Ok(JsModule {
            core: rb_self.core.clone(),
            module_id: cm.id,
            disposed: AtomicBool::new(false),
            cached_data: cm.cached_data,
            cache_rejected: cm.cache_rejected,
        })
    }
    // compile(source, filename:, cached_data:, produce_cache:, eager:) -> Script:
    // a classic <script>. Same cache semantics as compile_module.
    fn compile(
        ruby: &Ruby,
        rb_self: &Self,
        source: String,
        filename: String,
        cached_data: Option<magnus::RString>,
        produce_cache: bool,
        eager: bool,
    ) -> Result<Script, Error> {
        rb_self.check_live(ruby)?;
        let cache_in = binary_bytes(ruby, cached_data)?;
        let cs = rb_self
            .core
            .compile_script(ruby, rb_self.id, source, filename, cache_in, produce_cache, eager)?;
        Ok(Script {
            core: rb_self.core.clone(),
            script_id: cs.id,
            disposed: AtomicBool::new(false),
            cached_data: cs.cached_data,
            cache_rejected: cs.cache_rejected,
        })
    }
    fn dispose(ruby: &Ruby, rb_self: &Self) -> Result<(), Error> {
        // The default context (id 0) lives with the isolate — dispose the
        // Isolate to tear it down; disposing the handle is a no-op.
        if rb_self.id == 0 || rb_self.disposed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        // Best-effort: if the isolate was disposed first, the context went with
        // it, so a failed DisposeContext send is success — dispose stays quiet.
        let _ = rb_self.core.dispose_context(ruby, rb_self.id);
        Ok(())
    }
    fn disposed(&self) -> bool {
        self.disposed.load(Ordering::SeqCst) || self.core.is_disposed()
    }
}

impl Snapshot {
    // Snapshot.new(code = "") — run code into a fresh blob.
    fn new(args: &[Value]) -> Result<Snapshot, Error> {
        let ruby = Ruby::get().unwrap();
        let code = match args.first() {
            Some(v) if !v.is_nil() => String::try_convert(*v)?,
            _ => String::new(),
        };
        let blob = build_snapshot(&code, None, false)
            .map_err(|m| Error::new(err_class(&ruby, "SnapshotError"), m))?;
        Ok(Snapshot {
            blob: RefCell::new(blob),
        })
    }

    // Snapshot.load(blob) — rewrap raw bytes. Runs V8's own StartupData::is_valid
    // up front so the COMMON bad blob — garbage, empty, or (most realistically)
    // one built for a different V8 version after a gem/V8 upgrade — raises a
    // rescuable SnapshotError here instead of tripping a FATAL CHECK that aborts
    // the whole process at the first Isolate.new(snapshot:). NB: is_valid checks
    // the version/structure, not a full checksum (V8 exposes no checksum verify),
    // so a blob truncated AFTER an intact header can still slip through — pair
    // this with a content hash (e.g. a SHA sidecar) if full integrity matters.
    fn load(ruby: &Ruby, blob: magnus::RString) -> Result<Snapshot, Error> {
        // Safe: the slice is copied into an owned Vec before any Ruby code
        // (which could move/free the string) can run.
        let bytes = unsafe { blob.as_slice() }.to_vec();
        init_v8(); // is_valid() needs V8 initialized; idempotent.
        let data = v8::StartupData::from(bytes);
        if data.is_empty() || !data.is_valid() {
            return Err(Error::new(
                err_class(ruby, "SnapshotError"),
                "invalid V8 snapshot blob (corrupt, truncated, or built for a different V8 version)",
            ));
        }
        Ok(Snapshot {
            blob: RefCell::new(data.to_vec()),
        })
    }

    // warmup!(code) — re-snapshot the existing blob with |code| run in a
    // throwaway context, so its functions get pre-compiled into the blob's
    // code cache WITHOUT baking the run's heap state (V8's
    // WarmUpSnapshotDataBlob contract). Spike: returns nil (csim returns self).
    fn warmup(ruby: &Ruby, rb_self: &Self, code: String) -> Result<(), Error> {
        let base = rb_self.blob.borrow().clone();
        let blob = build_snapshot(&code, Some(base), true)
            .map_err(|m| Error::new(err_class(ruby, "SnapshotError"), m))?;
        *rb_self.blob.borrow_mut() = blob;
        Ok(())
    }

    fn dump(ruby: &Ruby, rb_self: &Self) -> Value {
        ruby.str_from_slice(&rb_self.blob.borrow()).as_value()
    }

    fn size(&self) -> usize {
        self.blob.borrow().len()
    }
}

impl JsModule {
    fn check_live(&self, ruby: &Ruby) -> Result<(), Error> {
        // Also refuse once the ISOLATE is disposed: the module's own flag stays
        // false, but the isolate (and the slot instantiate touches via iso_ptr
        // before run's guard) is gone — without this, instantiate after
        // iso.dispose is a use-after-free.
        if self.disposed.load(Ordering::SeqCst) || self.core.is_disposed() {
            return Err(Error::new(ruby.exception_runtime_error(), "disposed module"));
        }
        Ok(())
    }
    // _instantiate(resolver): resolver is the Ruby block (passed as a Proc by
    // the lib wrapper). resolver.(specifier, referrer_url) must return a Module.
    fn instantiate(ruby: &Ruby, rb_self: &Self, resolver: Proc) -> Result<Value, Error> {
        rb_self.check_live(ruby)?;
        rb_self.core.instantiate_module(ruby, rb_self.module_id, resolver)
    }
    fn evaluate(ruby: &Ruby, rb_self: &Self) -> Result<Value, Error> {
        rb_self.check_live(ruby)?;
        rb_self.core.evaluate_module(ruby, rb_self.module_id)
    }
    fn namespace(ruby: &Ruby, rb_self: &Self) -> Result<Value, Error> {
        rb_self.check_live(ruby)?;
        rb_self.core.module_namespace(ruby, rb_self.module_id)
    }
    // The V8 module status name ("uninstantiated", ...); the lib wrapper
    // exposes it as Module#status, a Symbol.
    fn status(ruby: &Ruby, rb_self: &Self) -> Result<Value, Error> {
        rb_self.check_live(ruby)?;
        rb_self.core.module_status(ruby, rb_self.module_id)
    }
    // The bytecode cache produced at compile (produce_cache: true), as a binary
    // String, or nil. Persist it cross-process and pass back via cached_data:.
    fn cached_data(ruby: &Ruby, rb_self: &Self) -> Value {
        code_cache_value(ruby, rb_self.cached_data.as_ref())
    }
    // True if a cached_data: supplied at compile was stale/incompatible and V8
    // recompiled from source instead.
    fn cache_rejected(&self) -> bool {
        self.cache_rejected
    }
    // Serialize a bytecode cache from the module's CURRENT compile state, as a
    // binary String (or nil if V8 can't). Called AFTER #evaluate it captures the
    // inner functions V8 compiled while running — the warm-cache the compile-time
    // produce_cache: can't include (see Script#create_code_cache).
    fn create_code_cache(ruby: &Ruby, rb_self: &Self) -> Result<Value, Error> {
        rb_self.check_live(ruby)?;
        let bytes = rb_self.core.module_code_cache(ruby, rb_self.module_id)?;
        Ok(code_cache_value(ruby, bytes.as_ref()))
    }
    fn dispose(ruby: &Ruby, rb_self: &Self) -> Result<(), Error> {
        if rb_self.disposed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let _ = rb_self.core.dispose_module(ruby, rb_self.module_id);
        Ok(())
    }
    fn disposed(&self) -> bool {
        self.disposed.load(Ordering::SeqCst)
    }
}

impl Script {
    fn check_live(&self, ruby: &Ruby) -> Result<(), Error> {
        // Also refuse once the isolate is disposed (see JsModule::check_live).
        if self.disposed.load(Ordering::SeqCst) || self.core.is_disposed() {
            return Err(Error::new(ruby.exception_runtime_error(), "disposed script"));
        }
        Ok(())
    }
    // Run the (already-compiled) script and return its completion value. A
    // thrown exception is a RuntimeError; a timeout/stop a ScriptTerminatedError.
    fn run(ruby: &Ruby, rb_self: &Self) -> Result<Value, Error> {
        rb_self.check_live(ruby)?;
        rb_self.core.run_script(ruby, rb_self.script_id)
    }
    fn cached_data(ruby: &Ruby, rb_self: &Self) -> Value {
        code_cache_value(ruby, rb_self.cached_data.as_ref())
    }
    fn cache_rejected(&self) -> bool {
        self.cache_rejected
    }
    // Serialize a bytecode cache from the script's CURRENT compile state, as a
    // binary String (or nil if V8 can't). Unlike compile(produce_cache: true) —
    // which caches only the top level, since V8 compiles inner functions lazily —
    // calling this AFTER #run captures the inner functions that actually ran, the
    // same warm-cache a browser keeps. Persist it and feed it back via cached_data:.
    fn create_code_cache(ruby: &Ruby, rb_self: &Self) -> Result<Value, Error> {
        rb_self.check_live(ruby)?;
        let bytes = rb_self.core.script_code_cache(ruby, rb_self.script_id)?;
        Ok(code_cache_value(ruby, bytes.as_ref()))
    }
    fn dispose(ruby: &Ruby, rb_self: &Self) -> Result<(), Error> {
        if rb_self.disposed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let _ = rb_self.core.dispose_script(ruby, rb_self.script_id);
        Ok(())
    }
    fn disposed(&self) -> bool {
        self.disposed.load(Ordering::SeqCst)
    }
}

// A bytecode cache as a binary (ASCII-8BIT) Ruby String, or nil when there's
// none — the shared return shape of #cached_data and #create_code_cache.
fn code_cache_value(ruby: &Ruby, bytes: Option<&Vec<u8>>) -> Value {
    match bytes {
        Some(b) => ruby.str_from_slice(b).as_value(),
        None => ruby.qnil().as_value(),
    }
}

// Map a ScriptCodeCache/ModuleCodeCache reply to its serialized bytes.
fn code_cache_from_reply(ruby: &Ruby, reply: VmReply) -> Result<Option<Vec<u8>>, Error> {
    match reply {
        VmReply::CodeCache(Ok(bytes)) => Ok(bytes),
        VmReply::CodeCache(Err(e)) => Err(vm_err(ruby, e)),
        _ => Err(Error::new(
            ruby.exception_runtime_error(),
            "internal: unexpected code-cache reply",
        )),
    }
}

// Read a Ruby cached_data arg as raw bytes, refusing a non-binary string so a
// cache file read without 'rb' (silently transcoded) fails loudly rather than
// being consumed as garbage and rejected with no signal.
fn binary_bytes(ruby: &Ruby, cached_data: Option<magnus::RString>) -> Result<Option<Vec<u8>>, Error> {
    match cached_data {
        None => Ok(None),
        Some(s) => {
            let enc: String = s
                .funcall::<_, _, Value>("encoding", ())?
                .funcall("to_s", ())?;
            if enc != "ASCII-8BIT" {
                let cls = ruby
                    .class_object()
                    .const_get::<_, ExceptionClass>("EncodingError")?;
                return Err(Error::new(
                    cls,
                    format!("cached_data must be ASCII-8BIT (binary), got {enc}"),
                ));
            }
            Ok(Some(unsafe { s.as_slice() }.to_vec()))
        }
    }
}

fn vm_err(ruby: &Ruby, e: VmError) -> Error {
    match e {
        VmError::Parse(m) => Error::new(err_class(ruby, "ParseError"), m),
        VmError::Runtime(m) => Error::new(err_class(ruby, "RuntimeError"), m),
        VmError::JsError { message, backtrace } => js_runtime_error(ruby, message, backtrace),
        VmError::Terminated => Error::new(
            err_class(ruby, "ScriptTerminatedError"),
            "JavaScript was terminated (timeout or stop)",
        ),
        VmError::OutOfMemory => Error::new(
            err_class(ruby, "V8OutOfMemoryError"),
            "JavaScript exceeded the isolate memory_limit",
        ),
    }
}

// Build a RustyRacer::RuntimeError carrying the JS stack as its Ruby backtrace.
// Constructs the exception instance so we can set_backtrace before raising;
// falls back to a plain Error if any of that fails.
fn js_runtime_error(ruby: &Ruby, message: String, backtrace: Vec<String>) -> Error {
    let class = err_class(ruby, "RuntimeError");
    let exc: Value = match class.funcall("new", (message.as_str(),)) {
        Ok(v) => v,
        Err(e) => return e,
    };
    // Always set it (even to []) so an empty/absent JS stack doesn't let Ruby
    // backfill the backtrace with host-side (magnus) frames.
    let _ = exc.funcall::<_, _, Value>("set_backtrace", (backtrace,));
    match magnus::Exception::from_value(exc) {
        Some(e) => Error::from(e),
        None => Error::new(class, message),
    }
}

// instantiate's resolve block returns a RustyRacer::Module (or nil for a
// genuinely-unresolved import); pull its module_id so the V8 thread can look up
// the V8 module. Verifies the module belongs to THIS Context (core identity),
// since module ids are per-V8-thread and a foreign id would alias a local one.
// A raised block propagates as Err (not silently swallowed into "not found").
fn resolve_module_via_ruby(
    core: &Core,
    resolve: Proc,
    specifier: &str,
    referrer_url: &str,
    // Some(realm id) for the dynamic_import_resolver: it then receives the
    // initiating realm as a 3rd Context arg so it can resolve per-realm. None for
    // the static instantiate block, whose (specifier, referrer_url) contract is
    // unchanged.
    initiating_context: Option<i32>,
) -> Result<Option<i32>, Error> {
    let ruby = Ruby::get().unwrap();
    let ret: Value = match core.me.upgrade().zip(initiating_context) {
        Some((core_arc, id)) => {
            let ctx = Context {
                core: core_arc,
                id,
                disposed: AtomicBool::new(false),
            };
            resolve.call((specifier, referrer_url, ctx))?
        }
        None => resolve.call((specifier, referrer_url))?,
    };
    if ret.is_nil() {
        return Ok(None); // legitimately unresolved
    }
    let obj = magnus::typed_data::Obj::<JsModule>::try_convert(ret).map_err(|_| {
        Error::new(
            ruby.exception_type_error(),
            "module resolver must return a RustyRacer::Module or nil",
        )
    })?;
    if !std::ptr::eq(Arc::as_ptr(&obj.core), core as *const Core) {
        return Err(Error::new(
            ruby.exception_runtime_error(),
            "module resolver returned a Module from a different Context",
        ));
    }
    if obj.disposed.load(Ordering::SeqCst) {
        return Ok(None);
    }
    Ok(Some(obj.module_id))
}

#[magnus::init]
fn init(ruby: &Ruby) -> Result<(), Error> {
    let module = ruby.define_module("RustyRacer")?;

    // The isolate (VM) + its isolate-level ops; hands out Contexts.
    let isolate = module.define_class("Isolate", ruby.class_object())?;
    // keyword-arg wrapper Isolate.new(snapshot:, ...) lives in lib/rusty_racer.rb
    isolate.define_singleton_method("_new", function!(Isolate::new, 5))?;
    isolate.define_method("context", method!(Isolate::context, 0))?;
    isolate.define_method("create_context", method!(Isolate::create_context, 0))?;
    isolate.define_method("terminate", method!(Isolate::terminate, 0))?;
    isolate.define_method(
        "perform_microtask_checkpoint",
        method!(Isolate::perform_microtask_checkpoint, 0),
    )?;
    // lib keeps the proc in an ivar (GC liveness) and calls this primitive.
    isolate.define_method(
        "_set_dynamic_import_resolver",
        method!(Isolate::set_dynamic_import_resolver, 1),
    )?;
    isolate.define_method("dispose", method!(Isolate::dispose, 0))?;
    isolate.define_method("disposed?", method!(Isolate::disposed, 0))?;

    // A v8::Context (realm): eval/call/attach/compile_module.
    let context = module.define_class("Context", ruby.class_object())?;
    // keyword-arg wrapper Context#eval(source, timeout_ms:, filename:) in lib.
    context.define_method("_eval", method!(Context::eval, 3))?;
    context.define_method("call", method!(Context::call, -1))?;
    context.define_method("call_void", method!(Context::call_void, -1))?;
    context.define_method("attach", method!(Context::attach, 2))?;
    context.define_method("attach_many", method!(Context::attach_many, 1))?;
    context.define_method("reset", method!(Context::reset, 0))?;
    context.define_method("id", method!(Context::id, 0))?;
    // keyword-arg wrappers Context#compile_module / #compile (source, ...) in lib.
    context.define_method("_compile_module", method!(Context::compile_module, 5))?;
    context.define_method("_compile", method!(Context::compile, 5))?;
    context.define_method("dispose", method!(Context::dispose, 0))?;
    context.define_method("disposed?", method!(Context::disposed, 0))?;

    // Classic compiled script: Context#compile -> #run / #cached_data.
    let script = module.define_class("Script", ruby.class_object())?;
    script.define_method("run", method!(Script::run, 0))?;
    script.define_method("cached_data", method!(Script::cached_data, 0))?;
    script.define_method("cache_rejected?", method!(Script::cache_rejected, 0))?;
    script.define_method("create_code_cache", method!(Script::create_code_cache, 0))?;
    script.define_method("dispose", method!(Script::dispose, 0))?;
    script.define_method("disposed?", method!(Script::disposed, 0))?;

    // V8 startup blob: Snapshot.new(code) -> Isolate.new(snapshot:).
    let snapshot = module.define_class("Snapshot", ruby.class_object())?;
    snapshot.define_singleton_method("new", function!(Snapshot::new, -1))?;
    snapshot.define_singleton_method("load", function!(Snapshot::load, 1))?;
    snapshot.define_method("warmup!", method!(Snapshot::warmup, 1))?;
    snapshot.define_method("dump", method!(Snapshot::dump, 0))?;
    snapshot.define_method("size", method!(Snapshot::size, 0))?;

    // Thin ES-module handle: Context#compile_module -> instantiate/evaluate.
    let jsmodule = module.define_class("Module", ruby.class_object())?;
    jsmodule.define_method("_instantiate", method!(JsModule::instantiate, 1))?;
    jsmodule.define_method("evaluate", method!(JsModule::evaluate, 0))?;
    jsmodule.define_method("namespace", method!(JsModule::namespace, 0))?;
    jsmodule.define_method("_status", method!(JsModule::status, 0))?;
    jsmodule.define_method("cached_data", method!(JsModule::cached_data, 0))?;
    jsmodule.define_method("cache_rejected?", method!(JsModule::cache_rejected, 0))?;
    jsmodule.define_method("create_code_cache", method!(JsModule::create_code_cache, 0))?;
    jsmodule.define_method("dispose", method!(JsModule::dispose, 0))?;
    jsmodule.define_method("disposed?", method!(JsModule::disposed, 0))?;

    let platform = module.define_module("Platform")?;
    platform.define_singleton_method("set_flags!", function!(platform_set_flags, -1))?;

    // Version tag for keying cross-process bytecode caches; changes when the V8
    // version/flags change so a stale cache can be discarded (avoids SEGV).
    module.define_singleton_method(
        "cached_data_version_tag",
        function!(cached_data_version_tag, 0),
    )?;
    // Observability for the thread-confined lifecycle (see Drop for Core).
    module.define_singleton_method("live_isolate_count", function!(live_isolate_count, 0))?;
    module.define_singleton_method("leaked_isolate_count", function!(leaked_isolate_count, 0))?;
    Ok(())
}
