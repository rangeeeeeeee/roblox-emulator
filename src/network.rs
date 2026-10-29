use mlua::{AnyUserData, Error, Function, Lua, MultiValue, Result, Table, Thread, Value, ffi, state::NativeCall};
use serde_json::{Value as Json, json};
use std::{cell::RefCell, collections::{BTreeMap, HashMap, VecDeque}, rc::Rc, sync::{atomic::{AtomicBool,Ordering}, mpsc}};

const MAX_URL: usize = 8192;
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BODY: usize = 256 * 1024;
const MAX_RESPONSE: usize = 8 * 1024 * 1024;
const MAX_IN_FLIGHT: usize = 8;
#[path = "websocket.rs"] mod websocket;
const MAX_SOCKET_ACTIONS: usize = 32;
const MAX_SOCKETS: usize = 8;
static EXTERNAL_BROKER: AtomicBool = AtomicBool::new(false);

pub(crate) fn enable_external_broker() { EXTERNAL_BROKER.store(true, Ordering::Relaxed); }

#[derive(Clone)]
struct Request {
    url: String,
    method: String,
    host: String,
    port: u16,
    path: String,
    secure: bool,
    headers: BTreeMap<String, String>,
    body: Option<Vec<u8>>,
}

struct Response { status: u16, message: String, headers: BTreeMap<String, String>, body: Vec<u8> }
#[derive(Clone,Copy,PartialEq,Eq)]
pub(crate) enum ResponseKind { ExecutorRequest, ExecutorHttpGet, HttpServiceRequest, HttpServiceGet, HttpServicePost, GameHttpGet, WebSocket }
impl ResponseKind {
    fn api(self) -> &'static str {
        match self {
            Self::ExecutorRequest => "executor.request", Self::ExecutorHttpGet => "executor.httpget", Self::HttpServiceRequest => "HttpService.RequestAsync",
            Self::HttpServiceGet => "HttpService.GetAsync", Self::HttpServicePost => "HttpService.PostAsync",
            Self::GameHttpGet => "game.HttpGet",
            Self::WebSocket => "executor.websocket",
        }
    }
    fn table(self) -> bool { matches!(self,Self::ExecutorRequest|Self::HttpServiceRequest) }
}
struct Pending { id: String, realm: String, context: String, thread: Thread, request: Request, kind: ResponseKind }
struct Flight { realm: String, thread: Thread, kind: ResponseKind }
struct Completed { id: String, realm: String, thread: Thread, kind: ResponseKind, result: std::result::Result<Response, String> }
struct Suspended { realm: String, thread: Thread, args: MultiValue }
struct WorkerResult { id: String, result: std::result::Result<Response, String> }
struct SocketState { realm:String, message:AnyUserData, close:AnyUserData, closing:bool }
struct SocketEvent { id:String, message:Option<Vec<u8>> }
struct State {
    external: bool,
    pending: Option<Pending>,
    flights: HashMap<String, Flight>,
    ready: VecDeque<Completed>,
    suspended: VecDeque<Suspended>,
    sockets: HashMap<String,SocketState>,
    socket_actions: VecDeque<Json>,
    socket_events: VecDeque<SocketEvent>,
    sender: mpsc::Sender<WorkerResult>,
    receiver: mpsc::Receiver<WorkerResult>,
}

#[derive(Clone)]
pub struct Broker(Rc<RefCell<State>>);
#[derive(Clone)]
struct Handle { broker: Broker, realm: String }

