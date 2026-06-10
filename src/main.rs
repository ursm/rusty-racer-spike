// Spike: can bare rusty_v8 (crate v8 v150) carry mini_racer-csim's V8 half?
//
// Probes, in order of architectural risk:
//   1. realms      — multiple v8::Context in ONE isolate, shared security token
//                    (csim's per-frame realm model; the thing deno_core removed)
//   2. termination — watchdog TerminateExecution from another thread via
//                    IsolateHandle (the C++ #63 stop-UAF / #3 stale-terminate
//                    class), recover and keep using the isolate
//   3. modules     — the load_module_graph slice: level-walk a synthetic
//                    ~83-module graph with batched fetch/resolve callbacks,
//                    native instantiate through a resolver, evaluate, fresh
//                    realm per visit. Timed.
//
// The fetch/resolve closures stand in for the Ruby roundtrip ('f'/'r' wire
// messages); the Magnus/GVL half is out of scope for this spike.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Per-realm module registry (csim: Realm::modules + module_id_by_url).
// instantiate_module's resolver is a plain fn (no closures), the same
// constraint as V8's C++ API — state goes in a thread_local, the moral
// equivalent of isolate->GetData(0).
// ---------------------------------------------------------------------------
#[derive(Default)]
struct Registry {
    by_url: HashMap<String, v8::Global<v8::Module>>,
    // module identity -> url, for referrer lookup in the resolver. csim's C++
    // does an O(N) scan here (audit finding #2); the hash map is the natural
    // shape in Rust.
    url_by_hash: HashMap<i32, Vec<(v8::Global<v8::Module>, String)>>,
    // (referrer_url, specifier) -> resolved url, filled by the resolve batch
    edges: HashMap<(String, String), String>,
}

impl Registry {
    fn register(&mut self, scope: &v8::Isolate, url: &str, module: v8::Local<v8::Module>) {
        let hash = module.get_identity_hash().get();
        let global = v8::Global::new(scope, module);
        self.by_url.insert(url.to_string(), global.clone());
        self.url_by_hash
            .entry(hash)
            .or_default()
            .push((global, url.to_string()));
    }

    fn url_of(
        &self,
        scope: &v8::PinScope<'_, '_, ()>,
        module: v8::Local<v8::Module>,
    ) -> Option<String> {
        let hash = module.get_identity_hash().get();
        let bucket = self.url_by_hash.get(&hash)?;
        for (global, url) in bucket {
            if v8::Local::new(scope, global) == module {
                return Some(url.clone());
            }
        }
        None
    }

    fn clear(&mut self) {
        self.by_url.clear();
        self.url_by_hash.clear();
        self.edges.clear();
    }
}

thread_local! {
    static REGISTRY: RefCell<Registry> = RefCell::new(Registry::default());
}

// V8 calls this once per import edge during InstantiateModule (csim:
// graph_resolve_callback). Pure registry lookup — no Ruby roundtrip.
fn resolve_module<'s>(
    context: v8::Local<'s, v8::Context>,
    specifier: v8::Local<'s, v8::String>,
    _import_attributes: v8::Local<'s, v8::FixedArray>,
    referrer: v8::Local<'s, v8::Module>,
) -> Option<v8::Local<'s, v8::Module>> {
    // The one unsafe in the file: materializing a scope inside a V8 callback.
    v8::callback_scope!(unsafe scope, context);
    let spec = specifier.to_rust_string_lossy(scope);
    REGISTRY.with(|r| {
        let r = r.borrow();
        let ref_url = r.url_of(scope, referrer)?;
        let url = r.edges.get(&(ref_url, spec))?;
        let module = r.by_url.get(url)?;
        Some(v8::Local::new(scope, module))
    })
}

fn module_origin<'s>(scope: &v8::PinScope<'s, '_>, url: &str) -> v8::ScriptOrigin<'s> {
    let name = v8::String::new(scope, url).unwrap();
    v8::ScriptOrigin::new(
        scope,
        name.into(),
        0,     // line offset
        0,     // column offset
        false, // shared cross-origin
        -1,    // script id
        None,  // source map
        false, // opaque
        false, // wasm
        true,  // is_module
        None,  // host-defined options
    )
}

