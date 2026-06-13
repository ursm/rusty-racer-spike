// The request/dispatch layer: the Request and VmReply value types, the
// service_request -> dispatch_one fan-out, every per-op handler (op_*), and the
// three op helpers run_source / call_function / compile_source. Extracted from
// lib.rs verbatim.
//
// Request, VmReply and Compiled are pub(crate) (the magnus method impls and
// Core/Isolate/Context/Module/Script wiring still in lib.rs build and consume
// them, and read Compiled's fields). service_request is pub(crate) (Core::run
// calls it) and run_source too (Snapshot warmup calls it). request_realm,
// dispatch_one, the op_* handlers, call_function and compile_source are used
// only here and stay private.
//
// ops.rs reaches the crate root's (private) helpers, structs and the istate!
// macro through the imports below; the marshal/watchdog symbols come from their
// own modules.

use crate::istate;
use crate::marshal::{js_to_jsval, jsval_to_js, JsVal};
use crate::*;

// One VM operation, built by a magnus method and run inline by Core::run ->
// service_request -> dispatch_one. |context_id| selects which realm the op runs
// in: 0 = the main realm (Context's own globalThis, swappable by reset_realm),
// N >= 1 = an extra realm made by create_context.
pub(crate) enum Request {
    Eval {
        context_id: i32,
        source: String,
        filename: String,
        timeout_ms: u64,
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
    },
    // Drain the isolate's microtask queue once (no auto event loop).
    DrainMicrotasks {
        timeout_ms: u64,
    },
    Attach {
        context_id: i32,
        name: String,
        host_fn_id: usize,
        timeout_ms: u64,
    },
    // Batch attach: install many (name, host_fn_id) host fns in one round-trip
    // (a fresh realm needs ~dozens). Same semantics as Attach, applied in order.
    AttachMany {
        context_id: i32,
        entries: Vec<(String, usize)>,
        timeout_ms: u64,
    },
    // reset: swap globalThis for a fresh v8::Context, reusing the same warm
    // isolate — csim's per-visit reset. Applies to the named context.
    Reset {
        context_id: i32,
    },
    // create_context: build a fresh, persistent v8::Context in the isolate and
    // return its id (the multi-realm model). DisposeContext frees one.
    CreateContext,
    DisposeContext {
        context_id: i32,
    },
    // Thin ES-module primitives (V8's raw compile/instantiate/evaluate). The
    // embedder owns the url->Module registry and the resolve policy; the binding
    // just exposes the steps. A compiled module is addressed by an id (like a
    // realm) since a v8::Local handle can't outlive the op's scope.
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
    },
    // instantiate: V8 walks imports, calling back to the Ruby resolve block
    // (parked in the slot for the op) per edge via resolve_imported.
    InstantiateModule {
        module_id: i32,
    },
    EvaluateModule {
        module_id: i32,
        timeout_ms: u64,
    },
    ModuleNamespace {
        module_id: i32,
    },
    // The module's v8::Module::Status, as a lowercase name ("uninstantiated",
    // "instantiated", ...) the Ruby wrapper symbolizes.
    ModuleStatus {
        module_id: i32,
    },
    DisposeModule {
        module_id: i32,
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
    },
    // Bind the script to its context and run it; returns the completion value.
    RunScript {
        script_id: i32,
        timeout_ms: u64,
    },
    DisposeScript {
        script_id: i32,
    },
    // Serialize a bytecode cache from a compiled handle's CURRENT compile state
    // (Script#create_code_cache / Module#create_code_cache). Called after run/
    // evaluate, it captures the inner functions V8 lazily compiled while running
    // — the only way (as of V8-150) to get inner-function bytecode into a cache,
    // since create_code_cache at compile time only sees the top level.
    ScriptCodeCache {
        script_id: i32,
    },
    ModuleCodeCache {
        module_id: i32,
    },
}

// compile_module result: the module's id plus any produced bytecode cache and
// whether a supplied cache was rejected.
pub(crate) struct Compiled {
    pub(crate) id: i32,
    pub(crate) cached_data: Option<Vec<u8>>,
    pub(crate) cache_rejected: bool,
}

// The terminal reply of an op: service_request returns it straight up to
// Core::run (no channel). Host callbacks and module resolvers don't round-trip
// through here — they run inline (with_gvl).
pub(crate) enum VmReply {
    Done(Result<JsVal, VmError>),
    // compile_module / compile's richer reply (id + produced cache + rejected).
    ModuleCompiled(Result<Compiled, VmError>),
    ScriptCompiled(Result<Compiled, VmError>),
    // Script#/Module#create_code_cache: the serialized bytes, or None when V8
    // can't produce a cache (or the handle's realm is gone).
    CodeCache(Result<Option<Vec<u8>>, VmError>),
}

