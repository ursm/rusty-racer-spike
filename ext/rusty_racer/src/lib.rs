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
//   - the watchdog joins before the reply and cancels unconditionally if it
//     fired, so audit #3's stale TerminateExecution cannot poison the next
//     request.
//
// Spike simplifications: marshalling is nil/bool/i64/f64/String; attached
// procs are kept alive by the Ruby caller (a real gem adds a GC mark); the
// GVL-released channel waits pass no unblock function.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use magnus::block::Proc;
use magnus::value::{Opaque, ReprValue};
use magnus::{
    function, method, prelude::*, Error, ExceptionClass, IntoValue, RArray, RHash, Ruby, TryConvert,
    Value,
};

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
        reply: Sender<VmReply>,
    },
    ModuleNamespace {
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
    // JS called host fn |id|; run the proc and send the answer back.
    Callback {
        host_fn_id: usize,
        args: Vec<JsVal>,
        answer: Sender<Answer>,
    },
    // instantiate's per-edge resolve: ask the Ruby resolve block for the module
    // that |specifier| (imported by |referrer_url|) refers to.
    ResolveModule {
        specifier: String,
        referrer_url: String,
        answer: Sender<Answer>,
    },
    // JS did import(specifier): ask the Context's dynamic_import_resolver for an
    // already-loaded module to fulfil the import() promise.
    DynamicImport {
        specifier: String,
        referrer_url: String,
        answer: Sender<Answer>,
    },
}

// Ruby thread -> the V8 thread suspended inside a callback / batch round-trip
enum Answer {
    Result(Result<JsVal, String>),
    // the proc's Ruby body called ctx.eval — serve it re-entrantly
    NestedEval {
        source: String,
        filename: String,
        reply: Sender<VmReply>,
    },
    // the proc's Ruby body called ctx.call — serve it re-entrantly
    NestedCall {
        name: String,
        args: Vec<JsVal>,
        void: bool,
        reply: Sender<VmReply>,
    },
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
    // Reply sender of the request currently being served (stack: nested evals
    // arriving through a suspended callback push their own sender).
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

fn jsval_to_js<'s>(scope: &mut v8::PinScope<'s, '_>, val: &JsVal) -> v8::Local<'s, v8::Value> {
    let mut built: HashMap<u32, v8::Local<'s, v8::Value>> = HashMap::new();
    jsval_to_js_d(scope, val, &mut built)
}

fn jsval_to_js_d<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    val: &JsVal,
    built: &mut HashMap<u32, v8::Local<'s, v8::Value>>,
) -> v8::Local<'s, v8::Value> {
    match val {
        JsVal::Undefined => v8::undefined(scope).into(),
        JsVal::Null => v8::null(scope).into(),
        JsVal::Bool(b) => v8::Boolean::new(scope, *b).into(),
        JsVal::Int(i) => v8::Number::new(scope, *i as f64).into(),
        JsVal::Num(n) => v8::Number::new(scope, *n).into(),
        JsVal::Str(s) => v8::String::new(scope, s)
            .map(|s| s.into())
            .unwrap_or_else(|| v8::undefined(scope).into()),
        JsVal::BigInt { negative, words } => v8::BigInt::new_from_words(scope, *negative, words)
            .map(|b| b.into())
            .unwrap_or_else(|| v8::undefined(scope).into()),
        JsVal::Date(ms) => v8::Date::new(scope, *ms)
            .map(|d| d.into())
            .unwrap_or_else(|| v8::undefined(scope).into()),
        // Register the container under its id BEFORE filling it, so a Ref from
        // a descendant (a cycle back to here) resolves to this same object.
        JsVal::Array { id, items } => {
            let arr = v8::Array::new(scope, items.len() as i32);
            built.insert(*id, arr.into());
            for (i, it) in items.iter().enumerate() {
                let v = jsval_to_js_d(scope, it, built);
                arr.set_index(scope, i as u32, v);
            }
            arr.into()
        }
        JsVal::Obj { id, entries } => {
            let obj = v8::Object::new(scope);
            built.insert(*id, obj.into());
            for (k, it) in entries {
                let Some(key) = v8::String::new(scope, k) else {
                    continue;
                };
                let v = jsval_to_js_d(scope, it, built);
                obj.set(scope, key.into(), v);
            }
            obj.into()
        }
        JsVal::Map { id, pairs } => {
            let map = v8::Map::new(scope);
            built.insert(*id, map.into());
            for (k, v) in pairs {
                let kk = jsval_to_js_d(scope, k, built);
                let vv = jsval_to_js_d(scope, v, built);
                map.set(scope, kk, vv);
            }
            map.into()
        }
        JsVal::Set { id, items } => {
            let set = v8::Set::new(scope);
            built.insert(*id, set.into());
            for it in items {
                let v = jsval_to_js_d(scope, it, built);
                set.add(scope, v);
            }
            set.into()
        }
        JsVal::Ref(id) => built
            .get(id)
            .copied()
            .unwrap_or_else(|| v8::undefined(scope).into()),
    }
}