// ---------------------------------------------------------------------------
// Synthetic graph: a binary tree of N side-effect modules. mod_i imports
// mod_{2i+1} and mod_{2i+2}; every module bumps a global counter so evaluate
// completeness is checkable. N modules, N-1 edges, depth ~log2(N).
// ---------------------------------------------------------------------------
fn synthetic_sources(n: usize) -> HashMap<String, String> {
    let mut sources = HashMap::new();
    for i in 0..n {
        let mut body = String::new();
        for child in [2 * i + 1, 2 * i + 2] {
            if child < n {
                body.push_str(&format!("import \"./mod_{child}.js\";\n"));
            }
        }
        body.push_str("globalThis.__loaded = (globalThis.__loaded || 0) + 1;\n");
        body.push_str(&format!("export const id = {i};\n"));
        sources.insert(format!("/mod_{i}.js"), body);
    }
    sources
}

// The load_module_graph slice (csim: walk_module_graph + instantiate_and_
// evaluate). fetch/resolve are batched closures standing in for Ruby.
fn load_module_graph(
    scope: &mut v8::PinScope<'_, '_>,
    entry_url: &str,
    fetch_batch: &dyn Fn(&[String]) -> Vec<Option<String>>,
    resolve_batch: &dyn Fn(&[(String, String)]) -> Vec<Option<String>>,
) -> Result<(f64, usize), String> {
    let mut to_fetch = vec![entry_url.to_string()];
    let mut seen: HashSet<String> = to_fetch.iter().cloned().collect();
    let mut new_modules = 0usize;

    while !to_fetch.is_empty() {
        // ---- FETCH batch (one "Ruby roundtrip" per level) ----
        let fetched = fetch_batch(&to_fetch);

        // ---- compile + register the level, collect edges ----
        let mut level_edges: Vec<(String, String)> = Vec::new(); // (specifier, referrer)
        for (url, source) in to_fetch.iter().zip(fetched) {
            let source = source.ok_or_else(|| format!("fetch failed: {url}"))?;
            let code = v8::String::new(scope, &source).ok_or("source alloc")?;
            let origin = module_origin(scope, url);
            let mut src = v8::script_compiler::Source::new(code, Some(&origin));
            let module = v8::script_compiler::compile_module(scope, &mut src)
                .ok_or_else(|| format!("compile failed: {url}"))?;
            REGISTRY.with(|r| r.borrow_mut().register(scope, url, module));
            new_modules += 1;
            let requests = module.get_module_requests();
            for i in 0..requests.length() {
                let req: v8::Local<v8::ModuleRequest> =
                    requests.get(scope, i).unwrap().try_into().unwrap();
                let spec = req.get_specifier().to_rust_string_lossy(scope);
                level_edges.push((spec, url.clone()));
            }
        }

        // ---- RESOLVE batch (one "Ruby roundtrip" per level) ----
        to_fetch.clear();
        if level_edges.is_empty() {
            continue;
        }
        let resolved = resolve_batch(&level_edges);
        for ((spec, referrer), url) in level_edges.into_iter().zip(resolved) {
            let url = url.ok_or_else(|| format!("unresolvable: {spec} from {referrer}"))?;
            REGISTRY.with(|r| {
                r.borrow_mut().edges.insert((referrer, spec), url.clone());
            });
            let registered = REGISTRY.with(|r| r.borrow().by_url.contains_key(&url));
            if !registered && seen.insert(url.clone()) {
                to_fetch.push(url);
            }
        }
    }

    // ---- native instantiate + evaluate (csim: instantiate_and_evaluate) ----
    let entry = REGISTRY
        .with(|r| r.borrow().by_url.get(entry_url).cloned())
        .ok_or("entry missing")?;
    let entry = v8::Local::new(scope, &entry);
    entry
        .instantiate_module(scope, resolve_module)
        .filter(|&ok| ok)
        .ok_or("instantiate failed")?;
    let value = entry.evaluate(scope).ok_or("evaluate failed")?;
    scope.perform_microtask_checkpoint();
    let promise: v8::Local<v8::Promise> =
        value.try_into().map_err(|_| "not a promise".to_string())?;
    if promise.state() != v8::PromiseState::Fulfilled {
        return Err("module evaluation not fulfilled".into());
    }

    let count = eval_number(scope, "globalThis.__loaded")?;
    Ok((count, new_modules))
}

fn eval_number(scope: &mut v8::PinScope<'_, '_>, code: &str) -> Result<f64, String> {
    let code = v8::String::new(scope, code).ok_or("code alloc")?;
    let script = v8::Script::compile(scope, code, None).ok_or("compile")?;
    let value = script.run(scope).ok_or("run")?;
    value
        .number_value(scope)
        .ok_or_else(|| "not a number".to_string())
}

