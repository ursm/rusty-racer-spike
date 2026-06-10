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
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Mutex, Once};
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
    Array(Vec<JsVal>),
    // JS object / Ruby Hash with string keys (mini_racer marshals objects to
    // string-keyed Hashes). Insertion order preserved.
    Obj(Vec<(String, JsVal)>),
}

// Recursion bound so a cyclic object graph degrades to a leaf instead of
// overflowing the stack (the hand-rolled serde.c uses a visited-set; the spike
// uses a depth cap — a real impl would mirror ValueSerializer's ref table).
const MAX_MARSHAL_DEPTH: u32 = 64;

#[derive(Debug)]
enum VmError {
    Parse(String),   // compile-time failure -> RustyRacer::ParseError
    Runtime(String), // runtime JS exception -> RustyRacer::RuntimeError
    Terminated,      // watchdog/stop -> RustyRacer::ScriptTerminatedError
}

// Ruby thread -> V8 thread
enum Request {
    Eval {
        source: String,
        timeout_ms: u64,
        reply: Sender<VmReply>,
    },
    Attach {
        name: String,
        host_fn_id: usize,
        reply: Sender<VmReply>,
    },
    // reset_realm: swap globalThis for a fresh Context in the same (warm)
    // isolate — csim's per-visit reset.
    Reset {
        reply: Sender<VmReply>,
    },
    // load_module_graph: walk the static import graph on the V8 thread,
    // round-tripping fetch/resolve batches to Ruby, then instantiate + evaluate.
    LoadModuleGraph {
        entry_url: String,
        reply: Sender<VmReply>,
    },
    Dispose,
}

// V8 thread -> the Ruby thread that is waiting on this request
enum VmReply {
    Done(Result<JsVal, VmError>),
    // load_module_graph result: the URLs newly compiled this load (csim builds
    // its {modules: [...]} from these), or an error.
    ModuleGraphDone(Result<Vec<String>, VmError>),
    // JS called host fn |id|; run the proc and send the answer back.
    Callback {
        host_fn_id: usize,
        args: Vec<JsVal>,
        answer: Sender<Answer>,
    },
    // Module-graph fetch batch: ask Ruby's fetch_batch proc for these URLs'
    // sources (one round-trip per graph level).
    FetchBatch {
        urls: Vec<String>,
        answer: Sender<Answer>,
    },
    // Module-graph resolve batch: ask Ruby's resolve proc to map these
    // (specifier, referrer) edges to URLs.
    ResolveBatch {
        edges: Vec<(String, String)>,
        answer: Sender<Answer>,
    },
}

// Ruby thread -> the V8 thread suspended inside a callback / batch round-trip
enum Answer {
    Result(Result<JsVal, String>),
    // the proc's Ruby body called ctx.eval — serve it re-entrantly
    NestedEval {
        source: String,
        reply: Sender<VmReply>,
    },
    // per-URL module source (None = fetch failed / 404)
    FetchResult(Vec<Option<String>>),
    // per-edge resolved URL (None = unresolved)
    ResolveResult(Vec<Option<String>>),
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

fn js_to_jsval(scope: &mut v8::PinScope<'_, '_>, value: v8::Local<v8::Value>) -> JsVal {
    js_to_jsval_d(scope, value, 0)
}

fn js_to_jsval_d(scope: &mut v8::PinScope<'_, '_>, value: v8::Local<v8::Value>, depth: u32) -> JsVal {
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
    if depth < MAX_MARSHAL_DEPTH && value.is_array() {
        let arr = v8::Local::<v8::Array>::try_from(value).unwrap();
        let mut out = Vec::with_capacity(arr.length() as usize);
        for i in 0..arr.length() {
            let el = arr
                .get_index(scope, i)
                .unwrap_or_else(|| v8::undefined(scope).into());
            out.push(js_to_jsval_d(scope, el, depth + 1));
        }
        return JsVal::Array(out);
    }
    // Plain object -> string-keyed Obj. Functions/Date/etc. fall through to
    // their toString (the spike's primitive escape hatch).
    if depth < MAX_MARSHAL_DEPTH && value.is_object() && !value.is_function() {
        let obj = v8::Local::<v8::Object>::try_from(value).unwrap();
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
                entries.push((key_str, js_to_jsval_d(scope, val, depth + 1)));
            }
            return JsVal::Obj(entries);
        }
    }
    JsVal::Str(value.to_rust_string_lossy(scope))
}