pub(crate) fn run_source(scope: &mut v8::PinScope<'_, '_>, source: &str, filename: &str) -> Result<JsVal, VmError> {
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

// Service ONE request inline on the owner thread and RETURN its terminal reply.
// This is the single dispatcher for BOTH a top-level op and a re-entrant one (a
// host proc / module resolver that issues another op), so EVERY op — not just
// eval/call — works re-entrantly. `outermost` (depth == 0, computed by Core::run
// before it bumped the depth) owns the terminate-flag cleanup; a nested op
// passes false.
pub(crate) fn service_request(scope: &mut v8::PinScope<'_, '_, ()>, request: Request, outermost: bool) -> VmReply {
    // Clear any terminate left over from BEFORE this request. An
    // Isolate#terminate fired while no JS was running arms the isolate-global
    // flag but no watchdog_fired, so the end-of-request sweep would miss it and
    // the next eval would abort spuriously — and an idle terminate isn't even
    // observable via is_execution_terminating() yet, so cancel unconditionally.
    // Only at the outermost frame: a terminate aimed at a SUSPENDED outer frame
    // must survive a nested request.
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
    let reply = dispatch_one(scope, request, outermost);
    if realm.is_some() {
        istate!(scope).active_realms.pop();
    }
    // Sweep a leftover terminate flag once the whole request stack has
    // unwound (see watchdog_fired for why nested frames must not cancel).
    if outermost && istate!(scope).watchdog_fired {
        istate!(scope).watchdog_fired = false;
        scope.cancel_terminate_execution();
    }
    reply
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
        | Request::CreateContext
        | Request::DisposeContext { .. }
        | Request::ModuleStatus { .. }
        | Request::DisposeModule { .. }
        | Request::DisposeScript { .. }
        | Request::ScriptCodeCache { .. }
        | Request::ModuleCodeCache { .. } => None,
    }
}

fn dispatch_one(scope: &mut v8::PinScope<'_, '_, ()>, request: Request, outermost: bool) -> VmReply {
    // A request-scoped handle scope, so handles created while servicing a
    // nested request don't pile up in the suspended callback's scope.
    v8::scope!(let scope, &mut *scope);
    match request {
        Request::Eval {
            context_id,
            source,
            filename,
            timeout_ms,
        } => op_eval(scope, context_id, source, filename, timeout_ms, outermost),
        Request::Call {
            context_id,
            name,
            args,
            void,
            timeout_ms,
        } => op_call(scope, context_id, name, args, void, timeout_ms, outermost),
        Request::DrainMicrotasks { timeout_ms } => op_drain_microtasks(scope, timeout_ms),
        Request::Attach {
            context_id,
            name,
            host_fn_id,
            timeout_ms,
        } => op_attach(scope, context_id, name, host_fn_id, timeout_ms, outermost),
        Request::AttachMany {
            context_id,
            entries,
            timeout_ms,
        } => op_attach_many(scope, context_id, entries, timeout_ms, outermost),
        Request::Reset { context_id } => op_reset(scope, context_id),
        Request::CreateContext => op_create_context(scope),
        Request::DisposeContext { context_id } => op_dispose_context(scope, context_id),
        Request::CompileModule {
            context_id,
            source,
            filename,
            cached_data,
            produce_cache,
            eager,
        } => op_compile_module(scope, context_id, source, filename, cached_data, produce_cache, eager),
        Request::InstantiateModule { module_id } => op_instantiate_module(scope, module_id),
        Request::EvaluateModule { module_id, timeout_ms } => op_evaluate_module(scope, module_id, timeout_ms, outermost),
        Request::ModuleNamespace { module_id } => op_module_namespace(scope, module_id),
        Request::ModuleStatus { module_id } => op_module_status(scope, module_id),
        Request::DisposeModule { module_id } => op_dispose_module(scope, module_id),
        Request::CompileScript {
            context_id,
            source,
            filename,
            cached_data,
            produce_cache,
            eager,
        } => op_compile_script(scope, context_id, source, filename, cached_data, produce_cache, eager),
        Request::RunScript {
            script_id,
            timeout_ms,
        } => op_run_script(scope, script_id, timeout_ms, outermost),
        Request::DisposeScript { script_id } => op_dispose_script(scope, script_id),
        // Serialize the script's CURRENT compile state. The stored handle is
        // the UnboundScript, which V8 fills in with inner-function bytecode as
        // run() lazily compiles them — so calling this after run() captures
        // the functions that actually executed (a warm cache). None when V8
        // can't serialize, or when the realm was reset/disposed out from under
        // the script (its handle is gone): produce nil, not an error.
        Request::ScriptCodeCache { script_id } => op_script_code_cache(scope, script_id),
        // Same, for a module: get_unbound_module_script gives the shared
        // compiled script, which evaluate() fills with inner-function bytecode.
        // It needs the module's context entered (unlike UnboundScript), so
        // a gone realm yields nil.
        Request::ModuleCodeCache { module_id } => op_module_code_cache(scope, module_id),
    }
}

