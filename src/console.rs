use mlua::{Lua, Value, ffi};
use serde_json::{Value as Json, json};
use std::{cell::RefCell, collections::HashSet, fmt::Write, rc::Rc};

const MAX_NODES: usize = 96;
const MAX_TEXT: usize = 2048;
const MAX_DEPTH: usize = 4;
const MAX_ENTRIES: usize = 32;

// Only Rust-owned snapshots cross into the UI. Keeping Lua table references here
// would keep printed objects alive and change what getgc observes.
#[derive(Clone, Default)]
pub(crate) struct Capture(Rc<RefCell<Option<Vec<Json>>>>);

pub(crate) struct CaptureGuard { capture: Capture, previous: Option<Vec<Json>> }
impl Drop for CaptureGuard {
    fn drop(&mut self) { *self.capture.0.borrow_mut() = self.previous.take(); }
}
pub(crate) fn stage(lua: &Lua, values: &mlua::MultiValue, strings: &[mlua::LuaString]) -> Option<CaptureGuard> {
    let capture = lua.app_data_ref::<Capture>()?.clone();
    let mut arguments = snapshot(lua, values.iter());
    let mut remaining = MAX_TEXT;
    for (node, text) in arguments.iter_mut().zip(strings) {
        if node["type"] == "table" { continue; }
        let (display, truncated) = render_bytes(&text.as_bytes(), remaining);
        remaining = remaining.saturating_sub(display.len());
        node["display"] = json!(display);
        node["displayTruncated"] = json!(truncated);
    }
    let previous = capture.0.replace(Some(arguments));
    Some(CaptureGuard { capture, previous })
}
pub(crate) fn take(lua: &Lua) -> Option<Vec<Json>> {
    lua.app_data_ref::<Capture>().and_then(|capture| capture.0.borrow_mut().take())
}

// Lua strings are byte strings. Escape invalid UTF-8 and non-printing controls
// only in the browser representation; LogService receives the original bytes.
pub(crate) fn display_bytes(bytes: &[u8], limit: usize) -> String {
    render_bytes(bytes, limit).0
}
pub(crate) fn render_bytes(bytes: &[u8], limit: usize) -> (String, bool) {
    let mut result = String::new();
    let mut remaining = bytes;
    while !remaining.is_empty() && result.len() < limit {
        let (valid, invalid) = match std::str::from_utf8(remaining) {
            Ok(text) => (text, 0),
            Err(error) => (std::str::from_utf8(&remaining[..error.valid_up_to()]).unwrap(), error.error_len().unwrap_or(remaining.len() - error.valid_up_to())),
        };
        for character in valid.chars() {
            if character.is_control() && !matches!(character, '\n' | '\r' | '\t') {
                let escaped = if character.is_ascii() {format!("\\x{:02X}", character as u32)} else {format!("\\u{{{:04X}}}", character as u32)};
                if result.len() + escaped.len() > limit { return (result, true); }
                result.push_str(&escaped);
            } else {
                if result.len() + character.len_utf8() > limit { return (result, true); }
                result.push(character);
            }
        }
        remaining = &remaining[valid.len()..];
        for byte in &remaining[..invalid] {
            if result.len() + 4 > limit { return (result, true); }
            let _ = write!(result, "\\x{byte:02X}");
        }
        remaining = &remaining[invalid..];
    }
    (result, !remaining.is_empty())
}

fn address(lua: &Lua, value: &Value) -> Option<String> {
    let pointer = value.to_pointer();
    if pointer.is_null() { return None; }
    let encoded = unsafe { ffi::lua_encodepointer(lua.state(), pointer as usize) };
    Some(format!("0x{encoded:016x}"))
}

pub(crate) fn snapshot<'a>(lua: &Lua, values: impl IntoIterator<Item = &'a Value>) -> Vec<Json> {
    let mut preview = Preview { lua, nodes: MAX_NODES, text: MAX_TEXT, seen: HashSet::new() };
    let mut result = Vec::new();
    for value in values.into_iter().take(32) {
        if preview.nodes == 0 { result.push(json!({"type":"...","truncated":true})); break; }
        result.push(preview.value(value, 0));
    }
    result
}

struct Preview<'a> { lua: &'a Lua, nodes: usize, text: usize, seen: HashSet<usize> }
impl Preview<'_> {
    fn text(&mut self, bytes: &[u8]) -> String {
        let rendered = display_bytes(bytes, self.text.min(512));
        self.text = self.text.saturating_sub(rendered.len());
        rendered
    }
    fn value(&mut self, value: &Value, depth: usize) -> Json {
        if self.nodes == 0 { return json!({"type":value.type_name(),"truncated":true}); }
        self.nodes -= 1;
        match value {
            Value::Nil => json!({"type":"nil"}),
            Value::Boolean(value) => json!({"type":"boolean","value":value}),
            Value::Integer(value) => json!({"type":"number","value":value}),
            Value::Number(value) => json!({"type":"number","value":if value.is_finite() { json!(value) } else { json!(value.to_string()) }}),
            Value::String(value) => {
                let bytes = value.as_bytes();
                let (text,truncated) = render_bytes(&bytes, self.text.min(512));
                self.text = self.text.saturating_sub(text.len());
                let escaped = std::str::from_utf8(&bytes).map_or(true, |text| text.chars().any(|c| c.is_control() && !matches!(c,'\n'|'\r'|'\t')));
                json!({"type":"string","value":text,"bytes":bytes.len(),"encoding":if escaped {"escaped-bytes"}else{"utf8"},"truncated":truncated})
            }
            Value::Table(table) => {
                let mut node = json!({"type":"table","address":address(self.lua,value),"entries":[]});
                if !self.seen.insert(table.to_pointer() as usize) { node["reference"] = json!(true); return node; }
                if depth >= MAX_DEPTH { node["truncated"] = json!(true); return node; }
                let mut entries = Vec::new();
                for pair in table.clone().pairs::<Value, Value>() {
                    if entries.len() >= MAX_ENTRIES || self.nodes < 2 { node["truncated"] = json!(true); break; }
                    match pair {
                        Ok((key,value)) => entries.push(json!({"key":self.value(&key,depth+1),"value":self.value(&value,depth+1)})),
                        Err(_) => { node["truncated"] = json!(true); break; }
                    }
                }
                node["entries"] = json!(entries);
                node
            }
            Value::UserData(data) => {
                if let Ok(instance) = data.borrow::<crate::instance::Instance>() {
                    let node = instance.0.borrow();
                    json!({"type":"Instance","name":self.text(node.name.as_bytes()),"class":self.text(node.class.as_bytes()),"address":address(self.lua,value)})
                } else { self.opaque(value) }
            }
            _ => self.opaque(value),
        }
    }
    fn opaque(&mut self, value: &Value) -> Json {
        if let Ok(Some(encoded)) = crate::types::encode_value(value) {
            return json!({"type":encoded.get("tag").and_then(Json::as_str).unwrap_or(value.type_name()),"value":encoded,"address":address(self.lua,value)});
        }
        json!({"type":value.type_name(),"address":address(self.lua,value)})
    }
}

#[cfg(all(test, emulator_local_tests))]
#[path = "../local_tests/console.rs"]
mod tests;
