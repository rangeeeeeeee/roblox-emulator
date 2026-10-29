use serde::Serialize;
use std::{ffi::{c_char, c_void, CString}, slice};

const SOURCE_LIMIT: usize = 256 * 1024;
const DIAGNOSTIC_LIMIT: usize = 100;

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Position {
    pub line: usize,
    pub column: usize,
}

#[derive(Debug, Serialize)]
pub struct Diagnostic {
    pub from: Position,
    pub to: Position,
    pub severity: &'static str,
    pub message: String,
    pub code: String,
}

#[derive(Debug, Serialize)]
pub struct LintResponse {
    pub diagnostics: Vec<Diagnostic>,
}

type DiagnosticCallback = unsafe extern "C" fn(*mut c_void, u32, u32, u32, u32, bool,
    *const c_char, usize, *const c_char, usize);

unsafe extern "C" {
    fn emu_lint(source: *const c_char, length: usize, globals: *const *const c_char,
        global_count: usize, context: *mut c_void, callback: DiagnosticCallback) -> i32;
}

const GAME_GLOBALS: &str = "_G _VERSION assert bit32 buffer collectgarbage coroutine debug error gcinfo getfenv getmetatable ipairs loadstring math newproxy next os pairs pcall print rawequal rawget rawlen rawset select setfenv setmetatable string table tonumber tostring type typeof unpack utf8 vector xpcall game Game workspace Workspace script shared task wait spawn delay tick time elapsedTime require warn version settings UserSettings stats printidentity Instance Enum Vector2 Vector3 CFrame Color3 Ray Axes BrickColor CatalogSearchParams ColorSequence ColorSequenceKeypoint Content DateTime DockWidgetPluginGuiInfo Faces FloatCurveKey Font NumberRange NumberSequence NumberSequenceKeypoint OverlapParams Path2DControlPoint PathWaypoint PhysicalProperties Random RaycastParams Rect Region3 Region3int16 RotationCurveKey SecurityCapabilities TweenInfo UDim UDim2 Vector2int16 Vector3int16";
const EXECUTOR_GLOBALS: &str = "cache crypt base64 Regex http syn httpget getgc getgcvalue filtergc getgenv getrenv getreg checkcaller isourthread identifyexecutor getexecutorname iscclosure islclosure isexecutorclosure checkclosure isourclosure newcclosure isnewcclosure newlclosure clonefunction hookfunction isfunctionhooked replaceclosure restorefunction hookmetamethod getfunctionhash getrawmetatable setrawmetatable setreadonly isreadonly makereadonly makewritable getnamecallmethod setnamecallmethod getconnections getconnection getsignalarguments getsignalargumentsinfo firesignal replicatesignal getinstances getnilinstances cloneref compareinstances getcallingscript getscripts getrunningscripts getloadedmodules getsenv getscriptclosure getscriptfunction getscriptbytecode dumpstring getscriptfromthread getscriptthread getscripthash getcallbackvalue gethui setinfo getproperties gethiddenproperties gethiddenproperty sethiddenproperty getthreadidentity setthreadidentity isscriptable setscriptable writefile appendfile readfile makefolder isfile isfolder listfiles delfile delfolder loadfile dofile getcustomasset getproximitypromptduration setproximitypromptduration getsimulationradius setsimulationradius setclipboard toclipboard setrbxclipboard getclipboard rconsoleprint consoleprint rconsolewarn rconsoleerr request http_request queue_on_teleport firetouchinterest fireclickdetector fireproximityprompt isrbxactive isgameactive setfpscap getfpscap getrendersteppedlist shutdown base64encode base64decode base64_encode base64_decode lz4compress lz4decompress Drawing isrenderobj getrenderproperty setrenderproperty cleardrawcache WebSocket websocket";

struct Collector<'a> {
    lines: Vec<&'a str>,
    diagnostics: Vec<Diagnostic>,
}

impl Collector<'_> {
    fn position(&self, line: u32, byte_column: u32) -> Position {
        let line = (line as usize).min(self.lines.len().saturating_sub(1));
        let text = self.lines[line].trim_end_matches('\r');
        let mut end = (byte_column as usize).min(text.len());
        while !text.is_char_boundary(end) { end -= 1; }
        Position { line, column: text[..end].encode_utf16().count() }
    }
}

unsafe extern "C" fn receive(context: *mut c_void, begin_line: u32, begin_column: u32,
    end_line: u32, end_column: u32, error: bool, message: *const c_char,
    message_length: usize, code: *const c_char, code_length: usize) {
    let collector = unsafe { &mut *context.cast::<Collector<'_>>() };
    if collector.diagnostics.len() >= DIAGNOSTIC_LIMIT { return; }
    collector.diagnostics.push(Diagnostic {
        from: collector.position(begin_line, begin_column),
        to: collector.position(end_line, end_column),
        severity: if error { "error" } else { "warning" },
        message: String::from_utf8_lossy(unsafe { slice::from_raw_parts(message.cast::<u8>(), message_length) }).into_owned(),
        code: String::from_utf8_lossy(unsafe { slice::from_raw_parts(code.cast::<u8>(), code_length) }).into_owned(),
    });
}

fn analyze_inner(source: &str, executor: bool) -> Result<LintResponse, String> {
    let names: Vec<_> = GAME_GLOBALS.split_whitespace()
        .chain(if executor { EXECUTOR_GLOBALS } else { "" }.split_whitespace())
        .map(|name| CString::new(name).expect("static global names")).collect();
    let pointers: Vec<_> = names.iter().map(|name| name.as_ptr()).collect();
    let mut collector = Collector { lines: source.split('\n').collect(), diagnostics: Vec::new() };
    let status = unsafe { emu_lint(source.as_ptr().cast(), source.len(), pointers.as_ptr(), pointers.len(),
        (&mut collector as *mut Collector<'_>).cast(), receive) };
    if status != 0 { return Err("Luau lint analysis failed".into()); }
    Ok(LintResponse { diagnostics: collector.diagnostics })
}

pub fn analyze(source: &str, context: &str) -> Result<LintResponse, String> {
    if source.len() > SOURCE_LIMIT { return Err("Source exceeds the 256 KiB lint limit".into()); }
    if !matches!(context, "server" | "client" | "executor") { return Err("Unknown lint context".into()); }
    let source = source.to_owned();
    let executor = context == "executor";
    // Parser recursion uses a separate bounded stack, without borrowing a running VM.
    std::thread::Builder::new().name("luau-lint".into()).stack_size(8 * 1024 * 1024)
        .spawn(move || analyze_inner(&source, executor)).map_err(|error| error.to_string())?
        .join().map_err(|_| "Luau lint worker failed".to_owned())?
}

#[cfg(all(test, emulator_local_tests))]
#[path = "../local_tests/lint.rs"]
mod tests;