impl Broker {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self(Rc::new(RefCell::new(State { external: EXTERNAL_BROKER.load(Ordering::Relaxed), pending: None, flights: HashMap::new(), ready: VecDeque::new(), suspended: VecDeque::new(), sockets:HashMap::new(),socket_actions:VecDeque::new(),socket_events:VecDeque::new(),sender, receiver })))
    }

    pub fn attach(&self, lua: &Lua, realm: &str) {
        lua.set_app_data(Handle { broker: self.clone(), realm: realm.into() });
    }

    pub fn has_pending(&self) -> bool { self.0.borrow().pending.is_some() }

    // Release Lua threads when a session is replaced, including one awaiting approval.
    // An already approved OS request may finish, but its result has no session to resume.
    pub(crate) fn cancel_all(&self) {
        let mut state = self.0.borrow_mut();
        state.pending = None;
        state.flights.clear();
        state.ready.clear();
        state.suspended.clear();
        state.sockets.clear();
        state.socket_actions.clear();
        state.socket_events.clear();
    }

    pub fn pending(&self) -> Option<Json> {
        self.0.borrow().pending.as_ref().map(|pending| {
            let body = pending.request.body.as_deref();
            json!({"id":pending.id,"realm":pending.realm,"context":pending.context,"api":pending.kind.api(),"method":pending.request.method,
                "url":pending.request.url,"headers":pending.request.headers,
                "bodyBase64":body.map(base64::encode),"bodyText":body.and_then(|bytes|std::str::from_utf8(bytes).ok()),
                "bodyBytes":body.map_or(0, |bytes|bytes.len()),"bodyPresent":body.is_some()})
        })
    }

    fn enqueue(&self, realm: String, context: String, thread: Thread, request: Request, kind: ResponseKind) -> Result<()> {
        let mut state = self.0.borrow_mut();
        if state.pending.is_some() { return Err(Error::lua_string("another HTTP request is awaiting approval")); }
        if state.flights.len() + state.ready.len() >= MAX_IN_FLIGHT { return Err(Error::lua_string("HTTP request limit exceeded")); }
        if kind==ResponseKind::WebSocket && state.sockets.len()+state.flights.values().filter(|flight|flight.kind==ResponseKind::WebSocket).count()>=MAX_SOCKETS {
            return Err(Error::lua_string("WebSocket connection limit exceeded"));
        }
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(Error::external)?;
        let id = random.iter().map(|byte|format!("{byte:02x}")).collect::<String>();
        state.pending = Some(Pending { id, realm, context, thread, request, kind });
        Ok(())
    }

    pub fn decide(&self, id: &str, allow: bool) -> std::result::Result<(), String> {
        let mut state = self.0.borrow_mut();
        if state.pending.as_ref().is_none_or(|pending|pending.id != id) { return Err("Request is no longer awaiting approval".into()); }
        let Pending { id, realm, thread, request, kind, .. } = state.pending.take().unwrap();
        if !allow {
            let error=if kind==ResponseKind::WebSocket {"WebSocket request denied by user"} else {"HTTP request denied by user"};
            state.ready.push_back(Completed { id, realm, thread, kind, result: Err(error.into()) });
            return Ok(());
        }
        if state.external {
            state.flights.insert(id, Flight { realm, thread, kind });
            return Ok(());
        }
        if kind==ResponseKind::WebSocket {
            state.ready.push_back(Completed { id,realm,thread,kind,result:Err("WebSocket requires the isolated host broker".into()) });
            return Ok(());
        }
        let sender = state.sender.clone();
        let worker_id = id.clone();
        let started = std::thread::Builder::new().name("emulator-http".into()).spawn(move || {
            let _ = sender.send(WorkerResult { id: worker_id, result: transport::send(request) });
        });
        match started {
            Ok(_) => { state.flights.insert(id, Flight { realm, thread, kind }); }
            Err(error) => state.ready.push_back(Completed { id, realm, thread, kind, result: Err(format!("HTTP worker could not start: {error}")) }),
        }
        Ok(())
    }

    pub(crate) fn complete_external(&self, id: &str, result: Json) -> std::result::Result<(), String> {
        let mut state = self.0.borrow_mut();
        if !state.external { return Err("External HTTP broker is not enabled".into()); }
        let flight=state.flights.get(id).ok_or("Network request is no longer active")?;
        let response=if flight.kind==ResponseKind::WebSocket {
            if result.get("ok").and_then(Json::as_bool)==Some(true) && result.get("websocket").and_then(Json::as_bool)==Some(true) {
                Ok(Response{status:101,message:String::new(),headers:BTreeMap::new(),body:Vec::new()})
            } else if result.get("ok").and_then(Json::as_bool)==Some(false) {
                Err(result.get("error").and_then(Json::as_str).filter(|error|error.len()<=4096).ok_or("Invalid WebSocket error")?.to_owned())
            } else {return Err("Invalid WebSocket connection response".into())}
        } else {decode_external_response(result)?};
        let flight=state.flights.remove(id).unwrap();
        state.ready.push_back(Completed { id:id.into(),realm: flight.realm, thread: flight.thread, kind: flight.kind, result: response });
        Ok(())
    }

    pub(crate) fn socket_actions(&self)->Vec<Json>{self.0.borrow_mut().socket_actions.drain(..).collect()}

    pub(crate) fn socket_event_external(&self,id:&str,event:Json)->std::result::Result<(),String>{
        let mut state=self.0.borrow_mut();
        if !state.external || !state.sockets.contains_key(id) {return Err("WebSocket is no longer active".into())}
        if state.socket_events.len()>=websocket::MAX_EVENTS{return Err("WebSocket event queue exceeds limit".into())}
        let message=match event.get("event").and_then(Json::as_str) {
            Some("message")=>{
                let text=event.get("data").and_then(Json::as_str).filter(|text|text.len()<=((websocket::MAX_MESSAGE+2)/3)*4).ok_or("Invalid WebSocket message")?;
                let bytes=base64::decode(text).map_err(|_|"Invalid WebSocket message")?;
                if bytes.len()>websocket::MAX_MESSAGE{return Err("WebSocket message exceeds limit".into())}
                Some(bytes)
            },
            Some("close")=>None,
            _=>return Err("Invalid WebSocket event".into()),
        };
        state.socket_events.push_back(SocketEvent{id:id.into(),message});Ok(())
    }

    fn socket_value(&self,lua:&Lua,id:&str,realm:&str)->Result<Table>{
        let message=crate::scheduler::new_signal(lua)?;
        let close=crate::scheduler::new_signal(lua)?;
        let socket=lua.create_table()?;
        socket.raw_set("OnMessage",message.clone())?;
        socket.raw_set("OnClose",close.clone())?;
        let identity=socket.to_pointer() as usize;
        let send_broker=self.clone();let send_id=id.to_owned();
        let send=lua.create_function(move|_,(object,data):(Value,mlua::LuaString)|{
            if !matches!(object,Value::Table(ref object) if object.to_pointer() as usize==identity) {return Err(Error::lua_string("WebSocket Send requires its socket"))}
            let data=data.as_bytes();
            if data.len()>websocket::MAX_MESSAGE{return Err(Error::lua_string("WebSocket message exceeds 256 KiB"))}
            let mut state=send_broker.0.borrow_mut();
            if state.sockets.get(&send_id).is_none_or(|socket|socket.closing){return Err(Error::lua_string("WebSocket is closed"))}
            if state.socket_actions.len()>=MAX_SOCKET_ACTIONS{return Err(Error::lua_string("WebSocket send queue is full"))}
            state.socket_actions.push_back(json!({"op":"send","id":send_id,"data":base64::encode(data.as_ref())}));
            Ok(())
        })?;
        crate::native::name_function(lua,&send,c"Send")?;
        socket.raw_set("Send",send)?;
        let close_broker=self.clone();let close_id=id.to_owned();
        let close_fn=lua.create_function(move|_,object:Value|{
            if !matches!(object,Value::Table(ref object) if object.to_pointer() as usize==identity) {return Err(Error::lua_string("WebSocket Close requires its socket"))}
            let mut state=close_broker.0.borrow_mut();
            if state.sockets.get(&close_id).is_none_or(|socket|socket.closing){return Ok(())}
            if state.socket_actions.len()>=MAX_SOCKET_ACTIONS{return Err(Error::lua_string("WebSocket send queue is full"))}
            state.sockets.get_mut(&close_id).unwrap().closing=true;
            state.socket_actions.push_back(json!({"op":"close","id":close_id}));
            Ok(())
        })?;
        crate::native::name_function(lua,&close_fn,c"Close")?;
        socket.raw_set("Close",close_fn)?;
        let mut state=self.0.borrow_mut();
        if state.sockets.len()>=MAX_SOCKETS {return Err(Error::lua_string("WebSocket connection limit exceeded"))}
        state.sockets.insert(id.into(),SocketState{realm:realm.into(),message,close,closing:false});
        Ok(socket)
    }

    pub fn poll(&self, lua: &Lua, realm: &str) -> Result<usize> {
        let (ready, suspended) = {
            let mut state = self.0.borrow_mut();
            while let Ok(result) = state.receiver.try_recv() {
                if let Some(flight) = state.flights.remove(&result.id) {
                    state.ready.push_back(Completed { id:result.id,realm: flight.realm, thread: flight.thread, kind: flight.kind, result: result.result });
                }
            }
            if state.pending.is_some() { return Ok(0); }
            let mut selected = Vec::new();
            let mut remaining = VecDeque::new();
            while let Some(item) = state.ready.pop_front() {
                if item.realm == realm { selected.push(item); } else { remaining.push_back(item); }
            }
            state.ready = remaining;
            let mut suspended = Vec::new();
            let mut remaining = VecDeque::new();
            while let Some(item) = state.suspended.pop_front() {
                if item.realm == realm { suspended.push(item); } else { remaining.push_back(item); }
            }
            state.suspended = remaining;
            (selected,suspended)
        };
        let mut count = ready.len() + suspended.len();
        crate::scheduler::resume_parents_before_batch(lua,suspended.into_iter().map(|item|(item.thread,item.args)).collect())?;
        for item in ready {
            match item.result {
                Ok(response) => {
                    if item.kind==ResponseKind::WebSocket {
                        match self.socket_value(lua,&item.id,realm) {
                            Ok(socket)=>crate::scheduler::schedule(lua,item.thread,MultiValue::from_vec(vec![Value::Table(socket)]),None)?,
                            Err(error)=>crate::scheduler::schedule_error(lua,item.thread,Value::String(lua.create_string(error.to_string())?))?,
                        }
                        continue;
                    }
                    if !item.kind.table() {
                        if (200..300).contains(&response.status) {
                            let body=Value::String(lua.create_string(&response.body)?);
                            crate::scheduler::schedule(lua,item.thread,MultiValue::from_vec(vec![body]),None)?;
                        } else {
                            let message=format!("HTTP {} ({})",response.status,response.message);
                            crate::scheduler::schedule_error(lua,item.thread,Value::String(lua.create_string(message)?))?;
                        }
                        continue;
                    }
                    let table = lua.create_table()?;
                    table.set("StatusCode", response.status)?;
                    table.set("StatusMessage", response.message)?;
                    table.set("Success", (200..300).contains(&response.status))?;
                    table.set("Body", lua.create_string(&response.body)?)?;
                    let headers = lua.create_table()?;
                    for (name, value) in response.headers { headers.set(name, value)?; }
                    table.set("Headers", headers)?;
                    crate::scheduler::schedule(lua, item.thread, MultiValue::from_vec(vec![Value::Table(table)]), None)?;
                }
                Err(error) => crate::scheduler::schedule_error(lua, item.thread, Value::String(lua.create_string(error)?))?,
            }
        }
        let event={
            let mut state=self.0.borrow_mut();
            let position=state.socket_events.iter().position(|event|state.sockets.get(&event.id).is_some_and(|socket|socket.realm==realm));
            position.and_then(|position|state.socket_events.remove(position))
        };
        if let Some(event)=event {
            let (signal,args)={
                let mut state=self.0.borrow_mut();
                if let Some(message)=event.message {
                    (state.sockets.get(&event.id).map(|socket|socket.message.clone()),MultiValue::from_vec(vec![Value::String(lua.create_string(message)?)]))
                } else {(state.sockets.remove(&event.id).map(|socket|socket.close),MultiValue::new())}
            };
            if let Some(signal)=signal {crate::scheduler::fire_host_signal(lua,&signal,args)?;count+=1;}
        }
        Ok(count)
    }
}