fn op_eval(scope: &mut v8::PinScope<'_, '_, ()>, context_id: i32, source: String, filename: String, timeout_ms: u64, outermost: bool) -> VmReply {
    let outcome = run_js_bracketed(scope, outermost, timeout_ms, "eval", |scope, outermost| {
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
    VmReply::Done(outcome)
}

#[allow(clippy::too_many_arguments)]
fn op_call(scope: &mut v8::PinScope<'_, '_, ()>, context_id: i32, name: String, args: Vec<JsVal>, void: bool, timeout_ms: u64, outermost: bool) -> VmReply {
    // A host fn invoked by the called function runs inline
    // (host_fn_callback, with_gvl) — no routing setup needed.
    let outcome = run_js_bracketed(scope, outermost, timeout_ms, "call", |scope, outermost| {
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
    VmReply::Done(outcome)
}

fn op_drain_microtasks(scope: &mut v8::PinScope<'_, '_, ()>, timeout_ms: u64) -> VmReply {
    // A microtask may call an attached host fn (a Promise .then ->
    // ruby), which runs inline via host_fn_callback — no routing
    // setup needed any more.
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
    let outcome = if fired {
        Err(VmError::Terminated)
    } else {
        Ok(JsVal::Undefined)
    };
    VmReply::Done(outcome)
}

fn op_attach(scope: &mut v8::PinScope<'_, '_, ()>, context_id: i32, name: String, host_fn_id: usize, timeout_ms: u64, outermost: bool) -> VmReply {
    // attach_at_path writes onto globalThis (and walks a dotted
    // path), which can fire a user-defined accessor or Proxy trap —
    // arbitrary JS. So it goes through the same bracket as Eval: a
    // host fn the trap calls routes back, and a looping trap is
    // time-capped.
    let outcome = run_js_bracketed(scope, outermost, timeout_ms, "attach", |scope, outermost| {
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
    VmReply::Done(outcome)
}

fn op_attach_many(scope: &mut v8::PinScope<'_, '_, ()>, context_id: i32, entries: Vec<(String, usize)>, timeout_ms: u64, outermost: bool) -> VmReply {
    // Same as Attach (arbitrary JS via accessors/Proxy traps), but
    // installs every entry under one bracket/drain. Applied in order;
    // stops at the first failure and reports its (name-tagged) error.
    // NOT transactional: entries before the failure stay attached —
    // the realm is not rolled back (matches single Attach, which also
    // commits its one write or fails it).
    let outcome = run_js_bracketed(scope, outermost, timeout_ms, "attach_many", |scope, outermost| {
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
    VmReply::Done(outcome)
}

fn op_reset(scope: &mut v8::PinScope<'_, '_, ()>, context_id: i32) -> VmReply {
    let known =
        context_id == 0 || istate!(scope).realms.contexts.contains_key(&context_id);
    if istate!(scope).draining {
        // A microtask from ANY realm may be mid-flight on the stack;
        // swapping a v8::Context out from under it corrupts state.
        VmReply::Done(Err(VmError::Runtime(
            "cannot reset a realm during a microtask checkpoint".into(),
        )))
    } else if !known {
        VmReply::Done(Err(VmError::Runtime(
            "context disposed or unknown".into(),
        )))
    } else if istate!(scope).active_realms.contains(&context_id) {
        // Swapping the v8::Context behind a suspended frame would
        // drop its in-flight modules/scripts and let the realm id
        // refer to a different context than the one on the stack
        // (defeating the cross-context import guards).
        VmReply::Done(Err(VmError::Runtime(
            "cannot reset a realm while a request for it is suspended on the V8 stack"
                .into(),
        )))
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
        VmReply::Done(Ok(JsVal::Undefined))
    }
}

fn op_create_context(scope: &mut v8::PinScope<'_, '_, ()>) -> VmReply {
    let id = {
        let realms = &mut istate!(scope).realms;
        let id = realms.next_context_id;
        realms.next_context_id += 1;
        id
    };
    let fresh = new_realm(scope, id);
    istate!(scope).realms.contexts.insert(id, fresh);
    VmReply::Done(Ok(JsVal::Int(id as i64)))
}

fn op_dispose_context(scope: &mut v8::PinScope<'_, '_, ()>, context_id: i32) -> VmReply {
    if istate!(scope).draining {
        // Same hazard as Reset: a microtask from any realm may be live.
        VmReply::Done(Err(VmError::Runtime(
            "cannot dispose a realm during a microtask checkpoint".into(),
        )))
    } else if istate!(scope).active_realms.contains(&context_id) {
        // Same hazard as Reset: a suspended frame still runs in it.
        VmReply::Done(Err(VmError::Runtime(
            "cannot dispose a realm while a request for it is suspended on the V8 stack"
                .into(),
        )))
    } else {
        // Dropping the Global lets V8 collect the context. id 0 is the
        // default context and never disposed independently.
        istate!(scope).realms.contexts.remove(&context_id);
        // Reclaim the modules compiled in it (else they leak until
        // isolate teardown).
        drop_context_artifacts(istate!(scope), context_id);
        VmReply::Done(Ok(JsVal::Undefined))
    }
}

#[allow(clippy::too_many_arguments)]
fn op_compile_module(scope: &mut v8::PinScope<'_, '_, ()>, context_id: i32, source: String, filename: String, cached_data: Option<Vec<u8>>, produce_cache: bool, eager: bool) -> VmReply {
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
    VmReply::ModuleCompiled(outcome)
}

fn op_instantiate_module(scope: &mut v8::PinScope<'_, '_, ()>, module_id: i32) -> VmReply {
    // V8's module instantiation is NOT re-entrant: a nested
    // instantiate issued from a resolve block walks the outer,
    // half-built module graph and SEGVs the process. Refuse it
    // cleanly — a resolve block may COMPILE dependencies lazily
    // and return them; the outer instantiate links them.
    if istate!(scope).instantiating {
        VmReply::Done(Err(VmError::Runtime(
            "instantiate is not re-entrant: another module is currently \
             instantiating (compile the dependency and return it; the outer \
             instantiate links it)"
                .into(),
        )))
    } else {
        istate!(scope).instantiating = true;
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
                                // A resolver that RAISED is re-raised with its
                                // real class by instantiate_module (via the
                                // stashed exception); this generic link error
                                // is only used when no resolver exception was
                                // stashed.
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
        istate!(scope).instantiating = false;
        VmReply::Done(outcome)
    }
}

fn op_evaluate_module(scope: &mut v8::PinScope<'_, '_, ()>, module_id: i32, timeout_ms: u64, outermost: bool) -> VmReply {
    // Top-level module code (and, under :auto, the microtasks its
    // TLA continuation drains) can loop, so it runs in the same
    // watchdog bracket as Eval/Call/RunScript.
    let outcome = run_js_bracketed(scope, outermost, timeout_ms, "evaluate_module", |scope, outermost| {
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
    VmReply::Done(outcome)
}

fn op_module_namespace(scope: &mut v8::PinScope<'_, '_, ()>, module_id: i32) -> VmReply {
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
    VmReply::Done(outcome)
}

fn op_module_status(scope: &mut v8::PinScope<'_, '_, ()>, module_id: i32) -> VmReply {
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
    VmReply::Done(outcome)
}

fn op_dispose_module(scope: &mut v8::PinScope<'_, '_, ()>, module_id: i32) -> VmReply {
    let m = &mut istate!(scope).modules;
    m.by_id.remove(&module_id);
    for bucket in m.by_hash.values_mut() {
        bucket.retain(|(_, id)| *id != module_id);
    }
    VmReply::Done(Ok(JsVal::Undefined))
}

#[allow(clippy::too_many_arguments)]
fn op_compile_script(scope: &mut v8::PinScope<'_, '_, ()>, context_id: i32, source: String, filename: String, cached_data: Option<Vec<u8>>, produce_cache: bool, eager: bool) -> VmReply {
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
    VmReply::ScriptCompiled(outcome)
}

fn op_run_script(scope: &mut v8::PinScope<'_, '_, ()>, script_id: i32, timeout_ms: u64, outermost: bool) -> VmReply {
    let outcome = run_js_bracketed(scope, outermost, timeout_ms, "run_script", |scope, outermost| {
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
    VmReply::Done(outcome)
}

fn op_dispose_script(scope: &mut v8::PinScope<'_, '_, ()>, script_id: i32) -> VmReply {
    istate!(scope).scripts.by_id.remove(&script_id);
    VmReply::Done(Ok(JsVal::Undefined))
}

fn op_script_code_cache(scope: &mut v8::PinScope<'_, '_, ()>, script_id: i32) -> VmReply {
    let handle = script_handle(istate!(scope), script_id);
    let outcome = match handle {
        None => Ok(None),
        Some((g, _cid)) => {
            let unbound = v8::Local::new(scope, &g);
            Ok(unbound.create_code_cache().map(|c| c.to_vec()))
        }
    };
    VmReply::CodeCache(outcome)
}

fn op_module_code_cache(scope: &mut v8::PinScope<'_, '_, ()>, module_id: i32) -> VmReply {
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
    VmReply::CodeCache(outcome)
}
