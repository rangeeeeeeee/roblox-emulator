//! Host-side control of a sandboxed worker. No Luau state is constructed here.
use crate::{Scenario, network::{self, ApprovedRequest}, session::Session as LocalSession, worker};
use mlua::{Error, Result};
use serde_json::{Value, json};
use std::{cell::RefCell, collections::{HashMap,HashSet}, io, path::Path, sync::mpsc, time::Duration};

const WORKER_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_IN_FLIGHT: usize = 8;
const MAX_SOCKETS: usize = 8;
const MAX_SOCKET_ACTIONS: usize = 32;
const MAX_SOCKET_MESSAGE: usize = 256 * 1024;

pub(crate) fn validate_socket_actions(value:Option<&Value>,connected:impl Fn(&str)->bool)->std::result::Result<Vec<(String,Value)>,String> {
    let actions=value.and_then(Value::as_array).ok_or("Invalid WebSocket action list")?;
    if actions.len()>MAX_SOCKET_ACTIONS{return Err("WebSocket action limit exceeded".into())}
    let mut result=Vec::with_capacity(actions.len());
    for action in actions {
        let id=action.get("id").and_then(Value::as_str).filter(|id|id.len()==32&&id.bytes().all(|byte|byte.is_ascii_hexdigit())).ok_or("Invalid WebSocket action id")?;
        if !connected(id){return Err("WebSocket is no longer active".into())}
        let command=match action.get("op").and_then(Value::as_str) {
            Some("send")=>{
                let data=action.get("data").and_then(Value::as_str).filter(|data|data.len()<=((MAX_SOCKET_MESSAGE+2)/3)*4).ok_or("Invalid WebSocket data")?;
                let bytes=base64::decode(data).map_err(|_|"Invalid WebSocket data")?;
                if bytes.len()>MAX_SOCKET_MESSAGE{return Err("WebSocket message exceeds limit".into())}
                json!({"op":"send","data":base64::encode(bytes)})
            },
            Some("close")=>json!({"op":"close"}),
            _=>return Err("Invalid WebSocket operation".into()),
        };
        result.push((id.to_owned(),command));
    }
    Ok(result)
}

struct RpcWorker {
    process: Option<crate::sandbox_windows::WorkerProcess>,
    sender: mpsc::SyncSender<Value>,
    receiver: mpsc::Receiver<std::result::Result<Value,String>>,
}
impl RpcWorker {
    fn start() -> std::result::Result<Self,String> {
        let process=crate::sandbox_windows::spawn_worker().map_err(|error|format!("Sandbox unavailable: {error}"))?;
        let mut input=process.stdout.try_clone().map_err(|error|error.to_string())?;
        let mut output=process.stdin.try_clone().map_err(|error|error.to_string())?;
        let (sender,requests)=mpsc::sync_channel::<Value>(1);
        let (responses,receiver)=mpsc::sync_channel(1);
        std::thread::Builder::new().name("emulator-worker-ipc".into()).spawn(move||{
            while let Ok(request)=requests.recv() {
                let result=worker::write_frame(&mut output,&request)
                    .and_then(|()|worker::read_frame(&mut input)?.ok_or_else(||io::Error::new(io::ErrorKind::UnexpectedEof,"Worker closed the pipe")))
                    .map_err(|error|error.to_string());
                let failed=result.is_err();
                if responses.send(result).is_err() || failed {break;}
            }
        }).map_err(|error|error.to_string())?;
        Ok(Self{process:Some(process),sender,receiver})
    }
    fn close(&mut self) {self.process.take();}
    fn request(&mut self, request: Value) -> std::result::Result<Value,String> {self.request_with_timeout(request,WORKER_TIMEOUT)}
    fn request_with_timeout(&mut self, mut request: Value,timeout:Duration) -> std::result::Result<Value,String> {
        if self.process.is_none() {return Err("Sandbox worker has stopped".into());}
        let mut nonce=[0u8;16];getrandom::fill(&mut nonce).map_err(|error|error.to_string())?;
        let nonce=nonce.iter().map(|byte|format!("{byte:02x}")).collect::<String>();
        request.as_object_mut().ok_or("Invalid worker request")?.insert("nonce".into(),json!(nonce));
        if self.sender.send(request).is_err() {self.close();return Err("Sandbox worker IPC is unavailable".into());}
        let deadline=std::time::Instant::now()+timeout;
        let reply=loop {
            if crate::stop_requested() {self.close();return Err("Execution interrupted".into());}
            let remaining=deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {self.close();return Err("Sandbox worker timed out".into());}
            match self.receiver.recv_timeout(remaining.min(Duration::from_millis(200))) {
                Ok(Ok(reply))=>break reply,
                Ok(Err(error))=>{self.close();return Err(format!("Sandbox worker IPC failed: {error}"));},
                Err(mpsc::RecvTimeoutError::Timeout)=>{},
                Err(mpsc::RecvTimeoutError::Disconnected)=>{self.close();return Err("Sandbox worker exited".into());},
            }
        };
        if reply.get("nonce").and_then(Value::as_str)!=Some(nonce.as_str()) {
            self.close();return Err("Sandbox worker sent an invalid response".into());
        }
        Ok(reply)
    }
}