pub(crate) fn frozen(lua: &Lua) -> bool {
    lua.app_data_ref::<Handle>().is_some_and(|handle|handle.broker.has_pending())
}

pub(crate) fn suspend_parent(lua: &Lua, args: MultiValue) -> Result<()> {
    let handle = lua.app_data_ref::<Handle>().map(|handle|handle.clone()).ok_or_else(||Error::lua_string("HTTP approval broker unavailable"))?;
    crate::scheduler::park_current(lua)?;
    let mut state = handle.broker.0.borrow_mut();
    if state.pending.is_none() { return Err(Error::lua_string("HTTP approval is no longer pending")); }
    state.suspended.push_back(Suspended { realm: handle.realm, thread: lua.current_thread(), args });
    Ok(())
}

fn parse_url(url: &str) -> Result<(bool, String, u16, String)> {
    if url.len() > MAX_URL || !url.is_ascii() || url.bytes().any(|c| !(0x21..=0x7e).contains(&c)) || url.contains('#') {
        return Err(Error::lua_string("Invalid HTTP URL"));
    }
    let (secure, rest) = if let Some(rest) = url.strip_prefix("https://") { (true, rest) }
        else if let Some(rest) = url.strip_prefix("http://") { (false, rest) }
        else { return Err(Error::lua_string("Only http:// and https:// URLs are supported")); };
    let split = rest.find(['/', '?']).unwrap_or(rest.len());
    let authority = &rest[..split];
    if authority.is_empty() || authority.contains('@') || authority.contains('\\') { return Err(Error::lua_string("Invalid HTTP URL authority")); }
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let end = bracketed.find(']').ok_or_else(||Error::lua_string("Invalid IPv6 URL"))?;
        let host = &authority[..end + 2];
        let tail = &authority[end + 2..];
        (host, if tail.is_empty() { None } else { Some(tail.strip_prefix(':').ok_or_else(||Error::lua_string("Invalid HTTP port"))?) })
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        if host.contains(':') { return Err(Error::lua_string("IPv6 URL must use brackets")); }
        (host, Some(port))
    } else { (authority, None) };
    if host.is_empty() { return Err(Error::lua_string("Invalid HTTP host")); }
    let port = match port { Some(port) => port.parse::<u16>().ok().filter(|port|*port != 0).ok_or_else(||Error::lua_string("Invalid HTTP port"))?, None => if secure { 443 } else { 80 } };
    let suffix = &rest[split..];
    let path = if suffix.is_empty() { "/".into() } else if suffix.starts_with('?') { format!("/{suffix}") } else { suffix.into() };
    Ok((secure, host.into(), port, path))
}