// JS called a host function: round-trip to the Ruby thread that is waiting on
// the current request, serving nested evals until the answer arrives.
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
                let v = jsval_to_js(scope, &val);
                rv.set(v);
                return;
            }
            Ok(Answer::Result(Err(message))) => {
                // The proc raised: surface as a JS exception (audit #24's
                // wedge becomes an ordinary throw).
                throw_js_error(scope, &message);
                return;
            }
            Ok(Answer::NestedEval { source, filename, reply }) => {
                // ruby -> js -> ruby -> js: run it re-entrantly right here.
                let outcome = run_source(scope, &source, &filename);
                let _ = reply.send(VmReply::Done(outcome));
            }
            Ok(Answer::NestedCall { name, args, void, reply }) => {
                // ruby -> js -> ruby calls ctx.call -> js: invoke re-entrantly.
                let outcome = call_function(scope, &name, &args, void);
                let _ = reply.send(VmReply::Done(outcome));
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
    args: &[JsVal],
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
    let argv: Vec<v8::Local<v8::Value>> = args.iter().map(|a| jsval_to_js(tc, a)).collect();
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

thread_local! {
    static MODULES: RefCell<ModuleReg> = RefCell::new(ModuleReg::default());
    static SCRIPTS: RefCell<ScriptReg> = RefCell::new(ScriptReg::default());
    // The context id the V8 thread is currently executing JS in. Set by the
    // handlers that enter a context (eval/call/instantiate/evaluate) so the
    // import callbacks can reject a module from a *different* context — V8
    // CHECK-aborts the process if an import resolves across v8::Contexts.
    static CURRENT_CTX: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
}

// Drop every module AND script compiled in `context_id` (its v8::Context is
// going away — on reset or dispose — so those handles are now dead).
fn drop_context_artifacts(context_id: i32) {
    MODULES.with(|m| {
        let mut m = m.borrow_mut();
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
    });
    SCRIPTS.with(|s| {
        s.borrow_mut().by_id.retain(|_, (_, cid)| *cid != context_id);
    });
}

// A script's (unbound handle, owning context id), for running it in that context.
fn script_handle(script_id: i32) -> Option<(v8::Global<v8::UnboundScript>, i32)> {
    SCRIPTS.with(|s| {
        s.borrow()
            .by_id
            .get(&script_id)
            .map(|(g, cid)| (g.clone(), *cid))
    })
}

// V8 calls this per import edge during InstantiateModule. Maps the referrer to
// its url, round-trips to the Ruby resolve block (carried on REPLY_STACK), and
// returns the module the block named. Blocks the V8 thread on the answer, just
// like host_fn_callback.
fn resolve_imported<'s>(
    context: v8::Local<'s, v8::Context>,
    specifier: v8::Local<'s, v8::String>,
    _import_attributes: v8::Local<'s, v8::FixedArray>,
    referrer: v8::Local<'s, v8::Module>,
) -> Option<v8::Local<'s, v8::Module>> {
    v8::callback_scope!(unsafe scope, context);
    let spec = specifier.to_rust_string_lossy(scope);
    let ref_url = MODULES.with(|m| {
        let m = m.borrow();
        let hash = referrer.get_identity_hash().get();
        m.by_hash
            .get(&hash)?
            .iter()
            .find(|(g, _)| v8::Local::new(scope, g) == referrer)
            .and_then(|(_, id)| m.by_id.get(id).map(|(_, u, _)| u.clone()))
    })?;
    let reply = REPLY_STACK.with(|s| s.borrow().last().cloned())?;
    let (atx, arx) = channel();
    reply
        .send(VmReply::ResolveModule {
            specifier: spec,
            referrer_url: ref_url,
            answer: atx,
        })
        .ok()?;
    let dep_id = match arx.recv() {
        Ok(Answer::ModuleId(Some(id))) => id,
        _ => return None,
    };
    let here = CURRENT_CTX.with(|c| c.get());
    MODULES.with(|m| {
        let m = m.borrow();
        let (g, _, cid) = m.by_id.get(&dep_id)?;
        // Refuse a module from another v8::Context — V8 would CHECK-abort the
        // process; None makes it a recoverable "failed to resolve" instead.
        if *cid != here {
            return None;
        }
        Some(v8::Local::new(scope, g))
    })
}

// V8 calls this for a JS `import(specifier)`. Returns a Promise fulfilled with
// the resolved module's namespace (or rejected). Round-trips to the Context's
// dynamic_import_resolver over the current request's reply channel (REPLY_STACK)
// — so import() only works inside an eval/call. As with instantiate, the
// resolver must return an *already-loaded* module (compiling lazily from the
// resolver would deadlock the parked V8 thread).
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
        if let Some(s) = v8::String::new(scope, msg) {
            let e = v8::Exception::error(scope, s);
            resolver.reject(scope, e);
        }
    };
    let spec = specifier.to_rust_string_lossy(scope);
    let referrer = resource_name.to_rust_string_lossy(scope);
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
            answer: atx,
        })
        .is_err()
    {
        reject(scope, "dynamic import caller went away");
        return Some(promise);
    }
    match arx.recv() {
        Ok(Answer::ModuleId(Some(id))) => {
            // The resolved module must live in the context import() ran in — a
            // foreign-context module would V8-CHECK-abort, so reject instead.
            let here = CURRENT_CTX.with(|c| c.get());
            let g = MODULES.with(|m| {
                m.borrow()
                    .by_id
                    .get(&id)
                    .filter(|(_, _, cid)| *cid == here)
                    .map(|(g, _, _)| g.clone())
            });
            match g {
                // The module must be at least instantiated to read its namespace
                // (get_module_namespace CHECK-aborts otherwise).
                Some(g) => {
                    let module = v8::Local::new(scope, &g);
                    match module.get_status() {
                        // Only a fully-evaluated module has live exports; resolve
                        // with its namespace.
                        v8::ModuleStatus::Evaluated => {
                            let ns = module.get_module_namespace();
                            resolver.resolve(scope, ns);
                        }
                        // A module that threw during evaluation rejects with its
                        // own exception, not a stale namespace.
                        v8::ModuleStatus::Errored => {
                            let exc = module.get_exception();
                            resolver.reject(scope, exc);
                        }
                        // Not yet evaluated: its namespace bindings are in TDZ, so
                        // reject rather than hand back an object that throws on use.
                        _ => reject(scope, "dynamically imported module is not evaluated"),
                    }
                }
                None => reject(scope, "resolved module not found"),
            }
        }
        _ => reject(scope, "import() was not resolved to a module"),
    }
    Some(promise)
}