pub(crate) fn run_batch(scenario:Scenario,snapshots:Vec<Value>)->std::result::Result<Value,String> {
    let mut worker=RpcWorker::start()?;
    let reply=worker.request_with_timeout(json!({"op":"batch","scenario":scenario,"snapshots":snapshots}),Duration::from_secs(120))?;
    if reply.get("ok").and_then(Value::as_bool)!=Some(true) {
        return Err(reply.get("value").and_then(Value::as_str).unwrap_or("Sandbox batch failed").to_owned());
    }
    let report=reply.get("value").cloned().ok_or("Sandbox batch returned no report")?;
    if report.get("passed").and_then(Value::as_bool).is_none()
        || report.get("logs").and_then(Value::as_array).is_none_or(|rows|rows.len()>4000)
        || report.get("realms").and_then(Value::as_array).is_none_or(|rows|rows.len()>16) {
        return Err("Sandbox batch returned an invalid report".into());
    }
    Ok(report)
}

pub(crate) fn run_probe(source:String,reference:Value,full_runtime:bool)->std::result::Result<Value,String> {
    let mut worker=RpcWorker::start()?;
    let reply=worker.request(json!({"op":"probe","source":source,"reference":reference,"fullRuntime":full_runtime}))?;
    if reply.get("ok").and_then(Value::as_bool)!=Some(true) {
        return Err(reply.get("value").and_then(Value::as_str).unwrap_or("Sandbox probe failed").to_owned());
    }
    let result=reply.get("value").cloned().ok_or("Sandbox probe returned no result")?;
    if result.get("nativeParity").and_then(Value::as_bool).is_none()
        || result.get("mismatches").and_then(Value::as_array).is_none() {return Err("Invalid sandbox probe result".into());}
    Ok(result)
}