fn parse_websocket_url(url:&str)->Result<(bool,String,u16,String)>{
    let normalized=if let Some(rest)=url.strip_prefix("ws://") {format!("http://{rest}")}
        else if let Some(rest)=url.strip_prefix("wss://") {format!("https://{rest}")}
        else {return Err(Error::lua_string("Only ws:// and wss:// URLs are supported"))};
    parse_url(&normalized)
}

fn header_token(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|c| c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&c))
}

// The worker is untrusted: the broker validates and retains exactly the request
// displayed to the user instead of accepting request data again at approval time.
pub(crate) struct ApprovedRequest { request: Request, child_id: String, display: Json }
impl ApprovedRequest {
    pub(crate) fn child_id(&self) -> &str { &self.child_id }
    pub(crate) fn display(&self) -> &Json { &self.display }
    pub(crate) fn is_websocket(&self)->bool {self.display.get("api").and_then(Json::as_str)==Some("executor.websocket")}
}

pub(crate) fn validate_worker_request(value: &Json) -> std::result::Result<ApprovedRequest, String> {
    let field = |name: &str| value.get(name).and_then(Json::as_str).ok_or_else(||format!("Invalid HTTP request {name}"));
    let child_id = field("id")?;
    if child_id.len() != 32 || !child_id.bytes().all(|c|c.is_ascii_hexdigit()) { return Err("Invalid HTTP request id".into()); }
    let realm = field("realm")?;
    let context = field("context")?;
    let api = field("api")?;
    if realm.is_empty() || realm.len() > 128 || !matches!(context,"server"|"client"|"executor")
        || !matches!(api,"executor.request"|"executor.httpget"|"HttpService.RequestAsync"|"HttpService.GetAsync"|"HttpService.PostAsync"|"game.HttpGet"|"executor.websocket") {
        return Err("Invalid HTTP request origin".into());
    }
    let url = field("url")?.to_owned();
    let websocket=api=="executor.websocket";
    let (secure,host,port,path) = if websocket {parse_websocket_url(&url)} else {parse_url(&url)}.map_err(|error|error.to_string())?;
    let method = field("method")?.to_owned();
    if !matches!(method.as_str(),"GET"|"HEAD"|"POST"|"PUT"|"DELETE"|"OPTIONS"|"PATCH") { return Err("Invalid HTTP method".into()); }
    let body = match value.get("bodyPresent").and_then(Json::as_bool) {
        Some(true) => {
            let encoded=field("bodyBase64")?;
            if encoded.len() > ((MAX_BODY+2)/3)*4 { return Err("HTTP request body exceeds limit".into()); }
            let bytes=base64::decode(encoded).map_err(|_|"Invalid HTTP request body")?;
            if bytes.len()>MAX_BODY { return Err("HTTP request body exceeds limit".into()); }
            Some(bytes)
        }
        Some(false) if value.get("bodyBase64").is_none_or(Json::is_null) => None,
        _ => return Err("Invalid HTTP request body".into()),
    };
    if websocket && (method!="GET"||body.is_some()){return Err("Invalid WebSocket handshake".into())}
    let raw_headers=value.get("headers").and_then(Json::as_object).ok_or("Invalid HTTP headers")?;
    let mut headers=BTreeMap::new();
    let mut total=0usize;
    for (name,raw) in raw_headers {
        let header=raw.as_str().ok_or("Invalid HTTP header value")?;
        total=total.saturating_add(name.len()+header.len()+4);
        if headers.len()>=128 || total>MAX_HEADER_BYTES || !header_token(name) || !header.is_ascii()
            || header.bytes().any(|c|(c<0x20 && c!=b'\t') || c==0x7f)
            || headers.keys().any(|existing:&String|existing.eq_ignore_ascii_case(name))
            || name.eq_ignore_ascii_case("transfer-encoding") { return Err("Invalid HTTP headers".into()); }
        headers.insert(name.clone(),header.to_owned());
    }
    let header = |name:&str| headers.iter().find(|(key,_)|key.eq_ignore_ascii_case(name)).map(|(_,value)|value.as_str());
    let expected_host=if port == if secure {443} else {80} {host.clone()} else {format!("{host}:{port}")};
    if header("host")!=Some(expected_host.as_str()) || header("connection")!=Some(if websocket {"Upgrade"} else {"close"})
        || header("user-agent").is_none() || header("accept").is_none() {
        return Err("HTTP transport headers do not match request".into());
    }
    if websocket && (headers.len()!=5 || header("upgrade")!=Some("websocket") || header("user-agent")!=Some("Roblox/WinInet") || header("accept")!=Some("*/*")) {
        return Err("Invalid WebSocket handshake headers".into());
    }
    let body_length=body.as_ref().map_or(0,Vec::len).to_string();
    let requires_length=body.is_some() || matches!(method.as_str(),"POST"|"PUT"|"PATCH");
    if (requires_length && header("content-length")!=Some(body_length.as_str()))
        || (!requires_length && header("content-length").is_some()) { return Err("HTTP body length does not match headers".into()); }
    let mut id_bytes=[0u8;16];
    getrandom::fill(&mut id_bytes).map_err(|error|error.to_string())?;
    let id=id_bytes.iter().map(|byte|format!("{byte:02x}")).collect::<String>();
    let display=json!({"id":id,"realm":realm,"context":context,"api":api,"method":method,"url":url,"headers":headers,
        "bodyBase64":body.as_deref().map(base64::encode),"bodyText":body.as_deref().and_then(|bytes|std::str::from_utf8(bytes).ok()),
        "bodyBytes":body.as_ref().map_or(0,Vec::len),"bodyPresent":body.is_some()});
    Ok(ApprovedRequest{request:Request{url,method,host,port,path,secure,headers,body},child_id:child_id.to_owned(),display})
}

