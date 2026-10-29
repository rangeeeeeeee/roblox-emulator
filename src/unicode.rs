use mlua::{ffi, state::NativeCall, Error, Function, Lua, MultiValue, Result, Table, Value};
use std::{cell::RefCell, ffi::{c_char, c_void}, ptr, rc::Rc};

#[link(name = "icu")]
unsafe extern "C" {
    fn unorm2_getNFCInstance(error: *mut i32) -> *const c_void;
    fn unorm2_getNFDInstance(error: *mut i32) -> *const c_void;
    fn unorm2_normalize(normalizer: *const c_void, source: *const u16, source_len: i32, destination: *mut u16, capacity: i32, error: *mut i32) -> i32;
    fn utext_openUTF8(text: *mut c_void, source: *const c_char, source_len: i64, error: *mut i32) -> *mut c_void;
    fn utext_close(text: *mut c_void) -> *mut c_void;
    fn ubrk_open(kind: i32, locale: *const c_char, text: *const u16, text_len: i32, error: *mut i32) -> *mut c_void;
    fn ubrk_setUText(iterator: *mut c_void, text: *mut c_void, error: *mut i32);
    fn ubrk_first(iterator: *mut c_void) -> i32;
    fn ubrk_next(iterator: *mut c_void) -> i32;
    fn ubrk_close(iterator: *mut c_void);
}

const MAX_UNICODE_BYTES: usize = 8 * 1024 * 1024;
fn icu_error() -> Error { Error::lua_string("Unicode processing failed") }

fn normalize(input: &[u8], compose: bool) -> Result<String> {
    if input.len() > MAX_UNICODE_BYTES { return Err(Error::runtime("BudgetExceeded: Unicode input size")); }
    let text = std::str::from_utf8(input).map_err(|_| Error::lua_string("String contains invalid UTF-8"))?;
    let source: Vec<u16> = text.encode_utf16().collect();
    let mut status = 0;
    let normalizer = unsafe { if compose { unorm2_getNFCInstance(&mut status) } else { unorm2_getNFDInstance(&mut status) } };
    if status > 0 || normalizer.is_null() { return Err(icu_error()); }
    let needed = unsafe { unorm2_normalize(normalizer, source.as_ptr(), source.len() as i32, ptr::null_mut(), 0, &mut status) };
    if (status > 0 && status != 15) || needed < 0 || needed as usize > MAX_UNICODE_BYTES * 2 { return Err(icu_error()); }
    status = 0;
    let mut output = vec![0u16; needed as usize + 1];
    let length = unsafe { unorm2_normalize(normalizer, source.as_ptr(), source.len() as i32, output.as_mut_ptr(), output.len() as i32, &mut status) };
    if status > 0 || length < 0 { return Err(icu_error()); }
    String::from_utf16(&output[..length as usize]).map_err(|_| icu_error())
}

struct Graphemes { _input: Vec<u8>, text: *mut c_void, iterator: *mut c_void, begin: i32, first: i64, last: i64 }
impl Drop for Graphemes {
    fn drop(&mut self) { unsafe { ubrk_close(self.iterator); utext_close(self.text); } }
}
impl Graphemes {
    fn next(&mut self) -> Option<(i64, i64)> {
        loop {
            let end = unsafe { ubrk_next(self.iterator) };
            if end < 0 || i64::from(self.begin) >= self.last { return None; }
            let begin = std::mem::replace(&mut self.begin, end);
            if i64::from(end) >= self.first { return Some((i64::from(begin) + 1, i64::from(end))); }
        }
    }
}
fn boundaries(input: &[u8], first: i64, last: i64) -> Result<Option<Graphemes>> {
    if input.len() > MAX_UNICODE_BYTES { return Err(Error::runtime("BudgetExceeded: Unicode input size")); }
    let len = input.len() as i64;
    let index = |value: i64| if value < 0 { len.saturating_add(value).saturating_add(1) } else { value.max(1) };
    let first = index(first); let last = index(last).min(len);
    if first <= 0 || last <= 0 || first > last || first > len { return Ok(None); }
    let input = input.to_vec();
    let mut status = 0;
    let text = unsafe { utext_openUTF8(ptr::null_mut(), input.as_ptr().cast(), len, &mut status) };
    if status > 0 || text.is_null() { return Err(icu_error()); }
    let iterator = unsafe { ubrk_open(0, c"root".as_ptr(), ptr::null(), 0, &mut status) };
    if status > 0 || iterator.is_null() { unsafe { utext_close(text); } return Err(icu_error()); }
    unsafe { ubrk_setUText(iterator, text, &mut status); }
    if status > 0 { unsafe { ubrk_close(iterator); utext_close(text); } return Err(icu_error()); }
    let begin = unsafe { ubrk_first(iterator) };
    Ok(Some(Graphemes { _input: input, text, iterator, begin, first, last }))
}

unsafe fn normalized(state: *mut ffi::lua_State, compose: bool) -> i32 {
    unsafe {
        ffi::luaL_checklstring(state, 1, ptr::null_mut());
        Lua::invoke_native_callback(state, move |lua, mut values| {
            let Some(Value::String(input)) = values.pop_front() else { return Err(icu_error()); };
            let output = normalize(&input.as_bytes(), compose)?;
            Ok(NativeCall::Return(MultiValue::from_vec(vec![Value::String(lua.create_string(output)?)])))
        })
    }
}
unsafe extern "C-unwind" fn nfc(state: *mut ffi::lua_State) -> i32 { unsafe { normalized(state, true) } }
unsafe extern "C-unwind" fn nfd(state: *mut ffi::lua_State) -> i32 { unsafe { normalized(state, false) } }
unsafe extern "C-unwind" fn graphemes(state: *mut ffi::lua_State) -> i32 {
    unsafe {
        ffi::luaL_checklstring(state, 1, ptr::null_mut());
        let first = ffi::luaL_optinteger(state, 2, 1) as i64;
        let last = ffi::luaL_optinteger(state, 3, -1) as i64;
        Lua::invoke_native_callback(state, move |lua, mut values| {
            let Some(Value::String(input)) = values.pop_front() else { return Err(icu_error()); };
            let spans = Rc::new(RefCell::new(boundaries(&input.as_bytes(), first, last)?));
            let iterator = lua.create_function(move |_, _: MultiValue| {
                let mut spans=spans.borrow_mut();
                if let Some((first,last))=spans.as_mut().and_then(Graphemes::next) {
                    Ok(MultiValue::from_vec(vec![Value::Number(first as f64),Value::Number(last as f64)]))
                } else { *spans=None; Ok(MultiValue::new()) }
            })?;
            Ok(NativeCall::Return(MultiValue::from_vec(vec![Value::Function(iterator)])))
        })
    }
}

pub(crate) fn install(lua: &Lua) -> Result<()> {
    let library: Table = lua.globals().get("utf8")?;
    let readonly = library.is_readonly();
    library.set_readonly(false);
    for (name, debug_name, callback) in [("nfcnormalize", c"nfcnormalize", nfc as ffi::lua_CFunction), ("nfdnormalize", c"nfdnormalize", nfd), ("graphemes", c"graphemes", graphemes)] {
        let function: Function = unsafe { lua.exec_raw((), |state| ffi::lua_pushcfunctiond(state, callback, debug_name.as_ptr()))? };
        library.raw_set(name, function)?;
    }
    library.set_readonly(readonly);
    Ok(())
}
