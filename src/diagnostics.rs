use mlua::{Lua, Result, ffi};
use serde::Serialize;
use serde_json::{Value as Json, json};
use std::{cell::RefCell, collections::{HashMap, VecDeque}, panic::{AssertUnwindSafe, catch_unwind}, rc::{Rc, Weak}};

const MAX_ERRORS: usize = 512;
const MAX_MESSAGE_BYTES: usize = 4096;
const MAX_FRAMES: i32 = 6;
type Hook = unsafe extern "C-unwind" fn(*mut ffi::lua_State);

struct Trace {
    previous: Option<Hook>,
    records: RefCell<Records>,
}
#[derive(Default)]
struct Records { total: u64, entries: VecDeque<Entry> }
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    sequence: u64,
    value_type: String,
    message: Option<String>,
    message_truncated: bool,
    frames: Vec<Frame>,
}
#[derive(Serialize)]
struct Frame { source: String, name: String, line: i32 }
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
        records.entries.push_back(Entry { sequence, value_type, message, message_truncated, frames });
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

pub fn install(lua: &Lua) -> Result<()> {
    if lua.app_data_ref::<Handle>().is_some() { return Ok(()); }
    let trace = unsafe {
        lua.exec_raw_lua(|raw| {
            let callbacks = ffi::lua_callbacks(raw.state());
            let trace = Rc::new(Trace {
                previous: (*callbacks).debugprotectederror,
                records: RefCell::new(Records { total: 0, entries: VecDeque::with_capacity(MAX_ERRORS) }),
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
    Ok(())
}

pub fn snapshot(lua: &Lua) -> Result<Json> {
    let Some(handle) = lua.app_data_ref::<Handle>() else {
        return Ok(json!({"enabled": false, "total": 0, "dropped": 0, "errors": []}));
    };
    let records = handle.0.records.borrow();
    Ok(json!({"enabled": true, "total": records.total,
        "dropped": records.total.saturating_sub(records.entries.len() as u64), "errors": records.entries}))
}

#[cfg(all(test, emulator_local_tests))]
#[path = "../local_tests/diagnostics.rs"]
mod tests;