pub(crate) fn perform_approved(request: ApprovedRequest) -> Json {
    #[cfg(windows)] let result=transport::send(request.request);
    #[cfg(not(windows))] let result: std::result::Result<Response,String> = { let _=request; Err("HTTP transport requires Windows".into()) };
    match result {
        Ok(response)=>json!({"ok":true,"status":response.status,"message":response.message,"headers":response.headers,"bodyBase64":base64::encode(response.body)}),
        Err(error)=>json!({"ok":false,"error":error}),
    }
}

fn decode_external_response(value: Json) -> std::result::Result<std::result::Result<Response,String>,String> {
    if value.get("ok").and_then(Json::as_bool)==Some(false) {
        let error=value.get("error").and_then(Json::as_str).filter(|error|error.len()<=4096).ok_or("Invalid HTTP error")?;
        return Ok(Err(error.to_owned()));
    }
    if value.get("ok").and_then(Json::as_bool)!=Some(true) { return Err("Invalid HTTP response".into()); }
    let status=value.get("status").and_then(Json::as_u64).filter(|value|(100..=599).contains(value)).ok_or("Invalid HTTP status")? as u16;
    let message=value.get("message").and_then(Json::as_str).filter(|value|value.len()<=4096).ok_or("Invalid HTTP status message")?.to_owned();
    let headers: BTreeMap<String,String>=serde_json::from_value(value.get("headers").cloned().ok_or("Missing HTTP response headers")?).map_err(|_|"Invalid HTTP response headers")?;
    if headers.len()>1024 || headers.iter().map(|(name,value)|name.len()+value.len()).sum::<usize>()>MAX_HEADER_BYTES {return Err("HTTP response headers exceed limit".into());}
    let encoded=value.get("bodyBase64").and_then(Json::as_str).filter(|value|value.len()<=((MAX_RESPONSE+2)/3)*4).ok_or("Invalid HTTP response body")?;
    let body=base64::decode(encoded).map_err(|_|"Invalid HTTP response body")?;
    if body.len()>MAX_RESPONSE {return Err("HTTP response body exceeds limit".into());}
    Ok(Ok(Response{status,message,headers,body}))
}

