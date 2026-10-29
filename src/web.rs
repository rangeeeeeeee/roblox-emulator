#[cfg(all(test, emulator_local_tests))]
use crate::session::Session;
#[cfg(not(all(test, emulator_local_tests)))]
use crate::remote_session::Session;
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
    let limit = match path.as_str() {
        "/api/place/load" => 16 * 1024 * 1024,
        "/api/place/update" => 6 * 1024 * 1024,
        // JSON escaping can expand each source byte to six bytes.
        "/api/execute" => 6 * 1024 * 1024 + 4096,
        _ => MAX_BODY,
    };
    if length > limit { return Err(format!("Request body exceeds {limit} bytes")); }
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

fn cache_control(status:u16, content_type:&str)->&'static str {
    if status==200 && content_type=="image/png" {"public, max-age=31536000, immutable"} else {"no-store"}
}
fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) {
    let label = match status { 200=>"OK", 400=>"Bad Request", 403=>"Forbidden", 404=>"Not Found", 405=>"Method Not Allowed", 422=>"Unprocessable Content", _=>"Internal Server Error" };
    let cache=cache_control(status,content_type);
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let header = format!("HTTP/1.1 {status} {label}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: {cache}\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'none'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; frame-ancestors 'none'; form-action 'none'\r\n\r\n", body.len());
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

include!(concat!(env!("OUT_DIR"),"/studio_icons.rs"));

struct App { session: Session, running: bool, runtime_error: Option<String>, source: Option<PathBuf>, place: crate::place::Place, using_place: bool, place_import_error: Option<String>, stopped: bool, trace_requested: bool, stopped_logs: Option<Value>, autosave_id: String }

fn autosave_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis();
    format!("{time}-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
}
fn autosave_root() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    Ok(exe.parent().ok_or("Cannot locate executable directory")?.join("autosaves"))
}
fn valid_autosave_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 96 && id.bytes().all(|b| b.is_ascii_digit() || b == b'-') && id.contains('-')
}
fn write_autosave(id: &str, place: &crate::place::Place) -> Result<(), String> {
    let root = autosave_root()?;
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let bytes = serde_json::to_vec(&place.snapshot()).map_err(|e| e.to_string())?;
    let tmp = root.join(format!("{id}.tmp"));
    let destination = root.join(format!("{id}.json"));
    std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &destination).map_err(|e| e.to_string())?;
    Ok(())
}
#[derive(serde::Deserialize)]
struct AutosaveSummary { name: String, profile: String }
fn list_autosaves() -> Result<Vec<Value>, String> {
    let root = autosave_root()?;
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
    };
    let mut items: Vec<(u64, String, Option<AutosaveSummary>)> = Vec::new();
    for entry in entries.flatten() {
        let filename = entry.file_name();
        let Some(name) = filename.to_str() else { continue };
        let Some(id) = name.strip_suffix(".json").filter(|id| valid_autosave_id(id)) else { continue };
        let Ok(metadata) = std::fs::symlink_metadata(entry.path()) else { continue };
        if !metadata.is_file() || metadata.len() > 8 * 1024 * 1024 { continue; }
        let modified = metadata.modified().ok().and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|time| time.as_millis().min(u64::MAX as u128) as u64).unwrap_or(0);
        let summary = std::fs::File::open(entry.path()).ok()
            .and_then(|file| serde_json::from_reader::<_, AutosaveSummary>(std::io::BufReader::new(file)).ok())
            .filter(|summary| summary.name.len() <= 256 && summary.profile.len() <= 64);
        items.push((modified, id.to_owned(), summary));
    }
    items.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    Ok(items.into_iter().map(|(modified, id, summary)| json!({"id":id,"modifiedMs":modified,
        "name":summary.as_ref().map(|summary| &summary.name),"profile":summary.as_ref().map(|summary| &summary.profile)})).collect())
}
fn read_autosave(id: &str) -> Result<crate::place::Place, String> {
    if !valid_autosave_id(id) { return Err("Invalid auto save ID".into()); }
    let path = autosave_root()?.join(format!("{id}.json"));
    let metadata = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.len() > 8 * 1024 * 1024 { return Err("Invalid auto save file".into()); }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let value = serde_json::from_slice(&bytes).map_err(|e| format!("Invalid auto save: {e}"))?;
    crate::place::Place::from_json(value)
}

impl App {
    fn snapshot(&mut self) -> mlua::Result<Value> {
        let pending = self.session.network_pending();
        let mut state = self.session.snapshot()?;
        state["running"] = json!(self.running && pending.is_none());
        state["network"] = json!({"pending":pending});
        state["runtimeError"] = json!(self.runtime_error);
        state["scenario"] = if self.using_place || self.stopped { self.place.snapshot()["name"].clone() } else { json!(self.source.as_ref().and_then(|path|path.file_name()).map(|name|name.to_string_lossy())) };
        state["usingPlace"] = json!(self.using_place);
        state["stopped"] = json!(self.stopped);
        state["traceCaughtErrors"] = json!(self.trace_requested);
        if self.stopped { state["logs"] = self.stopped_logs.clone().unwrap_or_else(||json!([])); }
        state["placeImportError"] = json!(self.place_import_error);
        Ok(state)
    }

