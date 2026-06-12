// Stage 2: the Ruby half. A Magnus extension with a dedicated V8 thread and a
// CHANNEL rendezvous — the same architecture as the C extension, minus the
// hand-rolled condvar protocol where csim's hang-class audit bugs live.
//
// Why this shape (and not inline-on-the-Ruby-thread): rusty_v8 v150 makes
// OwnedIsolate deliberately !Send and binds no v8::Locker, and Magnus requires
// wrapped data to be Send because Ruby objects migrate threads. The type
// system therefore REJECTS the unsound shortcut and forces the dedicated
// thread — the same conclusion mini_racer's C++ reached, arrived at by the
// compiler instead of by debugging.
//
// What the channels buy over the C condvar protocol, bug-for-bug:
//   - every request carries its OWN reply Sender (a oneshot in spirit), so
//     audit #12's "single cond_signal, multiple waiter classes" hang is
//     unrepresentable — there is no shared wakeup to misroute;
//   - dispose drops the request Receiver, so a late eval's send() returns Err
//     and raises cleanly — audit #13/#26's "wait predicate ignores quit"
//     blocked-forever state is unrepresentable;
//   - Context#stop uses IsolateHandle (Send + refcounted), so audit #63's
//     stop-vs-teardown use-after-free needs no stop_mtx: the handle is safe
//     to fire at any time, including after disposal;
//   - a Ruby exception in a host proc is a magnus Err return (no longjmp
//     through foreign frames), answered over the channel — audit #24's
//     "exception wedges the context forever" path is a clean error reply;
//   - the watchdog joins before the reply, and the OUTERMOST request cancels
//     if one fired, so audit #3's stale TerminateExecution cannot poison the
//     next request — while a nested request's cancel can never erase a
//     termination aimed at the suspended outer JS (the flag is isolate-global).
//
// Attached procs and the dynamic-import resolver are GC-rooted via
// rb_gc_register_address (see RootedProc): marked, so the extension may hold
// the only reference, and pinned, so GC.compact cannot move them behind the
// extension's back. Remaining spike simplification: the GVL-released channel
// waits pass no unblock function (a parked op ignores Thread#kill).

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, Once, Weak};
use std::time::{Duration, Instant};

use magnus::block::Proc;
use magnus::value::{BoxValue, ReprValue};
use magnus::{
    function, method, prelude::*, Error, ExceptionClass, IntoValue, RArray, RHash, RString, Ruby,
    TryConvert, Value,
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

// ---------------------------------------------------------------------------
// Values crossing threads: plain Rust data. No Ruby allocation off the Ruby
// thread, no V8 handles off the V8 thread, no wire format. Replaces serde.c.
// ---------------------------------------------------------------------------
#[derive(Debug, Clone)]
enum JsVal {
    Undefined,
    Null,
    Bool(bool),
    Int(i64),
    Num(f64),
    Str(String),
    // Binary bytes: a JS Uint8Array / ArrayBuffer (view) <-> a Ruby ASCII-8BIT
    // (binary-tagged) String. The encoding tag IS the type declaration, so the
    // round-trip is symmetric and faithful (Uint8Array -> binary String ->
    // Uint8Array), like BigInt/Date/Map/Set — no lossy text coercion. |id| (when
    // Some) registers it in the Ref table so a binary blob aliased in a graph
    // keeps ONE identity instead of being duplicated; None = not identity-tracked
    // (e.g. a to_str result).
    Bytes { id: Option<u32>, bytes: Vec<u8> },
    // Arbitrary-precision integer (JS BigInt <-> Ruby Integer). Carried as V8's
    // word representation: sign + little-endian u64 limbs. Both ends speak this
    // natively (V8 BigInt words; Ruby Integer via a hex string), so no value is
    // truncated — unlike routing a big int through f64.
    BigInt { negative: bool, words: Vec<u64> },
    // JS Date <-> Ruby Time, carried as milliseconds since the Unix epoch
    // (v8::Date::value_of's unit). mini_racer marshals Date to Time.
    Date(f64),
    // Containers carry a serialization id so shared/cyclic graphs survive the
    // round-trip: the first time an object is seen it is emitted with its id,
    // and any later occurrence (a sibling sharing it, or a cycle back to an
    // ancestor) is emitted as Ref(id) instead of being re-expanded.
    Array { id: u32, items: Vec<JsVal> },
    // JS object / Ruby Hash with string keys. Insertion order preserved.
    Obj { id: u32, entries: Vec<(String, JsVal)> },
    // JS Map <-> Ruby Hash. Keys are arbitrary values (not just strings), so
    // this is distinct from Obj. Insertion order preserved.
    Map { id: u32, pairs: Vec<(JsVal, JsVal)> },
    // JS Set <-> Ruby Set (stdlib).
    Set { id: u32, items: Vec<JsVal> },
    // Back-reference to an already-emitted container (preserves identity; makes
    // cycles representable instead of truncating at a depth cap).
    Ref(u32),
}

// Cycles and sharing are handled by the Ref table (see JsVal::Ref), so this is
// purely a native-stack backstop against a pathologically deep (but acyclic)
// graph — set well above any realistic nesting.
const MAX_MARSHAL_DEPTH: u32 = 256;

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
    Terminated, // watchdog/stop -> RustyRacer::ScriptTerminatedError
}

// Ruby thread -> V8 thread. |context_id| selects which realm in the isolate the
// op runs in: 0 = the main realm (Context's own globalThis, swappable by
// reset_realm), N >= 1 = an extra realm made by create_context.
enum Request {
    Eval {
        context_id: i32,
        source: String,
        filename: String,
        timeout_ms: u64,
        reply: Sender<VmReply>,
    },
    // Resolve a dotted function path on globalThis and invoke it with marshalled
    // args (v8::Function::call), preserving the holder as `this`. Distinct from
    // Eval so args keep full type/identity fidelity instead of a JSON literal.
    Call {
        context_id: i32,
        name: String,
        args: Vec<JsVal>,
        // void = don't marshal the return (fire-and-forget): the called fn may
        // return a huge/cyclic JS object the caller never reads.
        void: bool,
        timeout_ms: u64,
        reply: Sender<VmReply>,
    },
    // Drain the isolate's microtask queue once (no auto event loop).
    DrainMicrotasks {
        timeout_ms: u64,
        reply: Sender<VmReply>,
    },
    Attach {
        context_id: i32,
        name: String,
        host_fn_id: usize,
        timeout_ms: u64,
        reply: Sender<VmReply>,
    },
    // Batch attach: install many (name, host_fn_id) host fns in one round-trip
    // (a fresh realm needs ~dozens). Same semantics as Attach, applied in order.
    AttachMany {
        context_id: i32,
        entries: Vec<(String, usize)>,
        timeout_ms: u64,
        reply: Sender<VmReply>,
    },
    // reset: swap globalThis for a fresh v8::Context, reusing the same warm
    // isolate — csim's per-visit reset. Applies to the named context.
    Reset {
        context_id: i32,
        reply: Sender<VmReply>,
    },
    // create_context: build a fresh, persistent v8::Context in the isolate and
    // return its id (the multi-realm model). DisposeContext frees one.
    CreateContext {
        reply: Sender<VmReply>,
    },
    DisposeContext {
        context_id: i32,
        reply: Sender<VmReply>,
    },
    // Thin ES-module primitives (V8's raw compile/instantiate/evaluate). The
    // embedder owns the url->Module registry and the resolve policy; the binding
    // just exposes the steps. A compiled module is addressed by an id (like a
    // realm) since V8 handles can't cross to the Ruby thread.
    CompileModule {
        // The context to compile the module in (modules are realm-bound).
        context_id: i32,
        source: String,
        filename: String,
        // Bytecode cache to consume (skip reparse); None compiles fresh.
        cached_data: Option<Vec<u8>>,
        // Produce a fresh bytecode cache to hand back (Module#cached_data).
        produce_cache: bool,
        // Eager-compile every function up front (CompileOptions::EagerCompile)
        // instead of V8's default lazy top-level-only compile. Ignored when
        // cached_data is set (V8 forbids ConsumeCodeCache + EagerCompile).
        eager: bool,
        reply: Sender<VmReply>,
    },
    // instantiate: V8 walks imports, calling back to the Ruby resolve block
    // (carried by pump) per edge via VmReply::ResolveModule.
    InstantiateModule {
        module_id: i32,
        reply: Sender<VmReply>,
    },
    EvaluateModule {
        module_id: i32,
        timeout_ms: u64,
        reply: Sender<VmReply>,
    },
    ModuleNamespace {
        module_id: i32,
        reply: Sender<VmReply>,
    },
    // The module's v8::Module::Status, as a lowercase name ("uninstantiated",
    // "instantiated", ...) the Ruby wrapper symbolizes.
    ModuleStatus {
        module_id: i32,
        reply: Sender<VmReply>,
    },
    DisposeModule {
        module_id: i32,
        reply: Sender<VmReply>,
    },
    // Classic <script> primitives (V8 ScriptCompiler::CompileUnboundScript): an
    // unbound script, compiled in a context, runnable repeatedly, with the same
    // bytecode-cache options as modules. Addressed by id like a module.
    CompileScript {
        context_id: i32,
        source: String,
        filename: String,
        cached_data: Option<Vec<u8>>,
        produce_cache: bool,
        eager: bool,
        reply: Sender<VmReply>,
    },
    // Bind the script to its context and run it; returns the completion value.
    RunScript {
        script_id: i32,
        timeout_ms: u64,
        reply: Sender<VmReply>,
    },
    DisposeScript {
        script_id: i32,
        reply: Sender<VmReply>,
    },
    // Serialize a bytecode cache from a compiled handle's CURRENT compile state
    // (Script#create_code_cache / Module#create_code_cache). Called after run/
    // evaluate, it captures the inner functions V8 lazily compiled while running
    // — the only way (as of V8-150) to get inner-function bytecode into a cache,
    // since create_code_cache at compile time only sees the top level.
    ScriptCodeCache {
        script_id: i32,
        reply: Sender<VmReply>,
    },
    ModuleCodeCache {
        module_id: i32,
        reply: Sender<VmReply>,
    },
    Dispose,
}

// compile_module result: the module's id plus any produced bytecode cache and
// whether a supplied cache was rejected.
struct Compiled {
    id: i32,
    cached_data: Option<Vec<u8>>,
    cache_rejected: bool,
}

// V8 thread -> the Ruby thread that is waiting on this request
enum VmReply {
    Done(Result<JsVal, VmError>),
    // compile_module / compile's richer reply (id + produced cache + rejected).
    ModuleCompiled(Result<Compiled, VmError>),
    ScriptCompiled(Result<Compiled, VmError>),
    // Script#/Module#create_code_cache: the serialized bytes, or None when V8
    // can't produce a cache (or the handle's realm is gone).
    CodeCache(Result<Option<Vec<u8>>, VmError>),
    // JS called host fn |id|; run the proc and send the answer back.
    Callback {
        host_fn_id: usize,
        args: Vec<JsVal>,
        answer: Sender<Answer>,
    },
    // instantiate's per-edge resolve: ask the Ruby resolve block for the module
    // that |specifier| (imported by |referrer_url|) refers to. |initiating_context|
    // is the realm being linked — passed to the dynamic_import_resolver (which
    // auto-links a dynamic import's static deps) so it can resolve per-realm;
    // the static instantiate block ignores it (its contract stays 2-arg).
    ResolveModule {
        specifier: String,
        referrer_url: String,
        initiating_context: i32,
        answer: Sender<Answer>,
    },
    // JS did import(specifier): ask the Context's dynamic_import_resolver for an
    // already-loaded module to fulfil the import() promise. |initiating_context|
    // is the realm import() actually fired in (handed to the resolver as a
    // Context so iframe imports resolve in the iframe's realm, not the main one).
    DynamicImport {
        specifier: String,
        referrer_url: String,
        initiating_context: i32,
        answer: Sender<Answer>,
    },
}