fn parse_request(options: Table, kind: ResponseKind) -> Result<Request> {
    let raw_url = options.raw_get::<mlua::LuaString>("Url")?;
    if raw_url.as_bytes().len() > MAX_URL { return Err(Error::lua_string("Invalid HTTP URL")); }
    let url = raw_url.to_str()?.to_owned();
    let (secure, host, port, path) = if kind==ResponseKind::WebSocket {parse_websocket_url(&url)} else {parse_url(&url)}?;
    let method = match options.raw_get::<Option<mlua::LuaString>>("Method")? {
        Some(method) if method.as_bytes().len() <= 16 => method.to_str()?.to_ascii_uppercase(),
        Some(_) => return Err(Error::lua_string("Unsupported HTTP method")),
        None => "GET".into(),
    };
    if !matches!(method.as_str(), "GET"|"HEAD"|"POST"|"PUT"|"DELETE"|"OPTIONS"|"PATCH") {
        return Err(Error::lua_string("Unsupported HTTP method"));
    }
    let body = match options.raw_get::<Option<mlua::LuaString>>("Body")? {
        Some(body) if body.as_bytes().len() <= MAX_BODY => Some(body.as_bytes().to_vec()),
        Some(_) => return Err(Error::lua_string("HTTP request body exceeds 256 KiB")),
        None => None,
    };
    let mut headers = BTreeMap::new();
    let mut input_bytes = 0usize;
    if let Some(table) = options.raw_get::<Option<Table>>("Headers")? {
        for pair in table.pairs::<mlua::LuaString,mlua::LuaString>() {
            let (name,value) = pair?;
            input_bytes = input_bytes.saturating_add(name.as_bytes().len()).saturating_add(value.as_bytes().len()).saturating_add(4);
            if headers.len() >= 128 || input_bytes > MAX_HEADER_BYTES { return Err(Error::lua_string("HTTP request headers exceed limit")); }
            let name = name.to_str()?.to_owned();
            let value = value.to_str()?.to_owned();
            if !header_token(&name) || !value.is_ascii() || value.bytes().any(|c| (c < 0x20 && c != b'\t') || c == 0x7f) { return Err(Error::lua_string("Invalid HTTP header")); }
            if headers.keys().any(|existing: &String|existing.eq_ignore_ascii_case(&name)) { return Err(Error::lua_string("Duplicate HTTP header")); }
            if matches!(name.to_ascii_lowercase().as_str(), "content-length"|"transfer-encoding"|"host"|"connection") {
                return Err(Error::lua_string("HTTP transport manages Host, Connection and body framing headers"));
            }
            headers.insert(name,value);
        }
    }
    if let Some(table) = options.raw_get::<Option<Table>>("Cookies")? {
        if headers.keys().any(|name|name.eq_ignore_ascii_case("cookie")) { return Err(Error::lua_string("Specify Cookie header or Cookies, not both")); }
        let mut cookies = BTreeMap::new();
        for pair in table.pairs::<mlua::LuaString,mlua::LuaString>() {
            let (name,value) = pair?;
            input_bytes = input_bytes.saturating_add(name.as_bytes().len()).saturating_add(value.as_bytes().len()).saturating_add(3);
            if cookies.len() >= 128 || input_bytes > MAX_HEADER_BYTES { return Err(Error::lua_string("HTTP request headers exceed limit")); }
            let name = name.to_str()?.to_owned();
            let value = value.to_str()?.to_owned();
            if !header_token(&name) || value.bytes().any(|c| c <= 0x20 || c >= 0x7f || matches!(c, b';'|b','|b'\\'|b'"')) {
                return Err(Error::lua_string("Invalid HTTP cookie"));
            }
            cookies.insert(name,value);
        }
        if !cookies.is_empty() { headers.insert("Cookie".into(),cookies.into_iter().map(|(k,v)|format!("{k}={v}")).collect::<Vec<_>>().join("; ")); }
    }
    let has_user_agent = headers.keys().any(|name|name.eq_ignore_ascii_case("user-agent"));
    let http_service = matches!(kind,ResponseKind::HttpServiceRequest|ResponseKind::HttpServiceGet|ResponseKind::HttpServicePost);
    if http_service && has_user_agent { return Err(Error::lua_string("HttpService User-Agent is locked")); }
    if http_service && headers.keys().any(|name|name.eq_ignore_ascii_case("roblox-id")) { return Err(Error::lua_string("HttpService Roblox-Id is locked")); }
    if !has_user_agent { headers.insert("User-Agent".into(),if http_service {"RobloxStudio/WinInet"} else {"Roblox/WinInet"}.into()); }
    if !headers.keys().any(|name|name.eq_ignore_ascii_case("accept")) { headers.insert("Accept".into(),"*/*".into()); }
    let default_port = if secure { 443 } else { 80 };
    headers.insert("Host".into(),if port == default_port { host.clone() } else { format!("{host}:{port}") });
    if kind==ResponseKind::WebSocket {
        headers.insert("Connection".into(),"Upgrade".into());
        headers.insert("Upgrade".into(),"websocket".into());
    } else {headers.insert("Connection".into(),"close".into());}
    if body.is_some() || matches!(method.as_str(), "POST"|"PUT"|"PATCH") {
        headers.insert("Content-Length".into(),body.as_ref().map_or(0,Vec::len).to_string());
    }
    if headers.len() > 128 || headers.iter().map(|(name,value)|name.len()+value.len()+4).sum::<usize>() > MAX_HEADER_BYTES {
        return Err(Error::lua_string("HTTP request headers exceed limit"));
    }
    Ok(Request { url, method, host, port, path, secure, headers, body })
}

pub(crate) fn submit(lua: &Lua, options: Table, kind: ResponseKind) -> Result<NativeCall> {
    #[cfg(not(windows))]
    { let _ = (lua,options,kind); Err(Error::lua_string("UnsupportedApi: HTTP transport is currently available on Windows only")) }
    #[cfg(windows)]
    {
        let handle = lua.app_data_ref::<Handle>().map(|handle|handle.clone()).ok_or_else(||Error::lua_string("UnsupportedApi: request requires interactive user approval"))?;
        if handle.broker.has_pending() { return Err(Error::lua_string("another HTTP request is awaiting approval")); }
        if !crate::scheduler::network_freeze_safe(lua)? { return Err(Error::lua_string("UnsupportedApi: request from an unmanaged coroutine or unsafe nested callback")); }
        let context=if crate::scheduler::current_origin(lua)=="executor" { "executor".to_owned() }
            else { crate::internal::<Table>(lua,"__config")?.raw_get::<String>("side")? };
        let request = parse_request(options,kind)?;
        crate::scheduler::park_current(lua)?;
        handle.broker.enqueue(handle.realm, context, lua.current_thread(), request, kind)?;
        Ok(NativeCall::Yield(MultiValue::new()))
    }
}

