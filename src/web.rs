use crate::session::Session;
use serde_json::{Value, json};
use std::{collections::HashMap, error::Error, io::{Read, Write}, net::{TcpListener, TcpStream}, path::{Path, PathBuf}, time::{Duration, Instant}};

const PAGE: &str = include_str!("../ui/index.html");
const EDITOR: &str = include_str!("../ui/editor.bundle.js");
const MAX_HEADERS: usize = 16 * 1024;
const MAX_BODY: usize = 512 * 1024;

struct Request { method: String, path: String, headers: HashMap<String, String>, body: Vec<u8> }

fn read_request(stream: &mut TcpStream) -> Result<Request, String> {
    stream.set_nonblocking(false).map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(Duration::from_millis(500))).map_err(|e| e.to_string())?;
    let started = Instant::now();
    let mut bytes = Vec::with_capacity(4096);
    let mut buffer = [0u8; 4096];
    let end = loop {
        if started.elapsed() > Duration::from_secs(2) { return Err("Request timed out".into()); }
        let count = stream.read(&mut buffer).map_err(|_| "Request timed out")?;
        if count == 0 { return Err("Incomplete request".into()); }
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(index) = bytes.windows(4).position(|p| p == b"\r\n\r\n") {
            if index > MAX_HEADERS { return Err("Headers too large".into()); }
            break index + 4;
        }
        if bytes.len() > MAX_HEADERS { return Err("Headers too large".into()); }
    };
    let text = std::str::from_utf8(&bytes[..end]).map_err(|_| "Invalid request headers")?;
    let mut lines = text.split("\r\n");
    let first: Vec<_> = lines.next().unwrap_or_default().split_whitespace().collect();
    if first.len() != 3 || first[2] != "HTTP/1.1" || !first[1].starts_with('/') { return Err("Invalid request line".into()); }
    let method = first[0].to_owned();
    let path = first[1].split('?').next().unwrap_or_default().to_owned();
    let mut headers = HashMap::new();
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').ok_or("Invalid header")?;
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') { return Err("Invalid header name".into()); }
        if headers.insert(name.to_ascii_lowercase(), value.trim().to_owned()).is_some() { return Err("Duplicate header".into()); }
    }
    if headers.contains_key("transfer-encoding") { return Err("Chunked requests are not supported".into()); }
    let length = headers.get("content-length").map(|v|v.parse::<usize>()).transpose().map_err(|_| "Invalid content length")?.unwrap_or(0);
    if length > MAX_BODY { return Err("Request body exceeds 512 KiB".into()); }
    if method == "POST" && !headers.contains_key("content-length") { return Err("Content-Length is required".into()); }
    let mut body = bytes[end..].to_vec();
    if body.len() > length { return Err("Unexpected request bytes".into()); }
    while body.len() < length {
        if started.elapsed() > Duration::from_secs(2) { return Err("Request timed out".into()); }
        let remaining = (length - body.len()).min(buffer.len());
        let count = stream.read(&mut buffer[..remaining]).map_err(|_| "Incomplete request body")?;
        if count == 0 { return Err("Incomplete request body".into()); }
        body.extend_from_slice(&buffer[..count]);
    }
    Ok(Request { method, path, headers, body })
}

#[cfg(all(test, emulator_local_tests))]
#[path = "../local_tests/web.rs"]
mod tests;

fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) {
    let label = match status { 200=>"OK", 400=>"Bad Request", 403=>"Forbidden", 404=>"Not Found", 405=>"Method Not Allowed", 422=>"Unprocessable Content", _=>"Internal Server Error" };
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let header = format!("HTTP/1.1 {status} {label}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'none'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src data:; base-uri 'none'; frame-ancestors 'none'; form-action 'none'\r\n\r\n", body.len());
    let _ = stream.write_all(header.as_bytes()).and_then(|_| stream.write_all(body));
}

fn reply_json(stream: &mut TcpStream, status: u16, value: Value) {
    match serde_json::to_vec(&value) {
        Ok(bytes) => respond(stream, status, "application/json; charset=utf-8", &bytes),
        Err(_) => respond(stream, 500, "application/json", b"{\"error\":\"Response encoding failed\"}"),
    }
}