// Ruby thread -> the V8 thread suspended inside a callback / batch round-trip
enum Answer {
    Result(Result<JsVal, String>),
    // the proc's/resolver's Ruby body issued another VM op (eval, call,
    // compile, checkpoint, create_context, ... — ANY Request). The main queue
    // is not being read while the V8 thread awaits this answer, so the
    // suspended frame services it re-entrantly via the same dispatcher as the
    // main loop (service_request).
    Nested(Request),
    // the resolve block's answer: the dependency module's id (None = unresolved).
    ModuleId(Option<i32>),
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

// ---------------------------------------------------------------------------
// V8 thread
// ---------------------------------------------------------------------------
thread_local! {
    // Reply sender of the request currently being served (stack: nested
    // requests arriving through a suspended callback push their own sender).
    static REPLY_STACK: RefCell<Vec<Sender<VmReply>>> = const { RefCell::new(Vec::new()) };
}

// Little-endian u64 limbs -> big-endian hex magnitude (no sign, no "0x"). The
// shared currency between V8 BigInt words and Ruby Integer(str, 16).
fn words_to_hex(words: &[u64]) -> String {
    let mut hex = String::new();
    for w in words.iter().rev() {
        if hex.is_empty() {
            hex.push_str(&format!("{w:x}")); // top limb: no leading zeros
        } else {
            hex.push_str(&format!("{w:016x}")); // lower limbs: full width
        }
    }
    if hex.is_empty() {
        hex.push('0');
    }
    hex
}

// Big-endian hex magnitude -> little-endian u64 limbs (inverse of words_to_hex).
fn hex_to_words(hex: &str) -> Vec<u64> {
    let mut words = Vec::new();
    let mut end = hex.len();
    while end > 0 {
        let start = end.saturating_sub(16);
        words.push(u64::from_str_radix(&hex[start..end], 16).unwrap_or(0));
        end = start;
    }
    if words.is_empty() {
        words.push(0);
    }
    words
}

// Tracks objects already emitted this marshal so a re-encounter becomes a
// Ref instead of re-expansion. Buckets by V8 identity hash (which can collide),
// disambiguated by Local equality — the same trick the module registry uses.
#[derive(Default)]
struct JsSeen {
    next_id: u32,
    map: HashMap<i32, Vec<(v8::Global<v8::Object>, u32)>>,
}

// Decide how to emit a container object: Ok(id) = first sighting, register it
// and recurse; Err(jsval) = emit this directly and stop (a Ref to an already-
// seen object, or a truncated Str at the depth backstop). Centralising this in
// one place keeps the four container arms (array/object/map/set) in lockstep —
// and crucially orders the checks so a depth-truncated object is NEVER assigned
// an id (which would leave a sibling Ref dangling).
fn js_container_id(
    scope: &mut v8::PinScope<'_, '_>,
    seen: &mut JsSeen,
    value: v8::Local<v8::Value>,
    obj: v8::Local<v8::Object>,
    depth: u32,
) -> Result<u32, JsVal> {
    let hash = obj.get_identity_hash().get();
    if let Some(bucket) = seen.map.get(&hash) {
        for (g, id) in bucket {
            if v8::Local::new(scope, g) == obj {
                return Err(JsVal::Ref(*id));
            }
        }
    }
    // First sighting but too deep: truncate WITHOUT registering, so no later
    // Ref can target a container that was never emitted.
    if depth >= MAX_MARSHAL_DEPTH {
        return Err(JsVal::Str(value.to_rust_string_lossy(scope)));
    }
    let id = seen.next_id;
    seen.next_id += 1;
    let g = v8::Global::new(scope, obj);
    seen.map.entry(hash).or_default().push((g, id));
    Ok(id)
}

// Copy |len| bytes from a V8 (Shared)ArrayBuffer backing pointer into an owned
// Vec, with one allocation and no zero-fill (data is fully overwritten). |data|
// is None only for a zero-length buffer, where the empty Vec is already right.
fn copy_buffer_bytes(data: Option<std::ptr::NonNull<c_void>>, len: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(len);
    if let Some(p) = data {
        unsafe {
            std::ptr::copy_nonoverlapping(p.as_ptr() as *const u8, buf.as_mut_ptr(), len);
            buf.set_len(len);
        }
    }
    buf
}

fn js_to_jsval(scope: &mut v8::PinScope<'_, '_>, value: v8::Local<v8::Value>) -> JsVal {
    let mut seen = JsSeen::default();
    js_to_jsval_d(scope, value, &mut seen, 0)
}

fn js_to_jsval_d(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<v8::Value>,
    seen: &mut JsSeen,
    depth: u32,
) -> JsVal {
    if value.is_undefined() {
        return JsVal::Undefined;
    }
    if value.is_null() {
        return JsVal::Null;
    }
    if value.is_boolean() {
        return JsVal::Bool(value.boolean_value(scope));
    }
    if value.is_int32() {
        return JsVal::Int(value.integer_value(scope).unwrap_or(0));
    }
    if value.is_number() {
        return JsVal::Num(value.number_value(scope).unwrap_or(f64::NAN));
    }
    if value.is_big_int() {
        if let Ok(bi) = v8::Local::<v8::BigInt>::try_from(value) {
            let mut words = vec![0u64; bi.word_count()];
            let (negative, _) = bi.to_words_array(&mut words);
            return JsVal::BigInt { negative, words };
        }
    }
    // Date before the generic object branch (a Date *is* an object).
    if value.is_date() {
        if let Ok(date) = v8::Local::<v8::Date>::try_from(value) {
            return JsVal::Date(date.value_of());
        }
    }
    // Binary buffers before the generic object branch (they are objects too).
    // A TypedArray/DataView copies its VIEWED window; a bare ArrayBuffer or
    // SharedArrayBuffer copies the whole buffer. All become a Ruby binary
    // String. (Without the SharedArrayBuffer arm a bare SAB would fall through
    // to the plain-object branch and marshal as an empty Hash — silent loss.)
    if value.is_array_buffer_view() {
        if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(value) {
            let obj = v8::Local::<v8::Object>::try_from(value).unwrap();
            // depth 0: a buffer is a leaf (no recursion into children), so it
            // never risks native-stack overflow and must stay faithful bytes
            // even when deeply nested — only the identity (Ref) check applies,
            // never the depth-truncation-to-lossy-string the generic path uses.
            let id = match js_container_id(scope, seen, value, obj, 0) {
                Ok(id) => id,
                Err(jsval) => return jsval, // a Ref to the same buffer
            };
            let len = view.byte_length();
            let mut buf: Vec<u8> = Vec::with_capacity(len);
            // copy_contents_uninit writes into the UNINITIALIZED spare capacity
            // (a &mut [MaybeUninit<u8>]) — never forming a &mut [u8] over uninit
            // memory the way copy_contents would (that's UB). set_len to exactly
            // what it wrote so a detached/short view never exposes uninit bytes.
            let n = view.copy_contents_uninit(&mut buf.spare_capacity_mut()[..len]);
            unsafe { buf.set_len(n) };
            return JsVal::Bytes { id: Some(id), bytes: buf };
        }
    }
    if value.is_array_buffer() {
        if let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(value) {
            let obj = v8::Local::<v8::Object>::try_from(value).unwrap();
            // depth 0 — a buffer is a leaf; see the view arm above.
            let id = match js_container_id(scope, seen, value, obj, 0) {
                Ok(id) => id,
                Err(jsval) => return jsval,
            };
            return JsVal::Bytes {
                id: Some(id),
                bytes: copy_buffer_bytes(ab.data(), ab.byte_length()),
            };
        }
    }
    if value.is_shared_array_buffer() {
        if let Ok(sab) = v8::Local::<v8::SharedArrayBuffer>::try_from(value) {
            let obj = v8::Local::<v8::Object>::try_from(value).unwrap();
            // depth 0 — a buffer is a leaf; see the view arm above.
            let id = match js_container_id(scope, seen, value, obj, 0) {
                Ok(id) => id,
                Err(jsval) => return jsval,
            };
            let store = sab.get_backing_store();
            return JsVal::Bytes {
                id: Some(id),
                bytes: copy_buffer_bytes(store.data(), sab.byte_length()),
            };
        }
    }
    // Map/Set before the generic object branch (both are objects).
    if value.is_map() {
        let obj = v8::Local::<v8::Object>::try_from(value).unwrap();
        let id = match js_container_id(scope, seen, value, obj, depth) {
            Ok(id) => id,
            Err(jsval) => return jsval,
        };
        let map = v8::Local::<v8::Map>::try_from(value).unwrap();
        let arr = map.as_array(scope); // [k0, v0, k1, v1, ...]
        let mut pairs = Vec::with_capacity((arr.length() / 2) as usize);
        let mut i = 0;
        while i + 1 < arr.length() {
            let k = arr.get_index(scope, i).unwrap_or_else(|| v8::undefined(scope).into());
            let v = arr.get_index(scope, i + 1).unwrap_or_else(|| v8::undefined(scope).into());
            let kj = js_to_jsval_d(scope, k, seen, depth + 1);
            let vj = js_to_jsval_d(scope, v, seen, depth + 1);
            pairs.push((kj, vj));
            i += 2;
        }
        return JsVal::Map { id, pairs };
    }
    if value.is_set() {
        let obj = v8::Local::<v8::Object>::try_from(value).unwrap();
        let id = match js_container_id(scope, seen, value, obj, depth) {
            Ok(id) => id,
            Err(jsval) => return jsval,
        };
        let set = v8::Local::<v8::Set>::try_from(value).unwrap();
        let arr = set.as_array(scope);
        let mut items = Vec::with_capacity(arr.length() as usize);
        for i in 0..arr.length() {
            let el = arr.get_index(scope, i).unwrap_or_else(|| v8::undefined(scope).into());
            items.push(js_to_jsval_d(scope, el, seen, depth + 1));
        }
        return JsVal::Set { id, items };
    }
    if value.is_array() {
        let obj = v8::Local::<v8::Object>::try_from(value).unwrap();
        let id = match js_container_id(scope, seen, value, obj, depth) {
            Ok(id) => id,
            Err(jsval) => return jsval,
        };
        let arr = v8::Local::<v8::Array>::try_from(value).unwrap();
        let mut items = Vec::with_capacity(arr.length() as usize);
        for i in 0..arr.length() {
            let el = arr
                .get_index(scope, i)
                .unwrap_or_else(|| v8::undefined(scope).into());
            items.push(js_to_jsval_d(scope, el, seen, depth + 1));
        }
        return JsVal::Array { id, items };
    }
    // Plain object -> string-keyed Obj. Functions/Date/etc. fall through to
    // their toString (the spike's primitive escape hatch).
    if value.is_object() && !value.is_function() {
        let obj = v8::Local::<v8::Object>::try_from(value).unwrap();
        let id = match js_container_id(scope, seen, value, obj, depth) {
            Ok(id) => id,
            Err(jsval) => return jsval,
        };
        if let Some(names) = obj.get_own_property_names(scope, Default::default()) {
            let mut entries = Vec::with_capacity(names.length() as usize);
            for i in 0..names.length() {
                let Some(key) = names.get_index(scope, i) else {
                    continue;
                };
                let key_str = key.to_rust_string_lossy(scope);
                let val = obj
                    .get(scope, key)
                    .unwrap_or_else(|| v8::undefined(scope).into());
                entries.push((key_str, js_to_jsval_d(scope, val, seen, depth + 1)));
            }
            return JsVal::Obj { id, entries };
        }
    }
    JsVal::Str(value.to_rust_string_lossy(scope))
}

// Owned-by-value (not &JsVal): a JsVal::Bytes hands its Vec straight to V8's
// backing store with no copy of the payload, so a large binary blob crosses
// Ruby->JS with zero extra allocation.
fn jsval_to_js<'s>(scope: &mut v8::PinScope<'s, '_>, val: JsVal) -> v8::Local<'s, v8::Value> {
    let mut built: HashMap<u32, v8::Local<'s, v8::Value>> = HashMap::new();
    jsval_to_js_d(scope, val, &mut built)
}

fn jsval_to_js_d<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    val: JsVal,
    built: &mut HashMap<u32, v8::Local<'s, v8::Value>>,
) -> v8::Local<'s, v8::Value> {
    match val {
        JsVal::Undefined => v8::undefined(scope).into(),
        JsVal::Null => v8::null(scope).into(),
        JsVal::Bool(b) => v8::Boolean::new(scope, b).into(),
        JsVal::Int(i) => v8::Number::new(scope, i as f64).into(),
        JsVal::Num(n) => v8::Number::new(scope, n).into(),
        JsVal::Str(s) => v8::String::new(scope, &s)
            .map(|s| s.into())
            .unwrap_or_else(|| v8::undefined(scope).into()),
        // Bytes -> Uint8Array, moving the Vec into V8's backing store (no copy
        // of the payload). Registered under |id| so an aliased blob resolves to
        // the same Uint8Array via Ref.
        JsVal::Bytes { id, bytes } => {
            let len = bytes.len();
            let store = v8::ArrayBuffer::new_backing_store_from_vec(bytes).make_shared();
            let ab = v8::ArrayBuffer::with_backing_store(scope, &store);
            let arr: v8::Local<v8::Value> = v8::Uint8Array::new(scope, ab, 0, len)
                .map(|a| a.into())
                .unwrap_or_else(|| v8::undefined(scope).into());
            if let Some(id) = id {
                built.insert(id, arr);
            }
            arr
        }
        JsVal::BigInt { negative, words } => v8::BigInt::new_from_words(scope, negative, &words)
            .map(|b| b.into())
            .unwrap_or_else(|| v8::undefined(scope).into()),
        JsVal::Date(ms) => v8::Date::new(scope, ms)
            .map(|d| d.into())
            .unwrap_or_else(|| v8::undefined(scope).into()),
        // Register the container under its id BEFORE filling it, so a Ref from
        // a descendant (a cycle back to here) resolves to this same object.
        JsVal::Array { id, items } => {
            let arr = v8::Array::new(scope, items.len() as i32);
            built.insert(id, arr.into());
            for (i, it) in items.into_iter().enumerate() {
                let v = jsval_to_js_d(scope, it, built);
                arr.set_index(scope, i as u32, v);
            }
            arr.into()
        }
        JsVal::Obj { id, entries } => {
            let obj = v8::Object::new(scope);
            built.insert(id, obj.into());
            for (k, it) in entries {
                let Some(key) = v8::String::new(scope, &k) else {
                    continue;
                };
                let v = jsval_to_js_d(scope, it, built);
                obj.set(scope, key.into(), v);
            }
            obj.into()
        }
        JsVal::Map { id, pairs } => {
            let map = v8::Map::new(scope);
            built.insert(id, map.into());
            for (k, v) in pairs {
                let kk = jsval_to_js_d(scope, k, built);
                let vv = jsval_to_js_d(scope, v, built);
                map.set(scope, kk, vv);
            }
            map.into()
        }
        JsVal::Set { id, items } => {
            let set = v8::Set::new(scope);
            built.insert(id, set.into());
            for it in items {
                let v = jsval_to_js_d(scope, it, built);
                set.add(scope, v);
            }
            set.into()
        }
        JsVal::Ref(id) => built
            .get(&id)
            .copied()
            .unwrap_or_else(|| v8::undefined(scope).into()),
    }
}