unsafe extern "C-unwind" fn request(state: *mut ffi::lua_State) -> i32 {
    unsafe { Lua::invoke_native_callback(state, |lua, mut args| {
        let options = match args.pop_front() { Some(Value::Table(table)) => table, _ => return Err(Error::lua_string("request expects an options table")) };
        submit(lua,options,ResponseKind::ExecutorRequest)
    }) }
}

unsafe extern "C-unwind" fn httpget(state: *mut ffi::lua_State) -> i32 {
    unsafe { Lua::invoke_native_callback(state, |lua, mut args| {
        let url = match args.pop_front() { Some(Value::String(url)) => url, _ => return Err(Error::lua_string("httpget expects a URL string")) };
        let options=lua.create_table()?;
        options.raw_set("Url",url)?;
        options.raw_set("Method","GET")?;
        submit(lua,options,ResponseKind::ExecutorHttpGet)
    }) }
}

unsafe extern "C-unwind" fn websocket_connect(state:*mut ffi::lua_State)->i32 {
    unsafe {Lua::invoke_native_callback(state,|lua,mut args|{
        let url=match args.pop_front(){Some(Value::String(url))=>url,_=>return Err(Error::lua_string("WebSocket.connect expects a URL string"))};
        let options=lua.create_table()?;options.raw_set("Url",url)?;options.raw_set("Method","GET")?;
        submit(lua,options,ResponseKind::WebSocket)
    })}
}

#[cfg(windows)]
pub(crate) type HostedWebSocket=websocket::Socket;
#[cfg(windows)]
pub(crate) fn spawn_websocket(request:ApprovedRequest,events:mpsc::SyncSender<(String,Json)>)->std::result::Result<HostedWebSocket,String>{
    if !request.is_websocket(){return Err("Invalid WebSocket request".into())}
    websocket::spawn(&request.request.url,request.child_id,events)
}
#[cfg(windows)]
pub(crate) fn websocket_broker_main()->std::result::Result<(),String>{websocket::broker_main()}
#[cfg(not(windows))]
pub(crate) struct HostedWebSocket;
#[cfg(not(windows))]
impl HostedWebSocket {pub(crate) fn command(&self,_:Json)->std::result::Result<(),String>{Err("WebSocket requires Windows".into())}}
#[cfg(not(windows))]
pub(crate) fn spawn_websocket(_:ApprovedRequest,_:mpsc::SyncSender<(String,Json)>)->std::result::Result<HostedWebSocket,String>{Err("WebSocket requires Windows".into())}

pub(crate) fn install(lua: &Lua, env: &Table, caps: &crate::native::Capabilities) -> Result<()> {
    let function: Function = unsafe { lua.exec_raw((), |state|ffi::lua_pushcclosurek(state, request, c"request".as_ptr(), 0, None))? };
    crate::native::mark_executor_function(lua,&function,env)?;
    env.set("request",function.clone())?;
    env.set("http_request",function.clone())?;
    let http = lua.create_table()?;
    http.set("request",function.clone())?;
    env.set("http",http)?;
    let syn=lua.create_table()?;
    syn.set("request",function)?;
    env.set("syn",syn.clone())?;
    let httpget:Function=unsafe {lua.exec_raw((),|state|ffi::lua_pushcclosurek(state,httpget,c"httpget".as_ptr(),0,None))?};
    crate::native::mark_executor_function(lua,&httpget,env)?;
    env.set("httpget",httpget)?;
    let connect:Function=unsafe {lua.exec_raw((),|state|ffi::lua_pushcclosurek(state,websocket_connect,c"connect".as_ptr(),0,None))?};
    crate::native::mark_executor_function(lua,&connect,env)?;
    let ws=lua.create_table()?;ws.raw_set("connect",connect)?;
    env.set("WebSocket",ws.clone())?;env.set("websocket",ws.clone())?;syn.set("websocket",ws)?;
    caps.set("request","partial: Windows HTTP(S), user approval per request, no redirects")?;
    caps.set("httpget","partial: Windows HTTP(S), user approval per request, no redirects")?;
    caps.set("WebSocket.connect","partial: Windows ws/wss via approved host broker; connection/message queues are bounded")?;
    Ok(())
}

#[cfg(not(windows))]
mod transport {
    use super::*;
    pub fn send(_: Request) -> std::result::Result<Response,String> { Err("UnsupportedApi: HTTP transport is currently available on Windows only".into()) }
}

#[cfg(all(test, emulator_local_tests))]
#[path = "../local_tests/network.rs"]
mod tests;

#[cfg(windows)]
mod transport {
    use super::*;
    use std::{ffi::c_void, ptr};

    type Internet = *mut c_void;
    #[link(name = "winhttp")]
    unsafe extern "system" {
        fn WinHttpOpen(agent: *const u16, access: u32, proxy: *const u16, bypass: *const u16, flags: u32) -> Internet;
        fn WinHttpConnect(session: Internet, server: *const u16, port: u16, reserved: u32) -> Internet;
        fn WinHttpOpenRequest(connection: Internet, method: *const u16, object: *const u16, version: *const u16, referrer: *const u16, accept: *const *const u16, flags: u32) -> Internet;
        fn WinHttpSetOption(handle: Internet, option: u32, buffer: *mut c_void, length: u32) -> i32;
        fn WinHttpSetTimeouts(handle: Internet, resolve: i32, connect: i32, send: i32, receive: i32) -> i32;
        fn WinHttpSendRequest(handle: Internet, headers: *const u16, header_len: u32, body: *mut c_void, body_len: u32, total_len: u32, context: usize) -> i32;
        fn WinHttpReceiveResponse(handle: Internet, reserved: *mut c_void) -> i32;
        fn WinHttpQueryHeaders(handle: Internet, level: u32, name: *const u16, buffer: *mut c_void, length: *mut u32, index: *mut u32) -> i32;
        fn WinHttpReadData(handle: Internet, buffer: *mut c_void, length: u32, received: *mut u32) -> i32;
        fn WinHttpCloseHandle(handle: Internet) -> i32;
    }

