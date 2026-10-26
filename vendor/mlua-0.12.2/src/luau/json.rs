use std::array;
use std::collections::HashMap;
use std::iter::Peekable;
use std::ops::Index;
use std::str::CharIndices;

// A simple JSON parser and representation.
// This parser supports only a subset of JSON specification and is intended for Luau's use cases.

#[derive(Debug, PartialEq)]
pub(crate) enum Json<'a> {
    Null,
    Bool(bool),
    Integer(i64),
    Number(f64),
    String(&'a str),
    Array(Vec<Json<'a>>),
    Object(HashMap<&'a str, Json<'a>>),
}

impl<'a> Index<&str> for Json<'a> {
    type Output = Json<'a>;

    fn index(&self, key: &str) -> &Self::Output {
        match self {
            Json::Object(map) => map.get(key).unwrap_or(&Json::Null),
            _ => &Json::Null,
        }
    }
}

impl PartialEq<&str> for Json<'_> {
    fn eq(&self, other: &&str) -> bool {
        matches!(self, Json::String(s) if s == other)
    }
}

impl<'a> Json<'a> {
    pub(crate) fn as_str(&self) -> Option<&'a str> {
        match self {
            Json::String(s) => Some(s),
            _ => None,
        }
    }

    pub(crate) fn as_i64(&self) -> Option<i64> {
        match self {
            Json::Integer(i) => Some(*i),
            Json::Number(n) if n.fract() == 0.0 => Some(*n as i64),
            _ => None,
        }
    }

    pub(crate) fn as_u64(&self) -> Option<u64> {
        self.as_i64().and_then(|i| u64::try_from(i).ok())
    }

    pub(crate) fn as_array(&self) -> Option<&[Json<'a>]> {
        match self {
            Json::Array(arr) => Some(arr),
            _ => None,
        }
    }

    pub(crate) fn as_object(&self) -> Option<&HashMap<&'a str, Json<'a>>> {
        match self {
            Json::Object(map) => Some(map),
            _ => None,
        }
    }
}

pub(crate) fn parse<'a>(s: &'a str) -> Result<Json<'a>, &'static str> {
    let s = s.trim_ascii();
    let mut chars = s.char_indices().peekable();
    parse_value(s, &mut chars)
}

fn parse_value<'a>(s: &'a str, chars: &mut Peekable<CharIndices>) -> Result<Json<'a>, &'static str> {
    skip_whitespace(chars);
    match chars.peek() {
        Some((_, '{')) => parse_object(s, chars),
        Some((_, '[')) => parse_array(s, chars),
        Some((_, '"')) => parse_string(s, chars).map(Json::String),
        Some((_, 't' | 'f')) => parse_bool(chars),
        Some((_, 'n')) => parse_null(chars),
        Some((_, '-' | '0'..='9')) => parse_number(chars),
        Some(_) => Err("unexpected character"),
        None => Err("unexpected end of input"),
    }
}

fn parse_object<'a>(s: &'a str, chars: &mut Peekable<CharIndices>) -> Result<Json<'a>, &'static str> {
    chars.next(); // consume '{'

    let mut map = HashMap::new();
    skip_whitespace(chars);
    if matches!(chars.peek(), Some((_, '}'))) {
        chars.next();
        return Ok(Json::Object(map));
    }
    loop {
        skip_whitespace(chars);
        let key = parse_string(s, chars)?;
        skip_whitespace(chars);
        if !matches!(chars.next(), Some((_, ':'))) {
            return Err("expected ':'");
        }
        let value = parse_value(s, chars)?;
        map.insert(key, value);
        skip_whitespace(chars);
        match chars.next() {
            Some((_, ',')) => continue,
            Some((_, '}')) => break,
            _ => return Err("expected ',' or '}'"),
        }
    }
    Ok(Json::Object(map))
}

fn parse_array<'a>(s: &'a str, chars: &mut Peekable<CharIndices>) -> Result<Json<'a>, &'static str> {
    chars.next(); // consume '['

    let mut arr = Vec::new();
    skip_whitespace(chars);
    if matches!(chars.peek(), Some((_, ']'))) {
        chars.next();
        return Ok(Json::Array(arr));
    }
    loop {
        skip_whitespace(chars);
        arr.push(parse_value(s, chars)?);
        skip_whitespace(chars);
        match chars.next() {
            Some((_, ',')) => continue,
            Some((_, ']')) => return Ok(Json::Array(arr)),
            _ => return Err("expected ',' or ']'"),
        }
    }
}

fn parse_string<'a>(s: &'a str, chars: &mut Peekable<CharIndices>) -> Result<&'a str, &'static str> {
    if !matches!(chars.next(), Some((_, '"'))) {
        return Err("expected string starting with '\"'");
    }
    let start = chars.peek().map(|(i, _)| *i).unwrap_or(0);
    for (i, c) in chars {
        if c == '"' {
            return Ok(&s[start..i]);
        }
    }
    Err("unterminated string")
}

fn parse_number(chars: &mut Peekable<CharIndices>) -> Result<Json<'static>, &'static str> {
    let mut is_float = false;
    let mut num = String::new();
    while let Some((_, c @ ('0'..='9' | '-' | '.' | 'e' | 'E' | '+'))) = chars.peek() {
        num.push(*c);
        is_float = is_float || matches!(c, '.' | 'e' | 'E');
        chars.next();
    }
    if !is_float {
        let i = num.parse::<i64>().map_err(|_| "invalid integer")?;
        return Ok(Json::Integer(i));
    }
    let n = num.parse::<f64>().map_err(|_| "invalid number")?;
    Ok(Json::Number(n))
}

fn parse_bool(chars: &mut Peekable<CharIndices>) -> Result<Json<'static>, &'static str> {
    let bool = next_chars(chars);
    if bool == [Some('t'), Some('r'), Some('u'), Some('e')] {
        return Ok(Json::Bool(true));
    }
    if bool == [Some('f'), Some('a'), Some('l'), Some('s')] && matches!(chars.next(), Some((_, 'e'))) {
        return Ok(Json::Bool(false));
    }
    Err("invalid boolean literal")
}

fn parse_null(chars: &mut Peekable<CharIndices>) -> Result<Json<'static>, &'static str> {
    if next_chars(chars) == [Some('n'), Some('u'), Some('l'), Some('l')] {
        return Ok(Json::Null);
    }
    Err("invalid \"null\" literal")
}

fn skip_whitespace(chars: &mut Peekable<CharIndices>) {
    while let Some((_, ' ' | '\n' | '\r' | '\t')) = chars.peek() {
        chars.next();
    }
}

fn next_chars<const N: usize>(chars: &mut Peekable<CharIndices>) -> [Option<char>; N] {
    array::from_fn(|_| chars.next().map(|(_, c)| c))
}