// Per-request watchdog, split so neither half borrows the isolate (it runs off
// a Send IsolateHandle): start before running JS, finish after. Joining before
// the reply and cancelling unconditionally if it fired keeps a late
// TerminateExecution from poisoning the next request (audit #3).
fn start_watchdog(
    handle: v8::IsolateHandle,
    timeout_ms: u64,
) -> (Sender<()>, Option<std::thread::JoinHandle<bool>>) {
    let (cancel_tx, cancel_rx) = channel::<()>();
    let watchdog = (timeout_ms > 0).then(|| {
        std::thread::spawn(move || {
            match cancel_rx.recv_timeout(Duration::from_millis(timeout_ms)) {
                Ok(()) => false,
                Err(_) => {
                    handle.terminate_execution();
                    true
                }
            }
        })
    });
    (cancel_tx, watchdog)
}

// Returns true if the watchdog fired (caller must cancel_terminate_execution).
fn finish_watchdog(cancel_tx: Sender<()>, watchdog: Option<std::thread::JoinHandle<bool>>) -> bool {
    watchdog.is_some_and(|w| {
        let _ = cancel_tx.send(());
        w.join().unwrap_or(false)
    })
}

fn v8_thread_main(
    rx: Receiver<Request>,
    handle_tx: Sender<v8::IsolateHandle>,
    host_namespace: Option<String>,
    snapshot: Option<Vec<u8>>,
) {
    init_v8();
    // A snapshot blob bakes globalThis state into the isolate: the first
    // Context::new below then deserializes that default context for free.
    let create_params = match snapshot {
        Some(bytes) => v8::CreateParams::default().snapshot_blob(v8::StartupData::from(bytes)),
        None => Default::default(),
    };
    let mut isolate = v8::Isolate::new(create_params);
    // Explicit microtask policy: never auto-drain at end-of-script. Microtasks
    // run only on perform_microtask_checkpoint / the host-namespace
    // drainMicrotasks — the embedder owns the event loop (no real timers/loop).
    isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);
    // JS import() routes here; rejects unless a dynamic_import_resolver is set.
    isolate.set_host_import_module_dynamically_callback(dynamic_import_cb);
    let _ = handle_tx.send(isolate.thread_safe_handle());
    // mut: reset_realm swaps this for a fresh Context in the same warm isolate.
    let mut main_context = {
        v8::scope!(let scope, &mut isolate);
        let context = v8::Context::new(scope, Default::default());
        v8::Global::new(scope, context)
    };
    if let Some(ref name) = host_namespace {
        install_host_namespace(&mut isolate, &main_context, name);
    }
    // Extra contexts from create_context, keyed by id (1, 2, ...). The main realm
    // is id 0 and lives in `main_context` so reset_realm can swap it freely.
    let mut contexts: HashMap<i32, v8::Global<v8::Context>> = HashMap::new();
    let mut next_context_id: i32 = 1;

    while let Ok(request) = rx.recv() {
        match request {
            Request::Eval {
                context_id,
                source,
                filename,
                timeout_ms,
                reply,
            } => {
                REPLY_STACK.with(|s| s.borrow_mut().push(reply.clone()));
                CURRENT_CTX.with(|c| c.set(context_id));
                let (cancel_tx, watchdog) = start_watchdog(isolate.thread_safe_handle(), timeout_ms);
                let outcome = match context_for(&main_context, &contexts, context_id) {
                    Some(ctx) => {
                        v8::scope!(let scope, &mut isolate);
                        let context = v8::Local::new(scope, &ctx);
                        let scope = &mut v8::ContextScope::new(scope, context);
                        run_source(scope, &source, &filename)
                    }
                    None => Err(VmError::Runtime("realm disposed or unknown".into())),
                };
                if finish_watchdog(cancel_tx, watchdog) {
                    isolate.cancel_terminate_execution();
                }
                REPLY_STACK.with(|s| {
                    s.borrow_mut().pop();
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
                // REPLY_STACK so a host fn invoked by the called function routes
                // back to this request's waiter, exactly like Eval.
                REPLY_STACK.with(|s| s.borrow_mut().push(reply.clone()));
                CURRENT_CTX.with(|c| c.set(context_id));
                let (cancel_tx, watchdog) = start_watchdog(isolate.thread_safe_handle(), timeout_ms);
                let outcome = match context_for(&main_context, &contexts, context_id) {
                    Some(ctx) => {
                        v8::scope!(let scope, &mut isolate);
                        let context = v8::Local::new(scope, &ctx);
                        let scope = &mut v8::ContextScope::new(scope, context);
                        call_function(scope, &name, &args, void)
                    }
                    None => Err(VmError::Runtime("realm disposed or unknown".into())),
                };
                if finish_watchdog(cancel_tx, watchdog) {
                    isolate.cancel_terminate_execution();
                }
                REPLY_STACK.with(|s| {
                    s.borrow_mut().pop();
                });
                let _ = reply.send(VmReply::Done(outcome));
            }
            Request::DrainMicrotasks { timeout_ms, reply } => {
                // A microtask may call an attached host fn (a Promise .then ->
                // ruby), so push the reply onto REPLY_STACK exactly like Eval,
                // or that callback would find no waiter and silently no-op.
                REPLY_STACK.with(|s| s.borrow_mut().push(reply.clone()));
                CURRENT_CTX.with(|c| c.set(0)); // the checkpoint runs in the main context
                let (cancel_tx, watchdog) = start_watchdog(isolate.thread_safe_handle(), timeout_ms);
                {
                    v8::scope!(let scope, &mut isolate);
                    let context = v8::Local::new(scope, &main_context);
                    let scope = &mut v8::ContextScope::new(scope, context);
                    scope.perform_microtask_checkpoint();
                }
                let fired = finish_watchdog(cancel_tx, watchdog);
                if fired {
                    isolate.cancel_terminate_execution();
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
                reply,
            } => {
                let outcome = match context_for(&main_context, &contexts, context_id) {
                    Some(ctx) => {
                        v8::scope!(let scope, &mut isolate);
                        let context = v8::Local::new(scope, &ctx);
                        let scope = &mut v8::ContextScope::new(scope, context);
                        let external = v8::External::new(scope, host_fn_id as *mut c_void);
                        match v8::Function::builder(host_fn_callback)
                            .data(external.into())
                            .build(scope)
                        {
                            // A dotted name (e.g. "MiniRacer.foo") attaches under
                            // a namespace object, creating missing intermediates,
                            // so host fns needn't pollute the bare global.
                            Some(function) => attach_at_path(scope, context, &name, function),
                            None => Err(VmError::Runtime("failed to build function".into())),
                        }
                    }
                    None => Err(VmError::Runtime("realm disposed or unknown".into())),
                };
                let _ = reply.send(VmReply::Done(outcome));
            }
            Request::Reset { context_id, reply } => {
                if context_id != 0 && !contexts.contains_key(&context_id) {
                    let _ = reply.send(VmReply::Done(Err(VmError::Runtime(
                        "context disposed or unknown".into(),
                    ))));
                    continue;
                }
                let fresh = {
                    v8::scope!(let scope, &mut isolate);
                    let context = v8::Context::new(scope, Default::default());
                    v8::Global::new(scope, context)
                };
                if let Some(ref name) = host_namespace {
                    install_host_namespace(&mut isolate, &fresh, name);
                }
                if context_id == 0 {
                    main_context = fresh;
                } else {
                    contexts.insert(context_id, fresh);
                }
                // Drop modules bound to this context — their realm just changed.
                drop_context_artifacts(context_id);
                let _ = reply.send(VmReply::Done(Ok(JsVal::Undefined)));
            }
            Request::CreateContext { reply } => {
                let id = next_context_id;
                next_context_id += 1;
                let fresh = {
                    v8::scope!(let scope, &mut isolate);
                    let context = v8::Context::new(scope, Default::default());
                    v8::Global::new(scope, context)
                };
                if let Some(ref name) = host_namespace {
                    install_host_namespace(&mut isolate, &fresh, name);
                }
                contexts.insert(id, fresh);
                let _ = reply.send(VmReply::Done(Ok(JsVal::Int(id as i64))));
            }
            Request::DisposeContext { context_id, reply } => {
                // Dropping the Global lets V8 collect the context. id 0 is the
                // default context and never disposed independently.
                contexts.remove(&context_id);
                // Reclaim the modules compiled in it (else they leak until
                // isolate teardown).
                drop_context_artifacts(context_id);
                let _ = reply.send(VmReply::Done(Ok(JsVal::Undefined)));
            }
            Request::CompileModule {
                context_id,
                source,
                filename,
                cached_data,
                produce_cache,
                reply,
            } => {
                CURRENT_CTX.with(|c| c.set(context_id));
                let outcome = match context_for(&main_context, &contexts, context_id) {
                    None => Err(VmError::Runtime("context disposed or unknown".into())),
                    Some(cx) => {
                    v8::scope!(let scope, &mut isolate);
                    let context = v8::Local::new(scope, &cx);
                    let scope = &mut v8::ContextScope::new(scope, context);
                    v8::tc_scope!(let tc, scope);
                    match v8::String::new(tc, &source) {
                        None => Err(VmError::Runtime("module source too large".into())),
                        Some(code) => {
                            let origin = module_origin(tc, &filename);
                            // Consume a supplied bytecode cache (skip reparse) or
                            // compile fresh.
                            let (mut src, opts) = match &cached_data {
                                Some(bytes) => (
                                    v8::script_compiler::Source::new_with_cached_data(
                                        code,
                                        Some(&origin),
                                        v8::script_compiler::CachedData::new(bytes),
                                    ),
                                    v8::script_compiler::CompileOptions::ConsumeCodeCache,
                                ),
                                None => (
                                    v8::script_compiler::Source::new(code, Some(&origin)),
                                    v8::script_compiler::CompileOptions::NoCompileOptions,
                                ),
                            };
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
                                    let id = MODULES.with(|m| {
                                        let mut m = m.borrow_mut();
                                        let id = m.next_id;
                                        m.next_id += 1;
                                        let hash = module.get_identity_hash().get();
                                        let g = v8::Global::new(tc, module);
                                        m.by_id
                                            .insert(id, (g.clone(), filename.clone(), context_id));
                                        m.by_hash.entry(hash).or_default().push((g, id));
                                        id
                                    });
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
                // REPLY_STACK so resolve_imported can round-trip per import edge.
                REPLY_STACK.with(|s| s.borrow_mut().push(reply.clone()));
                let outcome = match module_handle(module_id) {
                    None => Err(VmError::Runtime("unknown module".into())),
                    Some((g, cid)) => match context_for(&main_context, &contexts, cid) {
                        None => Err(VmError::Runtime("module's context is gone".into())),
                        Some(cx) => {
                            CURRENT_CTX.with(|c| c.set(cid));
                            v8::scope!(let scope, &mut isolate);
                            let context = v8::Local::new(scope, &cx);
                            let scope = &mut v8::ContextScope::new(scope, context);
                            let module = v8::Local::new(scope, &g);
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
                };
                REPLY_STACK.with(|s| {
                    s.borrow_mut().pop();
                });
                let _ = reply.send(VmReply::Done(outcome));
            }
            Request::EvaluateModule { module_id, reply } => {
                // Top-level module code may call host fns -> REPLY_STACK.
                REPLY_STACK.with(|s| s.borrow_mut().push(reply.clone()));
                let outcome = match module_handle(module_id) {
                    None => Err(VmError::Runtime("unknown module".into())),
                    Some((g, cid)) => match context_for(&main_context, &contexts, cid) {
                        None => Err(VmError::Runtime("module's context is gone".into())),
                        Some(cx) => {
                            CURRENT_CTX.with(|c| c.set(cid));
                            v8::scope!(let scope, &mut isolate);
                            let context = v8::Local::new(scope, &cx);
                            let scope = &mut v8::ContextScope::new(scope, context);
                            let module = v8::Local::new(scope, &g);
                            // V8 CHECK-aborts the process if evaluate runs on a
                            // module that isn't exactly Instantiated, so guard
                            // status explicitly rather than crash.
                            match module.get_status() {
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
                                    v8::tc_scope!(let tc, scope);
                                    match module.evaluate(tc) {
                                        // evaluate returns a Promise; a synchronous
                                        // top-level throw yields a *rejected*
                                        // promise (not None), so check it — else the
                                        // error is silently lost. A pending (TLA) or
                                        // fulfilled promise is left for the embedder
                                        // to drain (explicit microtask policy).
                                        Some(value) => match v8::Local::<v8::Promise>::try_from(value) {
                                            Ok(p) if p.state() == v8::PromiseState::Rejected => {
                                                let reason = p.result(tc);
                                                Err(VmError::JsError {
                                                    message: reason.to_rust_string_lossy(tc),
                                                    backtrace: vec![],
                                                })
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
                            }
                        }
                    }
                };
                REPLY_STACK.with(|s| {
                    s.borrow_mut().pop();
                });
                let _ = reply.send(VmReply::Done(outcome));
            }
            Request::ModuleNamespace { module_id, reply } => {
                let outcome = match module_handle(module_id) {
                    None => Err(VmError::Runtime("unknown module".into())),
                    Some((g, cid)) => match context_for(&main_context, &contexts, cid) {
                        None => Err(VmError::Runtime("module's context is gone".into())),
                        Some(cx) => {
                            v8::scope!(let scope, &mut isolate);
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
            Request::DisposeModule { module_id, reply } => {
                MODULES.with(|m| {
                    let mut m = m.borrow_mut();
                    m.by_id.remove(&module_id);
                    for bucket in m.by_hash.values_mut() {
                        bucket.retain(|(_, id)| *id != module_id);
                    }
                });
                let _ = reply.send(VmReply::Done(Ok(JsVal::Undefined)));
            }
            Request::CompileScript {
                context_id,
                source,
                filename,
                cached_data,
                produce_cache,
                reply,
            } => {
                let outcome = match context_for(&main_context, &contexts, context_id) {
                    None => Err(VmError::Runtime("context disposed or unknown".into())),
                    Some(cx) => {
                        v8::scope!(let scope, &mut isolate);
                        let context = v8::Local::new(scope, &cx);
                        let scope = &mut v8::ContextScope::new(scope, context);
                        v8::tc_scope!(let tc, scope);
                        match v8::String::new(tc, &source) {
                            None => Err(VmError::Runtime("script source too large".into())),
                            Some(code) => {
                                let origin = script_origin(tc, &filename);
                                let (mut src, opts) = match &cached_data {
                                    Some(bytes) => (
                                        v8::script_compiler::Source::new_with_cached_data(
                                            code,
                                            Some(&origin),
                                            v8::script_compiler::CachedData::new(bytes),
                                        ),
                                        v8::script_compiler::CompileOptions::ConsumeCodeCache,
                                    ),
                                    None => (
                                        v8::script_compiler::Source::new(code, Some(&origin)),
                                        v8::script_compiler::CompileOptions::NoCompileOptions,
                                    ),
                                };
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
                                        let id = SCRIPTS.with(|s| {
                                            let mut s = s.borrow_mut();
                                            let id = s.next_id;
                                            s.next_id += 1;
                                            let g = v8::Global::new(tc, unbound);
                                            s.by_id.insert(id, (g, context_id));
                                            id
                                        });
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
                // REPLY_STACK so a host fn the script calls routes back, like Eval.
                REPLY_STACK.with(|s| s.borrow_mut().push(reply.clone()));
                let (cancel_tx, watchdog) = start_watchdog(isolate.thread_safe_handle(), timeout_ms);
                let outcome = match script_handle(script_id) {
                    None => Err(VmError::Runtime("unknown script".into())),
                    Some((g, cid)) => match context_for(&main_context, &contexts, cid) {
                        None => Err(VmError::Runtime("script's context is gone".into())),
                        Some(cx) => {
                            CURRENT_CTX.with(|c| c.set(cid));
                            v8::scope!(let scope, &mut isolate);
                            let context = v8::Local::new(scope, &cx);
                            let scope = &mut v8::ContextScope::new(scope, context);
                            let unbound = v8::Local::new(scope, &g);
                            let script = unbound.bind_to_current_context(scope);
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
                        }
                    },
                };
                if finish_watchdog(cancel_tx, watchdog) {
                    isolate.cancel_terminate_execution();
                }
                REPLY_STACK.with(|s| {
                    s.borrow_mut().pop();
                });
                let _ = reply.send(VmReply::Done(outcome));
            }
            Request::DisposeScript { script_id, reply } => {
                SCRIPTS.with(|s| {
                    s.borrow_mut().by_id.remove(&script_id);
                });
                let _ = reply.send(VmReply::Done(Ok(JsVal::Undefined)));
            }
            Request::Dispose => break,
        }
    }
    // Every v8::Global must die before the isolate it points into; dropping
    // them here (before isolate) makes the order explicit.
    drop(contexts);
    drop(main_context);
    drop(isolate);
}

// Pick the Global context for a realm id: 0 = main, N = an extra realm (None
// if it was disposed or never existed). Clones the Global (cheap, refcounted)
// so the caller can open a scope on &mut isolate without aliasing.
fn context_for(
    main: &v8::Global<v8::Context>,
    contexts: &HashMap<i32, v8::Global<v8::Context>>,
    context_id: i32,
) -> Option<v8::Global<v8::Context>> {
    if context_id == 0 {
        Some(main.clone())
    } else {
        contexts.get(&context_id).cloned()
    }
}

// A module's (handle, owning context id), for running its ops in the right
// v8::Context.
fn module_handle(module_id: i32) -> Option<(v8::Global<v8::Module>, i32)> {
    MODULES.with(|m| {
        m.borrow()
            .by_id
            .get(&module_id)
            .map(|(g, _, cid)| (g.clone(), *cid))
    })
}

// ---------------------------------------------------------------------------
// Ruby side
// ---------------------------------------------------------------------------
thread_local! {
    // Answer senders for callbacks this Ruby thread is currently serving;
    // a ctx.eval from inside a proc routes through the top as a NestedEval.
    static NESTED: RefCell<Vec<Sender<Answer>>> = const { RefCell::new(Vec::new()) };
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
    shared: Mutex<Shared>,
    // Shared across contexts (host_fn_id indexes this one vector). Mutex (not
    // RefCell) because contexts of one Context may be pumped on different threads.
    procs: Mutex<Vec<Opaque<Proc>>>,
    // Default per-eval/call timeout (ms); 0 = none. eval(timeout_ms:)'s explicit
    // value overrides it. Guards against an in-V8 infinite loop without a watchdog.
    default_timeout_ms: u64,
    // Set by Context#dynamic_import_resolver=; called for a JS import() to map
    // (specifier, referrer) to an already-loaded Module.
    dynamic_import_resolver: Mutex<Option<Opaque<Proc>>>,
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

// Inject globalThis.<name> = { drainMicrotasks } into a context. Re-run on
// reset_realm so the fresh realm keeps the namespace.
fn install_host_namespace(isolate: &mut v8::Isolate, ctx: &v8::Global<v8::Context>, name: &str) {
    v8::scope!(let scope, isolate);
    let context = v8::Local::new(scope, ctx);
    let scope = &mut v8::ContextScope::new(scope, context);
    let ns = v8::Object::new(scope);
    if let (Some(f), Some(k)) = (
        v8::Function::new(scope, drain_microtasks),
        v8::String::new(scope, "drainMicrotasks"),
    ) {
        ns.set(scope, k.into(), f.into());
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
// its default context. Runs entirely on the calling (Ruby) thread: the
// OwnedIsolate is a local, never stored in a Send wrapper, so the !Send dedicated
// -thread rule doesn't apply. |base| warms an existing blob further.
//
// NB: unlike Eval there is no watchdog here and the GVL is held throughout, so
// |code| must be trusted setup — an infinite loop would freeze the whole Ruby
// VM. Snapshot/warmup code is author-controlled, so that's an accepted tradeoff.
fn build_snapshot(code: &str, base: Option<Vec<u8>>) -> Result<Vec<u8>, String> {
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
                if let Err(e) = run_source(cscope, code, "<snapshot>") {
                    err = Some(match e {
                        VmError::Parse(m) | VmError::Runtime(m) => m,
                        VmError::JsError { message, .. } => message,
                        VmError::Terminated => "snapshot code was terminated".to_string(),
                    });
                }
            }
        }
        // Mark this context as the one to deserialize on boot (after the
        // ContextScope is dropped, like denoland/rusty_v8's snapshot path).
        scope.set_default_context(context);
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
    ) -> Result<Self, Error> {
        let snapshot_bytes = snapshot.map(|s| s.blob.borrow().clone());
        let (tx, rx) = channel::<Request>();
        let (handle_tx, handle_rx) = channel::<v8::IsolateHandle>();
        std::thread::spawn(move || {
            v8_thread_main(rx, handle_tx, host_namespace, snapshot_bytes)
        });
        let handle = handle_rx
            .recv()
            .map_err(|_| Error::new(ruby.exception_runtime_error(), "V8 thread failed to boot"))?;
        Ok(Isolate {
            core: Arc::new(Core {
                shared: Mutex::new(Shared {
                    tx,
                    handle,
                    disposed: false,
                }),
                procs: Mutex::new(Vec::new()),
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
                // compile_module / compile receive these directly, never via pump.
                Ok(VmReply::ModuleCompiled(_)) | Ok(VmReply::ScriptCompiled(_)) => {
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
                    answer,
                }) => {
                    match &module_resolve {
                        Some(resolve) => {
                            match resolve_module_via_ruby(self, *resolve, &specifier, &referrer_url) {
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
                            let _ = answer.send(Answer::ModuleId(None));
                        }
                    }
                }
                Ok(VmReply::DynamicImport {
                    specifier,
                    referrer_url,
                    answer,
                }) => {
                    // Read the Context's dynamic_import_resolver (set via the
                    // setter); reuse the same Module-id resolution as instantiate.
                    let resolver = {
                        let guard = self.dynamic_import_resolver.lock().unwrap();
                        guard.map(|o| ruby.get_inner(o))
                    };
                    match resolver {
                        Some(proc) => {
                            match resolve_module_via_ruby(self, proc, &specifier, &referrer_url) {
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
            let opaque = procs.get(host_fn_id).ok_or("unknown host function")?;
            ruby.get_inner(*opaque)
        };
        let ruby_args: Vec<Value> = args
            .iter()
            .map(|v| jsval_to_ruby(ruby, v))
            .collect::<Result<_, Error>>()
            .map_err(|e| e.to_string())?;
        NESTED.with(|n| n.borrow_mut().push(answer.clone()));
        let result: Result<Value, Error> = proc.call(ruby_args.as_slice());
        NESTED.with(|n| {
            n.borrow_mut().pop();
        });
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

        // Inside a proc serving a callback? Route through the suspended frame so
        // we don't deadlock on the busy main queue (mirrors eval_t). Like nested
        // eval, the nested call runs in the suspended frame's realm — a nested
        // realm.call targeting a *different* realm than that frame is not
        // supported (the common same-realm re-entry is correct).
        let nested = NESTED.with(|n| n.borrow().last().cloned());
        if let Some(answer) = nested {
            let (reply_tx, reply_rx) = channel::<VmReply>();
            answer
                .send(Answer::NestedCall {
                    name,
                    args: jsargs,
                    void,
                    reply: reply_tx,
                })
                .map_err(|_| Error::new(ruby.exception_runtime_error(), "V8 thread is gone"))?;
            return self.pump(ruby, reply_rx, None);
        }

        let (reply_tx, reply_rx) = channel::<VmReply>();
        self.send(
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
        self.send(
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
        // Inside a proc serving a callback? Route as a nested eval through the
        // suspended V8 frame instead of the main queue (which is busy). The
        // nested eval runs in whatever realm that frame is already in.
        let nested = NESTED.with(|n| n.borrow().last().cloned());
        if let Some(answer) = nested {
            let (reply_tx, reply_rx) = channel::<VmReply>();
            answer
                .send(Answer::NestedEval {
                    source,
                    filename,
                    reply: reply_tx,
                })
                .map_err(|_| Error::new(ruby.exception_runtime_error(), "V8 thread is gone"))?;
            return self.pump(ruby, reply_rx, None);
        }

        let (reply_tx, reply_rx) = channel::<VmReply>();
        self.send(
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
        let host_fn_id = {
            let mut procs = self.procs.lock().unwrap();
            procs.push(Opaque::from(proc));
            procs.len() - 1
        };
        let (reply_tx, reply_rx) = channel::<VmReply>();
        self.send(
            ruby,
            Request::Attach {
                context_id,
                name,
                host_fn_id,
                reply: reply_tx,
            },
        )?;
        self.pump(ruby, reply_rx, None)
    }

    fn reset(&self, ruby: &Ruby, context_id: i32) -> Result<Value, Error> {
        let (reply_tx, reply_rx) = channel::<VmReply>();
        self.send(ruby, Request::Reset { context_id, reply: reply_tx })?;
        self.pump(ruby, reply_rx, None)
    }

    // Build a new context; returns its id (the V8 thread replies with an Int).
    fn create_context(&self, ruby: &Ruby) -> Result<i32, Error> {
        let (reply_tx, reply_rx) = channel::<VmReply>();
        self.send(ruby, Request::CreateContext { reply: reply_tx })?;
        let id = self.pump(ruby, reply_rx, None)?;
        i32::try_convert(id)
    }

    fn dispose_context(&self, ruby: &Ruby, context_id: i32) -> Result<(), Error> {
        let (reply_tx, reply_rx) = channel::<VmReply>();
        self.send(ruby, Request::DisposeContext { context_id, reply: reply_tx })?;
        self.pump(ruby, reply_rx, None).map(|_| ())
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
    ) -> Result<Compiled, Error> {
        let (reply_tx, reply_rx) = channel::<VmReply>();
        self.send(
            ruby,
            Request::CompileModule {
                context_id,
                source,
                filename,
                cached_data,
                produce_cache,
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
    // it per import edge. The block must return an *already-compiled* Module (or
    // nil): the V8 thread is parked inside InstantiateModule awaiting the answer,
    // so compiling/instantiating lazily from within the block would deadlock the
    // main request queue. csim pre-compiles and the block just looks up.
    fn instantiate_module(&self, ruby: &Ruby, module_id: i32, resolve: Proc) -> Result<Value, Error> {
        let (reply_tx, reply_rx) = channel::<VmReply>();
        self.send(ruby, Request::InstantiateModule { module_id, reply: reply_tx })?;
        self.pump(ruby, reply_rx, Some(resolve))
    }

    fn evaluate_module(&self, ruby: &Ruby, module_id: i32) -> Result<Value, Error> {
        let (reply_tx, reply_rx) = channel::<VmReply>();
        self.send(ruby, Request::EvaluateModule { module_id, reply: reply_tx })?;
        self.pump(ruby, reply_rx, None)
    }

    fn module_namespace(&self, ruby: &Ruby, module_id: i32) -> Result<Value, Error> {
        let (reply_tx, reply_rx) = channel::<VmReply>();
        self.send(ruby, Request::ModuleNamespace { module_id, reply: reply_tx })?;
        self.pump(ruby, reply_rx, None)
    }

    fn dispose_module(&self, ruby: &Ruby, module_id: i32) -> Result<(), Error> {
        let (reply_tx, reply_rx) = channel::<VmReply>();
        self.send(ruby, Request::DisposeModule { module_id, reply: reply_tx })?;
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
    ) -> Result<Compiled, Error> {
        let (reply_tx, reply_rx) = channel::<VmReply>();
        self.send(
            ruby,
            Request::CompileScript {
                context_id,
                source,
                filename,
                cached_data,
                produce_cache,
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
        self.send(
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
        self.send(ruby, Request::DisposeScript { script_id, reply: reply_tx })?;
        self.pump(ruby, reply_rx, None).map(|_| ())
    }

    fn set_dynamic_import_resolver(&self, proc: Proc) {
        *self.dynamic_import_resolver.lock().unwrap() = Some(Opaque::from(proc));
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
    ) -> Result<JsModule, Error> {
        rb_self.check_live(ruby)?;
        let cache_in = binary_bytes(ruby, cached_data)?;
        let cm = rb_self
            .core
            .compile_module(ruby, rb_self.id, source, filename, cache_in, produce_cache)?;
        Ok(JsModule {
            core: rb_self.core.clone(),
            module_id: cm.id,
            disposed: AtomicBool::new(false),
            cached_data: cm.cached_data,
            cache_rejected: cm.cache_rejected,
        })
    }
    // compile(source, filename:, cached_data:, produce_cache:) -> Script: a
    // classic <script>. Same cache semantics as compile_module.
    fn compile(
        ruby: &Ruby,
        rb_self: &Self,
        source: String,
        filename: String,
        cached_data: Option<magnus::RString>,
        produce_cache: bool,
    ) -> Result<Script, Error> {
        rb_self.check_live(ruby)?;
        let cache_in = binary_bytes(ruby, cached_data)?;
        let cs = rb_self
            .core
            .compile_script(ruby, rb_self.id, source, filename, cache_in, produce_cache)?;
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
        let blob = build_snapshot(&code, None)
            .map_err(|m| Error::new(err_class(&ruby, "SnapshotError"), m))?;
        Ok(Snapshot {
            blob: RefCell::new(blob),
        })
    }

    // Snapshot.load(blob) — rewrap raw bytes (no validation until boot).
    fn load(blob: magnus::RString) -> Snapshot {
        // Safe: the slice is copied into an owned Vec before any Ruby code
        // (which could move/free the string) can run.
        let bytes = unsafe { blob.as_slice() }.to_vec();
        Snapshot {
            blob: RefCell::new(bytes),
        }
    }

    // warmup!(code) — re-snapshot the existing blob with extra code so its
    // functions are pre-compiled. Spike: returns nil (csim returns self).
    fn warmup(ruby: &Ruby, rb_self: &Self, code: String) -> Result<(), Error> {
        let base = rb_self.blob.borrow().clone();
        let blob = build_snapshot(&code, Some(base))
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
    // The bytecode cache produced at compile (produce_cache: true), as a binary
    // String, or nil. Persist it cross-process and pass back via cached_data:.
    fn cached_data(ruby: &Ruby, rb_self: &Self) -> Value {
        match &rb_self.cached_data {
            Some(bytes) => ruby.str_from_slice(bytes).as_value(),
            None => ruby.qnil().as_value(),
        }
    }
    // True if a cached_data: supplied at compile was stale/incompatible and V8
    // recompiled from source instead.
    fn cache_rejected(&self) -> bool {
        self.cache_rejected
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
        match &rb_self.cached_data {
            Some(bytes) => ruby.str_from_slice(bytes).as_value(),
            None => ruby.qnil().as_value(),
        }
    }
    fn cache_rejected(&self) -> bool {
        self.cache_rejected
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
) -> Result<Option<i32>, Error> {
    let ruby = Ruby::get().unwrap();
    let ret: Value = resolve.call((specifier, referrer_url))?;
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
    if let Ok(s) = String::try_convert(val) {
        return Ok(JsVal::Str(s));
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
            // String/Symbol keys -> String; anything else via to_s.
            let key = String::try_convert(k).or_else(|_| k.funcall::<_, _, String>("to_s", ()))?;
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
        return Ok(RbId::Reuse(JsVal::Str(val.funcall::<_, _, String>("to_s", ())?)));
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
    isolate.define_singleton_method("_new", function!(Isolate::new, 3))?;
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
    context.define_method("reset", method!(Context::reset, 0))?;
    context.define_method("id", method!(Context::id, 0))?;
    // keyword-arg wrappers Context#compile_module / #compile (source, ...) in lib.
    context.define_method("_compile_module", method!(Context::compile_module, 4))?;
    context.define_method("_compile", method!(Context::compile, 4))?;
    context.define_method("dispose", method!(Context::dispose, 0))?;
    context.define_method("disposed?", method!(Context::disposed, 0))?;

    // Classic compiled script: Context#compile -> #run / #cached_data.
    let script = module.define_class("Script", ruby.class_object())?;
    script.define_method("run", method!(Script::run, 0))?;
    script.define_method("cached_data", method!(Script::cached_data, 0))?;
    script.define_method("cache_rejected?", method!(Script::cache_rejected, 0))?;
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
    jsmodule.define_method("cached_data", method!(JsModule::cached_data, 0))?;
    jsmodule.define_method("cache_rejected?", method!(JsModule::cache_rejected, 0))?;
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