    struct HttpHandle(Internet);
    impl HttpHandle {
        fn new(handle: Internet, operation: &str) -> std::result::Result<Self,String> {
            if handle.is_null() { Err(format!("HTTP {operation}: {}",std::io::Error::last_os_error())) } else { Ok(Self(handle)) }
        }
    }
    impl Drop for HttpHandle { fn drop(&mut self) { unsafe { WinHttpCloseHandle(self.0); } } }
    fn wide(value: &str) -> Vec<u16> { value.encode_utf16().chain(std::iter::once(0)).collect() }
    fn check(ok: i32, operation: &str) -> std::result::Result<(),String> {
        if ok == 0 { Err(format!("HTTP {operation}: {}",std::io::Error::last_os_error())) } else { Ok(()) }
    }
    fn response_headers(request: Internet) -> std::result::Result<(u16,String,BTreeMap<String,String>),String> {
        let mut bytes = 0u32;
        unsafe { WinHttpQueryHeaders(request,22,ptr::null(),ptr::null_mut(),&mut bytes,ptr::null_mut()); }
        if bytes == 0 || bytes as usize > MAX_HEADER_BYTES || bytes % 2 != 0 { return Err("HTTP response headers exceed limit or are unavailable".into()); }
        let mut buffer = vec![0u16;bytes as usize / 2];
        check(unsafe { WinHttpQueryHeaders(request,22,ptr::null(),buffer.as_mut_ptr().cast(),&mut bytes,ptr::null_mut()) },"query headers")?;
        let text = String::from_utf16(&buffer[..(bytes as usize / 2)]).map_err(|_|"Invalid HTTP response headers")?;
        let mut lines = text.trim_end_matches('\0').split("\r\n");
        let status = lines.next().ok_or("Missing HTTP status line")?;
        let mut parts = status.splitn(3,' ');
        let protocol = parts.next().unwrap_or("");
        let code = parts.next().and_then(|value|value.parse::<u16>().ok()).ok_or("Invalid HTTP status code")?;
        if !protocol.starts_with("HTTP/") { return Err("Invalid HTTP status line".into()); }
        let message = parts.next().unwrap_or("").to_owned();
        let mut headers = BTreeMap::<String,String>::new();
        for line in lines.filter(|line|!line.is_empty()) {
            let (name,value) = line.split_once(':').ok_or("Invalid HTTP response header")?;
            let value = value.trim().to_owned();
            if let Some(existing) = headers.get_mut(name) { existing.push_str(", "); existing.push_str(&value); }
            else { headers.insert(name.into(),value); }
        }
        Ok((code,message,headers))
    }

    pub fn send(request: Request) -> std::result::Result<Response,String> {
        let started=std::time::Instant::now();
        let session = HttpHandle::new(unsafe { WinHttpOpen(ptr::null(),1,ptr::null(),ptr::null(),0) },"initialize")?;
        check(unsafe { WinHttpSetTimeouts(session.0,5000,5000,10000,10000) },"set timeouts")?;
        let mut retries = 1u32;
        check(unsafe { WinHttpSetOption(session.0,4,(&mut retries as *mut u32).cast(),4) },"set connection retries")?;
        let host = wide(&request.host);
        let connection = HttpHandle::new(unsafe { WinHttpConnect(session.0,host.as_ptr(),request.port,0) },"connect setup")?;
        let method = wide(&request.method);
        let path = wide(&request.path);
        let flags = 0x40 | 0x80 | if request.secure { 0x00800000 } else { 0 };
        let handle = HttpHandle::new(unsafe { WinHttpOpenRequest(connection.0,method.as_ptr(),path.as_ptr(),ptr::null(),ptr::null(),ptr::null(),flags) },"request setup")?;
        let mut disabled = 0x01u32 | 0x02 | 0x04 | 0x08;
        check(unsafe { WinHttpSetOption(handle.0,63,(&mut disabled as *mut u32).cast(),4) },"disable redirects, cookies and authentication")?;
        let header_block = request.headers.iter().map(|(name,value)|format!("{name}: {value}\r\n")).collect::<String>();
        let headers = wide(&header_block);
        let body = request.body.as_deref().unwrap_or(&[]);
        let body_ptr = if body.is_empty() { ptr::null_mut() } else { body.as_ptr() as *mut c_void };
        check(unsafe { WinHttpSendRequest(handle.0,headers.as_ptr(),u32::MAX,body_ptr,body.len() as u32,body.len() as u32,0) },"send")?;
        check(unsafe { WinHttpReceiveResponse(handle.0,ptr::null_mut()) },"receive")?;
        let (status,message,headers) = response_headers(handle.0)?;
        let mut body = Vec::new();
        let mut buffer = [0u8;16*1024];
        loop {
            if started.elapsed() > std::time::Duration::from_secs(30) { return Err("HTTP request exceeded 30 seconds".into()); }
            let mut received = 0u32;
            check(unsafe { WinHttpReadData(handle.0,buffer.as_mut_ptr().cast(),buffer.len() as u32,&mut received) },"read body")?;
            if received == 0 { break; }
            if body.len().saturating_add(received as usize) > MAX_RESPONSE { return Err("HTTP response body exceeds 8 MiB".into()); }
            body.extend_from_slice(&buffer[..received as usize]);
        }
        Ok(Response { status, message, headers, body })
    }
}