    fn api(&mut self, path: &str, body: Value) -> Result<Value, String> {
        if !body.is_object() { return Err("Expected a JSON object".into()); }
        if path == "/api/network/decide" {
            let id = body.get("id").and_then(Value::as_str).ok_or("Request ID must be a string")?;
            let allow = body.get("allow").and_then(Value::as_bool).ok_or("allow must be boolean")?;
            self.session.network_decide(id,allow).map_err(|error|error.to_string())?;
            return self.snapshot().map_err(|error|error.to_string());
        }
        if self.session.network_blocked() && !matches!(path,"/api/place/state"|"/api/place/new"|"/api/place/update"|"/api/place/save"|"/api/place/load"|"/api/place/autosave"|"/api/place/autosaves"|"/api/place/recover"|"/api/place/stop"|"/api/debug/trace-errors"|"/api/lint") {
            return Err("VMs are paused until the network request is allowed or denied".into());
        }
        match path {
            "/api/place/state" => return Ok(json!({"place":self.place.snapshot(),"importError":self.place_import_error})),
            "/api/place/new" => {
                let profile = body.get("profile").and_then(Value::as_str).unwrap_or("generic");
                self.place = crate::place::Place::new(profile)?;
                self.autosave_id = autosave_id();
                self.place_import_error=None;
                return Ok(json!({"place":self.place.snapshot()}));
            }
            "/api/place/update" => {
                let result = self.place.update(&body)?;
                let autosave_error = write_autosave(&self.autosave_id, &self.place).err();
                return Ok(json!({"place":self.place.snapshot(),"result":result,"autosaveError":autosave_error}));
            }
            "/api/place/autosave" => return Ok(json!({"autosaveError":write_autosave(&self.autosave_id, &self.place).err()})),
            "/api/place/save" => {
                if let Some(name) = body.get("name") { self.place.set_name(name.as_str().ok_or("Name must be a string")?)?; }
                let autosave_error = write_autosave(&self.autosave_id, &self.place).err();
                return Ok(json!({"place":self.place.snapshot(),"autosaveError":autosave_error}));
            }
            "/api/place/load" => {
                self.place = crate::place::Place::from_json(body.get("place").cloned().ok_or("Place is required")?)?;
                self.autosave_id = autosave_id();
                self.place_import_error=None;
                return Ok(json!({"place":self.place.snapshot()}));
            }
            "/api/place/autosaves" => return Ok(json!({"items":list_autosaves()?})),
            "/api/place/recover" => {
                let id = body.get("id").and_then(Value::as_str).ok_or("Auto save ID must be a string")?;
                self.place = read_autosave(id)?;
                self.autosave_id = autosave_id();
                self.place_import_error = None;
                return Ok(json!({"place":self.place.snapshot()}));
            }
            "/api/place/run" => {
                let replacement = Session::from_place_with_trace(&self.place,self.trace_requested).map_err(|error|error.to_string())?;
                self.session = replacement;
                self.using_place = true;
                self.stopped = false;
                self.stopped_logs = None;
                self.place_import_error=None;
                self.running = true;
                self.runtime_error = None;
                let mut state = self.snapshot().map_err(|error|error.to_string())?;
                state["place"] = self.place.snapshot();
                return Ok(state);
            }
            "/api/place/stop" => {
                if !self.stopped {
                    self.stopped_logs = Some(self.session.snapshot().ok().and_then(|state|state.get("logs").cloned()).unwrap_or_else(||self.session.last_output()));
                    self.session = Session::stopped();
                }
                self.running = false;
                self.using_place = false;
                self.stopped = true;
                self.runtime_error = None;
                let mut state = self.snapshot().map_err(|error|error.to_string())?;
                state["place"] = self.place.snapshot();
                return Ok(state);
            }
            "/api/debug/trace-errors" => {
                let enabled = body.get("enabled").and_then(Value::as_bool).ok_or("enabled must be boolean")?;
                if !self.stopped { self.session.set_trace_errors(enabled).map_err(|error|error.to_string())?; }
                self.trace_requested = enabled;
                return self.snapshot().map_err(|error|error.to_string());
            }
            _ => {}
        }
        if self.stopped && !matches!(path,"/api/lint"|"/api/clear"|"/api/reset") {
            return Err("Start the place before running or inspecting the VM".into());
        }
        if path == "/api/workspace/list" || path == "/api/workspace/read" {
            let realm=body.get("realm").and_then(Value::as_str).ok_or("Select a client VM")?;
            return if path == "/api/workspace/list" { self.session.workspace_files(realm) }
                else { self.session.workspace_file(realm,body.get("name").and_then(Value::as_str).ok_or("File name must be a string")?) }
                .map_err(|error|error.to_string());
        }
        if path == "/api/inspect" { return self.session.inspect(&body).map_err(|error|error.to_string()); }
        if path == "/api/packet-spy" { return self.session.packet_spy(&body).map_err(|error|error.to_string()); }
        if path == "/api/runtime/update" {
            let result = self.session.runtime_update(&body,self.running).map_err(|error|error.to_string())?;
            let mut state = self.snapshot().map_err(|error|error.to_string())?;
            state["result"] = result;
            return Ok(state);
        }
        if path == "/api/lint" {
            let source = body.get("source").and_then(Value::as_str).ok_or("Source must be a string")?;
            let context = body.get("context").and_then(Value::as_str).ok_or("Select a context")?;
            #[cfg(all(test, emulator_local_tests))]
            return serde_json::to_value(crate::lint::analyze(source,context)?).map_err(|error|error.to_string());
            #[cfg(not(all(test, emulator_local_tests)))]
            return self.session.lint(source,context);
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
            "/api/gc/create" => {
                let realm = body.get("realm").and_then(Value::as_str).ok_or("Select a client VM")?;
                self.session.create_gc_viewer(realm).map_err(|e|e.to_string())?;
            }
            "/api/input" => {
                let realm = body.get("realm").and_then(Value::as_str).ok_or("Select a client VM")?;
                let key = body.get("key").and_then(Value::as_str).ok_or("Key must be a string")?;
                let down = body.get("down").and_then(Value::as_bool).ok_or("down must be boolean")?;
                self.session.keyboard_input(realm,key,down).map_err(|e|e.to_string())?;
            }
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
            "/api/clear" => { if self.stopped { self.stopped_logs = Some(json!([])); } else { self.session.clear_output(); } },
            "/api/reset" => {
                let profile = body.get("profile").and_then(Value::as_str).ok_or("Select an executor profile")?;
                let replacement = if self.using_place || self.stopped {
                    self.place.update(&json!({"action":"profile","profile":profile}))?;
                    Session::from_place_with_trace(&self.place,self.trace_requested)
                } else { match &self.source {
                    Some(path)=>Session::from_path_with_profile_and_trace(path,profile,self.trace_requested),
                    None=>Session::new(profile).and_then(|mut session|{session.set_trace_errors(self.trace_requested)?;Ok(session)})
                } }.map_err(|e|e.to_string())?;
                self.session = replacement;
                self.using_place |= self.stopped;
                self.stopped = false;
                self.stopped_logs = None;
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

fn editable_place(session: &Session, import: bool) -> Result<(crate::place::Place, Option<String>), String> {
    Ok(match import {
        true=>match crate::place::Place::from_scenario(session.scenario_template()) {
            Ok(place)=>(place,None),
            Err(error)=>(crate::place::Place::new(session.primary_profile())?,Some(error)),
        },
        false=>(crate::place::Place::new(session.primary_profile())?,None),
    })
}

pub fn serve(path: Option<&Path>, port: u16, launch_browser: bool) -> Result<(), Box<dyn Error>> {
    let session = match path { Some(path)=>Session::from_path(path), None=>Session::new("generic") }?;
    let (place,place_import_error)=editable_place(&session,path.is_some()).map_err(std::io::Error::other)?;
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
    let trace_requested = session.trace_errors_enabled();
    let mut app = App { session, running: false, runtime_error: None, source: path.map(Path::to_path_buf), place, using_place: false, place_import_error, stopped: false, trace_requested, stopped_logs: None, autosave_id: autosave_id() };
    let mut next_frame = Instant::now();
    let frame = Duration::from_secs_f64(1.0/60.0);
    loop {
        if crate::stop_requested() {break;}
        if !app.session.network_blocked() {
            if let Err(error) = app.session.poll_network() { app.running = false; app.runtime_error = Some(error.to_string()); }
        }
        if app.running && !app.session.network_blocked() && Instant::now() >= next_frame {
            if let Err(error) = app.session.advance(1.0/60.0) { app.running = false; app.runtime_error = Some(error.to_string()); }
            // Virtual time advances one frame per scheduler pass; delayed requests do not cause a catch-up burst.
            next_frame = Instant::now() + frame;
        } else if !app.running || app.session.network_blocked() { next_frame = Instant::now(); }
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
                if let Some(name)=request.path.strip_prefix("/assets/studio-class/").and_then(|name|name.strip_suffix(".png")) {
                    if request.method!="GET" {reply_json(&mut stream,405,json!({"error":"GET required"}));}
                    else if let Some(bytes)=studio_icon(name) {respond(&mut stream,200,"image/png",bytes);}
                    else {reply_json(&mut stream,404,json!({"error":"Studio icon unavailable"}));}
                    continue;
                }
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
    Ok(())
}
