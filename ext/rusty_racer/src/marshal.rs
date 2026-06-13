// Value marshalling: the conversion layer between V8 handles, the thread-crossing
// plain-data JsVal, and Ruby objects. Extracted from lib.rs verbatim. JsVal and
// the four conversion entry points (js_to_jsval, jsval_to_js, jsval_to_ruby,
// ruby_to_jsval) are pub(crate); the depth-recursion helpers, the seen-tables,
// and the hex<->words BigInt codec stay private to this module.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;

use magnus::value::ReprValue;
use magnus::{
    prelude::*, Error, ExceptionClass, IntoValue, RArray, RHash, RString, Ruby, TryConvert, Value,
};

// ---------------------------------------------------------------------------
// Values crossing threads: plain Rust data. No Ruby allocation off the Ruby
// thread, no V8 handles off the V8 thread, no wire format. Replaces serde.c.
// ---------------------------------------------------------------------------
#[derive(Debug, Clone)]
pub(crate) enum JsVal {
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

pub(crate) fn js_to_jsval(scope: &mut v8::PinScope<'_, '_>, value: v8::Local<v8::Value>) -> JsVal {
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
pub(crate) fn jsval_to_js<'s>(scope: &mut v8::PinScope<'s, '_>, val: JsVal) -> v8::Local<'s, v8::Value> {
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

pub(crate) fn jsval_to_ruby(ruby: &Ruby, val: &JsVal) -> Result<Value, Error> {
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

pub(crate) fn ruby_to_jsval(val: Value) -> Result<JsVal, Error> {
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