fn jsval_to_js<'s>(scope: &mut v8::PinScope<'s, '_>, val: &JsVal) -> v8::Local<'s, v8::Value> {
    match val {
        JsVal::Undefined => v8::undefined(scope).into(),
        JsVal::Null => v8::null(scope).into(),
        JsVal::Bool(b) => v8::Boolean::new(scope, *b).into(),
        JsVal::Int(i) => v8::Number::new(scope, *i as f64).into(),
        JsVal::Num(n) => v8::Number::new(scope, *n).into(),
        JsVal::Str(s) => v8::String::new(scope, s)
            .map(|s| s.into())
            .unwrap_or_else(|| v8::undefined(scope).into()),
        JsVal::Array(items) => {
            let arr = v8::Array::new(scope, items.len() as i32);
            for (i, it) in items.iter().enumerate() {
                let v = jsval_to_js(scope, it);
                arr.set_index(scope, i as u32, v);
            }
            arr.into()
        }
        JsVal::Obj(entries) => {
            let obj = v8::Object::new(scope);
            for (k, it) in entries {
                let Some(key) = v8::String::new(scope, k) else {
                    continue;
                };
                let v = jsval_to_js(scope, it);
                obj.set(scope, key.into(), v);
            }
            obj.into()
        }
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
            Ok(Answer::NestedEval { source, reply }) => {
                // ruby -> js -> ruby -> js: run it re-entrantly right here.
                let outcome = run_source(scope, &source);
                let _ = reply.send(VmReply::Done(outcome));
            }
            Ok(Answer::FetchResult(_)) | Ok(Answer::ResolveResult(_)) => {
                // Module-graph answers can't arrive on a host-fn channel.
                throw_js_error(scope, "unexpected module-graph answer in host callback");
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

fn run_source(scope: &mut v8::PinScope<'_, '_>, source: &str) -> Result<JsVal, VmError> {
    v8::tc_scope!(let tc, scope);
    // Compile and run as distinct phases so a compile failure maps to
    // ParseError and a thrown exception to RuntimeError (csim rescues both).
    let Some(code) = v8::String::new(tc, source) else {
        return Err(VmError::Parse("source too large".into()));
    };
    let script = match v8::Script::compile(tc, code, None) {
        Some(script) => script,
        None if tc.has_terminated() => return Err(VmError::Terminated),
        None => {
            let msg = tc
                .exception()
                .map(|e| e.to_rust_string_lossy(tc))
                .unwrap_or_else(|| "parse error".to_string());
            return Err(VmError::Parse(msg));
        }
    };
    match script.run(tc) {
        Some(value) => Ok(js_to_jsval(tc, value)),
        None if tc.has_terminated() => Err(VmError::Terminated),
        None => {
            let msg = tc
                .exception()
                .map(|e| e.to_rust_string_lossy(tc))
                .unwrap_or_else(|| "unexpected failure".to_string());
            Err(VmError::Runtime(msg))
        }
    }
}

// ---------------------------------------------------------------------------
// Module graph (csim's load_module_graph): stage1's level-walk, but the fetch
// and resolve batches round-trip to Ruby over the rendezvous (like host fns)
// instead of in-process closures. The registry is a per-V8-thread thread_local
// (one isolate per thread); reset_realm clears it.
// ---------------------------------------------------------------------------
#[derive(Default)]
struct Registry {
    by_url: HashMap<String, v8::Global<v8::Module>>,
    url_by_hash: HashMap<i32, Vec<(v8::Global<v8::Module>, String)>>,
    edges: HashMap<(String, String), String>,
}

impl Registry {
    fn clear(&mut self) {
        self.by_url.clear();
        self.url_by_hash.clear();
        self.edges.clear();
    }
}

thread_local! {
    static REGISTRY: RefCell<Registry> = RefCell::new(Registry::default());
}

fn module_origin<'s>(scope: &v8::PinScope<'s, '_>, url: &str) -> v8::ScriptOrigin<'s> {
    let name = v8::String::new(scope, url).unwrap();
    v8::ScriptOrigin::new(
        scope, name.into(), 0, 0, false, -1, None, false, false, /*is_module*/ true, None,
    )
}

// V8 calls this per import edge during InstantiateModule. Pure registry lookup.
fn resolve_module<'s>(
    context: v8::Local<'s, v8::Context>,
    specifier: v8::Local<'s, v8::String>,
    _import_attributes: v8::Local<'s, v8::FixedArray>,
    referrer: v8::Local<'s, v8::Module>,
) -> Option<v8::Local<'s, v8::Module>> {
    v8::callback_scope!(unsafe scope, context);
    let spec = specifier.to_rust_string_lossy(scope);
    REGISTRY.with(|r| {
        let r = r.borrow();
        let hash = referrer.get_identity_hash().get();
        let ref_url = r
            .url_by_hash
            .get(&hash)?
            .iter()
            .find(|(g, _)| v8::Local::new(scope, g) == referrer)
            .map(|(_, u)| u.clone())?;
        let url = r.edges.get(&(ref_url, spec))?;
        Some(v8::Local::new(scope, r.by_url.get(url)?))
    })
}

// Round-trip a batch to Ruby; blocks the V8 thread until the answer arrives
// (exactly like host_fn_callback). The reply Sender is the current request's.
fn ruby_fetch(reply: &Sender<VmReply>, urls: &[String]) -> Option<Vec<Option<String>>> {
    let (atx, arx) = channel();
    reply
        .send(VmReply::FetchBatch { urls: urls.to_vec(), answer: atx })
        .ok()?;
    match arx.recv() {
        Ok(Answer::FetchResult(v)) => Some(v),
        _ => None,
    }
}

fn ruby_resolve(reply: &Sender<VmReply>, edges: &[(String, String)]) -> Option<Vec<Option<String>>> {
    let (atx, arx) = channel();
    reply
        .send(VmReply::ResolveBatch { edges: edges.to_vec(), answer: atx })
        .ok()?;
    match arx.recv() {
        Ok(Answer::ResolveResult(v)) => Some(v),
        _ => None,
    }
}

// Walk + instantiate + evaluate. Runs on the V8 thread inside the realm's
// ContextScope. Returns the URLs newly compiled this load.
fn load_module_graph_inner(
    scope: &mut v8::PinScope<'_, '_>,
    entry_url: &str,
    reply: &Sender<VmReply>,
) -> Result<Vec<String>, VmError> {
    let mut to_fetch: Vec<String> = Vec::new();
    if !REGISTRY.with(|r| r.borrow().by_url.contains_key(entry_url)) {
        to_fetch.push(entry_url.to_string());
    }
    let mut seen: HashSet<String> = to_fetch.iter().cloned().collect();
    let mut new_urls: Vec<String> = Vec::new();

    while !to_fetch.is_empty() {
        let fetched = ruby_fetch(reply, &to_fetch)
            .ok_or_else(|| VmError::Runtime("fetch_batch callback failed".into()))?;

        let mut level_edges: Vec<(String, String)> = Vec::new(); // (specifier, referrer)
        for (url, source) in to_fetch.iter().zip(fetched) {
            // None = fetch failed (404): leave uncompiled; a static import of
            // it then fails at instantiate, which is ESM-correct.
            let Some(source) = source else { continue };
            let code =
                v8::String::new(scope, &source).ok_or_else(|| VmError::Runtime("source alloc".into()))?;
            let origin = module_origin(scope, url);
            let mut src = v8::script_compiler::Source::new(code, Some(&origin));
            let module = v8::script_compiler::compile_module(scope, &mut src)
                .ok_or_else(|| VmError::Parse(format!("compile failed: {url}")))?;
            REGISTRY.with(|r| {
                let mut r = r.borrow_mut();
                let hash = module.get_identity_hash().get();
                let g = v8::Global::new(scope, module);
                r.by_url.insert(url.clone(), g.clone());
                r.url_by_hash.entry(hash).or_default().push((g, url.clone()));
            });
            new_urls.push(url.clone());
            let requests = module.get_module_requests();
            for i in 0..requests.length() {
                let req: v8::Local<v8::ModuleRequest> =
                    requests.get(scope, i).unwrap().try_into().unwrap();
                let spec = req.get_specifier().to_rust_string_lossy(scope);
                level_edges.push((spec, url.clone()));
            }
        }

        to_fetch.clear();
        if level_edges.is_empty() {
            continue;
        }
        let resolved = ruby_resolve(reply, &level_edges)
            .ok_or_else(|| VmError::Runtime("resolve callback failed".into()))?;
        for ((spec, referrer), url) in level_edges.into_iter().zip(resolved) {
            let Some(url) = url else { continue };
            REGISTRY.with(|r| {
                r.borrow_mut().edges.insert((referrer, spec), url.clone());
            });
            let registered = REGISTRY.with(|r| r.borrow().by_url.contains_key(&url));
            if !registered && seen.insert(url.clone()) {
                to_fetch.push(url);
            }
        }
    }

    let entry = REGISTRY
        .with(|r| r.borrow().by_url.get(entry_url).cloned())
        .ok_or_else(|| VmError::Runtime(format!("entry module not loaded: {entry_url}")))?;
    let entry = v8::Local::new(scope, &entry);
    if entry
        .instantiate_module(scope, resolve_module)
        .filter(|&ok| ok)
        .is_none()
    {
        return Err(VmError::Runtime(format!("instantiate failed: {entry_url}")));
    }
    let value = entry
        .evaluate(scope)
        .ok_or_else(|| VmError::Runtime("module evaluation failed".into()))?;
    scope.perform_microtask_checkpoint();
    if let Ok(promise) = v8::Local::<v8::Promise>::try_from(value) {
        if promise.state() == v8::PromiseState::Rejected {
            let reason = promise.result(scope);
            return Err(VmError::Runtime(reason.to_rust_string_lossy(scope)));
        }
    }
    Ok(new_urls)
}

fn v8_thread_main(
    rx: Receiver<Request>,
    handle_tx: Sender<v8::IsolateHandle>,
    host_namespace: Option<String>,
) {
    init_v8();
    let mut isolate = v8::Isolate::new(Default::default());
    let _ = handle_tx.send(isolate.thread_safe_handle());
    // mut: reset_realm swaps this for a fresh Context in the same warm isolate.
    let mut global_context = {
        v8::scope!(let scope, &mut isolate);
        let context = v8::Context::new(scope, Default::default());
        v8::Global::new(scope, context)
    };
    if let Some(ref name) = host_namespace {
        install_host_namespace(&mut isolate, &global_context, name);
    }

    while let Ok(request) = rx.recv() {
        match request {
            Request::Eval {
                source,
                timeout_ms,
                reply,
            } => {
                REPLY_STACK.with(|s| s.borrow_mut().push(reply.clone()));

                // Watchdog: fire-and-join per timed request. Joining before
                // the reply and cancelling unconditionally if it fired keeps a
                // late termination from poisoning the next request (audit #3).
                let (cancel_tx, cancel_rx) = channel::<()>();
                let watchdog = (timeout_ms > 0).then(|| {
                    let handle = isolate.thread_safe_handle();
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

                let outcome = {
                    v8::scope!(let scope, &mut isolate);
                    let context = v8::Local::new(scope, &global_context);
                    let scope = &mut v8::ContextScope::new(scope, context);
                    run_source(scope, &source)
                };

                let fired = watchdog.is_some_and(|w| {
                    let _ = cancel_tx.send(());
                    w.join().unwrap_or(false)
                });
                if fired {
                    isolate.cancel_terminate_execution();
                }

                REPLY_STACK.with(|s| {
                    s.borrow_mut().pop();
                });
                let _ = reply.send(VmReply::Done(outcome));
            }
            Request::Attach {
                name,
                host_fn_id,
                reply,
            } => {
                let outcome = {
                    v8::scope!(let scope, &mut isolate);
                    let context = v8::Local::new(scope, &global_context);
                    let scope = &mut v8::ContextScope::new(scope, context);
                    let external = v8::External::new(scope, host_fn_id as *mut c_void);
                    match v8::Function::builder(host_fn_callback)
                        .data(external.into())
                        .build(scope)
                    {
                        Some(function) => {
                            let key = v8::String::new(scope, &name).unwrap();
                            let global = context.global(scope);
                            global.set(scope, key.into(), function.into());
                            Ok(JsVal::Undefined)
                        }
                        None => Err(VmError::Runtime("failed to build function".into())),
                    }
                };
                let _ = reply.send(VmReply::Done(outcome));
            }
            Request::Reset { reply } => {
                let fresh = {
                    v8::scope!(let scope, &mut isolate);
                    let context = v8::Context::new(scope, Default::default());
                    v8::Global::new(scope, context)
                };
                global_context = fresh;
                REGISTRY.with(|r| r.borrow_mut().clear());
                if let Some(ref name) = host_namespace {
                    install_host_namespace(&mut isolate, &global_context, name);
                }
                let _ = reply.send(VmReply::Done(Ok(JsVal::Undefined)));
            }
            Request::LoadModuleGraph { entry_url, reply } => {
                // REPLY_STACK so a module's top-level code calling an attached
                // host fn routes back to this request's waiter, just like Eval.
                REPLY_STACK.with(|s| s.borrow_mut().push(reply.clone()));
                let result = {
                    v8::scope!(let scope, &mut isolate);
                    let context = v8::Local::new(scope, &global_context);
                    let scope = &mut v8::ContextScope::new(scope, context);
                    load_module_graph_inner(scope, &entry_url, &reply)
                };
                REPLY_STACK.with(|s| {
                    s.borrow_mut().pop();
                });
                let _ = reply.send(VmReply::ModuleGraphDone(result));
            }
            Request::Dispose => break,
        }
    }
    // global_context (a v8::Global) must die before the isolate it points into;
    // both are dropped here, declaration order makes that explicit.
    drop(global_context);
    drop(isolate);
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

#[magnus::wrap(class = "RustyRacer::Context")]
struct Context {
    shared: Mutex<Shared>,
    procs: RefCell<Vec<Opaque<Proc>>>,
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

// RustyRacer::Platform.set_flags!(*flags, **kwargs): symbol/string -> --flag,
// hash entry -> --key=value. Must run before the first Context.new.
fn platform_set_flags(args: &[Value]) -> Result<(), Error> {
    let ruby = Ruby::get().unwrap();
    if V8_INITED.load(Ordering::SeqCst) {
        return Err(Error::new(
            err_class(&ruby, "PlatformAlreadyInitialized"),
            "the V8 platform is already initialized; set flags before the first Context.new",
        ));
    }
    let mut flags = String::new();
    for a in args {
        if let Ok(h) = RHash::try_convert(*a) {
            h.foreach(|k: Value, v: Value| {
                let ks = k.funcall::<_, _, String>("to_s", ())?;
                let vs = v.funcall::<_, _, String>("to_s", ())?;
                flags.push_str(&format!(" --{ks}={vs}"));
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

impl Context {
    fn new(ruby: &Ruby, host_namespace: Option<String>) -> Result<Self, Error> {
        let (tx, rx) = channel::<Request>();
        let (handle_tx, handle_rx) = channel::<v8::IsolateHandle>();
        std::thread::spawn(move || v8_thread_main(rx, handle_tx, host_namespace));
        let handle = handle_rx
            .recv()
            .map_err(|_| Error::new(ruby.exception_runtime_error(), "V8 thread failed to boot"))?;
        Ok(Context {
            shared: Mutex::new(Shared {
                tx,
                handle,
                disposed: false,
            }),
            procs: RefCell::new(Vec::new()),
        })
    }

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

    // Wait for this request's reply, serving host-fn callbacks and module-graph
    // fetch/resolve batches as they arrive. The recv waits release the GVL; the
    // Ruby procs run with it held. |loader| carries load_module_graph's
    // (resolve, fetch) procs; other ops pass None.
    fn pump(
        &self,
        ruby: &Ruby,
        reply_rx: Receiver<VmReply>,
        loader: Option<(Proc, Proc)>,
    ) -> Result<Value, Error> {
        loop {
            let message = without_gvl(|| reply_rx.recv());
            match message {
                Ok(VmReply::Done(Ok(val))) => return Ok(jsval_to_ruby(ruby, &val)),
                Ok(VmReply::Done(Err(e))) => return Err(vm_err(ruby, e)),
                Ok(VmReply::ModuleGraphDone(Ok(urls))) => {
                    return Ok(module_graph_result(ruby, &urls));
                }
                Ok(VmReply::ModuleGraphDone(Err(e))) => return Err(vm_err(ruby, e)),
                Ok(VmReply::Callback {
                    host_fn_id,
                    args,
                    answer,
                }) => {
                    let result = self.call_proc(ruby, host_fn_id, &args, &answer);
                    let _ = answer.send(Answer::Result(result));
                }
                Ok(VmReply::FetchBatch { urls, answer }) => {
                    let r = match &loader {
                        Some((_, fetch)) => fetch_via_ruby(ruby, *fetch, &urls),
                        None => vec![None; urls.len()],
                    };
                    let _ = answer.send(Answer::FetchResult(r));
                }
                Ok(VmReply::ResolveBatch { edges, answer }) => {
                    let r = match &loader {
                        Some((resolve, _)) => resolve_via_ruby(ruby, *resolve, &edges),
                        None => vec![None; edges.len()],
                    };
                    let _ = answer.send(Answer::ResolveResult(r));
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
            let procs = self.procs.borrow();
            let opaque = procs.get(host_fn_id).ok_or("unknown host function")?;
            ruby.get_inner(*opaque)
        };
        let ruby_args: Vec<Value> = args.iter().map(|v| jsval_to_ruby(ruby, v)).collect();
        NESTED.with(|n| n.borrow_mut().push(answer.clone()));
        let result: Result<Value, Error> = proc.call(ruby_args.as_slice());
        NESTED.with(|n| {
            n.borrow_mut().pop();
        });
        let value = result.map_err(|e| e.to_string())?;
        ruby_to_jsval(value).map_err(|e| e.to_string())
    }

    fn eval(ruby: &Ruby, rb_self: &Self, source: String) -> Result<Value, Error> {
        Self::eval_t(ruby, rb_self, source, 0)
    }

    // Context#call(name, *args). Spike: reuse eval with JSON-injected args. A
    // real impl would use Function::Call (preserves receiver, non-JSON values);
    // for the supported primitive types this is equivalent and reuses the whole
    // rendezvous / nesting / error path for free.
    fn call(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<Value, Error> {
        let Some((name, call_args)) = args.split_first() else {
            return Err(Error::new(
                ruby.exception_arg_error(),
                "call requires a function name",
            ));
        };
        let name = String::try_convert(*name)?;
        let mut json = String::new();
        for (i, v) in call_args.iter().enumerate() {
            if i > 0 {
                json.push(',');
            }
            json.push_str(&jsval_to_json(&ruby_to_jsval(*v)?));
        }
        Self::eval_t(ruby, rb_self, format!("({name})(...[{json}])"), 0)
    }

    fn eval_t(ruby: &Ruby, rb_self: &Self, source: String, timeout_ms: u64) -> Result<Value, Error> {
        // Inside a proc serving a callback? Route as a nested eval through the
        // suspended V8 frame instead of the main queue (which is busy).
        let nested = NESTED.with(|n| n.borrow().last().cloned());
        if let Some(answer) = nested {
            let (reply_tx, reply_rx) = channel::<VmReply>();
            answer
                .send(Answer::NestedEval {
                    source,
                    reply: reply_tx,
                })
                .map_err(|_| Error::new(ruby.exception_runtime_error(), "V8 thread is gone"))?;
            return rb_self.pump(ruby, reply_rx, None);
        }

        let (reply_tx, reply_rx) = channel::<VmReply>();
        rb_self.send(
            ruby,
            Request::Eval {
                source,
                timeout_ms,
                reply: reply_tx,
            },
        )?;
        rb_self.pump(ruby, reply_rx, None)
    }

    fn attach(ruby: &Ruby, rb_self: &Self, name: String, proc: Proc) -> Result<Value, Error> {
        let host_fn_id = {
            let mut procs = rb_self.procs.borrow_mut();
            procs.push(Opaque::from(proc));
            procs.len() - 1
        };
        let (reply_tx, reply_rx) = channel::<VmReply>();
        rb_self.send(
            ruby,
            Request::Attach {
                name,
                host_fn_id,
                reply: reply_tx,
            },
        )?;
        rb_self.pump(ruby, reply_rx, None)
    }

    fn reset_realm(ruby: &Ruby, rb_self: &Self) -> Result<Value, Error> {
        let (reply_tx, reply_rx) = channel::<VmReply>();
        rb_self.send(ruby, Request::Reset { reply: reply_tx })?;
        rb_self.pump(ruby, reply_rx, None)
    }

    // Positional primitive behind the keyword-arg Ruby wrapper in
    // lib/rusty_racer.rb: load_module_graph(entry, resolve:, fetch_batch:).
    fn load_module_graph(
        ruby: &Ruby,
        rb_self: &Self,
        entry_url: String,
        resolve: Proc,
        fetch_batch: Proc,
    ) -> Result<Value, Error> {
        let (reply_tx, reply_rx) = channel::<VmReply>();
        rb_self.send(
            ruby,
            Request::LoadModuleGraph {
                entry_url,
                reply: reply_tx,
            },
        )?;
        rb_self.pump(ruby, reply_rx, Some((resolve, fetch_batch)))
    }

    // Terminate whatever is running. IsolateHandle is Send + refcounted —
    // safe at ANY time, even racing disposal (audit #63 without a stop_mtx).
    fn stop(&self) {
        let shared = self.shared.lock().unwrap();
        shared.handle.terminate_execution();
    }

    fn dispose(ruby: &Ruby, rb_self: &Self) -> Result<(), Error> {
        let mut shared = rb_self.shared.lock().unwrap();
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

fn json_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// JSON encoding for Context#call's argument injection (now incl. array/object).
fn jsval_to_json(val: &JsVal) -> String {
    match val {
        JsVal::Undefined | JsVal::Null => "null".to_string(),
        JsVal::Bool(b) => b.to_string(),
        JsVal::Int(i) => i.to_string(),
        JsVal::Num(n) if n.is_finite() => n.to_string(),
        JsVal::Num(_) => "null".to_string(),
        JsVal::Str(s) => json_quote(s),
        JsVal::Array(items) => {
            let parts: Vec<String> = items.iter().map(jsval_to_json).collect();
            format!("[{}]", parts.join(","))
        }
        JsVal::Obj(entries) => {
            let parts: Vec<String> = entries
                .iter()
                .map(|(k, v)| format!("{}:{}", json_quote(k), jsval_to_json(v)))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
    }
}

fn vm_err(ruby: &Ruby, e: VmError) -> Error {
    match e {
        VmError::Parse(m) => Error::new(err_class(ruby, "ParseError"), m),
        VmError::Runtime(m) => Error::new(err_class(ruby, "RuntimeError"), m),
        VmError::Terminated => Error::new(
            err_class(ruby, "ScriptTerminatedError"),
            "JavaScript was terminated (timeout or stop)",
        ),
    }
}

// csim's result shape: { modules: [{ url:, cache_rejected: }, ...] }. No
// cached_data in the spike, so cache_rejected is always false.
fn module_graph_result(ruby: &Ruby, urls: &[String]) -> Value {
    let mods = ruby.ary_new();
    for u in urls {
        let h = ruby.hash_new();
        let _ = h.aset(ruby.to_symbol("url"), u.as_str());
        let _ = h.aset(ruby.to_symbol("cache_rejected"), false);
        let _ = mods.push(h);
    }
    let result = ruby.hash_new();
    let _ = result.aset(ruby.to_symbol("modules"), mods);
    result.as_value()
}

// Call Ruby's fetch_batch proc with the URL list; marshal back per-URL source.
// A raised proc or wrong shape yields None for that slot (the walk then treats
// it as a 404 — spike behaviour; a real impl would propagate the error).
fn fetch_via_ruby(_ruby: &Ruby, fetch: Proc, urls: &[String]) -> Vec<Option<String>> {
    match fetch.call::<_, Value>((urls.to_vec(),)) {
        Ok(ret) => marshal_fetch(ret, urls.len()),
        Err(_) => vec![None; urls.len()],
    }
}

fn marshal_fetch(ret: Value, n: usize) -> Vec<Option<String>> {
    let Ok(arr) = RArray::try_convert(ret) else {
        return vec![None; n];
    };
    (0..n)
        .map(|i| {
            let el: Value = arr.entry::<Value>(i as isize).ok()?;
            if el.is_nil() {
                return None;
            }
            // csim: each element is [source, cached_data] or a bare source.
            if let Ok(pair) = RArray::try_convert(el) {
                return pair.entry::<String>(0).ok();
            }
            String::try_convert(el).ok()
        })
        .collect()
}

fn resolve_via_ruby(_ruby: &Ruby, resolve: Proc, edges: &[(String, String)]) -> Vec<Option<String>> {
    match resolve.call::<_, Value>((edges.to_vec(),)) {
        Ok(ret) => marshal_urls(ret, edges.len()),
        Err(_) => vec![None; edges.len()],
    }
}

fn marshal_urls(ret: Value, n: usize) -> Vec<Option<String>> {
    let Ok(arr) = RArray::try_convert(ret) else {
        return vec![None; n];
    };
    (0..n)
        .map(|i| {
            let el: Value = arr.entry::<Value>(i as isize).ok()?;
            if el.is_nil() {
                None
            } else {
                String::try_convert(el).ok()
            }
        })
        .collect()
}

fn jsval_to_ruby(ruby: &Ruby, val: &JsVal) -> Value {
    match val {
        JsVal::Undefined | JsVal::Null => ruby.qnil().as_value(),
        JsVal::Bool(b) => (*b).into_value_with(ruby),
        JsVal::Int(i) => (*i).into_value_with(ruby),
        JsVal::Num(n) => (*n).into_value_with(ruby),
        JsVal::Str(s) => s.clone().into_value_with(ruby),
        JsVal::Array(items) => {
            let arr = ruby.ary_new();
            for it in items {
                let _ = arr.push(jsval_to_ruby(ruby, it));
            }
            arr.as_value()
        }
        // mini_racer marshals JS objects to string-keyed Hashes.
        JsVal::Obj(entries) => {
            let h = ruby.hash_new();
            for (k, it) in entries {
                let _ = h.aset(k.as_str(), jsval_to_ruby(ruby, it));
            }
            h.as_value()
        }
    }
}

fn ruby_to_jsval(val: Value) -> Result<JsVal, Error> {
    ruby_to_jsval_d(val, 0)
}

fn ruby_to_jsval_d(val: Value, depth: u32) -> Result<JsVal, Error> {
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
    // Integer/Float/String try_convert are type-strict (Err on a mismatch).
    if let Ok(i) = i64::try_convert(val) {
        return Ok(JsVal::Int(i));
    }
    if let Ok(n) = f64::try_convert(val) {
        return Ok(JsVal::Num(n));
    }
    if let Ok(s) = String::try_convert(val) {
        return Ok(JsVal::Str(s));
    }
    if depth < MAX_MARSHAL_DEPTH {
        if let Ok(arr) = RArray::try_convert(val) {
            let mut out = Vec::with_capacity(arr.len());
            for i in 0..arr.len() {
                let el: Value = arr.entry::<Value>(i as isize)?;
                out.push(ruby_to_jsval_d(el, depth + 1)?);
            }
            return Ok(JsVal::Array(out));
        }
        if let Ok(hash) = RHash::try_convert(val) {
            let entries = RefCell::new(Vec::new());
            hash.foreach(|k: Value, v: Value| {
                // String/Symbol keys -> String; anything else via to_s.
                let key = String::try_convert(k).or_else(|_| {
                    k.funcall::<_, _, String>("to_s", ())
                })?;
                entries.borrow_mut().push((key, ruby_to_jsval_d(v, depth + 1)?));
                Ok(magnus::r_hash::ForEach::Continue)
            })?;
            return Ok(JsVal::Obj(entries.into_inner()));
        }
    }
    Err(Error::new(
        ruby.exception_type_error(),
        "unsupported type crossing into JS",
    ))
}

#[magnus::init]
fn init(ruby: &Ruby) -> Result<(), Error> {
    let module = ruby.define_module("RustyRacer")?;
    let class = module.define_class("Context", ruby.class_object())?;
    // keyword-arg wrapper Context.new(host_namespace:) lives in lib/rusty_racer.rb
    class.define_singleton_method("_new", function!(Context::new, 1))?;
    class.define_method("eval", method!(Context::eval, 1))?;
    class.define_method("eval_t", method!(Context::eval_t, 2))?;
    class.define_method("call", method!(Context::call, -1))?;
    class.define_method("attach", method!(Context::attach, 2))?;
    class.define_method("reset_realm", method!(Context::reset_realm, 0))?;
    // keyword-arg wrapper Context#load_module_graph lives in lib/rusty_racer.rb
    class.define_method("_load_module_graph", method!(Context::load_module_graph, 3))?;
    class.define_method("stop", method!(Context::stop, 0))?;
    class.define_method("dispose", method!(Context::dispose, 0))?;

    let platform = module.define_module("Platform")?;
    platform.define_singleton_method("set_flags!", function!(platform_set_flags, -1))?;
    Ok(())
}