struct Pending { raw: Value, approved: ApprovedRequest }
struct Live {
    worker: RpcWorker,
    state: Value,
    pending: Option<Pending>,
    inflight: HashSet<String>,
    sender: mpsc::Sender<(String,Value)>,
    receiver: mpsc::Receiver<(String,Value)>,
    sockets: HashMap<String,network::HostedWebSocket>,
    socket_sender: mpsc::SyncSender<(String,Value)>,
    socket_receiver: mpsc::Receiver<(String,Value)>,
}
impl Live {
    fn call(&mut self, request: Value) -> std::result::Result<Value,String> {
        let compact=matches!(request.get("op").and_then(Value::as_str),Some("advance"|"lint"));
        let reply=self.worker.request(request)?;
        let Some(ok)=reply.get("ok").and_then(Value::as_bool) else {
            self.worker.close();return Err("Invalid worker response".into());
        };
        if !ok && !compact && reply.get("state").is_none_or(Value::is_null) {
            return Err(reply.get("value").and_then(Value::as_str).unwrap_or("Worker initialization failed").to_owned());
        }
        let state=reply.get("state").and_then(Value::as_object);
        if let Some(state)=state {
            if state.get("realms").and_then(Value::as_array).is_none_or(|rows|rows.len()>16)
                || state.get("logs").and_then(Value::as_array).is_none_or(|rows|rows.len()>4000)
                || state.get("commands").and_then(Value::as_array).is_none_or(|rows|rows.len()>128) {
                self.worker.close();return Err("Sandbox worker returned an invalid state".into());
            }
        } else if !compact || !reply.get("state").is_some_and(Value::is_null) {
            self.worker.close();return Err("Invalid worker state".into());
        }
        let next=reply.get("pending").filter(|value|!value.is_null());
        match (&self.pending,next) {
            (Some(previous),Some(value)) if &previous.raw==value => {},
            (Some(_),_)=>{self.worker.close();return Err("Pending HTTP request changed before approval".into());},
            (None,Some(value))=>{
                let approved=match network::validate_worker_request(value) {
                    Ok(approved)=>approved,
                    Err(error)=>{self.worker.close();return Err(format!("Invalid worker HTTP request: {error}"));}
                };
                if self.inflight.contains(approved.child_id()) {
                    self.worker.close();return Err("Worker reused an active HTTP request ID".into());
                }
                self.pending=Some(Pending{raw:value.clone(),approved});
            }
            (None,None)=>{},
        }
        let actions=validate_socket_actions(reply.get("socketActions"),|id|self.sockets.contains_key(id)&&!self.inflight.contains(id));
        let actions=match actions {Ok(actions)=>actions,Err(error)=>{self.worker.close();self.sockets.clear();return Err(error)}};
        for (id,command) in actions {
            if let Err(error)=self.sockets.get(&id).unwrap().command(command) {self.worker.close();self.sockets.clear();return Err(error)}
        }
        if let Some(state)=state {self.state=Value::Object(state.clone());}
        if ok {Ok(reply.get("value").cloned().unwrap_or(Value::Null))}
        else {Err(reply.get("value").and_then(Value::as_str).unwrap_or("Sandbox worker operation failed").to_owned())}
    }
}