fn session_token() -> std::io::Result<String> {
    let mut bytes = [0u8; 32];
    crate::native::random_bytes(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn token_matches(actual: Option<&String>, expected: &str) -> bool {
    let Some(actual) = actual else { return false; };
    actual.len() == expected.len() && actual.bytes().zip(expected.bytes()).fold(0u8, |result, (a,b)| result | (a ^ b)) == 0
}

fn open_browser(url: &str) -> std::io::Result<()> {
    #[cfg(windows)] let result = std::process::Command::new("rundll32.exe").args(["url.dll,FileProtocolHandler", url]).spawn();
    #[cfg(target_os = "macos")] let result = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(not(windows), not(target_os = "macos")))] let result = std::process::Command::new("xdg-open").arg(url).spawn();
    result.map(|_| ())
}

struct App { session: Session, running: bool, runtime_error: Option<String>, source: Option<PathBuf> }

impl App {
    fn snapshot(&mut self) -> mlua::Result<Value> {
        let mut state = self.session.snapshot()?;
        state["running"] = json!(self.running);
        state["runtimeError"] = json!(self.runtime_error);
        state["scenario"] = json!(self.source.as_ref().and_then(|path|path.file_name()).map(|name|name.to_string_lossy()));
        Ok(state)
    }

    fn api(&mut self, path: &str, body: Value) -> Result<Value, String> {
        if !body.is_object() { return Err("Expected a JSON object".into()); }
        if path == "/api/inspect" { return self.session.inspect(&body).map_err(|error|error.to_string()); }
        if path == "/api/lint" {
            let source = body.get("source").and_then(Value::as_str).ok_or("Source must be a string")?;
            let context = body.get("context").and_then(Value::as_str).ok_or("Select a context")?;
            return serde_json::to_value(crate::lint::analyze(source,context)?).map_err(|error|error.to_string());
        }
        if path == "/api/completions" {
            let realm = body.get("realm").and_then(Value::as_str).ok_or("Select a VM")?;
            let context = body.get("context").and_then(Value::as_str).ok_or("Select a context")?;
            let segments = body.get("path").and_then(Value::as_array).ok_or("Path must be a string array")?;
            if segments.len() > 16 { return Err("Completion path requires at most 16 segments".into()); }
            let segments = segments.iter().map(|segment|segment.as_str().map(str::to_owned).ok_or("Path must be a string array")).collect::<Result<Vec<_>,_>>()?;
            return self.session.completions(realm,context,&segments).map_err(|error|error.to_string());
        }
        let mut command = None;
        match path {
            "/api/execute" => {
                let realm = body.get("realm").and_then(Value::as_str).ok_or("Select a VM")?;
                let context = body.get("context").and_then(Value::as_str).ok_or("Select a context")?;
                let source = body.get("source").and_then(Value::as_str).ok_or("Source must be a string")?;
                let format = match body.get("format") { None => "source", Some(value) => value.as_str().ok_or("Format must be a string")? };
                command = Some(self.session.execute_input(realm, context, source, format).map_err(|e|e.to_string())?);
                self.runtime_error = None;
            }
            "/api/step" => {
                if self.running { return Err("Pause execution before stepping".into()); }
                let seconds = body.get("seconds").and_then(Value::as_f64).ok_or("Seconds must be a number")?;
                self.session.advance(seconds).map_err(|e|e.to_string())?;
                self.runtime_error = None;
            }
            "/api/running" => { self.running = body.get("running").and_then(Value::as_bool).ok_or("running must be boolean")?; self.runtime_error = None; }
            "/api/budget" => {
                let cpu_ms = body.get("cpuMs").and_then(Value::as_u64).ok_or("cpuMs must be an integer")?;
                self.session.set_cpu_budget(cpu_ms).map_err(|e|e.to_string())?;
            }
            "/api/clear" => self.session.clear_output(),
            "/api/reset" => {
                let profile = body.get("profile").and_then(Value::as_str).ok_or("Select an executor profile")?;
                let replacement = match &self.source { Some(path)=>Session::from_path_with_profile(path,profile), None=>Session::new(profile) }.map_err(|e|e.to_string())?;
                self.session = replacement;
                self.running = false;
                self.runtime_error = None;
            }
            _ => return Err("Unknown API endpoint".into()),
        }
        let mut state = self.snapshot().map_err(|e|e.to_string())?;
        if let Some(command) = command { state["command"] = command; }
        Ok(state)
    }
}

pub fn serve(path: Option<&Path>, port: u16, launch_browser: bool) -> Result<(), Box<dyn Error>> {
    let session = match path { Some(path)=>Session::from_path(path), None=>Session::new("generic") }?;
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    let host = address.to_string();
    let origin = format!("http://{host}");
    let token = session_token()?;
    let url = format!("{origin}/#token={token}");
    println!("INTERPRETER_URL {url}");
    println!("Local interpreter; Ctrl+C stops the process.");
    if launch_browser && open_browser(&url).is_err() { eprintln!("Open the interpreter URL in your browser."); }
    let mut app = App { session, running: false, runtime_error: None, source: path.map(Path::to_path_buf) };
    let mut next_frame = Instant::now();
    let frame = Duration::from_secs_f64(1.0/60.0);
    loop {
        if app.running && Instant::now() >= next_frame {
            if let Err(error) = app.session.advance(1.0/60.0) { app.running = false; app.runtime_error = Some(error.to_string()); }
            // Virtual time advances one frame per scheduler pass; delayed requests do not cause a catch-up burst.
            next_frame = Instant::now() + frame;
        } else if !app.running { next_frame = Instant::now(); }
        match listener.accept() {
            Ok((mut stream, peer)) => {
                if !peer.ip().is_loopback() { reply_json(&mut stream,403,json!({"error":"Loopback only"})); continue; }
                let request = match read_request(&mut stream) {
                    Ok(request)=>request,
                    Err(error)=> { reply_json(&mut stream,400,json!({"error":error})); continue; }
                };
                if request.headers.get("host") != Some(&host) || request.headers.get("origin").is_some_and(|value|value != &origin) {
                    reply_json(&mut stream,403,json!({"error":"Invalid origin"})); continue;
                }
                if request.path == "/" && request.method == "GET" { respond(&mut stream,200,"text/html; charset=utf-8",PAGE.as_bytes()); continue; }
                if request.path == "/assets/editor.js" && request.method == "GET" { respond(&mut stream,200,"text/javascript; charset=utf-8",EDITOR.as_bytes()); continue; }
                if !request.path.starts_with("/api/") { reply_json(&mut stream,404,json!({"error":"Not found"})); continue; }
                if !token_matches(request.headers.get("x-emulator-token"), &token) {
                    reply_json(&mut stream,403,json!({"error":"Open the session URL printed by the emulator"})); continue;
                }
                if request.path == "/api/state" && request.method == "GET" {
                    match app.snapshot() { Ok(state)=>reply_json(&mut stream,200,state), Err(error)=>reply_json(&mut stream,500,json!({"error":error.to_string()})) }
                    continue;
                }
                if request.method != "POST" { reply_json(&mut stream,405,json!({"error":"POST required"})); continue; }
                if request.headers.get("content-type").and_then(|s|s.split(';').next()).map(str::trim) != Some("application/json") {
                    reply_json(&mut stream,400,json!({"error":"application/json required"})); continue;
                }
                let body = match serde_json::from_slice(&request.body) {
                    Ok(body)=>body,
                    Err(_)=> { reply_json(&mut stream,400,json!({"error":"Invalid JSON"})); continue; }
                };
                match app.api(&request.path,body) {
                    Ok(state)=>reply_json(&mut stream,200,state),
                    Err(error)=>reply_json(&mut stream,422,json!({"error":error})),
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(4)),
            Err(error)=>return Err(error.into()),
        }
    }
}