fn main() {
    let platform = v8::new_default_platform(0, false).make_shared();
    v8::V8::initialize_platform(platform);
    v8::V8::initialize();

    let isolate = &mut v8::Isolate::new(Default::default());

    // =======================================================================
    // 1. realms — N contexts in one isolate, shared security token
    // =======================================================================
    {
        v8::scope!(let scope, isolate);
        let realm_a = v8::Context::new(scope, Default::default());
        let realm_b = v8::Context::new(scope, Default::default());
        // csim: every realm shares the first realm's token (same-origin iframes)
        let token = realm_a.get_security_token(scope);
        realm_b.set_security_token(token);

        {
            let scope = &mut v8::ContextScope::new(scope, realm_a);
            eval_number(scope, "globalThis.fromA = 41; 0").unwrap();
        }
        {
            // cross-realm access: realm B reads realm A's global (realmGlobal)
            let scope = &mut v8::ContextScope::new(scope, realm_b);
            let a_global = realm_a.global(scope);
            let key = v8::String::new(scope, "fromA").unwrap();
            let value = a_global.get(scope, key.into()).unwrap();
            let n = value.number_value(scope).unwrap();
            assert_eq!(n, 41.0);
            println!("[1] realms: 2 contexts, shared token, cross-realm read = {n} ok");
        }
    }

    // =======================================================================
    // 2. termination — watchdog from another thread, then recover.
    //    IsolateHandle is Send + refcounted: the C++ #63 class (stop() on a
    //    freed State*) is unrepresentable — the handle outlives safely.
    // =======================================================================
    {
        let handle = isolate.thread_safe_handle();
        v8::scope!(let scope, isolate);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);

        let watchdog = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            handle.terminate_execution();
        });

        let started = Instant::now();
        {
            v8::tc_scope!(let tc, scope);
            let code = v8::String::new(tc, "for(;;){}").unwrap();
            let script = v8::Script::compile(tc, code, None).unwrap();
            let result = script.run(tc); // None on termination — no abort path
            assert!(result.is_none() && tc.has_terminated());
        }
        watchdog.join().unwrap();
        scope.cancel_terminate_execution(); // explicit, in one place — not 13 epilogues

        let n = eval_number(scope, "6 * 7").unwrap();
        assert_eq!(n, 42.0);
        println!(
            "[2] termination: watchdog fired at ~{:?}, isolate recovered, eval after = {n} ok",
            started.elapsed()
        );
    }

    // =======================================================================
    // 3. the load_module_graph slice + bench
    // =======================================================================
    {
        const N_MODULES: usize = 83;
        const VISITS: usize = 50;
        let sources = synthetic_sources(N_MODULES);

        let fetch_batch = |urls: &[String]| -> Vec<Option<String>> {
            urls.iter().map(|u| sources.get(u).cloned()).collect()
        };
        let resolve_batch = |edges: &[(String, String)]| -> Vec<Option<String>> {
            edges
                .iter()
                .map(|(spec, _ref)| Some(spec.replace("./", "/")))
                .collect()
        };

        let mut timings = Vec::with_capacity(VISITS);
        for visit in 0..VISITS {
            // fresh realm per visit (csim: reset_realm / per-frame realm)
            v8::scope!(let scope, isolate);
            let realm = v8::Context::new(scope, Default::default());
            let scope = &mut v8::ContextScope::new(scope, realm);
            REGISTRY.with(|r| r.borrow_mut().clear());

            let t = Instant::now();
            let (loaded, compiled) =
                load_module_graph(scope, "/mod_0.js", &fetch_batch, &resolve_batch)
                    .expect("graph load");
            timings.push(t.elapsed());

            assert_eq!(loaded as usize, N_MODULES);
            assert_eq!(compiled, N_MODULES);
            if visit == 0 {
                println!(
                    "[3] module graph: {compiled} modules compiled, {loaded} evaluated, \
                     2 roundtrips/level ok"
                );
            }
        }

        timings.sort();
        let mean: Duration = timings.iter().sum::<Duration>() / VISITS as u32;
        println!(
            "[3] bench: {VISITS} visits x {N_MODULES} modules - mean {:?}, median {:?}, min {:?}",
            mean,
            timings[VISITS / 2],
            timings[0]
        );
    }

    println!("spike: all probes passed");
}
