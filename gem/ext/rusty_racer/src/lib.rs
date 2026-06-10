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
use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Mutex, Once};
use std::time::Duration;

use magnus::block::Proc;
use magnus::value::{Opaque, ReprValue};
use magnus::{function, method, prelude::*, Error, ExceptionClass, IntoValue, Ruby, TryConvert, Value};

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
}

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
    Dispose,
}

// V8 thread -> the Ruby thread that is waiting on this request
enum VmReply {
    Done(Result<JsVal, VmError>),
    // JS called host fn |id|; run the proc and send the answer back.
    Callback {
        host_fn_id: usize,
        args: Vec<JsVal>,
        answer: Sender<Answer>,
    },
}

// Ruby thread -> the V8 thread suspended inside a host-fn callback
enum Answer {
    Result(Result<JsVal, String>),
    // the proc's Ruby body called ctx.eval — serve it re-entrantly
    NestedEval {
        source: String,
        reply: Sender<VmReply>,
    },
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
    if value.is_undefined() {
        JsVal::Undefined
    } else if value.is_null() {
        JsVal::Null
    } else if value.is_boolean() {
        JsVal::Bool(value.boolean_value(scope))
    } else if value.is_int32() {
        JsVal::Int(value.integer_value(scope).unwrap_or(0))
    } else if value.is_number() {
        JsVal::Num(value.number_value(scope).unwrap_or(f64::NAN))
    } else {
        JsVal::Str(value.to_rust_string_lossy(scope))
    }
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

fn v8_thread_main(rx: Receiver<Request>, handle_tx: Sender<v8::IsolateHandle>) {
    init_v8();
    let mut isolate = v8::Isolate::new(Default::default());
    let _ = handle_tx.send(isolate.thread_safe_handle());
    let global_context = {
        v8::scope!(let scope, &mut isolate);
        let context = v8::Context::new(scope, Default::default());
        v8::Global::new(scope, context)
    };

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

fn init_v8() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let platform = v8::new_default_platform(0, false).make_shared();
        v8::V8::initialize_platform(platform);
        v8::V8::initialize();
    });
}

impl Context {
    fn new(ruby: &Ruby) -> Result<Self, Error> {
        let (tx, rx) = channel::<Request>();
        let (handle_tx, handle_rx) = channel::<v8::IsolateHandle>();
        std::thread::spawn(move || v8_thread_main(rx, handle_tx));
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

    // Wait for this request's reply, serving host-fn callbacks as they
    // arrive. The recv waits release the GVL; the proc runs with it held.
    fn pump(&self, ruby: &Ruby, reply_rx: Receiver<VmReply>) -> Result<Value, Error> {
        loop {
            let message = without_gvl(|| reply_rx.recv());
            match message {
                Ok(VmReply::Done(Ok(val))) => return Ok(jsval_to_ruby(ruby, &val)),
                Ok(VmReply::Done(Err(VmError::Parse(message)))) => {
                    return Err(Error::new(err_class(ruby, "ParseError"), message));
                }
                Ok(VmReply::Done(Err(VmError::Runtime(message)))) => {
                    return Err(Error::new(err_class(ruby, "RuntimeError"), message));
                }
                Ok(VmReply::Done(Err(VmError::Terminated))) => {
                    return Err(Error::new(
                        err_class(ruby, "ScriptTerminatedError"),
                        "JavaScript was terminated (timeout or stop)",
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
            return rb_self.pump(ruby, reply_rx);
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
        rb_self.pump(ruby, reply_rx)
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
        rb_self.pump(ruby, reply_rx)
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

fn jsval_to_ruby(ruby: &Ruby, val: &JsVal) -> Value {
    match val {
        JsVal::Undefined | JsVal::Null => ruby.qnil().as_value(),
        JsVal::Bool(b) => (*b).into_value_with(ruby),
        JsVal::Int(i) => (*i).into_value_with(ruby),
        JsVal::Num(n) => (*n).into_value_with(ruby),
        JsVal::Str(s) => s.clone().into_value_with(ruby),
    }
}

fn ruby_to_jsval(val: Value) -> Result<JsVal, Error> {
    if val.is_nil() {
        return Ok(JsVal::Null);
    }
    if let Ok(b) = bool::try_convert(val) {
        return Ok(JsVal::Bool(b));
    }
    if let Ok(i) = i64::try_convert(val) {
        return Ok(JsVal::Int(i));
    }
    if let Ok(n) = f64::try_convert(val) {
        return Ok(JsVal::Num(n));
    }
    if let Ok(s) = String::try_convert(val) {
        return Ok(JsVal::Str(s));
    }
    Err(Error::new(
        Ruby::get().unwrap().exception_type_error(),
        "unsupported type crossing into JS (spike supports nil/bool/int/float/string)",
    ))
}

#[magnus::init]
fn init(ruby: &Ruby) -> Result<(), Error> {
    let module = ruby.define_module("RustyRacer")?;
    let class = module.define_class("Context", ruby.class_object())?;
    class.define_singleton_method("new", function!(Context::new, 0))?;
    class.define_method("eval", method!(Context::eval, 1))?;
    class.define_method("eval_t", method!(Context::eval_t, 2))?;
    class.define_method("attach", method!(Context::attach, 2))?;
    class.define_method("stop", method!(Context::stop, 0))?;
    class.define_method("dispose", method!(Context::dispose, 0))?;
    Ok(())
}
