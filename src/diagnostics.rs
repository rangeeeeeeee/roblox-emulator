use mlua::{Lua, Result, ffi};
use serde::Serialize;
use serde_json::{Value as Json, json};
use std::{cell::{Cell, RefCell}, collections::{HashMap, VecDeque}, panic::{AssertUnwindSafe, catch_unwind}, rc::{Rc, Weak}};

const MAX_ERRORS: usize = 512;
const MAX_MESSAGE_BYTES: usize = 4096;
const MAX_FRAMES: i32 = 6;
type Hook = unsafe extern "C-unwind" fn(*mut ffi::lua_State);
unsafe extern "C" {
    fn luau_emulator_set_protected_observer(observer: Option<unsafe extern "C" fn(*mut ffi::lua_State)>);
}

type ErrorSink = Rc<dyn Fn(&Entry)>;
struct Trace {
    previous: Option<Hook>,
    enabled: Cell<bool>,
    records: RefCell<Records>,
    sink: RefCell<Option<ErrorSink>>,
    origin: Option<Rc<dyn Fn(usize) -> Option<String>>>,
}
#[derive(Default)]
struct Records { total: u64, entries: VecDeque<Entry> }
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Entry {
    pub(crate) sequence: u64,
    pub(crate) value_type: String,
    pub(crate) message: Option<String>,
    pub(crate) message_truncated: bool,
    pub(crate) frames: Vec<Frame>,
    pub(crate) origin: Option<String>,
}
#[derive(Clone, Serialize)]
pub(crate) struct Frame { pub(crate) source: String, pub(crate) name: String, pub(crate) line: i32 }
struct Handle(Rc<Trace>);

thread_local! {
    static TRACES: RefCell<HashMap<usize, Weak<Trace>>> = RefCell::new(HashMap::new());
}

unsafe fn bounded_c_string(pointer: *const std::ffi::c_char, limit: usize) -> String {
    if pointer.is_null() { return String::new(); }
    unsafe {
        let mut length = 0;
        while length < limit && *pointer.add(length) != 0 { length += 1; }
        String::from_utf8_lossy(std::slice::from_raw_parts(pointer.cast::<u8>(), length)).into_owned()
    }
}

// Only inspection APIs are used here. In particular, no Lua allocation, coercion,
// function call, stack push/pop, or metamethod can be caused by this observer.
unsafe fn capture(state: *mut ffi::lua_State, trace: &Trace) {
    if !trace.enabled.get() { return; }
    let Ok(mut records) = trace.records.try_borrow_mut() else { return; };
    unsafe {
        let kind = ffi::lua_type(state, -1);
        let value_type = if kind < 0 { "no value".into() } else { bounded_c_string(ffi::lua_typename(state, kind), 32) };
        let (message, message_truncated) = if kind == ffi::LUA_TSTRING {
            let mut length = 0;
            let pointer = ffi::lua_tolstring(state, -1, &mut length);
            let bytes = std::slice::from_raw_parts(pointer.cast::<u8>(), length.min(MAX_MESSAGE_BYTES));
            (Some(String::from_utf8_lossy(bytes).into_owned()), length > MAX_MESSAGE_BYTES)
        } else { (None, false) };
        let mut frames = Vec::with_capacity(MAX_FRAMES as usize);
        for level in 0..MAX_FRAMES {
            let mut debug: ffi::lua_Debug = std::mem::zeroed();
            if ffi::lua_getinfo(state, level, c"sln".as_ptr(), &mut debug) == 0 { break; }
            frames.push(Frame {
                source: bounded_c_string(debug.source, 512),
                name: bounded_c_string(debug.name, 128),
                line: debug.currentline,
            });
        }
        records.total = records.total.saturating_add(1);
        let sequence = records.total;
        if records.entries.len() == MAX_ERRORS { records.entries.pop_front(); }
        let origin = trace.origin.as_ref().and_then(|resolve| resolve(state as usize));
        let entry = Entry { sequence, value_type, message, message_truncated, frames, origin };
        records.entries.push_back(entry.clone());
        drop(records);
        if let Ok(sink) = trace.sink.try_borrow() {
            if let Some(sink) = sink.as_ref() { sink(&entry); }
        }
    }
}

unsafe extern "C-unwind" fn protected_error(state: *mut ffi::lua_State) {
    let key = unsafe { ffi::lua_callbacks(state) } as usize;
    let trace = TRACES.try_with(|traces| traces.try_borrow().ok().and_then(|traces| traces.get(&key).and_then(Weak::upgrade))).ok().flatten();
    if let Some(trace) = trace {
        // Observer failures never cross the VM boundary. The previous native hook
        // retains its own behavior and receives the unmodified VM state.
        let _ = catch_unwind(AssertUnwindSafe(|| unsafe { capture(state, &trace) }));
        if let Some(previous) = trace.previous { unsafe { previous(state) }; }
    }
}

