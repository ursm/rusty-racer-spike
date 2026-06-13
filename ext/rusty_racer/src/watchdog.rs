// The execution watchdog: a persistent per-isolate thread that fires
// TerminateExecution when an armed deadline passes, plus the request bracket
// (run_js_bracketed) that arms/disarms it around every JS-running op and maps a
// fired deadline to VmError::Terminated. Extracted from lib.rs verbatim.
//
// Only WatchdogShared is pub(crate) (IsolateState holds the Arc<WatchdogShared>
// and watchdog_loop takes it); its fields and the whole WatchdogInner/
// WatchdogFrame state stay PRIVATE — lib.rs touches the watchdog through just
// two methods, WatchdogShared::new() (initial state) and request_shutdown()
// (teardown: set the flag + wake the loop). run_js_bracketed, arm_watchdog,
// disarm_watchdog, watchdog_loop and WATCHDOG_DEBUG are pub(crate) because the
// op handlers and isolate setup (still in lib.rs) call them;
// report_watchdog_anomaly is private to this module.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::istate;
use crate::{IsolateState, JsVal, VmError};

// The watchdog runs on ONE persistent thread per isolate rather than a fresh
// std::thread per request: spawning + joining a thread on every op cost ~16µs
// (5.5x) when a timeout was set, dwarfing the actual work. The thread sleeps on
// a condvar until a deadline is armed, terminates execution once the deadline
// passes, then goes back to sleep.
pub(crate) struct WatchdogShared {
    inner: Mutex<WatchdogInner>,
    cv: Condvar,
}

impl WatchdogShared {
    // The initial (idle) watchdog state, boxed in the Arc IsolateState holds.
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(WatchdogShared {
            inner: Mutex::new(WatchdogInner {
                frames: Vec::new(),
                next_generation: 0,
                fired_generation: None,
                shutdown: false,
            }),
            cv: Condvar::new(),
        })
    }

    // Signal the loop to stop and wake it. Called once at isolate teardown,
    // before the isolate is touched, so the loop can't fire a terminate into an
    // isolate we're mid-disposing.
    pub(crate) fn request_shutdown(&self) {
        self.inner.lock().unwrap().shutdown = true;
        self.cv.notify_one();
    }
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
pub(crate) fn watchdog_loop(shared: Arc<WatchdogShared>, handle: v8::IsolateHandle) {
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
pub(crate) fn arm_watchdog(scope: &mut v8::PinScope<'_, '_, ()>, timeout_ms: u64) -> Option<u64> {
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
pub(crate) fn disarm_watchdog(scope: &mut v8::PinScope<'_, '_, ()>, generation: Option<u64>) -> bool {
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

// Set from RUSTY_RACER_WATCHDOG_DEBUG at init (OFF by default); gates the
// watchdog-anomaly canary in run_js_bracketed — a diagnostic for the rare
// next-op-spuriously-terminated leak. Off in production (it would also fire on a
// legitimate Isolate#terminate); CI turns it on so a recurrence is diagnosable.
pub(crate) static WATCHDOG_DEBUG: AtomicBool = AtomicBool::new(false);

// The shared bracket every JS-running request (Eval/Call/Attach/RunScript/
// EvaluateModule) needs: arm the watchdog, run |body|, then on a watchdog
// timeout flag the leftover terminate for the outermost sweep and — only if
// |body| actually ran JS (the bool it returns) — override its outcome to
// Terminated. |body| owns its ContextScope, JS call, and auto_drain, and
// returns (ran_js, outcome); the realm-disposed/unknown paths return
// (false, Err(..)) so a raced watchdog can't poison an error for work that ran
// no JS. Collapsing the five arms onto this keeps the terminate discipline in
// ONE place.
pub(crate) fn run_js_bracketed(
    scope: &mut v8::PinScope<'_, '_, ()>,
    outermost: bool,
    timeout_ms: u64,
    label: &'static str,
    body: impl FnOnce(&mut v8::PinScope<'_, '_, ()>, bool) -> (bool, Result<JsVal, VmError>),
) -> Result<JsVal, VmError> {
    let started = Instant::now();
    let watchdog = arm_watchdog(scope, timeout_ms);
    let (ran_js, mut outcome) = body(scope, outermost);
    let fired = disarm_watchdog(scope, watchdog);
    // CANARY (RUSTY_RACER_WATCHDOG_DEBUG): the op's JS was terminated but THIS
    // op's OWN watchdog frame did NOT fire — so a terminate LEAKED in from
    // elsewhere (a prior op's timeout surviving both the end- and start-sweep
    // cancels, or a user Isolate#terminate). The rare CI "next op spuriously
    // terminated" bug lands here; dump the watchdog state + timing so a
    // recurrence is diagnosable instead of an unreproducible mystery.
    if WATCHDOG_DEBUG.load(Ordering::Relaxed)
        && ran_js
        && !fired
        && matches!(outcome, Err(VmError::Terminated))
    {
        report_watchdog_anomaly(scope, label, watchdog, timeout_ms, started.elapsed());
    }
    if fired {
        istate!(scope).watchdog_fired = true;
        if ran_js {
            outcome = Err(VmError::Terminated);
        }
    }
    outcome
}

// Dump watchdog/terminate state on the leaked-terminate anomaly (see the CANARY
// in run_js_bracketed). Only reached on that rare path, and only with the debug
// flag on. elapsed_ms << timeout_ms with a clean inner = a V8-level stale
// terminate (not the Rust bookkeeping); a non-empty inner.frames /
// fired_generation would instead point at a frame-lifecycle bug.
fn report_watchdog_anomaly(
    scope: &mut v8::PinScope<'_, '_, ()>,
    label: &str,
    this_gen: Option<u64>,
    timeout_ms: u64,
    elapsed: Duration,
) {
    let terminating = scope.is_execution_terminating();
    let st = istate!(scope);
    let watchdog_fired_flag = st.watchdog_fired;
    let inner = st.watchdog.inner.lock().unwrap();
    let frames: Vec<u64> = inner.frames.iter().map(|f| f.generation).collect();
    eprintln!(
        "[rusty watchdog ANOMALY] op={label} terminated but its OWN watchdog frame \
         did NOT fire (leaked terminate). this_gen={this_gen:?} timeout_ms={timeout_ms} \
         elapsed_ms={:.2} is_terminating={terminating} watchdog_fired_flag={watchdog_fired_flag} \
         inner.frames={frames:?} inner.fired_generation={:?} inner.next_generation={}",
        elapsed.as_secs_f64() * 1000.0,
        inner.fired_generation,
        inner.next_generation,
    );
}