// JS called a host function: round-trip to the Ruby thread that is waiting on
// the current request, servicing nested requests until the answer arrives.
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

    let reply = REPLY_STACK.with(|s| s.borrow().last().cloned());
    let Some(reply) = reply else { return };
    let (answer_tx, answer_rx) = channel::<Answer>();
    if reply
        .send(VmReply::Callback {
            host_fn_id,
            args: js_args,
            answer: answer_tx,
        })
        .is_err()
    {
        // The requesting Ruby thread is gone; fail the JS call cleanly.
        throw_js_error(scope, "host function caller went away");
        return;
    }

    loop {
        match answer_rx.recv() {
            Ok(Answer::Result(Ok(val))) => {
                let v = jsval_to_js(scope, val);
                rv.set(v);
                return;
            }
            Ok(Answer::Result(Err(message))) => {
                // The proc raised: surface as a JS exception (audit #24's
                // wedge becomes an ordinary throw).
                throw_js_error(scope, &message);
                return;
            }
            Ok(Answer::Nested(request)) => {
                // ruby -> js -> ruby -> VM op: the proc issued another request
                // while the main loop is parked in this very callback. Service
                // it right here with the main loop's dispatcher — ANY op, not
                // just eval/call, or the rendezvous deadlocks (the proc waits
                // on the V8 thread, which waits on the proc's answer).
                service_request(scope, request);
            }
            Ok(Answer::ModuleId(_)) => {
                // A module-resolve answer can't arrive on a host-fn channel.
                throw_js_error(scope, "unexpected module answer in host callback");
                return;
            }
            Err(_) => {
                throw_js_error(scope, "host function caller went away");
                return;
            }
        }
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

fn run_source(scope: &mut v8::PinScope<'_, '_>, source: &str, filename: &str) -> Result<JsVal, VmError> {
    v8::tc_scope!(let tc, scope);
    // Compile and run as distinct phases so a compile failure maps to
    // ParseError and a thrown exception to RuntimeError (csim rescues both).
    let Some(code) = v8::String::new(tc, source) else {
        return Err(VmError::Parse("source too large".into()));
    };
    let origin = script_origin(tc, filename);
    let script = match v8::Script::compile(tc, code, Some(&origin)) {
        Some(script) => script,
        None if tc.has_terminated() => return Err(VmError::Terminated),
        None => {
            let msg = tc
                .exception()
                .map(|e| e.to_rust_string_lossy(tc))
                .unwrap_or_else(|| "parse error".to_string());
            // Append the location V8 recorded; always name the file, add the
            // line when V8 reports one.
            let message = tc.message();
            let res = message
                .and_then(|m| m.get_script_resource_name(tc))
                .filter(|v| v.is_string())
                .map(|v| v.to_rust_string_lossy(tc))
                .unwrap_or_else(|| filename.to_string());
            let loc = match message.and_then(|m| m.get_line_number(tc)) {
                Some(line) => format!(" at {res}:{line}"),
                None => format!(" at {res}"),
            };
            return Err(VmError::Parse(format!("{msg}{loc}")));
        }
    };
    match script.run(tc) {
        Some(value) => Ok(js_to_jsval(tc, value)),
        None if tc.has_terminated() => Err(VmError::Terminated),
        None => {
            let exc = tc.exception();
            let stack = tc.stack_trace();
            Err(capture_js_error(tc, exc, stack))
        }
    }
}

// Resolve a dotted property path on globalThis to a function and invoke it via
// v8::Function::call, with the property's holder as `this` (so `a.b.f` gets the
// right receiver). Args/result marshal through the ref-preserving paths.
fn call_function(
    scope: &mut v8::PinScope<'_, '_>,
    name: &str,
    args: Vec<JsVal>,
    void: bool,
) -> Result<JsVal, VmError> {
    v8::tc_scope!(let tc, scope);
    let context = tc.get_current_context();
    let global = context.global(tc);
    let mut recv: v8::Local<v8::Value> = global.into();
    let mut target: v8::Local<v8::Value> = global.into();
    for part in name.split('.') {
        let Some(obj) = target.to_object(tc) else {
            // The holder of `part` (a preceding segment) was null/undefined, so
            // there's nothing to read `part` from — name the holder, not `part`.
            return Err(VmError::Runtime(format!(
                "`{name}`: cannot read `{part}` (a preceding path segment is not an object)"
            )));
        };
        let Some(key) = v8::String::new(tc, part) else {
            return Err(VmError::Runtime("property name too large".into()));
        };
        let Some(next) = obj.get(tc, key.into()) else {
            if tc.has_terminated() {
                return Err(VmError::Terminated);
            }
            let msg = tc
                .exception()
                .map(|e| e.to_rust_string_lossy(tc))
                .unwrap_or_else(|| format!("cannot read `{part}` of `{name}`"));
            return Err(VmError::Runtime(msg));
        };
        recv = target;
        target = next;
    }
    let Ok(func) = v8::Local::<v8::Function>::try_from(target) else {
        return Err(VmError::Runtime(format!("`{name}` is not a function")));
    };
    let argv: Vec<v8::Local<v8::Value>> = args.into_iter().map(|a| jsval_to_js(tc, a)).collect();
    match func.call(tc, recv, &argv) {
        // void: skip marshalling the return so a huge/cyclic result is never walked.
        Some(_) if void => Ok(JsVal::Undefined),
        Some(value) => Ok(js_to_jsval(tc, value)),
        None if tc.has_terminated() => Err(VmError::Terminated),
        None => {
            let exc = tc.exception();
            let stack = tc.stack_trace();
            Err(capture_js_error(tc, exc, stack))
        }
    }
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

// The (Source, CompileOptions) pair shared by the module and script compile
// handlers: consume a supplied bytecode cache (skip reparse), else eager-compile
// every function up front, else compile lazily (V8's default — only the top
// level). A supplied cache wins over `eager`: V8's CompileOptionsIsValid forbids
// ConsumeCodeCache + EagerCompile together, so `eager` is ignored on the consume
// path. (Source is an owned struct — V8 copies the origin in — so returning it
// across this fn boundary keeps the same handle-lifetime contract as inlining.)
fn compile_source<'s>(
    code: v8::Local<'s, v8::String>,
    origin: &v8::ScriptOrigin<'s>,
    cached_data: &Option<Vec<u8>>,
    eager: bool,
) -> (v8::script_compiler::Source, v8::script_compiler::CompileOptions) {
    use v8::script_compiler::{CompileOptions, Source};
    match cached_data {
        Some(bytes) => (
            Source::new_with_cached_data(code, Some(origin), v8::script_compiler::CachedData::new(bytes)),
            CompileOptions::ConsumeCodeCache,
        ),
        None if eager => (Source::new(code, Some(origin)), CompileOptions::EagerCompile),
        None => (Source::new(code, Some(origin)), CompileOptions::NoCompileOptions),
    }
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
#[allow(dead_code)]
struct IsolateState {
    realms: V8State,
    modules: ModuleReg,
    scripts: ScriptReg,
    // The host-fn registry — no longer a Mutex: one isolate is driven by one
    // (owning) thread, so there's no cross-thread contention to guard.
    procs: ProcTable,
    dynamic_import_resolver: Option<RootedProc>,
    active_realms: Vec<i32>,
    instantiating: bool,
    watchdog_fired: bool,
    auto_microtasks: bool,
    draining: bool,
    default_timeout_ms: u64,
    // Reentry-depth: 0 at a top-level op; >0 while a host callback is on the V8
    // stack (a nested op then bootstraps its scope via callback_scope! instead of
    // re-borrowing the OwnedIsolate, which is already borrowed by the outer run).
    depth: u32,
    watchdog: Arc<WatchdogShared>,
}

#[allow(dead_code)]
impl IsolateState {
    fn new(host_namespace: Option<String>, default_timeout_ms: u64, auto_microtasks: bool) -> Self {
        IsolateState {
            realms: V8State {
                host_namespace,
                next_context_id: 1,
                ..V8State::default()
            },
            modules: ModuleReg::default(),
            scripts: ScriptReg::default(),
            procs: ProcTable::default(),
            dynamic_import_resolver: None,
            active_realms: Vec::new(),
            instantiating: false,
            watchdog_fired: false,
            auto_microtasks,
            draining: false,
            default_timeout_ms,
            depth: 0,
            watchdog: Arc::new(WatchdogShared {
                inner: Mutex::new(WatchdogInner {
                    frames: Vec::new(),
                    next_generation: 0,
                    fired_generation: None,
                    shutdown: false,
                }),
                cv: Condvar::new(),
            }),
        }
    }
}

// The IsolateState parked in a scope's isolate slot (a macro, not a fn, so it
// works on any scope type — HandleScope/ContextScope/TryCatch all reach
// get_slot_mut via deref, which a generic fn can't express over the PinScope
// alias). Borrows the scope mutably, so use it in SHORT bursts, never held
// across a JS run (a re-entrant host callback must be able to borrow it again).
// Panics if absent — every isolate the binding makes installs one, so a miss is
// a bug.
macro_rules! istate {
    ($scope:expr) => {
        $scope
            .get_slot_mut::<IsolateState>()
            .expect("IsolateState missing from isolate slot")
    };
}

// The owning thread's live OwnedIsolates, keyed by id. This is the ONLY piece
// that must stay thread-local: OwnedIsolate is !Send and is needed by-&mut to
// open a top-level scope, so it can't live in the (Send) magnus wrapper. The
// wrapper holds just the id + owner ThreadId and asserts owner == current on
// every op, so a cross-thread use raises instead of corrupting V8 (see i-c).
thread_local! {
    #[allow(clippy::type_complexity)]
    static ISOLATES: RefCell<HashMap<u32, v8::OwnedIsolate>> = RefCell::new(HashMap::new());
}

#[allow(dead_code)]
static NEXT_ISOLATE_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

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

// The shared bracket every JS-running request (Eval/Call/Attach/RunScript/
// EvaluateModule) needs: push this request's reply onto REPLY_STACK (so a host
// fn it calls routes back here), arm the watchdog, run |body|, then on a
// watchdog timeout flag the leftover terminate for the outermost sweep and —
// only if |body| actually ran JS (the bool it returns) — override its outcome
// to Terminated. |body| owns its ContextScope, JS call, and auto_drain, and
// returns (ran_js, outcome); the realm-disposed/unknown paths return
// (false, Err(..)) so a raced watchdog can't poison an error for work that ran
// no JS. Collapsing the five arms onto this keeps the terminate discipline in
// ONE place.
fn run_js_bracketed(
    scope: &mut v8::PinScope<'_, '_, ()>,
    outermost: bool,
    timeout_ms: u64,
    reply: &Sender<VmReply>,
    body: impl FnOnce(&mut v8::PinScope<'_, '_, ()>, bool) -> (bool, Result<JsVal, VmError>),
) -> Result<JsVal, VmError> {
    REPLY_STACK.with(|s| s.borrow_mut().push(reply.clone()));
    let watchdog = arm_watchdog(scope, timeout_ms);
    let (ran_js, mut outcome) = body(scope, outermost);
    if disarm_watchdog(scope, watchdog) {
        istate!(scope).watchdog_fired = true;
        if ran_js {
            outcome = Err(VmError::Terminated);
        }
    }
    REPLY_STACK.with(|s| {
        s.borrow_mut().pop();
    });
    outcome
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

// Park until the Ruby resolver answers with a module id, servicing any nested
// requests that arrive meanwhile (the resolver may compile/evaluate lazily).
// None = unresolved, a protocol violation, or the caller went away. Shared by
// resolve_imported and dynamic_import_cb so the wait loops can't drift apart.
fn recv_module_id(scope: &mut v8::PinScope<'_, '_, ()>, arx: &Receiver<Answer>) -> Option<i32> {
    loop {
        match arx.recv() {
            Ok(Answer::ModuleId(id)) => return id,
            Ok(Answer::Nested(request)) => {
                service_request(scope, request);
            }
            _ => return None,
        }
    }
}

// V8 calls this per import edge during InstantiateModule. Maps the referrer to
// its url, round-trips to the Ruby resolve block (carried on REPLY_STACK), and
// returns the module the block named. Blocks the V8 thread on the answer, just
// like host_fn_callback.
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
    let reply = REPLY_STACK.with(|s| s.borrow().last().cloned())?;
    let (atx, arx) = channel();
    reply
        .send(VmReply::ResolveModule {
            specifier: spec,
            referrer_url: ref_url,
            initiating_context: here.unwrap_or(0),
            answer: atx,
        })
        .ok()?;
    let dep_id = recv_module_id(scope, &arx)?;
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
// the resolved module's namespace (or rejected). Round-trips to the Context's
// dynamic_import_resolver over the current request's reply channel (REPLY_STACK)
// — so import() only works inside an eval/call. The resolver may return a
// merely COMPILED module: per V8's host contract, link + evaluate happen here
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
    let reply = match REPLY_STACK.with(|s| s.borrow().last().cloned()) {
        Some(r) => r,
        None => {
            reject(scope, "import() is only available during eval/call");
            return Some(promise);
        }
    };
    let (atx, arx) = channel();
    if reply
        .send(VmReply::DynamicImport {
            specifier: spec,
            referrer_url: referrer,
            initiating_context: initiating,
            answer: atx,
        })
        .is_err()
    {
        reject(scope, "dynamic import caller went away");
        return Some(promise);
    }
    match recv_module_id(scope, &arx) {
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

// The watchdog runs on ONE persistent thread per isolate rather than a fresh
// std::thread per request: spawning + joining a thread on every op cost ~16µs
// (5.5x) when a timeout was set, dwarfing the actual work. The thread sleeps on
// a condvar until a deadline is armed, terminates execution once the deadline
// passes, then goes back to sleep.
struct WatchdogShared {
    inner: Mutex<WatchdogInner>,
    cv: Condvar,
}

// One armed request's deadline. `run_js_bracketed` is RE-ENTRANT — a host fn
// called from JS can issue a nested op that arms again while the outer op is
// still running — so the armed deadlines form a LIFO stack, not a single slot.
// (The old per-op design gave each op its own watchdog thread; collapsing onto
// one thread must not let a nested arm/disarm clobber the outer op's deadline,
// or the outer op would run unbounded after the nested call returns.)
#[derive(Clone, Copy)]
struct WatchdogFrame {
    generation: u64,
    deadline: Instant,
}

struct WatchdogInner {
    // Every currently-armed op (with timeout_ms > 0), pushed on arm and removed
    // on disarm. The loop honours the EARLIEST deadline across all frames: the
    // most urgent timeout fires first, and since TerminateExecution is
    // isolate-global it tears down whatever is running (escalating outward).
    frames: Vec<WatchdogFrame>,
    // Monotonic; each arm takes the next value as its frame's id.
    next_generation: u64,
    // The generation whose deadline the loop terminated on — consumed (and
    // cleared) by that op's disarm so it can map its outcome to Terminated.
    fired_generation: Option<u64>,
    // Set at isolate teardown to break the loop.
    shutdown: bool,
}

// The persistent watchdog loop. Runs off a Send IsolateHandle so it never
// borrows the isolate the V8 thread owns.
fn watchdog_loop(shared: Arc<WatchdogShared>, handle: v8::IsolateHandle) {
    let mut inner = shared.inner.lock().unwrap();
    loop {
        if inner.shutdown {
            return;
        }
        // The earliest deadline among all armed frames is the one to enforce.
        match inner.frames.iter().min_by_key(|f| f.deadline).copied() {
            // Idle: sleep until a frame is armed (or shutdown).
            None => inner = shared.cv.wait(inner).unwrap(),
            Some(frame) => {
                let now = Instant::now();
                if now >= frame.deadline {
                    handle.terminate_execution();
                    inner.fired_generation = Some(frame.generation);
                    // Drop the fired frame so the loop moves on to the next
                    // deadline instead of re-firing this one every wakeup.
                    inner.frames.retain(|f| f.generation != frame.generation);
                } else {
                    let (next, _) = shared.cv.wait_timeout(inner, frame.deadline - now).unwrap();
                    inner = next;
                }
            }
        }
    }
}

// (The watchdog Arc now lives in IsolateState; arm/disarm reach it via istate!.)

// Arm the watchdog for this request: push a frame with its own deadline and
// wake the loop. Returns the generation token to hand to `disarm_watchdog`
// (None when timeout_ms is 0 — no watchdog for this request).
fn arm_watchdog(scope: &mut v8::PinScope<'_, '_, ()>, timeout_ms: u64) -> Option<u64> {
    if timeout_ms == 0 {
        return None;
    }
    let shared = &istate!(scope).watchdog;
    let mut inner = shared.inner.lock().unwrap();
    inner.next_generation += 1;
    let generation = inner.next_generation;
    inner.frames.push(WatchdogFrame {
        generation,
        deadline: Instant::now() + Duration::from_millis(timeout_ms),
    });
    shared.cv.notify_one();
    Some(generation)
}

// Disarm: drop THIS request's frame (leaving any outer frame still armed) and
// report whether its deadline fired. On fire the caller maps the outcome to
// Terminated and the outermost frame sweeps the leftover terminate via
// WATCHDOG_FIRED; removing only this frame keeps a late terminate from
// poisoning the next request without clobbering a still-running outer op.
fn disarm_watchdog(scope: &mut v8::PinScope<'_, '_, ()>, generation: Option<u64>) -> bool {
    let Some(generation) = generation else {
        return false;
    };
    let shared = &istate!(scope).watchdog;
    let mut inner = shared.inner.lock().unwrap();
    inner.frames.retain(|f| f.generation != generation);
    let fired = inner.fired_generation == Some(generation);
    if fired {
        inner.fired_generation = None;
    }
    shared.cv.notify_one();
    fired
}

fn v8_thread_main(
    rx: Receiver<Request>,
    handle_tx: Sender<v8::IsolateHandle>,
    host_namespace: Option<String>,
    snapshot: Option<Vec<u8>>,
    explicit_microtasks: bool,
) {
    init_v8();
    // A snapshot blob bakes globalThis state into the isolate: the first
    // Context::new below then deserializes that default context for free.
    let create_params = match snapshot {
        Some(bytes) => v8::CreateParams::default().snapshot_blob(v8::StartupData::from(bytes)),
        None => Default::default(),
    };
    let mut isolate = v8::Isolate::new(create_params);
    // Always Explicit at the V8 level; the binding performs the kAuto
    // end-of-script drain itself (auto_drain), so the drain stays inside the
    // request's watchdog bracket and honours TerminateExecution. Relying on
    // V8's own kAuto would put the drain outside our bracket — and worse,
    // V8's auto-drain inside Function::Call does not stop on termination, so
    // a timed-out call that re-queues a microtask would spin forever. The
    // :auto/:explicit distinction lives in AUTO_MICROTASKS (read by
    // auto_drain): :auto drains at the outermost request, :explicit only on
    // perform_microtask_checkpoint / the host-namespace drainMicrotasks.
    isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);
    // JS import() routes here; rejects unless a dynamic_import_resolver is set.
    isolate.set_host_import_module_dynamically_callback(dynamic_import_cb);
    // Fills import.meta.url (the module's compile-time filename) on first access.
    isolate.set_host_initialize_import_meta_object_callback(import_meta_cb);
    // Unhandled-rejection notifications route to the JS recorder, if one was
    // registered via NS.setPromiseRejectHandler (no-op otherwise).
    isolate.set_promise_reject_callback(promise_reject_cb);
    let _ = handle_tx.send(isolate.thread_safe_handle());
    // All per-isolate state now lives in the isolate's slot (reached anywhere via
    // istate!(scope)). IsolateState::new seeds the host namespace + next realm id
    // and creates the watchdog; install it, then spawn the one persistent
    // watchdog thread (requests arm/disarm it — see WatchdogShared). The timeout
    // here is unused in the channel model (Core still holds default_timeout_ms).
    let state = IsolateState::new(host_namespace, 0, !explicit_microtasks);
    let watchdog_shared = Arc::clone(&state.watchdog);
    isolate.set_slot(state);
    let watchdog_thread = {
        let shared = Arc::clone(&watchdog_shared);
        let handle = isolate.thread_safe_handle();
        std::thread::spawn(move || watchdog_loop(shared, handle))
    };
    // Boot the main realm (id 0) into the slot, where service_request — callable
    // with only a scope in hand — reaches it. new_realm reads the host namespace
    // from the slot (seeded above).
    {
        v8::scope!(let scope, &mut isolate);
        let main_context = new_realm(scope, 0);
        istate!(scope).realms.main_context = Some(main_context);
    }

    while let Ok(request) = rx.recv() {
        v8::scope!(let scope, &mut isolate);
        if service_request(scope, request) {
            break;
        }
    }
    // Every v8::Global parked in the slot's registries must die before the
    // isolate it points into. Clear them here explicitly (the slot's IsolateState
    // is otherwise dropped by V8 during isolate disposal, too late for Globals).
    {
        v8::scope!(let scope, &mut isolate);
        let st = istate!(scope);
        st.realms = V8State::default();
        st.modules = ModuleReg::default();
        st.scripts = ScriptReg::default();
    }
    // Stop the watchdog and join it before the isolate (whose handle it holds)
    // drops, so no terminate can land after teardown.
    {
        let mut inner = watchdog_shared.inner.lock().unwrap();
        inner.shutdown = true;
        watchdog_shared.cv.notify_one();
    }
    let _ = watchdog_thread.join();
    drop(isolate);
}

// Service ONE request on the V8 thread; returns true when the isolate should
// shut down (Request::Dispose). This is the single dispatcher for BOTH the
// main request loop and the nested wait loops (host_fn_callback /
// resolve_imported / dynamic_import_cb), so EVERY op — not just eval/call —
// works re-entrantly from inside a host proc or module resolver. Anything
// less deadlocks the rendezvous: the Ruby thread waits on the V8 thread,
// which waits on that same Ruby thread's answer.
fn service_request(scope: &mut v8::PinScope<'_, '_, ()>, request: Request) -> bool {
    // True when no other request is suspended beneath this one — the frame
    // that owns terminate-flag cleanup (see WATCHDOG_FIRED).
    let outermost = REPLY_STACK.with(|s| s.borrow().is_empty());
    // Clear any terminate left over from BEFORE this request. An
    // Isolate#terminate fired while the V8 thread was idle (no JS running)
    // arms the isolate-global flag but no WATCHDOG_FIRED, so the end-of-request
    // sweep would miss it and the next eval would abort spuriously — and an
    // idle terminate isn't even observable via is_execution_terminating() yet,
    // so cancel unconditionally. Only at the outermost frame: a terminate aimed
    // at a SUSPENDED outer frame must survive a nested request.
    if outermost {
        scope.cancel_terminate_execution();
    }
    // Mark the realm this request runs in active while it is on the stack, so
    // Reset/DisposeContext can refuse to pull a live realm out from under a
    // suspended frame.
    let realm = request_realm(istate!(scope), &request);
    if let Some(id) = realm {
        istate!(scope).active_realms.push(id);
    }
    let dispose = dispatch_one(scope, request, outermost);
    if realm.is_some() {
        istate!(scope).active_realms.pop();
    }
    // Sweep a leftover terminate flag once the whole request stack has
    // unwound (see watchdog_fired for why nested frames must not cancel).
    if outermost && istate!(scope).watchdog_fired {
        istate!(scope).watchdog_fired = false;
        scope.cancel_terminate_execution();
    }
    dispose
}

// The realm a request will run in (None for realm-independent ops); feeds
// ACTIVE_REALMS above.
fn request_realm(state: &IsolateState, request: &Request) -> Option<i32> {
    match request {
        Request::Eval { context_id, .. }
        | Request::Call { context_id, .. }
        | Request::Attach { context_id, .. }
        | Request::AttachMany { context_id, .. }
        | Request::CompileModule { context_id, .. }
        | Request::CompileScript { context_id, .. } => Some(*context_id),
        Request::DrainMicrotasks { .. } => Some(0),
        Request::InstantiateModule { module_id, .. }
        | Request::EvaluateModule { module_id, .. }
        | Request::ModuleNamespace { module_id, .. } => {
            module_handle(state, *module_id).map(|(_, cid)| cid)
        }
        Request::RunScript { script_id, .. } => script_handle(state, *script_id).map(|(_, cid)| cid),
        Request::Reset { .. }
        | Request::CreateContext { .. }
        | Request::DisposeContext { .. }
        | Request::ModuleStatus { .. }
        | Request::DisposeModule { .. }
        | Request::DisposeScript { .. }
        | Request::ScriptCodeCache { .. }
        | Request::ModuleCodeCache { .. }
        | Request::Dispose => None,
    }
}

fn dispatch_one(scope: &mut v8::PinScope<'_, '_, ()>, request: Request, outermost: bool) -> bool {
    // A request-scoped handle scope, so handles created while servicing a
    // nested request don't pile up in the suspended callback's scope.
    v8::scope!(let scope, &mut *scope);
    {
        match request {
            Request::Eval {
                context_id,
                source,
                filename,
                timeout_ms,
                reply,
            } => {
                let outcome = run_js_bracketed(scope, outermost, timeout_ms, &reply, |scope, outermost| {
                    let realm = context_for(istate!(scope), context_id);
                    match realm {
                        Some(ctx) => {
                            let context = v8::Local::new(scope, &ctx);
                            let scope = &mut v8::ContextScope::new(scope, context);
                            let out = run_source(scope, &source, &filename);
                            auto_drain(scope, outermost);
                            (true, out)
                        }
                        None => (false, Err(VmError::Runtime("realm disposed or unknown".into()))),
                    }
                });
                let _ = reply.send(VmReply::Done(outcome));
            }
            Request::Call {
                context_id,
                name,
                args,
                void,
                timeout_ms,
                reply,
            } => {
                // The bracket pushes REPLY_STACK so a host fn invoked by the
                // called function routes back to this request's waiter.
                let outcome = run_js_bracketed(scope, outermost, timeout_ms, &reply, |scope, outermost| {
                    let realm = context_for(istate!(scope), context_id);
                    match realm {
                        Some(ctx) => {
                            let context = v8::Local::new(scope, &ctx);
                            let scope = &mut v8::ContextScope::new(scope, context);
                            let out = call_function(scope, &name, args, void);
                            auto_drain(scope, outermost);
                            (true, out)
                        }
                        None => (false, Err(VmError::Runtime("realm disposed or unknown".into()))),
                    }
                });
                let _ = reply.send(VmReply::Done(outcome));
            }
            Request::DrainMicrotasks { timeout_ms, reply } => {
                // A microtask may call an attached host fn (a Promise .then ->
                // ruby), so push the reply onto REPLY_STACK exactly like Eval,
                // or that callback would find no waiter and silently no-op.
                REPLY_STACK.with(|s| s.borrow_mut().push(reply.clone()));
                let watchdog = arm_watchdog(scope, timeout_ms);
                let main = context_for(istate!(scope), 0);
                if let Some(ctx) = main {
                    let context = v8::Local::new(scope, &ctx);
                    let scope = &mut v8::ContextScope::new(scope, context);
                    checkpoint_draining(scope);
                }
                let fired = disarm_watchdog(scope, watchdog);
                if fired {
                    istate!(scope).watchdog_fired = true;
                }
                REPLY_STACK.with(|s| {
                    s.borrow_mut().pop();
                });
                let outcome = if fired {
                    Err(VmError::Terminated)
                } else {
                    Ok(JsVal::Undefined)
                };
                let _ = reply.send(VmReply::Done(outcome));
            }
            Request::Attach {
                context_id,
                name,
                host_fn_id,
                timeout_ms,
                reply,
            } => {
                // attach_at_path writes onto globalThis (and walks a dotted
                // path), which can fire a user-defined accessor or Proxy trap —
                // arbitrary JS. So it goes through the same bracket as Eval: a
                // host fn the trap calls routes back, and a looping trap is
                // time-capped.
                let outcome = run_js_bracketed(scope, outermost, timeout_ms, &reply, |scope, outermost| {
                    let realm = context_for(istate!(scope), context_id);
                    match realm {
                        Some(ctx) => {
                            let context = v8::Local::new(scope, &ctx);
                            let scope = &mut v8::ContextScope::new(scope, context);
                            let external = v8::External::new(scope, host_fn_id as *mut c_void);
                            let out = match v8::Function::builder(host_fn_callback)
                                .data(external.into())
                                .build(scope)
                            {
                                // A dotted name (e.g. "MiniRacer.foo") attaches
                                // under a namespace object, creating missing
                                // intermediates, so host fns needn't pollute the
                                // bare global.
                                Some(function) => attach_at_path(scope, context, &name, function),
                                None => Err(VmError::Runtime("failed to build function".into())),
                            };
                            auto_drain(scope, outermost);
                            (true, out)
                        }
                        None => (false, Err(VmError::Runtime("realm disposed or unknown".into()))),
                    }
                });
                let _ = reply.send(VmReply::Done(outcome));
            }
            Request::AttachMany {
                context_id,
                entries,
                timeout_ms,
                reply,
            } => {
                // Same as Attach (arbitrary JS via accessors/Proxy traps), but
                // installs every entry under one bracket/drain. Applied in order;
                // stops at the first failure and reports its (name-tagged) error.
                // NOT transactional: entries before the failure stay attached —
                // the realm is not rolled back (matches single Attach, which also
                // commits its one write or fails it).
                let outcome = run_js_bracketed(scope, outermost, timeout_ms, &reply, |scope, outermost| {
                    let realm = context_for(istate!(scope), context_id);
                    match realm {
                        Some(ctx) => {
                            let context = v8::Local::new(scope, &ctx);
                            let scope = &mut v8::ContextScope::new(scope, context);
                            let mut out = Ok(JsVal::Undefined);
                            for (name, host_fn_id) in &entries {
                                let external = v8::External::new(scope, *host_fn_id as *mut c_void);
                                out = match v8::Function::builder(host_fn_callback)
                                    .data(external.into())
                                    .build(scope)
                                {
                                    Some(function) => attach_at_path(scope, context, name, function),
                                    None => Err(VmError::Runtime(format!(
                                        "failed to build function for `{name}`"
                                    ))),
                                };
                                if out.is_err() {
                                    break;
                                }
                            }
                            auto_drain(scope, outermost);
                            (true, out)
                        }
                        None => (false, Err(VmError::Runtime("realm disposed or unknown".into()))),
                    }
                });
                let _ = reply.send(VmReply::Done(outcome));
            }
            Request::Reset { context_id, reply } => {
                let known =
                    context_id == 0 || istate!(scope).realms.contexts.contains_key(&context_id);
                if istate!(scope).draining {
                    // A microtask from ANY realm may be mid-flight on the stack;
                    // swapping a v8::Context out from under it corrupts state.
                    let _ = reply.send(VmReply::Done(Err(VmError::Runtime(
                        "cannot reset a realm during a microtask checkpoint".into(),
                    ))));
                } else if !known {
                    let _ = reply.send(VmReply::Done(Err(VmError::Runtime(
                        "context disposed or unknown".into(),
                    ))));
                } else if istate!(scope).active_realms.contains(&context_id) {
                    // Swapping the v8::Context behind a suspended frame would
                    // drop its in-flight modules/scripts and let the realm id
                    // refer to a different context than the one on the stack
                    // (defeating the cross-context import guards).
                    let _ = reply.send(VmReply::Done(Err(VmError::Runtime(
                        "cannot reset a realm while a request for it is suspended on the V8 stack"
                            .into(),
                    ))));
                } else {
                    let fresh = new_realm(scope, context_id);
                    {
                        let realms = &mut istate!(scope).realms;
                        if context_id == 0 {
                            realms.main_context = Some(fresh);
                        } else {
                            realms.contexts.insert(context_id, fresh);
                        }
                    }
                    // Drop modules bound to this context — their realm just changed.
                    drop_context_artifacts(istate!(scope), context_id);
                    let _ = reply.send(VmReply::Done(Ok(JsVal::Undefined)));
                }
            }
            Request::CreateContext { reply } => {
                let id = {
                    let realms = &mut istate!(scope).realms;
                    let id = realms.next_context_id;
                    realms.next_context_id += 1;
                    id
                };
                let fresh = new_realm(scope, id);
                istate!(scope).realms.contexts.insert(id, fresh);
                let _ = reply.send(VmReply::Done(Ok(JsVal::Int(id as i64))));
            }
            Request::DisposeContext { context_id, reply } => {
                if istate!(scope).draining {
                    // Same hazard as Reset: a microtask from any realm may be live.
                    let _ = reply.send(VmReply::Done(Err(VmError::Runtime(
                        "cannot dispose a realm during a microtask checkpoint".into(),
                    ))));
                } else if istate!(scope).active_realms.contains(&context_id) {
                    // Same hazard as Reset: a suspended frame still runs in it.
                    let _ = reply.send(VmReply::Done(Err(VmError::Runtime(
                        "cannot dispose a realm while a request for it is suspended on the V8 stack"
                            .into(),
                    ))));
                } else {
                    // Dropping the Global lets V8 collect the context. id 0 is the
                    // default context and never disposed independently.
                    istate!(scope).realms.contexts.remove(&context_id);
                    // Reclaim the modules compiled in it (else they leak until
                    // isolate teardown).
                    drop_context_artifacts(istate!(scope), context_id);
                    let _ = reply.send(VmReply::Done(Ok(JsVal::Undefined)));
                }
            }
            Request::CompileModule {
                context_id,
                source,
                filename,
                cached_data,
                produce_cache,
                eager,
                reply,
            } => {
                let ctx = context_for(istate!(scope), context_id);
                let outcome = match ctx {
                    None => Err(VmError::Runtime("context disposed or unknown".into())),
                    Some(cx) => {
                    let context = v8::Local::new(scope, &cx);
                    let scope = &mut v8::ContextScope::new(scope, context);
                    v8::tc_scope!(let tc, scope);
                    match v8::String::new(tc, &source) {
                        None => Err(VmError::Runtime("module source too large".into())),
                        Some(code) => {
                            let origin = module_origin(tc, &filename);
                            // Consume a supplied bytecode cache (skip reparse),
                            // eager-compile every function, or compile fresh
                            // (lazy). cached_data wins: V8 forbids combining
                            // ConsumeCodeCache with EagerCompile.
                            let (mut src, opts) = compile_source(code, &origin, &cached_data, eager);
                            let compiled = v8::script_compiler::compile_module2(
                                tc,
                                &mut src,
                                opts,
                                v8::script_compiler::NoCacheReason::NoReason,
                            );
                            match compiled {
                                Some(module) => {
                                    // V8 marks a stale/incompatible supplied cache
                                    // rejected; the embedder recompiles & re-caches.
                                    let cache_rejected = cached_data.is_some()
                                        && src.get_cached_data().is_some_and(|c| c.rejected());
                                    // Produce a fresh cache from the unbound script.
                                    let produced = if produce_cache {
                                        module
                                            .get_unbound_module_script(tc)
                                            .create_code_cache()
                                            .map(|c| c.to_vec())
                                    } else {
                                        None
                                    };
                                    let hash = module.get_identity_hash().get();
                                    let g = v8::Global::new(tc, module);
                                    let id = {
                                        let m = &mut istate!(tc).modules;
                                        let id = m.next_id;
                                        m.next_id += 1;
                                        m.by_id
                                            .insert(id, (g.clone(), filename.clone(), context_id));
                                        m.by_hash.entry(hash).or_default().push((g, id));
                                        id
                                    };
                                    Ok(Compiled {
                                        id,
                                        cached_data: produced,
                                        cache_rejected,
                                    })
                                }
                                None if tc.has_terminated() => Err(VmError::Terminated),
                                // A module compile failure is a parse error
                                // (compile-time), not a thrown exception.
                                None => {
                                    let msg = tc
                                        .exception()
                                        .map(|e| e.to_rust_string_lossy(tc))
                                        .unwrap_or_else(|| "module parse error".to_string());
                                    let message = tc.message();
                                    let res = message
                                        .and_then(|m| m.get_script_resource_name(tc))
                                        .filter(|v| v.is_string())
                                        .map(|v| v.to_rust_string_lossy(tc))
                                        .unwrap_or_else(|| filename.clone());
                                    let loc = match message.and_then(|m| m.get_line_number(tc)) {
                                        Some(line) => format!(" at {res}:{line}"),
                                        None => format!(" at {res}"),
                                    };
                                    Err(VmError::Parse(format!("{msg}{loc}")))
                                }
                            }
                        }
                    }
                    }
                };
                let _ = reply.send(VmReply::ModuleCompiled(outcome));
            }
            Request::InstantiateModule { module_id, reply } => {
                // V8's module instantiation is NOT re-entrant: a nested
                // instantiate issued from a resolve block walks the outer,
                // half-built module graph and SEGVs the process. Refuse it
                // cleanly — a resolve block may COMPILE dependencies lazily
                // and return them; the outer instantiate links them.
                if istate!(scope).instantiating {
                    let _ = reply.send(VmReply::Done(Err(VmError::Runtime(
                        "instantiate is not re-entrant: another module is currently \
                         instantiating (compile the dependency and return it; the outer \
                         instantiate links it)"
                            .into(),
                    ))));
                } else {
                    istate!(scope).instantiating = true;
                    // REPLY_STACK so resolve_imported can round-trip per import edge.
                    REPLY_STACK.with(|s| s.borrow_mut().push(reply.clone()));
                    let handle = module_handle(istate!(scope), module_id);
                    let outcome = match handle {
                        None => Err(VmError::Runtime("unknown module".into())),
                        Some((g, cid)) => match context_for(istate!(scope), cid) {
                            None => Err(VmError::Runtime("module's context is gone".into())),
                            Some(cx) => {
                                let context = v8::Local::new(scope, &cx);
                                let scope = &mut v8::ContextScope::new(scope, context);
                                let module = v8::Local::new(scope, &g);
                                match module.get_status() {
                                    // Already linked (or further along): a no-op,
                                    // not an error — instantiate is idempotent.
                                    v8::ModuleStatus::Instantiated
                                    | v8::ModuleStatus::Evaluating
                                    | v8::ModuleStatus::Evaluated => Ok(JsVal::Undefined),
                                    // V8 CHECK-aborts on instantiating an errored
                                    // module; surface its exception instead.
                                    v8::ModuleStatus::Errored => Err(VmError::JsError {
                                        message: module
                                            .get_exception()
                                            .to_rust_string_lossy(scope),
                                        backtrace: vec![],
                                    }),
                                    _ => {
                                        v8::tc_scope!(let tc, scope);
                                        match module.instantiate_module(tc, resolve_imported) {
                                            Some(true) => Ok(JsVal::Undefined),
                                            _ if tc.has_terminated() => Err(VmError::Terminated),
                                            _ => {
                                                let exc = tc.exception();
                                                let stack = tc.stack_trace();
                                                Err(capture_js_error(tc, exc, stack))
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    };
                    REPLY_STACK.with(|s| {
                        s.borrow_mut().pop();
                    });
                    istate!(scope).instantiating = false;
                    let _ = reply.send(VmReply::Done(outcome));
                }
            }
            Request::EvaluateModule { module_id, timeout_ms, reply } => {
                // Top-level module code (and, under :auto, the microtasks its
                // TLA continuation drains) can loop, so it runs in the same
                // watchdog/REPLY_STACK bracket as Eval/Call/RunScript.
                let outcome = run_js_bracketed(scope, outermost, timeout_ms, &reply, |scope, outermost| {
                let handle = module_handle(istate!(scope), module_id);
                match handle {
                    None => (false, Err(VmError::Runtime("unknown module".into()))),
                    Some((g, cid)) => match context_for(istate!(scope), cid) {
                        None => (false, Err(VmError::Runtime("module's context is gone".into()))),
                        Some(cx) => {
                            let context = v8::Local::new(scope, &cx);
                            let scope = &mut v8::ContextScope::new(scope, context);
                            let module = v8::Local::new(scope, &g);
                            // A top-level-await module's evaluate() returns a
                            // PENDING promise that only settles once the drain
                            // runs its continuation — remember it so we can read
                            // its post-drain state instead of reporting a stale Ok.
                            let mut eval_promise: Option<v8::Global<v8::Promise>> = None;
                            // ran_js is true ONLY for the Instantiated arm that
                            // actually calls evaluate(); the Errored/Evaluated/
                            // non-instantiated arms run no JS, so a raced watchdog
                            // must not override their real outcome to Terminated.
                            let mut did_eval = false;
                            // V8 CHECK-aborts the process if evaluate runs on a
                            // module that isn't exactly Instantiated, so guard
                            // status explicitly rather than crash.
                            let out = match module.get_status() {
                                v8::ModuleStatus::Errored => {
                                    Err(VmError::JsError {
                                        message: module
                                            .get_exception()
                                            .to_rust_string_lossy(scope),
                                        backtrace: vec![],
                                    })
                                }
                                v8::ModuleStatus::Evaluated => Ok(JsVal::Undefined),
                                v8::ModuleStatus::Instantiated => {
                                    did_eval = true;
                                    v8::tc_scope!(let tc, scope);
                                    match module.evaluate(tc) {
                                        // A synchronous top-level throw yields a
                                        // *rejected* promise (not None); a pending
                                        // (TLA) or fulfilled one is remembered and
                                        // re-checked after the drain.
                                        Some(value) => match v8::Local::<v8::Promise>::try_from(value) {
                                            Ok(p) if p.state() == v8::PromiseState::Rejected => {
                                                let reason = p.result(tc);
                                                Err(VmError::JsError {
                                                    message: reason.to_rust_string_lossy(tc),
                                                    backtrace: vec![],
                                                })
                                            }
                                            Ok(p) => {
                                                eval_promise = Some(v8::Global::new(tc, p));
                                                Ok(JsVal::Undefined)
                                            }
                                            _ => Ok(JsVal::Undefined),
                                        },
                                        None if tc.has_terminated() => Err(VmError::Terminated),
                                        None => {
                                            let exc = tc.exception();
                                            let stack = tc.stack_trace();
                                            Err(capture_js_error(tc, exc, stack))
                                        }
                                    }
                                }
                                _ => Err(VmError::Runtime(
                                    "module must be instantiated before evaluate".into(),
                                )),
                            };
                            auto_drain(scope, outermost);
                            // The drain may have settled a TLA module's promise to
                            // rejected — surface that instead of the provisional Ok.
                            let result = if let (true, Some(g)) = (out.is_ok(), eval_promise) {
                                let p = v8::Local::new(scope, &g);
                                if p.state() == v8::PromiseState::Rejected {
                                    let reason = p.result(scope);
                                    Err(VmError::JsError {
                                        message: reason.to_rust_string_lossy(scope),
                                        backtrace: vec![],
                                    })
                                } else {
                                    out
                                }
                            } else {
                                out
                            };
                            (did_eval, result)
                        }
                    }
                }
                });
                let _ = reply.send(VmReply::Done(outcome));
            }
            Request::ModuleNamespace { module_id, reply } => {
                let handle = module_handle(istate!(scope), module_id);
                let outcome = match handle {
                    None => Err(VmError::Runtime("unknown module".into())),
                    Some((g, cid)) => match context_for(istate!(scope), cid) {
                        None => Err(VmError::Runtime("module's context is gone".into())),
                        Some(cx) => {
                            let context = v8::Local::new(scope, &cx);
                            let scope = &mut v8::ContextScope::new(scope, context);
                            let module = v8::Local::new(scope, &g);
                            // get_module_namespace CHECK-aborts unless the module
                            // is at least Instantiated.
                            match module.get_status() {
                                v8::ModuleStatus::Uninstantiated
                                | v8::ModuleStatus::Instantiating => Err(VmError::Runtime(
                                    "module must be instantiated before namespace".into(),
                                )),
                                _ => {
                                    let ns = module.get_module_namespace();
                                    Ok(js_to_jsval(scope, ns))
                                }
                            }
                        }
                    }
                };
                let _ = reply.send(VmReply::Done(outcome));
            }
            Request::ModuleStatus { module_id, reply } => {
                let handle = module_handle(istate!(scope), module_id);
                let outcome = match handle {
                    None => Err(VmError::Runtime("unknown module".into())),
                    Some((g, _cid)) => {
                        let module = v8::Local::new(scope, &g);
                        let name = match module.get_status() {
                            v8::ModuleStatus::Uninstantiated => "uninstantiated",
                            v8::ModuleStatus::Instantiating => "instantiating",
                            v8::ModuleStatus::Instantiated => "instantiated",
                            v8::ModuleStatus::Evaluating => "evaluating",
                            v8::ModuleStatus::Evaluated => "evaluated",
                            v8::ModuleStatus::Errored => "errored",
                        };
                        Ok(JsVal::Str(name.into()))
                    }
                };
                let _ = reply.send(VmReply::Done(outcome));
            }
            Request::DisposeModule { module_id, reply } => {
                let m = &mut istate!(scope).modules;
                m.by_id.remove(&module_id);
                for bucket in m.by_hash.values_mut() {
                    bucket.retain(|(_, id)| *id != module_id);
                }
                let _ = reply.send(VmReply::Done(Ok(JsVal::Undefined)));
            }
            Request::CompileScript {
                context_id,
                source,
                filename,
                cached_data,
                produce_cache,
                eager,
                reply,
            } => {
                let ctx = context_for(istate!(scope), context_id);
                let outcome = match ctx {
                    None => Err(VmError::Runtime("context disposed or unknown".into())),
                    Some(cx) => {
                        let context = v8::Local::new(scope, &cx);
                        let scope = &mut v8::ContextScope::new(scope, context);
                        v8::tc_scope!(let tc, scope);
                        match v8::String::new(tc, &source) {
                            None => Err(VmError::Runtime("script source too large".into())),
                            Some(code) => {
                                let origin = script_origin(tc, &filename);
                                let (mut src, opts) = compile_source(code, &origin, &cached_data, eager);
                                match v8::script_compiler::compile_unbound_script(
                                    tc,
                                    &mut src,
                                    opts,
                                    v8::script_compiler::NoCacheReason::NoReason,
                                ) {
                                    Some(unbound) => {
                                        let cache_rejected = cached_data.is_some()
                                            && src.get_cached_data().is_some_and(|c| c.rejected());
                                        let produced = if produce_cache {
                                            unbound.create_code_cache().map(|c| c.to_vec())
                                        } else {
                                            None
                                        };
                                        let g = v8::Global::new(tc, unbound);
                                        let id = {
                                            let s = &mut istate!(tc).scripts;
                                            let id = s.next_id;
                                            s.next_id += 1;
                                            s.by_id.insert(id, (g, context_id));
                                            id
                                        };
                                        Ok(Compiled {
                                            id,
                                            cached_data: produced,
                                            cache_rejected,
                                        })
                                    }
                                    None if tc.has_terminated() => Err(VmError::Terminated),
                                    // Compile failure = a parse error (with location).
                                    None => {
                                        let msg = tc
                                            .exception()
                                            .map(|e| e.to_rust_string_lossy(tc))
                                            .unwrap_or_else(|| "script parse error".to_string());
                                        let message = tc.message();
                                        let res = message
                                            .and_then(|m| m.get_script_resource_name(tc))
                                            .filter(|v| v.is_string())
                                            .map(|v| v.to_rust_string_lossy(tc))
                                            .unwrap_or_else(|| filename.clone());
                                        let loc = match message.and_then(|m| m.get_line_number(tc)) {
                                            Some(line) => format!(" at {res}:{line}"),
                                            None => format!(" at {res}"),
                                        };
                                        Err(VmError::Parse(format!("{msg}{loc}")))
                                    }
                                }
                            }
                        }
                    }
                };
                let _ = reply.send(VmReply::ScriptCompiled(outcome));
            }
            Request::RunScript {
                script_id,
                timeout_ms,
                reply,
            } => {
                let outcome = run_js_bracketed(scope, outermost, timeout_ms, &reply, |scope, outermost| {
                    let handle = script_handle(istate!(scope), script_id);
                    match handle {
                        None => (false, Err(VmError::Runtime("unknown script".into()))),
                        Some((g, cid)) => match context_for(istate!(scope), cid) {
                            None => (false, Err(VmError::Runtime("script's context is gone".into()))),
                            Some(cx) => {
                            let context = v8::Local::new(scope, &cx);
                            let scope = &mut v8::ContextScope::new(scope, context);
                            let unbound = v8::Local::new(scope, &g);
                            let script = unbound.bind_to_current_context(scope);
                            let out = {
                                v8::tc_scope!(let tc, scope);
                                match script.run(tc) {
                                    Some(value) => Ok(js_to_jsval(tc, value)),
                                    None if tc.has_terminated() => Err(VmError::Terminated),
                                    None => {
                                        let exc = tc.exception();
                                        let stack = tc.stack_trace();
                                        Err(capture_js_error(tc, exc, stack))
                                    }
                                }
                            };
                            auto_drain(scope, outermost);
                            (true, out)
                            }
                        }
                    }
                });
                let _ = reply.send(VmReply::Done(outcome));
            }
            Request::DisposeScript { script_id, reply } => {
                istate!(scope).scripts.by_id.remove(&script_id);
                let _ = reply.send(VmReply::Done(Ok(JsVal::Undefined)));
            }
            // Serialize the script's CURRENT compile state. The stored handle is
            // the UnboundScript, which V8 fills in with inner-function bytecode as
            // run() lazily compiles them — so calling this after run() captures
            // the functions that actually executed (a warm cache). None when V8
            // can't serialize, or when the realm was reset/disposed out from under
            // the script (its handle is gone): produce nil, not an error.
            Request::ScriptCodeCache { script_id, reply } => {
                let handle = script_handle(istate!(scope), script_id);
                let outcome = match handle {
                    None => Ok(None),
                    Some((g, _cid)) => {
                        let unbound = v8::Local::new(scope, &g);
                        Ok(unbound.create_code_cache().map(|c| c.to_vec()))
                    }
                };
                let _ = reply.send(VmReply::CodeCache(outcome));
            }
            // Same, for a module: get_unbound_module_script gives the shared
            // compiled script, which evaluate() fills with inner-function bytecode.
            // It needs the module's context entered (unlike UnboundScript), so
            // a gone realm yields nil.
            Request::ModuleCodeCache { module_id, reply } => {
                let mh = module_handle(istate!(scope), module_id);
                let handle = mh.and_then(|(g, cid)| context_for(istate!(scope), cid).map(|cx| (g, cx)));
                let outcome = match handle {
                    None => Ok(None),
                    Some((g, cx)) => {
                        let context = v8::Local::new(scope, &cx);
                        let scope = &mut v8::ContextScope::new(scope, context);
                        let module = v8::Local::new(scope, &g);
                        let unbound = module.get_unbound_module_script(scope);
                        Ok(unbound.create_code_cache().map(|c| c.to_vec()))
                    }
                };
                let _ = reply.send(VmReply::CodeCache(outcome));
            }
            // Shuts the main loop down. Never arrives at a nested servicing
            // site: Core::dispose bypasses dispatch and queues directly.
            Request::Dispose => return true,
        }
    }
    false
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
thread_local! {
    // Suspended frames this Ruby thread is currently serving (a host proc or
    // module resolver), innermost last. Each frame is tagged with its Core's
    // address so Core::dispatch only routes ops for THAT isolate through it —
    // an op aimed at a different isolate must use that isolate's own queue
    // (its V8 thread is not the suspended one).
    static NESTED: RefCell<Vec<(usize, Sender<Answer>)>> = const { RefCell::new(Vec::new()) };
}

// Run |f| with |answer| pushed as this thread's innermost suspended frame,
// popping it afterwards — any VM op |f| issues routes through the frame via
// Core::dispatch. Centralised so push/pop can't fall out of balance.
fn with_nested<R>(core: &Core, answer: &Sender<Answer>, f: impl FnOnce() -> R) -> R {
    let tag = core as *const Core as usize;
    NESTED.with(|n| n.borrow_mut().push((tag, answer.clone())));
    let r = f();
    NESTED.with(|n| {
        n.borrow_mut().pop();
    });
    r
}

struct Shared {
    tx: Sender<Request>,
    handle: v8::IsolateHandle,
    disposed: bool,
}

// The channel + isolate handle + shared host-fn registry that drive the one V8
// thread. A Context and all the Realms it spawns share ONE Core via Arc, so
// any of them can issue requests and they all see the same attached procs and
// the same dispose state. Rust's refcount keeps the V8 thread alive until the
// last wrapper is gone — no GC-mark bookkeeping (mini_racer's Realm has to mark
// its parent Context by hand; here the type system does it).
struct Core {
    // Weak self-handle so a &Core method can mint an Arc<Core> again (built via
    // Arc::new_cyclic). Needed to hand a fresh Context wrapper to the dynamic
    // import resolver — Context owns an Arc<Core> and &self can't recover it.
    me: Weak<Core>,
    shared: Mutex<Shared>,
    // Shared across contexts (host_fn_id indexes ProcTable.slots). Mutex (not
    // RefCell) because contexts of one Context may be pumped on different
    // threads. Each proc is GC-rooted (marked + pinned) while live — see
    // RootedProc/ProcSlot; reset/dispose of a realm releases its roots and
    // recycles their slots (ProcTable.free).
    procs: Mutex<ProcTable>,
    // Default per-eval/call timeout (ms); 0 = none. eval(timeout_ms:)'s explicit
    // value overrides it. Guards against an in-V8 infinite loop without a watchdog.
    default_timeout_ms: u64,
    // Set by Context#dynamic_import_resolver=; called for a JS import() to map
    // (specifier, referrer) to an already-loaded Module. GC-rooted like procs.
    dynamic_import_resolver: Mutex<Option<RootedProc>>,
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
    fn new(
        ruby: &Ruby,
        host_namespace: Option<String>,
        snapshot: Option<magnus::typed_data::Obj<Snapshot>>,
        timeout_ms: u64,
        explicit_microtasks: bool,
    ) -> Result<Self, Error> {
        let snapshot_bytes = snapshot.map(|s| s.blob.borrow().clone());
        let (tx, rx) = channel::<Request>();
        let (handle_tx, handle_rx) = channel::<v8::IsolateHandle>();
        std::thread::spawn(move || {
            v8_thread_main(rx, handle_tx, host_namespace, snapshot_bytes, explicit_microtasks)
        });
        let handle = handle_rx
            .recv()
            .map_err(|_| Error::new(ruby.exception_runtime_error(), "V8 thread failed to boot"))?;
        Ok(Isolate {
            core: Arc::new_cyclic(|me| Core {
                me: me.clone(),
                shared: Mutex::new(Shared {
                    tx,
                    handle,
                    disposed: false,
                }),
                procs: Mutex::new(ProcTable::default()),
                default_timeout_ms: timeout_ms,
                dynamic_import_resolver: Mutex::new(None),
            }),
        })
    }
}

impl Core {
    fn send(&self, ruby: &Ruby, request: Request) -> Result<(), Error> {
        let shared = self.shared.lock().unwrap();
        if shared.disposed {
            return Err(Error::new(ruby.exception_runtime_error(), "disposed context"));
        }
        shared
            .tx
            .send(request)
            .map_err(|_| Error::new(ruby.exception_runtime_error(), "V8 thread is gone"))
    }

    // Deliver a request to the V8 thread. Normally via the main queue; but
    // when this Ruby thread is inside a host proc / module resolver for THIS
    // isolate (a NESTED frame tagged with this Core), the V8 thread is parked
    // in that callback's answer loop and is NOT reading the queue — route
    // through the innermost such frame instead, whose loop services any op
    // re-entrantly (service_request). Queueing there would deadlock: the V8
    // thread waits for the proc's answer while the proc waits for the queued
    // request. A frame belonging to a DIFFERENT isolate is skipped: that
    // isolate's V8 thread is the suspended one, not ours, so our queue is
    // being read normally.
    //
    // NB: the frame lookup is per-Ruby-thread. An op issued from a Ruby
    // thread *spawned inside* a proc still goes to the main queue and blocks
    // until the outer request completes — if the proc waits on that thread,
    // they deadlock. Re-entry is same-thread only.
    fn dispatch(&self, ruby: &Ruby, request: Request) -> Result<(), Error> {
        let me = self as *const Core as usize;
        let nested = NESTED.with(|n| {
            n.borrow()
                .iter()
                .rev()
                .find(|(core, _)| *core == me)
                .map(|(_, answer)| answer.clone())
        });
        match nested {
            Some(answer) => answer
                .send(Answer::Nested(request))
                .map_err(|_| Error::new(ruby.exception_runtime_error(), "V8 thread is gone")),
            None => self.send(ruby, request),
        }
    }

    // Wait for this request's reply, serving host-fn callbacks and the
    // instantiate resolve round-trip as they arrive. The recv waits release the
    // GVL; the Ruby procs run with it held. |module_resolve| carries
    // Module#instantiate's resolve block; other ops pass None.
    fn pump(
        &self,
        ruby: &Ruby,
        reply_rx: Receiver<VmReply>,
        module_resolve: Option<Proc>,
    ) -> Result<Value, Error> {
        loop {
            let message = without_gvl(|| reply_rx.recv());
            match message {
                Ok(VmReply::Done(Ok(val))) => return jsval_to_ruby(ruby, &val),
                Ok(VmReply::Done(Err(e))) => return Err(vm_err(ruby, e)),
                // compile_module / compile / create_code_cache receive these
                // directly, never via pump.
                Ok(VmReply::ModuleCompiled(_))
                | Ok(VmReply::ScriptCompiled(_))
                | Ok(VmReply::CodeCache(_)) => {
                    return Err(Error::new(
                        ruby.exception_runtime_error(),
                        "unexpected compile reply",
                    ));
                }
                Ok(VmReply::Callback {
                    host_fn_id,
                    args,
                    answer,
                }) => {
                    let result = self.call_proc(ruby, host_fn_id, &args, &answer);
                    let _ = answer.send(Answer::Result(result));
                }
                Ok(VmReply::ResolveModule {
                    specifier,
                    referrer_url,
                    initiating_context,
                    answer,
                }) => {
                    match &module_resolve {
                        Some(resolve) => {
                            // The resolve block may itself call back into the VM
                            // (e.g. compile_module for a lazily-loaded dep), so
                            // run it under a NESTED frame like a host proc's:
                            // those ops then route nested instead of into the
                            // (busy) main queue. The static instantiate block
                            // keeps its 2-arg contract (None).
                            let resolved = with_nested(self, &answer, || {
                                resolve_module_via_ruby(self, *resolve, &specifier, &referrer_url, None)
                            });
                            match resolved {
                                Ok(id) => {
                                    let _ = answer.send(Answer::ModuleId(id));
                                }
                                // Unblock the V8 thread (import fails to resolve),
                                // then propagate the resolver's real error instead
                                // of masking it as "module not found".
                                Err(e) => {
                                    let _ = answer.send(Answer::ModuleId(None));
                                    return Err(e);
                                }
                            }
                        }
                        None => {
                            // No instantiate block on this request: the V8
                            // thread is auto-linking a DYNAMIC import, whose
                            // static imports resolve through the
                            // dynamic_import_resolver. A raising resolver only
                            // fails the import() (the surrounding request is
                            // still owed its Done reply), like the
                            // DynamicImport arm below.
                            let resolver = {
                                let guard = self.dynamic_import_resolver.lock().unwrap();
                                guard.as_ref().map(|r| r.get())
                            };
                            let id = match resolver {
                                Some(proc) => with_nested(self, &answer, || {
                                    resolve_module_via_ruby(
                                        self,
                                        proc,
                                        &specifier,
                                        &referrer_url,
                                        Some(initiating_context),
                                    )
                                })
                                .unwrap_or(None),
                                None => None,
                            };
                            let _ = answer.send(Answer::ModuleId(id));
                        }
                    }
                }
                Ok(VmReply::DynamicImport {
                    specifier,
                    referrer_url,
                    initiating_context,
                    answer,
                }) => {
                    // Read the Context's dynamic_import_resolver (set via the
                    // setter); reuse the same Module-id resolution as instantiate.
                    let resolver = {
                        let guard = self.dynamic_import_resolver.lock().unwrap();
                        guard.as_ref().map(|r| r.get())
                    };
                    match resolver {
                        Some(proc) => {
                            // Like ResolveModule: ops issued by the resolver
                            // (compile/instantiate/evaluate a lazy module)
                            // route nested through this answer sender. The
                            // resolver gets the initiating realm as a 3rd Context
                            // arg so iframe imports resolve in the iframe's realm.
                            let resolved = with_nested(self, &answer, || {
                                resolve_module_via_ruby(
                                    self,
                                    proc,
                                    &specifier,
                                    &referrer_url,
                                    Some(initiating_context),
                                )
                            });
                            match resolved {
                                Ok(id) => {
                                    let _ = answer.send(Answer::ModuleId(id));
                                }
                                // import() happens mid-eval; a raising resolver
                                // must only reject the import() promise, NOT abort
                                // the surrounding eval (whose Done reply is still
                                // coming). Unblock V8 and keep pumping — the
                                // import() rejects generically (unlike instantiate,
                                // which is its own request and may propagate Err).
                                Err(_) => {
                                    let _ = answer.send(Answer::ModuleId(None));
                                }
                            }
                        }
                        None => {
                            let _ = answer.send(Answer::ModuleId(None));
                        }
                    }
                }
                Err(_) => {
                    return Err(Error::new(
                        ruby.exception_runtime_error(),
                        "V8 thread went away mid-request",
                    ));
                }
            }
        }
    }

    fn call_proc(
        &self,
        ruby: &Ruby,
        host_fn_id: usize,
        args: &[JsVal],
        answer: &Sender<Answer>,
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
        // is not mutated while the slice is borrowed — as_slice's contract.
        let result: Result<Value, Error> =
            with_nested(self, answer, || proc.call(unsafe { ruby_args.as_slice() }));
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

        let (reply_tx, reply_rx) = channel::<VmReply>();
        self.dispatch(
            ruby,
            Request::Call {
                context_id,
                name,
                args: jsargs,
                void,
                timeout_ms: self.default_timeout_ms,
                reply: reply_tx,
            },
        )?;
        self.pump(ruby, reply_rx, None)
    }

    fn drain_microtasks(&self, ruby: &Ruby) -> Result<Value, Error> {
        let (reply_tx, reply_rx) = channel::<VmReply>();
        self.dispatch(
            ruby,
            Request::DrainMicrotasks {
                timeout_ms: self.default_timeout_ms,
                reply: reply_tx,
            },
        )?;
        self.pump(ruby, reply_rx, None)
    }

    fn eval_t(
        &self,
        ruby: &Ruby,
        context_id: i32,
        source: String,
        filename: String,
        timeout_ms: u64,
    ) -> Result<Value, Error> {
        let (reply_tx, reply_rx) = channel::<VmReply>();
        self.dispatch(
            ruby,
            Request::Eval {
                context_id,
                source,
                filename,
                timeout_ms,
                reply: reply_tx,
            },
        )?;
        self.pump(ruby, reply_rx, None)
    }

    fn attach(&self, ruby: &Ruby, context_id: i32, name: String, proc: Proc) -> Result<Value, Error> {
        let host_fn_id = self.procs.lock().unwrap().alloc(ProcSlot {
            context_id,
            proc: Some(RootedProc(BoxValue::new(proc))),
        });
        let (reply_tx, reply_rx) = channel::<VmReply>();
        self.dispatch(
            ruby,
            Request::Attach {
                context_id,
                name,
                host_fn_id,
                timeout_ms: self.default_timeout_ms,
                reply: reply_tx,
            },
        )?;
        self.pump(ruby, reply_rx, None)
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
        let (reply_tx, reply_rx) = channel::<VmReply>();
        self.dispatch(
            ruby,
            Request::AttachMany {
                context_id,
                entries: named_ids,
                timeout_ms: self.default_timeout_ms,
                reply: reply_tx,
            },
        )?;
        self.pump(ruby, reply_rx, None)
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
        let (reply_tx, reply_rx) = channel::<VmReply>();
        self.dispatch(ruby, Request::Reset { context_id, reply: reply_tx })?;
        let out = self.pump(ruby, reply_rx, None)?;
        // Only on success — a refused reset (unknown/suspended realm) keeps
        // its attached fns callable.
        self.release_context_procs(context_id);
        Ok(out)
    }

    // Build a new context; returns its id (the V8 thread replies with an Int).
    fn create_context(&self, ruby: &Ruby) -> Result<i32, Error> {
        let (reply_tx, reply_rx) = channel::<VmReply>();
        self.dispatch(ruby, Request::CreateContext { reply: reply_tx })?;
        let id = self.pump(ruby, reply_rx, None)?;
        i32::try_convert(id)
    }

    fn dispose_context(&self, ruby: &Ruby, context_id: i32) -> Result<(), Error> {
        let (reply_tx, reply_rx) = channel::<VmReply>();
        self.dispatch(ruby, Request::DisposeContext { context_id, reply: reply_tx })?;
        self.pump(ruby, reply_rx, None)?;
        self.release_context_procs(context_id);
        Ok(())
    }

    // Thin ESM primitives. compile_module returns the new module's id.
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
        let (reply_tx, reply_rx) = channel::<VmReply>();
        self.dispatch(
            ruby,
            Request::CompileModule {
                context_id,
                source,
                filename,
                cached_data,
                produce_cache,
                eager,
                reply: reply_tx,
            },
        )?;
        // compile_module can't trigger host callbacks, so receive directly
        // (release the GVL) rather than via pump.
        match without_gvl(|| reply_rx.recv()) {
            Ok(VmReply::ModuleCompiled(Ok(cm))) => Ok(cm),
            Ok(VmReply::ModuleCompiled(Err(e))) => Err(vm_err(ruby, e)),
            _ => Err(Error::new(
                ruby.exception_runtime_error(),
                "V8 thread went away mid-compile",
            )),
        }
    }

    // instantiate carries the resolve block to pump so resolve_imported can ask
    // it per import edge. The block may compile a dependency lazily — while the
    // V8 thread is parked inside InstantiateModule, its wait loop services
    // nested ops (service_request) — and returns the dep Module (or nil for
    // unresolved).
    fn instantiate_module(&self, ruby: &Ruby, module_id: i32, resolve: Proc) -> Result<Value, Error> {
        let (reply_tx, reply_rx) = channel::<VmReply>();
        self.dispatch(ruby, Request::InstantiateModule { module_id, reply: reply_tx })?;
        self.pump(ruby, reply_rx, Some(resolve))
    }

    fn evaluate_module(&self, ruby: &Ruby, module_id: i32) -> Result<Value, Error> {
        let (reply_tx, reply_rx) = channel::<VmReply>();
        self.dispatch(
            ruby,
            Request::EvaluateModule {
                module_id,
                timeout_ms: self.default_timeout_ms,
                reply: reply_tx,
            },
        )?;
        self.pump(ruby, reply_rx, None)
    }

    fn module_namespace(&self, ruby: &Ruby, module_id: i32) -> Result<Value, Error> {
        let (reply_tx, reply_rx) = channel::<VmReply>();
        self.dispatch(ruby, Request::ModuleNamespace { module_id, reply: reply_tx })?;
        self.pump(ruby, reply_rx, None)
    }

    fn module_status(&self, ruby: &Ruby, module_id: i32) -> Result<Value, Error> {
        let (reply_tx, reply_rx) = channel::<VmReply>();
        self.dispatch(ruby, Request::ModuleStatus { module_id, reply: reply_tx })?;
        self.pump(ruby, reply_rx, None)
    }

    fn dispose_module(&self, ruby: &Ruby, module_id: i32) -> Result<(), Error> {
        let (reply_tx, reply_rx) = channel::<VmReply>();
        self.dispatch(ruby, Request::DisposeModule { module_id, reply: reply_tx })?;
        self.pump(ruby, reply_rx, None).map(|_| ())
    }

    // Classic script: compile (no host callbacks -> direct recv), run, dispose.
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
        let (reply_tx, reply_rx) = channel::<VmReply>();
        self.dispatch(
            ruby,
            Request::CompileScript {
                context_id,
                source,
                filename,
                cached_data,
                produce_cache,
                eager,
                reply: reply_tx,
            },
        )?;
        match without_gvl(|| reply_rx.recv()) {
            Ok(VmReply::ScriptCompiled(Ok(cs))) => Ok(cs),
            Ok(VmReply::ScriptCompiled(Err(e))) => Err(vm_err(ruby, e)),
            _ => Err(Error::new(
                ruby.exception_runtime_error(),
                "V8 thread went away mid-compile",
            )),
        }
    }

    fn run_script(&self, ruby: &Ruby, script_id: i32) -> Result<Value, Error> {
        let (reply_tx, reply_rx) = channel::<VmReply>();
        self.dispatch(
            ruby,
            Request::RunScript {
                script_id,
                timeout_ms: self.default_timeout_ms,
                reply: reply_tx,
            },
        )?;
        self.pump(ruby, reply_rx, None)
    }

    fn dispose_script(&self, ruby: &Ruby, script_id: i32) -> Result<(), Error> {
        let (reply_tx, reply_rx) = channel::<VmReply>();
        self.dispatch(ruby, Request::DisposeScript { script_id, reply: reply_tx })?;
        self.pump(ruby, reply_rx, None).map(|_| ())
    }

    // Serialize a fresh bytecode cache from a compiled handle's current state
    // (Script#/Module#create_code_cache). Pure serialization — no host callbacks
    // — so receive directly off the GVL like compile_script. None = V8 couldn't
    // produce one (or the realm is gone).
    fn script_code_cache(&self, ruby: &Ruby, script_id: i32) -> Result<Option<Vec<u8>>, Error> {
        let (reply_tx, reply_rx) = channel::<VmReply>();
        self.dispatch(ruby, Request::ScriptCodeCache { script_id, reply: reply_tx })?;
        recv_code_cache(ruby, reply_rx)
    }

    fn module_code_cache(&self, ruby: &Ruby, module_id: i32) -> Result<Option<Vec<u8>>, Error> {
        let (reply_tx, reply_rx) = channel::<VmReply>();
        self.dispatch(ruby, Request::ModuleCodeCache { module_id, reply: reply_tx })?;
        recv_code_cache(ruby, reply_rx)
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

    fn dispose(&self, ruby: &Ruby) -> Result<(), Error> {
        let mut shared = self.shared.lock().unwrap();
        if shared.disposed {
            return Ok(());
        }
        shared.disposed = true;
        // Queued behind any in-flight request; its requester still gets its
        // reply (it owns its own channel). Send can only fail if the V8
        // thread already exited, which is fine.
        let _ = shared.tx.send(Request::Dispose);
        drop(shared);
        // Release every GC root now (we are on a Ruby thread): a disposed
        // isolate must not keep the attached procs — and whatever their
        // closures capture — alive and pinned until the last wrapper object
        // is itself collected.
        {
            let mut procs = self.procs.lock().unwrap();
            procs.slots.clear();
            procs.free.clear();
        }
        *self.dynamic_import_resolver.lock().unwrap() = None;
        let _ = ruby;
        Ok(())
    }
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
        if self.disposed.load(Ordering::SeqCst) {
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
        if self.disposed.load(Ordering::SeqCst) {
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

// Await a ScriptCodeCache/ModuleCodeCache reply off the GVL (it runs no JS, so
// no host callbacks to service — a direct recv, not pump).
fn recv_code_cache(ruby: &Ruby, reply_rx: Receiver<VmReply>) -> Result<Option<Vec<u8>>, Error> {
    match without_gvl(|| reply_rx.recv()) {
        Ok(VmReply::CodeCache(Ok(bytes))) => Ok(bytes),
        Ok(VmReply::CodeCache(Err(e))) => Err(vm_err(ruby, e)),
        _ => Err(Error::new(
            ruby.exception_runtime_error(),
            "V8 thread went away mid-cache",
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
    // backfill the backtrace with host-side (pump/magnus) frames.
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

fn jsval_to_ruby(ruby: &Ruby, val: &JsVal) -> Result<Value, Error> {
    let mut built: HashMap<u32, Value> = HashMap::new();
    jsval_to_ruby_d(ruby, val, &mut built)
}

// `built` is a HashMap<u32, Value> — the same "bare Values in a heap container,
// hidden from the GC mark phase" shape that's a use-after-free in call_proc. It
// is safe HERE only because every entry is, at every allocating safepoint, ALSO
// reachable from a live stack local: each container arm (Array/Obj/Map/Set)
// keeps its arr/h/set as a live local while its children recurse and grafts each
// child into it (push/aset), so the child is marked transitively; Bytes inserts
// then immediately returns its live local `s`. So `built` never holds the sole
// reference. This invariant is load-bearing: do NOT refactor an arm to stash a
// value in `built` without keeping it rooted by a live local until it's grafted.

fn jsval_to_ruby_d(
    ruby: &Ruby,
    val: &JsVal,
    built: &mut HashMap<u32, Value>,
) -> Result<Value, Error> {
    Ok(match val {
        JsVal::Undefined | JsVal::Null => ruby.qnil().as_value(),
        JsVal::Bool(b) => (*b).into_value_with(ruby),
        JsVal::Int(i) => (*i).into_value_with(ruby),
        JsVal::Num(n) => (*n).into_value_with(ruby),
        JsVal::Str(s) => s.clone().into_value_with(ruby),
        // Bytes -> a binary (ASCII-8BIT) String: str_from_slice uses rb_str_new,
        // which tags the result ASCII-8BIT — so it round-trips back to bytes.
        // Registered under |id| so an aliased blob stays one String via Ref.
        JsVal::Bytes { id, bytes } => {
            let s = ruby.str_from_slice(bytes).as_value();
            if let Some(id) = id {
                built.insert(*id, s);
            }
            s
        }
        // Reconstruct the Ruby Integer from the hex magnitude (arbitrary
        // precision); negate via Ruby so bignums stay exact.
        JsVal::BigInt { negative, words } => {
            let mag: Value = ruby
                .str_new(&words_to_hex(words))
                .funcall("to_i", (16i64,))?;
            if *negative {
                mag.funcall("-@", ())?
            } else {
                mag
            }
        }
        // Time.at takes seconds; carry sub-second precision as the Float. An
        // invalid Date (value_of NaN) raises RangeError, matching csim's
        // des_date — never a silent nil.
        JsVal::Date(ms) => {
            if !ms.is_finite() {
                return Err(Error::new(ruby.exception_range_error(), "invalid Date"));
            }
            ruby.class_object()
                .const_get::<_, magnus::RClass>("Time")?
                .funcall::<_, _, Value>("at", (*ms / 1000.0,))?
        }
        // Register before filling so a Ref from a descendant resolves to the
        // same Ruby object (shared/cyclic graphs keep their identity).
        JsVal::Array { id, items } => {
            let arr = ruby.ary_new();
            built.insert(*id, arr.as_value());
            for it in items {
                let _ = arr.push(jsval_to_ruby_d(ruby, it, built)?);
            }
            arr.as_value()
        }
        // JS objects -> string-keyed Hashes.
        JsVal::Obj { id, entries } => {
            let h = ruby.hash_new();
            built.insert(*id, h.as_value());
            for (k, it) in entries {
                let _ = h.aset(k.as_str(), jsval_to_ruby_d(ruby, it, built)?);
            }
            h.as_value()
        }
        // JS Map -> Ruby Hash (arbitrary marshalled keys, not just strings).
        JsVal::Map { id, pairs } => {
            let h = ruby.hash_new();
            built.insert(*id, h.as_value());
            for (k, v) in pairs {
                let kk = jsval_to_ruby_d(ruby, k, built)?;
                let vv = jsval_to_ruby_d(ruby, v, built)?;
                let _ = h.aset(kk, vv);
            }
            h.as_value()
        }
        // JS Set -> Ruby Set (stdlib); build empty then add so a cyclic Set
        // (a Set containing itself) resolves through the Ref table.
        JsVal::Set { id, items } => {
            let set: Value = ruby
                .class_object()
                .const_get::<_, magnus::RClass>("Set")?
                .funcall("new", ())?;
            built.insert(*id, set);
            for it in items {
                let v = jsval_to_ruby_d(ruby, it, built)?;
                let _: Value = set.funcall("add", (v,))?;
            }
            set
        }
        JsVal::Ref(id) => built
            .get(id)
            .copied()
            .unwrap_or_else(|| ruby.qnil().as_value()),
    })
}

// A Ruby String marshalled by its encoding TAG (the tag is the type):
//   - ASCII-8BIT (binary) -> JsVal::Bytes (a JS Uint8Array);
//   - any text encoding   -> JsVal::Str (UTF-8). Already-UTF-8 text is taken
//     as-is; other text encodings transcode (Ruby raises on unmappable bytes).
//     Either way the bytes must be VALID UTF-8 — invalid bytes RAISE, never
//     silently degrade to U+FFFD (loud failure beats silent corruption). A
//     text String mis-tagged binary surfaces loudly too (it becomes a Uint8Array).
fn string_to_jsval(ruby: &Ruby, s: RString) -> Result<JsVal, Error> {
    use magnus::encoding::EncodingCapable;
    if s.enc_get() == ruby.ascii8bit_encindex() {
        // Binary: the bytes ARE the value (O(n) copy, no inflation). id: None —
        // the identity-tracked path is the direct-String branch in
        // ruby_to_jsval_d; a to_str result reaching here is transient.
        return Ok(JsVal::Bytes {
            id: None,
            bytes: unsafe { s.as_slice() }.to_vec(),
        });
    }
    // Text. encode('UTF-8') on an already-UTF-8 source is a no-op that does NOT
    // validate, so skip it (one fewer copy) and let the from_utf8 check below
    // catch invalid bytes; other encodings transcode (raising on unmappable).
    let utf8: RString = if s.enc_get() == ruby.utf8_encindex() {
        s
    } else {
        s.funcall("encode", ("UTF-8",))?
    };
    // Build the Rust String with a real UTF-8 check (not lossy): invalid bytes
    // in a text-tagged String are an error, not silent U+FFFD substitution.
    match String::from_utf8(unsafe { utf8.as_slice() }.to_vec()) {
        Ok(s) => Ok(JsVal::Str(s)),
        Err(_) => Err(Error::new(
            ruby
                .class_object()
                .const_get::<_, ExceptionClass>("EncodingError")
                .unwrap_or_else(|_| ruby.exception_runtime_error()),
            "text-tagged String contains invalid UTF-8 bytes",
        )),
    }
}

// A JS object key must be a string. A Ruby String key crosses by its bytes as
// UTF-8 — but unlike a binary VALUE (which becomes a Uint8Array), a key has
// nowhere to put raw bytes, so invalid UTF-8 RAISES rather than silently
// degrading to U+FFFD. None for a non-String (the caller then tries to_s).
fn string_key(ruby: &Ruby, val: Value) -> Option<Result<String, Error>> {
    let s = RString::from_value(val)?;
    let bytes = unsafe { s.as_slice() }.to_vec();
    Some(String::from_utf8(bytes).map_err(|_| {
        Error::new(
            ruby.class_object()
                .const_get::<_, ExceptionClass>("EncodingError")
                .unwrap_or_else(|_| ruby.exception_runtime_error()),
            "hash key is not valid UTF-8",
        )
    }))
}

// A Ruby String's bytes interpreted as UTF-8 (invalid sequences become U+FFFD),
// regardless of the encoding tag. Used for the depth-truncation to_s fallback,
// where the value is already being lossily summarised.
fn lossy_string(val: Value) -> Option<String> {
    let s = RString::from_value(val)?;
    // Copy the bytes out before any further Ruby call can move/free them.
    let bytes = unsafe { s.as_slice() }.to_vec();
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

// Tracks Ruby containers already emitted this marshal (by object_id, which is
// exact — no collision handling needed) so shared/cyclic structures become Refs.
#[derive(Default)]
struct RbSeen {
    next_id: u32,
    map: HashMap<usize, u32>,
}

fn ruby_to_jsval(val: Value) -> Result<JsVal, Error> {
    let mut seen = RbSeen::default();
    ruby_to_jsval_d(val, &mut seen, 0)
}

fn ruby_to_jsval_d(val: Value, seen: &mut RbSeen, depth: u32) -> Result<JsVal, Error> {
    let ruby = Ruby::get().unwrap();
    if val.is_nil() {
        return Ok(JsVal::Null);
    }
    // NB: bool::try_convert is RTEST (truthiness) — it returns Ok(true) for
    // ANY non-false value — so check the actual true/false singletons by
    // identity instead, or every Integer/String/Array would marshal as `true`.
    if val.eql(ruby.qtrue()).unwrap_or(false) {
        return Ok(JsVal::Bool(true));
    }
    if val.eql(ruby.qfalse()).unwrap_or(false) {
        return Ok(JsVal::Bool(false));
    }
    // Ruby Time -> JS Date. Must precede the numeric checks: magnus's
    // i64/f64 TryConvert coerces a Time via to_i/to_f, so it would otherwise
    // marshal as a bare epoch number. Time#to_f is epoch seconds; Date wants ms.
    if let Ok(time_class) = ruby.class_object().const_get::<_, magnus::RClass>("Time") {
        if val.is_kind_of(time_class) {
            let sec = val.funcall::<_, _, f64>("to_f", ())?;
            return Ok(JsVal::Date(sec * 1000.0));
        }
    }
    // Integer. A JS Number is an f64, so only integers exactly representable
    // there (|n| <= 2^53) become Int/Number; anything larger (the rest of the
    // i64 range AND true bignums) becomes a BigInt so no precision is lost.
    // Use a strict Integer type check, NOT magnus::Integer::try_convert, which
    // coerces a Float / to_int object — that would turn e.g. 1e300 into a BigInt
    // instead of a Number.
    if let Ok(int_class) = ruby.class_object().const_get::<_, magnus::RClass>("Integer") {
        if val.is_kind_of(int_class) {
            if let Ok(i) = i64::try_convert(val) {
                if i.unsigned_abs() <= (1u64 << 53) {
                    return Ok(JsVal::Int(i));
                }
            }
            let abs: Value = val.funcall("abs", ())?;
            let hex: String = abs.funcall("to_s", (16i64,))?;
            let negative = val.funcall::<_, _, bool>("negative?", ())?;
            return Ok(JsVal::BigInt {
                negative,
                words: hex_to_words(&hex),
            });
        }
    }
    if let Ok(n) = f64::try_convert(val) {
        return Ok(JsVal::Num(n));
    }
    // Bare Symbol -> JS string (one-way: it comes back as a Ruby String). A
    // binary-encoded symbol surfaces the same curated EncodingError as a text
    // String with invalid UTF-8, not magnus's raw "expected utf-8" message.
    if let Some(sym) = magnus::Symbol::from_value(val) {
        let name = sym.name().map_err(|_| {
            Error::new(
                ruby.class_object()
                    .const_get::<_, ExceptionClass>("EncodingError")
                    .unwrap_or_else(|_| ruby.exception_runtime_error()),
                "symbol name is not valid UTF-8",
            )
        })?;
        return Ok(JsVal::Str(name.into_owned()));
    }
    // Real Strings: the encoding tag is the type declaration. A binary
    // (ASCII-8BIT) String -> bytes (JS Uint8Array), identity-tracked so an
    // aliased blob stays one Uint8Array; any text encoding -> a JS string.
    if let Some(rstr) = RString::from_value(val) {
        use magnus::encoding::EncodingCapable;
        if rstr.enc_get() == ruby.ascii8bit_encindex() {
            // depth 0 — a binary blob is a leaf, so it stays faithful bytes even
            // when deeply nested (never the depth-truncation-to-lossy-string);
            // only the identity (Ref) check applies. Frozen/interned binary
            // Strings share an object_id, so two `-"x".b` literals deliberately
            // collapse to ONE Uint8Array (they ARE the same Ruby object).
            let id = match rb_container_id(seen, val, 0)? {
                RbId::New(id) => id,
                RbId::Reuse(jv) => return Ok(jv),
            };
            return Ok(JsVal::Bytes {
                id: Some(id),
                bytes: unsafe { rstr.as_slice() }.to_vec(),
            });
        }
        return string_to_jsval(&ruby, rstr);
    }
    // A String-like (to_str) gets the same tag-driven treatment, but its result
    // is transient so it is not identity-tracked.
    if val.respond_to("to_str", false).unwrap_or(false) {
        let s: Value = val.funcall("to_str", ())?;
        if let Some(rstr) = RString::from_value(s) {
            return string_to_jsval(&ruby, rstr);
        }
    }
    // Ruby Set -> JS Set. Before the Array/Hash checks (a Set is neither).
    if let Ok(set_class) = ruby.class_object().const_get::<_, magnus::RClass>("Set") {
        if val.is_kind_of(set_class) {
            let id = match rb_container_id(seen, val, depth)? {
                RbId::New(id) => id,
                RbId::Reuse(jv) => return Ok(jv),
            };
            let arr: RArray = val.funcall("to_a", ())?;
            let mut items = Vec::with_capacity(arr.len());
            for i in 0..arr.len() {
                let el: Value = arr.entry::<Value>(i as isize)?;
                items.push(ruby_to_jsval_d(el, seen, depth + 1)?);
            }
            return Ok(JsVal::Set { id, items });
        }
    }
    if let Ok(arr) = RArray::try_convert(val) {
        let id = match rb_container_id(seen, val, depth)? {
            RbId::New(id) => id,
            RbId::Reuse(jv) => return Ok(jv),
        };
        let mut items = Vec::with_capacity(arr.len());
        for i in 0..arr.len() {
            let el: Value = arr.entry::<Value>(i as isize)?;
            items.push(ruby_to_jsval_d(el, seen, depth + 1)?);
        }
        return Ok(JsVal::Array { id, items });
    }
    if let Ok(hash) = RHash::try_convert(val) {
        let id = match rb_container_id(seen, val, depth)? {
            RbId::New(id) => id,
            RbId::Reuse(jv) => return Ok(jv),
        };
        let entries = RefCell::new(Vec::new());
        hash.foreach(|k: Value, v: Value| {
            // String/Symbol keys -> a UTF-8 String; anything else via to_s. A JS
            // object key has nowhere to put raw bytes, so unlike a binary VALUE
            // (-> Uint8Array) a binary KEY with invalid UTF-8 RAISES (string_key),
            // and a to_s returning a non-String is a loud error, not a silent "".
            let key = match string_key(&ruby, k) {
                Some(r) => r?,
                None => {
                    // A non-String key (Symbol, Integer, ...) -> to_s, then the
                    // same UTF-8 rule.
                    let s: Value = k.funcall("to_s", ())?;
                    match string_key(&ruby, s) {
                        Some(r) => r?,
                        None => {
                            return Err(Error::new(
                                ruby.exception_type_error(),
                                "hash key's to_s did not return a String",
                            ))
                        }
                    }
                }
            };
            entries
                .borrow_mut()
                .push((key, ruby_to_jsval_d(v, seen, depth + 1)?));
            Ok(magnus::r_hash::ForEach::Continue)
        })?;
        return Ok(JsVal::Obj {
            id,
            entries: entries.into_inner(),
        });
    }
    Err(Error::new(
        ruby.exception_type_error(),
        "unsupported type crossing into JS",
    ))
}

enum RbId {
    New(u32),
    Reuse(JsVal),
}

// Ruby-side mirror of js_container_id: New(id) to register and recurse, or
// Reuse(jsval) to emit directly (a Ref to an already-seen object, or a
// depth-truncated Str). Computes object_id once.
fn rb_container_id(seen: &mut RbSeen, val: Value, depth: u32) -> Result<RbId, Error> {
    let oid = val.funcall::<_, _, usize>("object_id", ())?;
    if let Some(id) = seen.map.get(&oid) {
        return Ok(RbId::Reuse(JsVal::Ref(*id)));
    }
    if depth >= MAX_MARSHAL_DEPTH {
        let ruby = Ruby::get().unwrap();
        let s: Value = val.funcall("to_s", ())?;
        let s = lossy_string(s).ok_or_else(|| {
            Error::new(ruby.exception_type_error(), "to_s did not return a String")
        })?;
        return Ok(RbId::Reuse(JsVal::Str(s)));
    }
    let id = seen.next_id;
    seen.next_id += 1;
    seen.map.insert(oid, id);
    Ok(RbId::New(id))
}

#[magnus::init]
fn init(ruby: &Ruby) -> Result<(), Error> {
    let module = ruby.define_module("RustyRacer")?;

    // The isolate (VM) + its isolate-level ops; hands out Contexts.
    let isolate = module.define_class("Isolate", ruby.class_object())?;
    // keyword-arg wrapper Isolate.new(snapshot:, ...) lives in lib/rusty_racer.rb
    isolate.define_singleton_method("_new", function!(Isolate::new, 4))?;
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
    Ok(())
}