// Separate from Luau's debugger callback: invoking a previous debugger hook at
// non-yieldable boundaries would violate its contract.
unsafe extern "C" fn protected_error_nonyieldable(state: *mut ffi::lua_State) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let key = unsafe { ffi::lua_callbacks(state) } as usize;
        let trace = TRACES.try_with(|traces| traces.try_borrow().ok().and_then(|traces| traces.get(&key).and_then(Weak::upgrade))).ok().flatten();
        if let Some(trace) = trace { unsafe { capture(state, &trace) }; }
    }));
}

pub fn install(lua: &Lua) -> Result<()> {
    if lua.app_data_ref::<Handle>().is_some() { return Ok(()); }
    let origin = crate::scheduler::origin_resolver(lua);
    let trace = unsafe {
        lua.exec_raw_lua(|raw| {
            let callbacks = ffi::lua_callbacks(raw.state());
            let trace = Rc::new(Trace {
                previous: (*callbacks).debugprotectederror,
                enabled: Cell::new(true),
                records: RefCell::new(Records { total: 0, entries: VecDeque::with_capacity(MAX_ERRORS) }),
                sink: RefCell::new(None),
                origin,
            });
            TRACES.with(|traces| {
                let mut traces = traces.borrow_mut();
                traces.retain(|_, trace| trace.strong_count() != 0);
                traces.insert(callbacks as usize, Rc::downgrade(&trace));
            });
            (*callbacks).debugprotectederror = Some(protected_error);
            trace
        })
    };
    lua.set_app_data(Handle(trace));
    unsafe { luau_emulator_set_protected_observer(Some(protected_error_nonyieldable)); }
    Ok(())
}

// The sink runs entirely on the host and must not call into Luau or coerce error objects.
pub(crate) fn configure(lua: &Lua, enabled: bool, sink: Option<ErrorSink>) -> Result<()> {
    if enabled { install(lua)?; }
    if let Some(handle) = lua.app_data_ref::<Handle>() {
        *handle.0.sink.borrow_mut() = sink;
        handle.0.enabled.set(enabled);
    }
    Ok(())
}

// Inspecting debug info does not mutate the Lua stack or run script code.
pub(crate) fn caller(lua: &Lua) -> Option<(String, i32)> {
    unsafe {
        for level in 1..32 {
            let mut debug: ffi::lua_Debug = std::mem::zeroed();
            if ffi::lua_getinfo(lua.state(), level, c"sl".as_ptr(), &mut debug) == 0 { break; }
            let source = bounded_c_string(debug.source, 512);
            if debug.currentline > 0 && !source.is_empty() && source != "[C]" {
                return Some((source, debug.currentline));
            }
        }
    }
    None
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SpyCaller {
    source: String,
    line: i32,
    short_source: String,
    defined_line: i32,
    name: String,
    nupvalues: u8,
    nparams: u8,
    isvararg: bool,
}

// These options only read active debug frames; unlike 'f', they never push a
// function or execute Lua code. Keep this separate from ordinary output.
pub(crate) fn spy_caller(lua: &Lua) -> Option<SpyCaller> {
    unsafe {
        for level in 1..32 {
            let mut debug: ffi::lua_Debug = std::mem::zeroed();
            if ffi::lua_getinfo(lua.state(), level, c"slnua".as_ptr(), &mut debug) == 0 { break; }
            let source = bounded_c_string(debug.source, 512);
            if debug.currentline > 0 && !source.is_empty() && source != "[C]" {
                return Some(SpyCaller { source, line: debug.currentline,
                    short_source: bounded_c_string(debug.short_src, 512), defined_line: debug.linedefined,
                    name: bounded_c_string(debug.name, 128), nupvalues: debug.nupvals,
                    nparams: debug.nparams, isvararg: debug.isvararg != 0 });
            }
        }
    }
    None
}

pub fn snapshot(lua: &Lua) -> Result<Json> {
    let Some(handle) = lua.app_data_ref::<Handle>() else {
        return Ok(json!({"enabled": false, "total": 0, "dropped": 0, "errors": []}));
    };
    let records = handle.0.records.borrow();
    Ok(json!({"enabled": handle.0.enabled.get(), "total": records.total,
        "dropped": records.total.saturating_sub(records.entries.len() as u64), "errors": records.entries}))
}

#[cfg(all(test, emulator_local_tests))]
#[path = "../local_tests/diagnostics.rs"]
mod tests;
