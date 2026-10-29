use mlua::{Error, Lua, LuaString, Result, Table, UserData, UserDataMethods};
use regex::bytes::{Captures, Regex, RegexBuilder};
use std::{cell::Cell, rc::Rc, time::{Duration, Instant}};

const MAX_PATTERN: usize = 2048;
const MAX_INPUT: usize = 8192;
const MAX_REPLACEMENT: usize = 8192;
const MAX_OUTPUT: usize = 65536;
const MAX_MATCHES: usize = 1024;
const MAX_SCANNED: usize = 1_048_576;
const MAX_PATTERNS: usize = 64;
const MAX_SEARCH_TIME: Duration = Duration::from_millis(100);

struct Pattern { regex: Regex, count: Rc<Cell<usize>> }
impl Drop for Pattern {
    fn drop(&mut self) { self.count.set(self.count.get().saturating_sub(1)); }
}
fn limited_input(value: &LuaString, max: usize) -> Result<()> {
    if value.as_bytes().len() > max { return Err(Error::lua_string("RegexLimit: input is too large")); }
    Ok(())
}
fn deadline(start: Instant) -> Result<()> {
    if start.elapsed() > MAX_SEARCH_TIME { return Err(Error::lua_string("RegexLimit: wall-time budget exceeded")); }
    Ok(())
}
fn append(output: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_OUTPUT.saturating_sub(output.len()) { return Err(Error::lua_string("RegexLimit: output is too large")); }
    output.extend_from_slice(bytes);
    Ok(())
}

fn replacement(captures: &Captures<'_>, input: &[u8], replacement: &[u8]) -> Result<Vec<u8>> {
    let found = captures.get(0).expect("captures always include the match");
    let mut output = Vec::new();
    append(&mut output, &input[..found.start()])?;
    let mut index = 0;
    while index < replacement.len() {
        if replacement[index] != b'$' || index + 1 == replacement.len() {
            append(&mut output, &replacement[index..index + 1])?;
            index += 1;
            continue;
        }
        match replacement[index + 1] {
            b'$' => { append(&mut output, b"$")?; index += 2; }
            b'&' => { append(&mut output, found.as_bytes())?; index += 2; }
            b'`' => { append(&mut output, &input[..found.start()])?; index += 2; }
            b'\'' => { append(&mut output, &input[found.end()..])?; index += 2; }
            b'<' => return Err(Error::lua_string("UnsupportedApi: named capture replacement")),
            first @ b'1'..=b'9' => {
                let first = (first - b'0') as usize;
                if first >= captures.len() { append(&mut output, b"$")?; index += 1; continue; }
                let mut group = first;
                let mut consumed = 2;
                if let Some(second @ b'0'..=b'9') = replacement.get(index + 2).copied() {
                    let candidate = first * 10 + (second - b'0') as usize;
                    if candidate < captures.len() { group = candidate; consumed = 3; }
                }
                if let Some(value) = captures.get(group) { append(&mut output, value.as_bytes())?; }
                index += consumed;
            }
            _ => { append(&mut output, b"$")?; index += 1; }
        }
    }
    append(&mut output, &input[found.end()..])?;
    Ok(output)
}

impl UserData for Pattern {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("Match", |lua, this, value: LuaString| {
            limited_input(&value, MAX_INPUT)?;
            let input = value.as_bytes();
            let start = Instant::now();
            let found = this.regex.find(&input);
            deadline(start)?;
            found.map(|found| lua.create_string(found.as_bytes())).transpose()
        });
        methods.add_method("MatchMany", |lua, this, value: LuaString| {
            limited_input(&value, MAX_INPUT)?;
            let input = value.as_bytes();
            let start = Instant::now();
            let mut scan = 0usize;
            let mut next = 0usize;
            let mut bytes = 0usize;
            let mut count = 0usize;
            let mut matches = this.regex.find_iter(&input);
            let output = lua.create_table()?;
            loop {
                scan = scan.saturating_add(input.len().saturating_sub(next).saturating_add(1));
                if scan > MAX_SCANNED { return Err(Error::lua_string("RegexLimit: search work exceeded")); }
                deadline(start)?;
                let found = matches.next();
                deadline(start)?;
                let Some(found) = found else { break; };
                count += 1;
                bytes = bytes.saturating_add(found.len());
                if count > MAX_MATCHES || bytes > MAX_OUTPUT { return Err(Error::lua_string("RegexLimit: too many matches")); }
                output.raw_set(count, lua.create_string(found.as_bytes())?)?;
                next = if found.start() == found.end() { found.end().saturating_add(1) } else { found.end() };
            }
            Ok(output)
        });
        methods.add_method("Replace", |lua, this, (value, text): (LuaString, LuaString)| {
            limited_input(&value, MAX_INPUT)?;
            limited_input(&text, MAX_REPLACEMENT)?;
            let input = value.as_bytes();
            let replacement_text = text.as_bytes();
            let start = Instant::now();
            let found = this.regex.captures(&input);
            deadline(start)?;
            let output = match found { Some(captures) => replacement(&captures, &input, &replacement_text)?, None => input.to_vec() };
            deadline(start)?;
            lua.create_string(output)
        });
    }
}

pub(super) fn install(lua: &Lua, env: &Table, caps: &Table) -> Result<()> {
    let library = lua.create_table()?;
    let count = Rc::new(Cell::new(0usize));
    super::set(lua, &library, "new", move |lua, pattern: LuaString| {
        limited_input(&pattern, MAX_PATTERN)?;
        if count.get() >= MAX_PATTERNS { return Err(Error::lua_string("RegexLimit: too many live patterns")); }
        let pattern = pattern.to_str().map_err(|_|Error::lua_string("UnsupportedApi: regex patterns must be UTF-8; use \\xNN for bytes"))?;
        let regex = RegexBuilder::new(&pattern).unicode(false).size_limit(8192).dfa_size_limit(65536).nest_limit(100)
            .build().map_err(|error|Error::lua_string(format!("UnsupportedApi: invalid or unsupported regex: {error}")))?;
        count.set(count.get() + 1);
        lua.create_userdata(Pattern { regex, count: count.clone() })
    })?;
    super::set(lua, &library, "Escape", |lua, input: LuaString| {
        limited_input(&input, MAX_PATTERN / 4)?;
        let mut output = String::new();
        for &byte in input.as_bytes().iter() {
            if !(0x20..=0x7e).contains(&byte) { use std::fmt::Write; write!(&mut output, "\\x{byte:02X}").unwrap(); }
            else {
                if b"\\.^$|?*+()[]{}".contains(&byte) { output.push('\\'); }
                output.push(byte as char);
            }
        }
        lua.create_string(output)
    })?;
    for name in ["new", "Escape"] { crate::native::mark_executor_function(lua,&library.get(name)?,env)?; }
    env.set("Regex", library)?;
    caps.set("Regex", "partial: byte-oriented regular expressions; bounded input, output, patterns and work; wall time checked between native searches")?;
    Ok(())
}