pub(crate) struct Session {
    live: RefCell<Option<Live>>,
    lint_worker: RefCell<Option<RpcWorker>>,
    scenario: Scenario,
    profile: String,
    trace: bool,
    stopped: Value,
}
impl Session {
    fn failure(error: impl Into<String>) -> Error {Error::runtime(error.into())}
    fn start(scenario: Scenario, origin: Option<String>) -> Result<Self> {
        let profile=scenario.realms.iter().find(|realm|realm.side=="client" && realm.executor)
            .map(|realm|realm.executor_profile.clone()).unwrap_or_else(||"generic".into());
        let trace=scenario.trace_errors;
        let worker=RpcWorker::start().map_err(Self::failure)?;
        let (sender,receiver)=mpsc::channel();
        let (socket_sender,socket_receiver)=mpsc::sync_channel(64);
        let mut live=Live{worker,state:Value::Null,pending:None,inflight:HashSet::new(),sender,receiver,sockets:HashMap::new(),socket_sender,socket_receiver};
        live.call(json!({"op":"init","scenario":scenario,"origin":origin})).map_err(Self::failure)?;
        Ok(Self{live:RefCell::new(Some(live)),lint_worker:RefCell::new(None),scenario,profile,trace,stopped:Value::Null})
    }
    pub fn new(profile: &str) -> Result<Self> {Self::start(LocalSession::default_scenario(profile)?,None)}
    pub fn from_path(path: &Path) -> Result<Self> {
        Self::start(LocalSession::prepare_scenario(path,None,None)?,Some(path.to_string_lossy().into_owned()))
    }
    pub(crate) fn from_place_with_trace(place:&crate::place::Place,enabled:bool)->Result<Self> {
        let mut scenario=place.scenario().map_err(Self::failure)?;scenario.trace_errors=enabled;Self::start(scenario,None)
    }
    pub(crate) fn from_path_with_profile_and_trace(path:&Path,profile:&str,enabled:bool)->Result<Self> {
        Self::start(LocalSession::prepare_scenario(path,Some(profile),Some(enabled))?,Some(path.to_string_lossy().into_owned()))
    }
    pub(crate) fn stopped() -> Self {
        // This constructor creates an empty host with no VM and executes no script.
        let local=LocalSession::stopped();
        let stopped=local.snapshot().unwrap_or(Value::Null);
        Self{live:RefCell::new(None),lint_worker:RefCell::new(None),scenario:local.scenario_template().clone(),profile:"generic".into(),trace:false,stopped}
    }
    fn call(&self,request:Value)->Result<Value> {
        self.live.borrow_mut().as_mut().ok_or_else(||Self::failure("Sandbox worker is stopped"))?
            .call(request).map_err(Self::failure)
    }
    pub(crate) fn primary_profile(&self)->&str {&self.profile}
    pub(crate) fn scenario_template(&self)->&Scenario {&self.scenario}
    pub(crate) fn trace_errors_enabled(&self)->bool {self.trace}
    pub(crate) fn set_trace_errors(&mut self,enabled:bool)->Result<()> {
        self.call(json!({"op":"trace","enabled":enabled}))?;self.trace=enabled;Ok(())
    }
    pub(crate) fn network_pending(&self)->Option<Value> {
        self.live.borrow().as_ref().filter(|live|live.worker.process.is_some())
            .and_then(|live|live.pending.as_ref()).map(|pending|pending.approved.display().clone())
    }
    pub(crate) fn network_blocked(&self)->bool {self.network_pending().is_some()}
    pub(crate) fn network_decide(&mut self,id:&str,allow:bool)->Result<()> {
        let mut borrowed=self.live.borrow_mut();
        let live=borrowed.as_mut().ok_or_else(||Self::failure("Sandbox worker is stopped"))?;
        let pending=live.pending.take().ok_or_else(||Self::failure("Request is no longer awaiting approval"))?;
        if pending.approved.display().get("id").and_then(Value::as_str)!=Some(id) {
            live.pending=Some(pending);return Err(Self::failure("Request is no longer awaiting approval"));
        }
        if allow && live.inflight.len()>=MAX_IN_FLIGHT {live.pending=Some(pending);return Err(Self::failure("HTTP request limit exceeded"));}
        if allow && pending.approved.is_websocket() && live.sockets.len()>=MAX_SOCKETS {live.pending=Some(pending);return Err(Self::failure("WebSocket connection limit exceeded"));}
        let child_id=pending.approved.child_id().to_owned();
        live.call(json!({"op":"network_decide","id":child_id,"allow":allow})).map_err(Self::failure)?;
        if allow {
            if pending.approved.is_websocket() {
                match network::spawn_websocket(pending.approved,live.socket_sender.clone()) {
                    Ok(socket)=>{live.inflight.insert(child_id.clone());live.sockets.insert(child_id,socket);},
                    Err(error)=>{live.call(json!({"op":"network_complete","id":child_id,"response":{"ok":false,"error":error}})).map_err(Self::failure)?;}
                }
                return Ok(());
            }
            let send=live.sender.clone();let approved=pending.approved;let completion_id=child_id.clone();
            match std::thread::Builder::new().name("emulator-approved-http".into()).spawn(move||{
                let _=send.send((completion_id,network::perform_approved(approved)));
            }) {
                Ok(_)=>{live.inflight.insert(child_id);},
                Err(error)=>{live.call(json!({"op":"network_complete","id":child_id,"response":{"ok":false,"error":format!("HTTP worker could not start: {error}")}})).map_err(Self::failure)?;}
            }
        }
        Ok(())
    }
    pub(crate) fn poll_network(&mut self)->Result<()> {
        let mut borrowed=self.live.borrow_mut();let Some(live)=borrowed.as_mut() else{return Ok(())};
        if live.pending.is_some(){return Ok(());}
        while let Ok((id,response))=live.receiver.try_recv() {
            if !live.inflight.remove(&id) {continue;}
            live.call(json!({"op":"network_complete","id":id,"response":response})).map_err(Self::failure)?;
            if live.pending.is_some(){break;}
        }
        while live.pending.is_none() {
            let Ok((id,event))=live.socket_receiver.try_recv() else {break};
            if !live.sockets.contains_key(&id){continue}
            let kind=event.get("event").and_then(Value::as_str).unwrap_or("");
            if kind=="open" && live.inflight.remove(&id) {
                live.call(json!({"op":"network_complete","id":id,"response":{"ok":true,"websocket":true}})).map_err(Self::failure)?;
            } else if kind=="message" && !live.inflight.contains(&id) {
                let data=event.get("data").and_then(Value::as_str).filter(|data|data.len()<=((MAX_SOCKET_MESSAGE+2)/3)*4).ok_or_else(||Self::failure("Invalid WebSocket event"))?;
                let bytes=base64::decode(data).map_err(|_|Self::failure("Invalid WebSocket event"))?;
                if bytes.len()>MAX_SOCKET_MESSAGE{return Err(Self::failure("WebSocket event exceeds limit"))}
                live.call(json!({"op":"websocket_event","id":id,"event":{"event":"message","data":base64::encode(bytes)}})).map_err(Self::failure)?;
            } else if matches!(kind,"close"|"error") {
                let connecting=live.inflight.remove(&id);
                if connecting {
                    let error=event.get("error").and_then(Value::as_str).filter(|error|error.len()<=4096).unwrap_or("WebSocket closed before connecting");
                    live.call(json!({"op":"network_complete","id":id,"response":{"ok":false,"error":error}})).map_err(Self::failure)?;
                } else {
                    live.call(json!({"op":"websocket_event","id":id,"event":{"event":"close"}})).map_err(Self::failure)?;
                }
                live.sockets.remove(&id);
            } else {live.worker.close();live.sockets.clear();return Err(Self::failure("Invalid WebSocket broker event"));}
        }
        Ok(())
    }
    pub(crate) fn snapshot(&self)->Result<Value> {
        match self.live.borrow_mut().as_mut() {
            Some(live) if live.worker.process.is_some()=>{
                live.call(json!({"op":"snapshot"})).map_err(Self::failure)?;
                Ok(live.state.clone())
            },
            Some(_)=>Err(Self::failure("Sandbox worker has stopped")),
            None=>Ok(self.stopped.clone()),
        }
    }
    pub(crate) fn execute_input(&mut self,realm:&str,context:&str,source:&str,format:&str)->Result<Value> {
        self.call(json!({"op":"execute","realm":realm,"context":context,"source":source,"format":format}))
    }
    pub(crate) fn create_gc_viewer(&mut self,realm:&str)->Result<()> {
        self.call(json!({"op":"gc_create","realm":realm})).map(|_|())
    }
    pub(crate) fn keyboard_input(&mut self,realm:&str,key:&str,down:bool)->Result<()> {
        self.call(json!({"op":"input","realm":realm,"key":key,"down":down})).map(|_|())
    }
    pub(crate) fn advance(&mut self,seconds:f64)->Result<Value> {self.call(json!({"op":"advance","seconds":seconds}))}
    pub(crate) fn completions(&self,realm:&str,context:&str,path:&[String])->Result<Value> {
        self.call(json!({"op":"completions","realm":realm,"context":context,"path":path}))
    }
    pub(crate) fn workspace_files(&self,realm:&str)->Result<Value> {self.call(json!({"op":"workspace_list","realm":realm}))}
    pub(crate) fn workspace_file(&self,realm:&str,name:&str)->Result<Value> {self.call(json!({"op":"workspace_read","realm":realm,"name":name}))}
    pub(crate) fn inspect(&self,body:&Value)->Result<Value> {self.call(json!({"op":"inspect","body":body}))}
    pub(crate) fn runtime_update(&mut self,body:&Value,running:bool)->Result<Value> {self.call(json!({"op":"runtime_update","body":body,"running":running}))}
    pub(crate) fn set_cpu_budget(&mut self,ms:u64)->Result<()> {self.call(json!({"op":"budget","cpuMs":ms})).map(|_|())}
    pub(crate) fn clear_output(&mut self) {let _=self.call(json!({"op":"clear"}));}
    pub(crate) fn last_output(&self)->Value {
        self.live.borrow().as_ref().and_then(|live|live.state.get("logs").cloned())
            .or_else(||self.stopped.get("logs").cloned()).unwrap_or_else(||json!([]))
    }
    pub(crate) fn lint(&self,source:&str,context:&str)->std::result::Result<Value,String> {
        let request=json!({"op":"lint","source":source,"context":context});
        if self.live.borrow().is_some() {return self.call(request).map_err(|error|error.to_string());}
        let mut worker=self.lint_worker.borrow_mut();
        if worker.is_none() {*worker=Some(RpcWorker::start()?);}
        let reply=worker.as_mut().unwrap().request(request)?;
        if reply.get("ok").and_then(Value::as_bool)==Some(true) {Ok(reply.get("value").cloned().unwrap_or(Value::Null))}
        else {Err(reply.get("value").and_then(Value::as_str).unwrap_or("Sandbox lint failed").to_owned())}
    }
}
