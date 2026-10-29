use mlua::{Error as LuaError, Function, Lua, LuaSerdeExt, Result as LuaResult, Table, Value, VmState};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use std::{cell::{Cell, RefCell}, collections::{HashMap, VecDeque}, env, fs, io::Read, path::{Path, PathBuf}, rc::Rc, time::{Duration, Instant}};
mod native;
mod instance;
mod types;
mod scheduler;
mod model;
mod spatial;
mod executor;
mod encoding;
mod drawing;
mod network;
mod diagnostics;
mod place;
mod session;
mod web;
mod worker;
mod remote_session;
#[cfg(windows)] mod sandbox_windows;
#[cfg(not(windows))]
mod sandbox_windows {
    use std::{fs::File,io};
    pub(crate) struct WorkerProcess {pub(crate) stdin:File,pub(crate) stdout:File}
    pub(crate) fn spawn_worker()->io::Result<WorkerProcess> {Err(io::Error::new(io::ErrorKind::Unsupported,"OS sandbox is available on Windows only"))}
}
mod console;
mod inspector;
mod packet_spy;
mod lint;
mod bytecode;

#[cfg(windows)]
#[link(name="kernel32")]
unsafe extern "system" {
    fn QueryPerformanceCounter(value:*mut i64)->i32;
    fn QueryPerformanceFrequency(value:*mut i64)->i32;
    fn SetConsoleCtrlHandler(handler:Option<unsafe extern "system" fn(u32)->i32>,add:i32)->i32;
}
static STOP_REQUESTED:std::sync::atomic::AtomicBool=std::sync::atomic::AtomicBool::new(false);
fn stop_requested()->bool {STOP_REQUESTED.load(std::sync::atomic::Ordering::Relaxed)}
#[cfg(windows)]
unsafe extern "system" fn console_interrupt(kind:u32)->i32 {
    if kind<=1 {STOP_REQUESTED.store(true,std::sync::atomic::Ordering::Relaxed);1}else{0}
}
struct BudgetClock {
    #[cfg(windows)] frequency:Option<i64>,
    #[cfg(windows)] cached:Cell<Option<(Instant,i64)>>,
    #[cfg(windows)] previous:Cell<i64>,
}
impl BudgetClock {
    fn new()->Self {
        #[cfg(windows)] {
            let mut frequency=0;
            let valid=unsafe{QueryPerformanceFrequency(&mut frequency)}!=0&&frequency>0;
            Self{frequency:valid.then_some(frequency),cached:Cell::new(None),previous:Cell::new(0)}
        }
        #[cfg(not(windows))] {Self{}}
    }
    fn expired(&self,deadline:Instant)->bool {
        #[cfg(windows)] if let Some(frequency)=self.frequency {
            let mut current=0;
            if unsafe{QueryPerformanceCounter(&mut current)}!=0&&current>=0&&current>=self.previous.replace(current) {
                let threshold=match self.cached.get() {
                    Some((cached,threshold))if cached==deadline=>Some(threshold),
                    _=>{
                        // Counter sampled before Instant makes this threshold conservative.
                        // Recalibrate only when a caller changes the live deadline Cell.
                        let threshold=deadline.checked_duration_since(Instant::now()).and_then(|remaining|{
                            let ticks=remaining.as_nanos().checked_mul(frequency as u128)?/1_000_000_000;
                            current.checked_add(i64::try_from(ticks).ok()?)
                        });
                        self.cached.set(threshold.map(|threshold|(deadline,threshold)));threshold
                    }
                };
                if threshold.is_some_and(|threshold|current<=threshold){return false}
            }else{self.cached.set(None);}
        }
        // Preserve the exact existing timeout decision, including all fallback paths.
        Instant::now()>deadline
    }
}


#[derive(Default)]
struct HostInternals(RefCell<HashMap<String,i64>>);

fn initialize_internals(lua:&Lua)->LuaResult<()> {
    crate::native::set_private_registry_value(lua,"emulator.internal",lua.create_table()?)?;
    lua.set_app_data(HostInternals::default());
    Ok(())
}

pub(crate) fn internal<T: mlua::FromLua>(lua:&Lua,name:&str)->LuaResult<T> {
    let index=lua.app_data_ref::<HostInternals>().map(|state|state.0.borrow().get(name).copied());
    match index {
        None=>lua.globals().get(name),
        Some(Some(index))=>crate::native::private_registry_value::<Table>(lua,"emulator.internal")?.raw_get(index),
        Some(None)=>T::from_lua(Value::Nil,lua),
    }
}

pub(crate) fn set_internal<T:mlua::IntoLua>(lua:&Lua,name:&str,value:T)->LuaResult<()> {
    let index=lua.app_data_ref::<HostInternals>().map(|state|{
        let mut names=state.0.borrow_mut();
        if let Some(index)=names.get(name){*index}else{let index=(names.len()+1) as i64;names.insert(name.to_owned(),index);index}
    });
    if let Some(index)=index {crate::native::private_registry_value::<Table>(lua,"emulator.internal")?.raw_set(index,value)}
    else {lua.globals().set(name,value)}
}

struct HostLogSink(Rc<dyn Fn(&Lua, &str, mlua::LuaString) -> LuaResult<()>>);
pub(crate) fn host_log(lua:&Lua,level:&str,message:mlua::LuaString)->LuaResult<()> {
    let sink=lua.app_data_ref::<HostLogSink>().map(|sink|sink.0.clone());
    if let Some(sink)=sink { return sink(lua,level,message); }
    if let Ok(logger)=internal::<Function>(lua,"__host_log") { logger.call((level,message)) } else { Ok(()) }
}

fn hide_internals(lua:&Lua)->LuaResult<()> {
    let needs_registry=lua.app_data_ref::<HostInternals>().is_none();
    if needs_registry {initialize_internals(lua)?;}
    let globals=lua.globals();
    let keys=globals.clone().pairs::<Value,Value>().filter_map(|entry|match entry{Ok((Value::String(k),v)) if k.to_string_lossy().starts_with("__")=>Some(Ok((k,v))),Ok(_)=>None,Err(e)=>Some(Err(e))}).collect::<LuaResult<Vec<_>>>()?;
    for (key,value) in keys {set_internal(lua,&key.to_string_lossy(),value)?;globals.raw_set(key,Value::Nil)?;}
    globals.set("_G",lua.create_table()?)?;
    globals.set_safeenv(true);
    Ok(())
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RealmConfig {
    id: String,
    side: String,
    #[serde(default)] client_id: Option<String>,
    #[serde(default)] actor_id: Option<String>,
    #[serde(default)] executor: bool,
    #[serde(default = "default_executor_profile")] executor_profile: String,
    #[serde(default)] entry: Vec<String>,
    #[serde(default)] executor_entry: Vec<String>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExpectedError {
    realm: String,
    message: String,
    max_count: usize,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Scenario {
    name: String,
    #[serde(default = "default_engine")] engine: String,
    #[serde(default = "default_signal_behavior")] signal_behavior: String,
    #[serde(default)] trace_errors: bool,
    #[serde(default)] default_animate: bool,
    #[serde(default = "default_duration")] duration: f64,
    #[serde(default = "default_step")] step: f64,
    #[serde(default = "default_memory")] memory_mb: usize,
    #[serde(default = "default_cpu")] cpu_ms: u64,
    #[serde(default)] latency: f64,
    #[serde(default)] nodes: Vec<Json>,
    realms: Vec<RealmConfig>,
    #[serde(default)] expect: Vec<String>,
    #[serde(default)] reject: Vec<String>,
    #[serde(default)] expected_errors: Vec<ExpectedError>,
}
fn default_duration() -> f64 { 1.0 }
fn default_step() -> f64 { 1.0 / 60.0 }
fn default_memory() -> usize { 64 }
fn default_cpu() -> u64 { 2000 }
fn default_engine() -> String { "native".into() }
fn default_executor_profile() -> String { "generic".into() }
fn default_signal_behavior() -> String { "deferred".into() }

#[derive(Clone)]
struct Message { from: String, kind: String, payload: Json, at: f64, bytes: usize, origin: Option<diagnostics::SpyCaller> }
struct Realm { config: RealmConfig, lua: Lua, deadline: Rc<Cell<Instant>>, wall_time_exceeded: Rc<Cell<bool>>, native: bool }
struct Host {
    messages: Rc<RefCell<VecDeque<Message>>>,
    logs: Rc<RefCell<Vec<Json>>>,
    clock: Rc<Cell<f64>>,
    emitted: Rc<Cell<usize>>,
    pending_bytes: Rc<Cell<usize>>,
    log_sequence: Rc<Cell<u64>>,
    packet_spy: Rc<RefCell<packet_spy::PacketSpy>>,
    bindables: model::BindableRegistry,
    log_registry: model::LogRegistry,
    network: Option<network::Broker>,
    interactive: bool,
    quiet: bool,
}

impl Host {
    fn new() -> Self {
        Self { messages: Default::default(), logs: Default::default(), clock: Default::default(), emitted: Default::default(), pending_bytes: Default::default(), log_sequence: Default::default(), packet_spy: Default::default(), bindables: Default::default(), log_registry: Default::default(), network: None, interactive: false, quiet: false }
    }

    fn interactive() -> Self { Self { interactive: true, network: Some(network::Broker::new()), ..Self::new() } }

    fn realm(&self, cfg: &RealmConfig, scenario: &Scenario) -> LuaResult<Realm> {
        let lua = Lua::new();
        let native_mode=scenario.engine=="native";
        if native_mode {initialize_internals(&lua)?;}
        if let Some(network) = &self.network { network.attach(&lua, &cfg.id); }
        self.log_registry.attach(&lua,&cfg.side,cfg.client_id.as_deref().unwrap_or(""));
        lua.set_app_data(self.bindables.clone());
        if self.interactive { lua.set_app_data(console::Capture::default()); inspector::install(&lua)?; }
        native::install(&lua)?;
        native::set_executor_profile(&lua,&cfg.executor_profile)?;
        lua.set_memory_limit(scenario.memory_mb * 1024 * 1024)?;
        let deadline = Rc::new(Cell::new(Instant::now() + Duration::from_millis(scenario.cpu_ms)));
        let guard = deadline.clone();
        let wall_time_exceeded=Rc::new(Cell::new(false));
        let exceeded=wall_time_exceeded.clone();
        let budget_clock=BudgetClock::new();
        lua.set_interrupt(move |_| {
            if budget_clock.expired(guard.get()) { exceeded.set(true); Err(LuaError::runtime("BudgetExceeded: Luau wall-time budget")) }
            else { Ok(VmState::Continue) }
        });
        let globals = lua.globals();
        let mut config = json!({"realmId":cfg.id,"side":cfg.side,"executor":cfg.executor,"maxResumes":10000,"maxTasks":10000,"signalBehavior":scenario.signal_behavior});
        if let Some(id) = &cfg.client_id { config["clientId"] = json!(id); }
        if let Some(id) = &cfg.actor_id { config["actorId"] = json!(id); }
        let config = lua.to_value(&config)?;
        set_internal(&lua,"__config",config)?;
        let logs = self.logs.clone();
        let clock = self.clock.clone();
        let realm_id = cfg.id.clone();
        let log_sequence = self.log_sequence.clone();
        let interactive = self.interactive;
        let quiet = self.quiet;
        let side = cfg.side.clone();
        let logger=Rc::new(move |lua:&Lua, level:&str, message:mlua::LuaString| -> LuaResult<()> {
            let arguments = console::take(lua);
            if logs.borrow().len() >= 4000 {
                if interactive { logs.borrow_mut().drain(..2000); }
                else { return Err(LuaError::runtime("BudgetExceeded: console log limit")); }
            }
            let text = console::display_bytes(&message.as_bytes(), 4096);
            if !interactive && !quiet { println!("[{:.3}][{}][{}] {}", clock.get(), realm_id, level, text); }
            log_sequence.set(log_sequence.get().saturating_add(1));
            let context = if native_mode && scheduler::current_origin(lua)=="executor" { "executor" } else { side.as_str() };
            let mut record = json!({"seq":log_sequence.get(),"time":clock.get(),"realm":realm_id,"context":context,"level":level,"message":text});
            if let Some((source, line)) = diagnostics::caller(lua) {
                record["source"] = json!(source);
                record["sourceLine"] = json!(line);
            }
            if let Some(arguments) = arguments { record["arguments"] = json!(arguments); }
            logs.borrow_mut().push(record);
            if native_mode { model::log(lua,level,&message.as_bytes())?; }
            Ok(())
        });
        if native_mode { lua.set_app_data(HostLogSink(logger)); }
        else { globals.set("__host_log",lua.create_function(move |lua,(level,message):(String,mlua::LuaString)|logger(lua,&level,message))?)?; }
        let messages = self.messages.clone();
        let emitted = self.emitted.clone();
        let pending_bytes = self.pending_bytes.clone();
        let packet_spy = self.packet_spy.clone();
        let clock = self.clock.clone();
        let realm_id = cfg.id.clone();
        let latency = scenario.latency;
        let emit=Rc::new(move |lua:&Lua,kind:String,payload:Json| -> LuaResult<()> {
            let bytes = serde_json::to_vec(&payload).map_err(LuaError::external)?.len();
            if bytes > 1024 * 1024 {
                return Err(LuaError::runtime("BudgetExceeded: message size"));
            }
            if (!interactive && emitted.get() >= 20000) || messages.borrow().len() >= 20000 { return Err(LuaError::runtime("BudgetExceeded: message count")); }
            if pending_bytes.get().saturating_add(bytes) > 32 * 1024 * 1024 { return Err(LuaError::runtime("BudgetExceeded: pending message bytes")); }
            emitted.set(emitted.get().saturating_add(1));
            pending_bytes.set(pending_bytes.get() + bytes);
            let delay = if kind.starts_with("remote_") { latency } else { 0.0 };
            // Source metadata stays host-side and is captured only while the
            // sender's VM is being inspected. It is never sent over the wire.
            let origin=if matches!(kind.as_str(),"remote_event"|"remote_call") && packet_spy.borrow().watching(&realm_id) {diagnostics::spy_caller(lua)} else {None};
            messages.borrow_mut().push_back(Message { from: realm_id.clone(), kind, payload, at: clock.get() + delay, bytes, origin });
            Ok(())
        });
        if native_mode {
            lua.set_app_data(model::MessageSink(emit));
        } else {
            globals.set("__host_emit", lua.create_function(move |lua, (kind, payload): (String, Value)| emit(lua,kind,lua.from_value(payload)?))?)?;
            globals.set("__host_compile", lua.create_function(|lua, (source, name, environment): (String, String, Table)| {
                if source.len() > 8 * 1024 * 1024 { return Err(LuaError::runtime("BudgetExceeded: source size")); }
                lua.load(&source).set_mode(mlua::chunk::ChunkMode::Text).set_name(name).set_environment(environment).into_function()
            })?)?;
            globals.set("__host_sha256", lua.create_function(|lua, (source, raw): (mlua::LuaString, Option<bool>)| {
                let hash = Sha256::digest(source.as_bytes());
                if raw.unwrap_or(false) { lua.create_string(hash) }
                else { lua.create_string(hash.iter().map(|b| format!("{b:02x}")).collect::<String>()) }
            })?)?;
            globals.set("__host_json_encode", model::json_encode_function(&lua)?)?;
            globals.set("__host_json_decode", lua.create_function(|lua, value: String| {
                if value.len() > 1024 * 1024 { return Err(LuaError::runtime("BudgetExceeded: JSON size")); }
                let value: Json = serde_json::from_str(&value).map_err(LuaError::external)?;
                lua.to_value(&value)
            })?)?;
            globals.set("__host_gc_collect", lua.create_function(|lua, ()| { lua.gc_collect()?; lua.gc_collect() })?)?;
        }
        if scenario.engine == "native" {
            scheduler::install_native_host(&lua)?;
            types::install(&lua)?;
            model::install_native_host(&lua)?;
            executor::install(&lua)?;
        } else {
            lua.load(include_str!("../runtime/scheduler.luau")).set_name("@emulator/scheduler").exec()?;
            lua.load(include_str!("../runtime/types.luau")).set_name("@emulator/types").exec()?;
            lua.load(include_str!("../runtime/datamodel.luau")).set_name("@emulator/datamodel").exec()?;
            lua.load(include_str!("../runtime/executor.luau")).set_name("@emulator/executor").exec()?;
        }
        if scenario.engine == "native" { hide_internals(&lua)?; }
        if scenario.trace_errors { diagnostics::install(&lua)?; }
        Ok(Realm { config: cfg.clone(), lua, deadline, wall_time_exceeded, native: scenario.engine == "native" })
    }
}

impl Realm {
    fn arm(&self, cpu_ms: u64) { self.deadline.set(Instant::now() + Duration::from_millis(cpu_ms)); }
    fn model(&self) -> LuaResult<Table> { internal(&self.lua,"__model") }
    fn get_instance(&self,id:&str)->LuaResult<Option<mlua::AnyUserData>> {
        if self.native {model::get_instance(&self.lua,id)} else {self.model()?.get::<Function>("get")?.call(id)}
    }
    fn script_environment(&self,script:mlua::AnyUserData)->LuaResult<Table> {
        if self.native {model::env(&self.lua,&script.borrow::<instance::Instance>()?.0)}
        else {self.model()?.get::<Function>("getEnvironment")?.call(script)}
    }
    fn call_model<A: mlua::IntoLuaMulti, R: mlua::FromLuaMulti>(&self, name: &str, args: A) -> LuaResult<R> {
        self.model()?.get::<Function>(name)?.call(args)
    }
    fn seed(&self, nodes: &[Json]) -> LuaResult<()> { if self.native { model::seed(&self.lua,nodes) } else {self.call_model("seed", self.lua.to_value(nodes)?)} }
    fn changes(&self) -> LuaResult<Vec<Json>> {
        if self.native { return model::export_changes(&self.lua); }
        let value: Value = self.call_model("exportChanges", ())?;
        self.lua.from_value(value)
    }
    fn apply(&self, changes: &[Json]) -> LuaResult<()> { if self.native {model::apply_changes(&self.lua,changes)} else {self.call_model("applyChanges", self.lua.to_value(changes)?)} }
    fn step(&self, now: f64) -> LuaResult<()> {
        if self.native { scheduler::step(&self.lua,now)?; return Ok(()); }
        internal::<Table>(&self.lua,"__rt")?.get::<Function>("step")?.call::<Value>(now)?;
        Ok(())
    }
    fn deliver(&self, kind: &str, payload: &Json) -> LuaResult<()> {
        if self.native { return model::deliver(&self.lua,kind,payload.clone()); }
        self.call_model("deliver", (kind, self.lua.to_value(payload)?))
    }
    fn run_script(&self, id: &str) -> LuaResult<()> { if self.native {model::run_script(&self.lua,id)?;}else{self.call_model::<_,Value>("runScript",id)?;}Ok(()) }
    fn frame(&self, dt:f64)->LuaResult<()> {if self.native {model::frame(&self.lua,dt)}else{self.call_model("frame",dt)}}
}

fn node_id(node: &Json) -> &str { node.get("id").and_then(Json::as_str).unwrap_or("") }

fn visible(node: &Json, nodes: &HashMap<String, Json>) -> bool {
    let mut id = node_id(node);
    for _ in 0..100 {
        if matches!(id, "ServerStorage" | "ServerScriptService") { return false; }
        let Some(n) = nodes.get(id) else { return true };
        let Some(parent) = n.get("parent").and_then(Json::as_str) else { return true };
        id = parent;
    }
    false
}

fn table_field<'a>(table: &'a Json, key: &str) -> Option<&'a Json> {
    if table["tag"]=="table" {
        table["entries"].as_array()?.iter().find(|entry|entry["key"]["value"]==key).map(|entry|&entry["value"])
    } else { table.get(key) }
}
fn json_text(value: &Json) -> Option<&str> {
    value.as_str().or_else(||(value["tag"]=="string").then(||value["value"].as_str()).flatten())
}

// Only scenarios that model Roblox's automatic character scripts opt in.
// An explicitly supplied Animate always wins over the headless default.
fn prepare_default_animations(scenario: &mut Scenario) {
    if !scenario.default_animate { return; }
    let mut used:std::collections::HashSet<String>=scenario.nodes.iter().map(|node|node_id(node).to_owned()).collect();
    let mut characters=std::collections::HashSet::new();
    let mut targets=Vec::new();
    for (index, realm) in scenario.realms.iter().enumerate() {
        if realm.side!="client" || realm.actor_id.is_some() { continue; }
        let Some(player_id)=realm.client_id.as_deref() else { continue; };
        let Some(player)=scenario.nodes.iter().find(|node|node_id(node)==player_id && node["class"]=="Player") else { continue; };
        let Some(character)=table_field(&player["properties"],"Character")
            .filter(|value|value["tag"]=="instance").and_then(|value|value["id"].as_str()) else { continue; };
        if !scenario.nodes.iter().any(|node|node_id(node)==character && node["class"]=="Model" && node["destroyed"]!=true)
            || !characters.insert(character.to_owned()) { continue; }
        let Some(humanoid)=scenario.nodes.iter().find(|node|node["parent"]==character && node["class"]=="Humanoid" && node["destroyed"]!=true)
            .map(node_id) else { continue; };
        let needs_animator=!scenario.nodes.iter().any(|node|node["parent"]==humanoid && node["class"]=="Animator" && node["destroyed"]!=true);
        let needs_script=!scenario.nodes.iter().any(|node|node["parent"]==character && node["name"]=="Animate" && node["destroyed"]!=true);
        targets.push((index,character.to_owned(),humanoid.to_owned(),needs_animator,needs_script));
    }
    let mut serial=0usize;
    let mut fresh=||loop {
        serial+=1;let id=format!("__default_animate_{serial}");
        if used.insert(id.clone()) {break id}
    };
    for (realm,character,humanoid,needs_animator,needs_script) in targets {
        // Animator must originate in the shared model for client tracks to replicate.
        if needs_animator {
            scenario.nodes.push(json!({"id":fresh(),"class":"Animator","name":"Animator","parent":humanoid}));
        }
        if !needs_script {continue}
        let id=fresh();
        scenario.nodes.push(json!({"id":id,"class":"LocalScript","name":"Animate","parent":character,
            "source":include_str!("../runtime/character_animate.luau")}));
        scenario.realms[realm].entry.insert(0,id.clone());
        for (group, clips) in [
            ("idle", &[("Animation1","507766666"),("Animation2","507766951")][..]),
            ("walk", &[("WalkAnim","507777826")]), ("run", &[("RunAnim","507767714")]),
            ("jump", &[("JumpAnim","507765000")]), ("fall", &[("FallAnim","507767968")]),
            ("climb", &[("ClimbAnim","507765644")]), ("swim", &[("Swim","507784897")]),
            ("swimidle", &[("SwimIdle","507785072")]), ("sit", &[("SitAnim","2506281703")]),
        ] {
            let folder=fresh();
            scenario.nodes.push(json!({"id":folder,"class":"Folder","name":group,"parent":id}));
            for (name,asset) in clips {
                scenario.nodes.push(json!({"id":fresh(),"class":"Animation","name":name,"parent":folder,
                    "properties":{"AnimationId":format!("rbxassetid://{asset}")}}));
            }
        }
    }
}
fn client_script_source(node: &Json, scenario: &Scenario) -> bool {
    if node["class"]!="Script" {return true}
    if scenario.realms.iter().any(|realm|realm.side=="client" && realm.entry.iter().any(|id|id==node_id(node))) {return true}
    let Some(context)=table_field(&node["properties"],"RunContext") else{return false};
    if json_text(context).is_some_and(|value|matches!(value,"Client"|"Enum.RunContext.Client")) {return true}
    if context.as_i64()==Some(2) || (context["tag"]=="number" && context["value"].as_i64()==Some(2)) {return true}
    table_field(context,"enum").and_then(json_text)==Some("RunContext") && table_field(context,"name").and_then(json_text)==Some("Client")
}
fn include_source_fields(node: &mut Json) {
    if let Some(changed)=node.get_mut("$changed").and_then(Json::as_object_mut) {
        let fields=changed.entry("fields").or_insert_with(||json!([]));
        if let Some(fields)=fields.as_array_mut(){for name in ["source","sourceInitialized","linkedSource"]{if !fields.iter().any(|field|field==name){fields.push(json!(name));}}}
    }
}
fn strip_client_source(node: &mut Json) {
    if let Some(fields)=node.as_object_mut() {
        fields.insert("source".into(),Json::Null);
        fields.insert("sourceInitialized".into(),Json::Bool(false));
        fields.insert("linkedSource".into(),Json::Null);
        for key in ["sourcePath","sourceInsertions","sourcePrependPaths","sourcePrefix","sourceSuffix"]{fields.remove(key);}
    }
    include_source_fields(node);
}
fn redact_client_source(node: &mut Json, scenario: &Scenario) {
    if !client_script_source(node,scenario) {strip_client_source(node);}
}

fn same_machine(a: &RealmConfig, b: &RealmConfig) -> bool {
    a.side == b.side && a.client_id == b.client_id
}

fn sync_realms(realms: &[Realm], scenario: &Scenario, server_nodes: &mut HashMap<String, Json>, known: &mut [HashMap<String, Json>]) -> LuaResult<()> {
    // Capture every writer before applying a peer's changes to its live model.
    let batches:Vec<_>=realms.iter().map(|realm|{realm.arm(scenario.cpu_ms);realm.changes()}).collect::<LuaResult<_>>()?;
    for (i, from) in realms.iter().enumerate() {
        let changes = &batches[i];
        if changes.is_empty() { continue; }
        if from.config.side == "server" {
            for n in changes { let id=node_id(n);let merged=model::merge_replica(server_nodes.get(id),n);server_nodes.insert(id.to_owned(),merged); }
        }
        for (j, to) in realms.iter().enumerate() {
            // Replay native deltas on the writer too: all copies resolve competing
            // writes to the same key in the same deterministic realm order.
            if i == j && !from.native { continue; }
            let same = same_machine(&from.config, &to.config);
            if !same && !(from.config.side == "server" && to.config.side == "client") { continue; }
            let mut outgoing = Vec::new();
            for n in changes {
                let id = node_id(n);
                if same || visible(n, server_nodes) {
                    let previous=known[j].get(id);
                    let mut merged=model::merge_replica(previous,n);
                    let canonical=server_nodes.get(id).unwrap_or(&merged);
                    let mut delta=if previous.is_none(){
                        if from.config.side=="server"{canonical.clone()}else{merged.clone()}
                    }else{n.clone()};
                    if to.config.side=="client" {
                        if !client_script_source(canonical,scenario) {
                            strip_client_source(&mut delta);
                            strip_client_source(&mut merged);
                        } else if previous.is_some_and(|old|old["source"].is_null()) && canonical["source"].is_string() {
                            let source=canonical["source"].clone();let initialized=json!(canonical["sourceInitialized"].as_bool().unwrap_or(true));let linked=canonical["linkedSource"].clone();
                            delta["source"]=source.clone();delta["sourceInitialized"]=initialized.clone();delta["linkedSource"]=linked.clone();
                            merged["source"]=source;merged["sourceInitialized"]=initialized;merged["linkedSource"]=linked;
                            include_source_fields(&mut delta);
                        }
                    }
                    outgoing.push(delta);
                    if n.get("destroyed").and_then(Json::as_bool) == Some(true) { known[j].remove(id); }
                    else { known[j].insert(id.to_owned(), merged); }
                } else if known[j].remove(id).is_some() {
                    outgoing.push(json!({"id":id,"destroyed":true}));
                }
            }
            if !outgoing.is_empty() { to.arm(scenario.cpu_ms); to.apply(&outgoing)?; }
        }
    }
    Ok(())
}

fn route(host: &Host, realms: &[Realm], scenario: &Scenario) -> LuaResult<()> {
    // Broadcast engine log events only to other VMs in the same process. The
    // group already shares history; delivering a signal must not log again.
    let count=host.log_registry.pending();
    for _ in 0..count {
        let Some(mut item)=host.log_registry.pop() else{break};
        let Some(from)=realms.iter().find(|realm|realm.config.id==item.from) else{continue};
        while item.next<realms.len() {
            let index=item.next;item.next+=1;let to=&realms[index];
            if to.config.id==item.from || !same_machine(&from.config,&to.config){continue}
            to.arm(scenario.cpu_ms);scheduler::with_network_root(&to.lua,||model::deliver_log(&to.lua,&item))?;
            if host.network.as_ref().is_some_and(|network|network.has_pending()) {
                if item.next<realms.len(){host.log_registry.restore(item);}
                return Ok(())
            }
        }
    }
    let count = host.messages.borrow().len();
    for _ in 0..count {
        let Some(mut msg) = host.messages.borrow_mut().pop_front() else { break };
        if msg.at > host.clock.get() { host.messages.borrow_mut().push_back(msg); continue; }
        host.pending_bytes.set(host.pending_bytes.get().saturating_sub(msg.bytes));
        let Some(from) = realms.iter().find(|r| r.config.id == msg.from) else { continue };
        msg.payload["sourceRealm"] = json!(msg.from);
        msg.payload["clientId"] = json!(from.config.client_id);
        let mut target = msg.payload.get("target").and_then(Json::as_str).unwrap_or("").to_owned();
        if msg.kind=="bindable_call" {
            if !model::bindable_request_alive(&from.lua,msg.payload["requestId"].as_str().unwrap_or(""))?{continue;}
            let id=msg.payload.get("remoteId").and_then(Json::as_str).unwrap_or("");
            let Some(owner)=model::bindable_owner(&from.lua,id)?else{
                host.pending_bytes.set(host.pending_bytes.get()+msg.bytes);host.messages.borrow_mut().push_back(msg);continue;
            };
            target=owner;
        }
        if msg.kind=="animation" {
            let Some(server)=realms.iter().find(|realm|realm.config.side=="server")else{continue};
            if !model::animation_authorized(&server.lua,&msg.payload,if from.config.side=="client"{from.config.client_id.as_deref()}else{None})?{continue;}
        }
        let remote_id=msg.payload.get("remoteId").and_then(Json::as_str).unwrap_or("");
        let outgoing_blocked=msg.kind=="remote_event" && host.packet_spy.borrow().blocked(&from.lua,&from.config.id,"outgoing",remote_id);
        if matches!(msg.kind.as_str(),"remote_event"|"remote_call"|"remote_return") && host.packet_spy.borrow().watching(&from.config.id) {
            host.packet_spy.borrow_mut().observe(&from.lua,&from.config.id,&from.config.id,&from.config.side,&msg.kind,&target,&msg.payload,host.clock.get(),"outgoing",outgoing_blocked,msg.origin.as_ref());
        }
        if outgoing_blocked {continue;}
        for to in realms {
            let selected = match msg.kind.as_str() {
                "animation" => to.config.id!=from.config.id,
                "actor_message" => same_machine(&from.config, &to.config) && to.config.actor_id.as_deref() == msg.payload.get("actorId").and_then(Json::as_str),
                "bindable_handler" => same_machine(&from.config,&to.config),
                "bindable_event" => from.config.id!=to.config.id && same_machine(&from.config,&to.config) && msg.payload["targets"].as_array().is_some_and(|targets|targets.iter().any(|target|target.as_str()==Some(to.config.id.as_str()))),
                "bindable_call" | "bindable_return" => to.config.id==target && same_machine(&from.config,&to.config),
                "remote_event" | "remote_call" => {
                    if target == "server" { from.config.side == "client" && to.config.side == "server" }
                    else { from.config.side == "server" && to.config.side == "client" && (target == "*" || to.config.client_id.as_deref() == Some(target.as_str())) }
                }
                "remote_return" => to.config.id == target,
                _ => return Err(LuaError::runtime(format!("UnsupportedMessage: {}", msg.kind))),
            };
            if selected {
                let incoming_blocked=msg.kind=="remote_event" && host.packet_spy.borrow().blocked(&to.lua,&to.config.id,"incoming",remote_id);
                if matches!(msg.kind.as_str(),"remote_event"|"remote_call"|"remote_return")
                    && host.packet_spy.borrow().watching(&to.config.id)
                    && model::remote_spy_deliverable(&to.lua,&msg.kind,&msg.payload) {
                    host.packet_spy.borrow_mut().observe(&to.lua,&to.config.id,&from.config.id,&from.config.side,&msg.kind,&target,&msg.payload,host.clock.get(),"incoming",incoming_blocked,None);
                }
                if incoming_blocked {continue;}
                to.arm(scenario.cpu_ms); to.deliver(&msg.kind, &msg.payload)?;
            }
        }
    }
    Ok(())
}

const MAX_ASSEMBLED_SOURCE:usize=8*1024*1024;
const MAX_SCENARIO_SOURCES:usize=32*1024*1024;
fn read_bounded(path:&Path,limit:usize)->Result<Vec<u8>,Box<dyn std::error::Error>> {
    let mut bytes=Vec::new();
    fs::File::open(path)?.take(limit as u64+1).read_to_end(&mut bytes)?;
    if bytes.len()>limit {return Err(format!("File exceeds size limit: {}",path.display()).into());}
    Ok(bytes)
}
fn read_source(path:&Path, files:&mut Vec<Json>) -> Result<String,Box<dyn std::error::Error>> {
    let bytes=read_bounded(path,MAX_ASSEMBLED_SOURCE)?;
    files.push(json!({"path":path.to_string_lossy(),"bytes":bytes.len(),"sha256":format!("{:x}",Sha256::digest(&bytes))}));
    Ok(String::from_utf8(bytes)?.replace("\r\n","\n"))
}

fn load_nodes(nodes: &mut [Json], base: &Path) -> Result<Vec<Json>, Box<dyn std::error::Error>> {
    let mut snapshots=Vec::new();
    let mut source_bytes=0usize;
    for node in nodes {
        let mut files=Vec::new();
        if let Some(path) = node.get("sourcePath").and_then(Json::as_str) {
            let path = base.join(path);
            let mut source = read_source(&path,&mut files)?;
            if let Some(insertions) = node.get("sourceInsertions").and_then(Json::as_array) {
                for insertion in insertions {
                    let marker = insertion.get("after").and_then(Json::as_str).filter(|s| !s.is_empty()).ok_or("sourceInsertions requires nonempty after")?;
                    let piece = insertion.get("path").and_then(Json::as_str).ok_or("sourceInsertions requires path")?;
                    if source.matches(marker).count() != 1 { return Err(format!("Source insertion marker must occur exactly once: {}", path.display()).into()); }
                    let piece = read_source(&base.join(piece),&mut files)?;
                    if source.len().saturating_add(piece.len()).saturating_add(2)>MAX_ASSEMBLED_SOURCE {return Err("Assembled source exceeds 8 MiB".into());}
                    source = source.replacen(marker, &format!("{marker}\n{piece}\n"), 1);
                }
            }
            if let Some(paths) = node.get("sourcePrependPaths").and_then(Json::as_array) {
                let mut prelude = String::new();
                for piece in paths {
                    let piece=read_source(&base.join(piece.as_str().ok_or("sourcePrependPaths must contain strings")?),&mut files)?;
                    if prelude.len().saturating_add(piece.len()).saturating_add(source.len()).saturating_add(1)>MAX_ASSEMBLED_SOURCE {return Err("Assembled source exceeds 8 MiB".into());}
                    prelude.push_str(&piece);prelude.push('\n');
                }
                prelude.push_str(&source); source = prelude;
            }
            let prefix = node.get("sourcePrefix").and_then(Json::as_str).unwrap_or("");
            let suffix = node.get("sourceSuffix").and_then(Json::as_str).unwrap_or("");
            if prefix.len().saturating_add(source.len()).saturating_add(suffix.len()).saturating_add(2)>MAX_ASSEMBLED_SOURCE {return Err("Assembled source exceeds 8 MiB".into());}
            node["source"] = json!(format!("{prefix}\n{source}\n{suffix}"));
        }
        if node_id(node).is_empty() { return Err("Every node needs a nonempty id".into()); }
        if let Some(source)=node.get("source").and_then(Json::as_str) {
            if source.len()>MAX_ASSEMBLED_SOURCE {return Err("Assembled source exceeds 8 MiB".into());}
            source_bytes=source_bytes.saturating_add(source.len());
            if source_bytes>MAX_SCENARIO_SOURCES {return Err("Scenario sources exceed 32 MiB".into());}
            snapshots.push(json!({"nodeId":node_id(node),"assembledBytes":source.len(),"assembledSha256":format!("{:x}",Sha256::digest(source.as_bytes())),"files":files}));
        }
    }
    Ok(snapshots)
}

fn run_scenario(mut scenario: Scenario, source_snapshots: Vec<Json>) -> Result<Json, Box<dyn std::error::Error>> {
    if !matches!(scenario.engine.as_str(), "native"|"behavioral") { return Err("engine must be native or behavioral".into()); }
    if scenario.default_animate && scenario.engine!="native" {return Err("defaultAnimate requires the native engine".into());}
    prepare_default_animations(&mut scenario);
    if !matches!(scenario.signal_behavior.as_str(), "immediate"|"deferred") { return Err("signalBehavior must be immediate or deferred".into()); }
    if !scenario.duration.is_finite() || !(0.0..=3600.0).contains(&scenario.duration) || !scenario.step.is_finite() || !(0.001..=1.0).contains(&scenario.step) || !scenario.latency.is_finite() || !(0.0..=60.0).contains(&scenario.latency) || !(8..=512).contains(&scenario.memory_mb) || !(10..=10000).contains(&scenario.cpu_ms) || scenario.realms.is_empty() || scenario.realms.len()>16 {
        return Err("Invalid scenario limits".into());
    }
    let mut ids = std::collections::HashSet::new();
    for r in &scenario.realms {
        if !ids.insert(r.id.clone()) || !matches!(r.side.as_str(), "server"|"client") || (r.side == "client" && r.client_id.is_none()) { return Err("Invalid/duplicate realm identity".into()); }
        if !matches!(r.executor_profile.as_str(),"generic"|"unc"|"macsploit-744.2"|"macsploit-744.4") {return Err("Unknown executor profile".into());}
        if scenario.engine!="native" && r.executor_profile!="generic" {return Err("Executor compatibility profiles require the native engine".into());}
    }
    let mut error_rules = std::collections::HashSet::new();
    for rule in &scenario.expected_errors {
        if !ids.contains(&rule.realm) || !(1..=1000).contains(&rule.max_count)
            || !error_rules.insert((rule.realm.as_str(),rule.message.as_str())) {return Err("Invalid/duplicate expected error rule".into());}
    }
    let host = Host { quiet:true, ..Host::new() };
    let mut realms = Vec::new();
    let mut server_nodes: HashMap<_,_> = scenario.nodes.iter().map(|n|(node_id(n).to_owned(),n.clone())).collect();
    if server_nodes.len() != scenario.nodes.len() { return Err("Duplicate node IDs".into()); }
    let mut known = Vec::new();
    for cfg in &scenario.realms {
        let realm = host.realm(cfg, &scenario)?;
        let nodes: Vec<_> = scenario.nodes.iter().filter(|n| cfg.side=="server" || visible(n,&server_nodes)).cloned().map(|mut node|{
            if cfg.side=="client" {redact_client_source(&mut node,&scenario);}node
        }).collect();
        realm.seed(&nodes)?;
        realm.changes()?;
        known.push(nodes.iter().map(|n|(node_id(n).to_owned(),n.clone())).collect());
        realms.push(realm);
    }
    for realm in &realms {
        realm.arm(scenario.cpu_ms);
        for id in &realm.config.entry { realm.run_script(id)?; }
        for id in &realm.config.executor_entry {
            let node = scenario.nodes.iter().find(|n| node_id(n)==id).ok_or("Executor entry node missing")?;
            let source = node.get("source").and_then(Json::as_str).ok_or("Executor source missing")?;
            if realm.native { executor::run(&realm.lua,source,&format!("@executor/{id}"))?; }
            else { internal::<Table>(&realm.lua,"__emu")?.get::<Function>("runExecutor")?.call::<Value>((source,format!("@executor/{id}")))?; }
        }
    }
    let steps = (scenario.duration/scenario.step).ceil() as usize;
    for tick in 0..=steps {
        let now = (tick as f64 * scenario.step).min(scenario.duration);
        host.clock.set(now);
        sync_realms(&realms, &scenario, &mut server_nodes, &mut known)?;
        route(&host, &realms, &scenario)?;
        for realm in &realms { realm.arm(scenario.cpu_ms); realm.step(now)?; realm.frame(scenario.step)?; }
    }
    let mut statuses = Vec::new();
    let mut scheduler_errors = Vec::new();
    let mut failed = false;
    for realm in &realms {
        realm.arm(scenario.cpu_ms);
        let status: Json = if realm.native { serde_json::to_value(scheduler::status(&realm.lua)?)? } else {
            let status: Value = internal::<Table>(&realm.lua,"__rt")?.get::<Function>("status")?.call(())?;
            realm.lua.from_value(status)?
        };
        let errors = status.get("errors").and_then(Json::as_u64).unwrap_or_else(||status.get("errors").and_then(Json::as_array).map_or(0,|x|x.len() as u64));
        scheduler_errors.push((realm.config.id.clone(),errors));
        let unsupported = if realm.native { model::unsupported_calls(&realm.lua)? } else {Default::default()};
        if !unsupported.is_empty() { failed=true; }
        let unsupported_executor = if realm.native { executor::unsupported_calls(&realm.lua)? } else {Default::default()};
        if !unsupported_executor.is_empty() { failed=true; }
        let trace=if scenario.trace_errors {diagnostics::snapshot(&realm.lua)?} else {Json::Null};
        if realm.wall_time_exceeded.get() {failed=true;}
        let profiles = if realm.native {
            realm.lua.from_value::<Json>(Value::Table(scheduler::profile_status(&realm.lua)?))?
        } else { Json::Null };
        let memory_categories=if realm.native {
            realm.lua.from_value::<Json>(Value::Table(scheduler::memory_status(&realm.lua)?))?
        } else {Json::Null};
        statuses.push(json!({"realm":realm.config.id,"executorProfile":realm.config.executor_profile,"memoryBytes":realm.lua.used_memory(),"hostLimits":{"wallTimeExceeded":realm.wall_time_exceeded.get()},"scheduler":status,"unsupportedApiCalls":unsupported,"unsupportedExecutorApiCalls":unsupported_executor,"protectedErrors":trace,"profiles":profiles,"memoryCategories":memory_categories}));
    }
    let logs = host.logs.borrow();
    let mut expected_counts=vec![0usize;scenario.expected_errors.len()];
    let mut accepted_by_realm:HashMap<&str,u64>=HashMap::new();
    for entry in logs.iter().filter(|entry|entry["level"].as_str().is_some_and(|level|level.eq_ignore_ascii_case("error"))) {
        let matched=scenario.expected_errors.iter().enumerate().find(|(index,rule)|{
            expected_counts[*index]<rule.max_count && entry["realm"].as_str()==Some(rule.realm.as_str()) && entry["message"].as_str()==Some(rule.message.as_str())
        });
        if let Some((index,rule))=matched {expected_counts[index]+=1;*accepted_by_realm.entry(rule.realm.as_str()).or_default()+=1;}
        else {failed=true;}
    }
    failed |= scheduler_errors.iter().any(|(realm,count)|*count>accepted_by_realm.get(realm.as_str()).copied().unwrap_or(0));
    let expected_errors:Vec<_>=scenario.expected_errors.iter().zip(&expected_counts).map(|(rule,count)|json!({"realm":rule.realm,"message":rule.message,"maxCount":rule.max_count,"observed":count})).collect();
    let missing: Vec<_> = scenario.expect.iter().filter(|expected| !logs.iter().any(|l|l["message"].as_str().is_some_and(|s|s.contains(expected.as_str())))).collect();
    failed |= !missing.is_empty();
    let rejected:Vec<_>=scenario.reject.iter().filter(|pattern|logs.iter().any(|l|l["message"].as_str().is_some_and(|s|s.contains(pattern.as_str())))).collect();
    failed |= !rejected.is_empty();
    let report = json!({"name":scenario.name,"emulatorVersion":env!("CARGO_PKG_VERSION"),"engine":scenario.engine,"sourceSnapshots":source_snapshots,"passed":!failed,"duration":scenario.duration,"messages":host.emitted.get(),"pendingMessages":host.messages.borrow().len(),"missingExpectations":missing,"rejectedMessages":rejected,"expectedErrors":expected_errors,"realms":statuses,"logs":*logs,"proof":"Offline emulator scenario only; not Roblox equivalence or a complete anti-cheat verdict."});
    Ok(report)
}

fn run(path: &Path, report_path: Option<&Path>) -> Result<bool, Box<dyn std::error::Error>> {
    let mut scenario:Scenario=serde_json::from_slice(&read_bounded(path,16*1024*1024)?)?;
    session::restrict_source_paths(&mut scenario.nodes,path).map_err(|error|error.to_string())?;
    let snapshots=load_nodes(&mut scenario.nodes,path.parent().unwrap_or(Path::new(".")))?;
    for node in &mut scenario.nodes {
        if let Some(fields)=node.as_object_mut() {
            for key in ["sourcePath","sourceInsertions","sourcePrependPaths","sourcePrefix","sourceSuffix"] {fields.remove(key);}
        }
    }
    println!("{} MODE: offline scenario; only measured API comparisons establish Roblox parity.",scenario.engine.to_uppercase());
    let report=remote_session::run_batch(scenario,snapshots).map_err(io_error)?;
    if let Some(path)=report_path {fs::write(path,serde_json::to_string_pretty(&report)?)?;}
    if let Some(logs)=report["logs"].as_array() {
        for entry in logs {
            println!("[{:.3}][{}][{}] {}",entry["time"].as_f64().unwrap_or(0.0),entry["realm"].as_str().unwrap_or("?"),
                entry["level"].as_str().unwrap_or("?"),entry["message"].as_str().unwrap_or(""));
        }
    }
    if let Some(realms)=report["realms"].as_array() {
        for realm in realms {
            let id=realm["realm"].as_str().unwrap_or("?");
            for (key,label) in [("unsupportedApiCalls","engine"),("unsupportedExecutorApiCalls","executor")] {
                if realm[key].as_object().is_some_and(|entries|!entries.is_empty()) {println!("Unsupported {label} API in {id}: {}",realm[key]);}
            }
            if realm["hostLimits"]["wallTimeExceeded"]==true {println!("BudgetExceeded: Luau wall-time budget in {id}");}
        }
    }
    let passed=report["passed"].as_bool().ok_or("Invalid sandbox report")?;
    println!("{}: {} ({} realms, {} messages, {} log entries)",if passed {"PASS"} else {"FAIL"},report["name"].as_str().unwrap_or("scenario"),
        report["realms"].as_array().map_or(0,Vec::len),report["messages"].as_u64().unwrap_or(0),report["logs"].as_array().map_or(0,Vec::len));
    for (key,label) in [("missingExpectations","Missing expectations"),("rejectedMessages","Rejected messages")] {
        if report[key].as_array().is_some_and(|values|!values.is_empty()) {println!("{label}: {}",report[key]);}
    }
    Ok(passed)
}

fn io_error(message:String)->std::io::Error {std::io::Error::other(message)}

fn main() {
    let args: Vec<_> = env::args_os().skip(1).collect();
    #[cfg(windows)]
    if args.first().is_some_and(|arg|arg=="--websocket-broker") {
        if args.len()!=1 {eprintln!("Invalid WebSocket broker arguments");std::process::exit(2);}
        if let Err(error)=network::websocket_broker_main() {eprintln!("WebSocket broker failed: {error}");std::process::exit(2);}
        return;
    }
    if args.first().is_some_and(|arg|arg=="--isolated-worker") {
        if args.len()!=1 {eprintln!("Invalid worker arguments");std::process::exit(2);}
        #[cfg(windows)]
        if let Err(error)=sandbox_windows::verify_worker_security() {eprintln!("Sandbox worker rejected: {error}");std::process::exit(2);}
        #[cfg(not(windows))]
        {eprintln!("Sandbox worker requires Windows");std::process::exit(2);}
    } else {
        #[cfg(windows)] unsafe {SetConsoleCtrlHandler(Some(console_interrupt),1);}
    }
    for flag in ["LuauFastpcall", "LuauCompileFastpcall", "LuauFastpcallInterrupt"] {
        if Lua::set_fflag(flag, true).is_err() { eprintln!("Required {flag} flag is unavailable"); std::process::exit(2); }
    }
    if args.first().is_some_and(|arg|arg=="--isolated-worker") {
        if let Err(error)=worker::run() {eprintln!("Sandbox worker failed: {error}");std::process::exit(1);}
        return;
    }
    if args.is_empty() || args[0] == "--ui" {
        let mut path = None;
        let mut port = 0;
        let mut open_browser = true;
        let mut options = args.iter().skip(1);
        while let Some(option) = options.next() {
            if option == "--no-browser" { open_browser = false; }
            else if option == "--port" {
                let Some(value) = options.next().and_then(|s|s.to_str()).and_then(|s|s.parse::<u16>().ok()) else {
                    eprintln!("--port requires an integer from 0 to 65535"); std::process::exit(2);
                };
                port = value;
            } else if !option.to_string_lossy().starts_with("--") && path.is_none() { path = Some(PathBuf::from(option)); }
            else { eprintln!("Usage: roblox-emulator --ui [scenario.json] [--port N] [--no-browser]"); std::process::exit(2); }
        }
        if let Err(error) = web::serve(path.as_deref(), port, open_browser) { eprintln!("FAIL: {error}"); std::process::exit(1); }
        return;
    }
    if args.first().is_some_and(|s| s == "--native-probe" || s == "--runtime-probe") {
        let result = if args.len() == 3 { native_probe(Path::new(&args[1]), Path::new(&args[2]),args[0]=="--runtime-probe") } else { Err("Usage: --native-probe|--runtime-probe <probe.luau> <studio-reference.json>".into()) };
        match result { Ok(true) => (), Ok(false) => std::process::exit(1), Err(e) => { eprintln!("FAIL: {e}"); std::process::exit(1); } }
        return;
    }
    if args[0] == "--help" || args[0] == "-h" {
        println!("roblox-emulator --ui [scenario.json] [--port N] [--no-browser]\nroblox-emulator <scenario.json> [--report <report.json>]\nWith no arguments, opens the interactive UI.");
        return;
    }
    let path = PathBuf::from(&args[0]);
    let report = if args.len()==3 && args[1]=="--report" { Some(PathBuf::from(&args[2])) } else if args.len()==1 { None } else { eprintln!("Invalid arguments"); std::process::exit(2); };
    match run(&path, report.as_deref()) {
        Ok(true) => (),
        Ok(false) => std::process::exit(1),
        Err(e) => { eprintln!("FAIL: {e}"); std::process::exit(1); }
    }
}

fn normalize_probe(value: &mut Json) {
    match value {
        Json::Object(map) => {
            if let Some(Json::String(source)) = map.get_mut("source") {
                if source == "AssistantCommand" || source == "NativeParity" { *source = "<probe>".into(); }
            }
            if let Some(Json::String(source)) = map.get_mut("rootSource") { *source = "<probe>".into(); }
            if map.get("stack").is_some_and(|v| v.is_object() && v.as_object().unwrap().is_empty()) { map.insert("stack".into(),json!([])); }
            for v in map.values_mut() { normalize_probe(v); }
        }
        Json::Array(values) => for v in values { normalize_probe(v); },
        Json::String(s) => { *s = s.replace("AssistantCommand:", "<probe>:").replace("NativeParity:", "<probe>:"); },
        _ => (),
    }
}

fn native_probe_data(probe: &str, mut expected: Json, full_runtime: bool) -> Result<Json, Box<dyn std::error::Error>> {
    let lua = Lua::new();
    if full_runtime {initialize_internals(&lua)?;}
    lua.set_memory_limit(64 * 1024 * 1024)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    lua.set_interrupt(move |_| if Instant::now() > deadline { Err(LuaError::runtime("BudgetExceeded: native probe")) } else { Ok(VmState::Continue) });
    native::install(&lua)?;
    types::install(&lua)?;
    if full_runtime {
        set_internal(&lua,"__config",lua.to_value(&json!({"realmId":"probe","side":"server","isStudio":true,"executor":false,"signalBehavior":"deferred"}))?)?;
        set_internal(&lua,"__host_log",lua.create_function(|_,(level,message):(String,mlua::LuaString)|{eprintln!("[probe][{level}] {}",console::display_bytes(&message.as_bytes(),4096));Ok(())})?)?;
        scheduler::install_native_host(&lua)?;
        model::install_native_host(&lua)?;
        hide_internals(&lua)?;
    } else { instance::install(&lua)?; }
    lua.globals().set_safeenv(true);
    let entry = lua.load(probe).set_mode(mlua::chunk::ChunkMode::Text).set_name("=NativeParity").into_function()?;
    let thread=lua.create_thread(entry.clone())?;
    if full_runtime { scheduler::set_script_category(&lua,&thread,&entry,"NativeParity")?; }
    let result: Value = thread.resume(())?;
    let mut actual: Json = lua.from_value(result)?;
    normalize_probe(&mut actual);
    normalize_probe(&mut expected);
    let mut mismatches = Vec::new();
    let mut matches = Vec::new();
    for (name, value) in expected.as_object().ok_or("Invalid reference")? {
        if actual.get(name) != Some(value) { mismatches.push(json!({"check":name,"studio":value,"emulator":actual.get(name)})); }
        else { matches.push(name.clone()); }
    }
    Ok(json!({"nativeParity":mismatches.is_empty(),"matches":matches,"mismatches":mismatches,"note":"Probe chunk names and empty-stack JSON array shape normalized. Stack depth, native names, error text and frame layout compared unchanged. Results cover only the supplied probe and reference."}))
}

fn native_probe(probe: &Path, reference: &Path, full_runtime: bool) -> Result<bool, Box<dyn std::error::Error>> {
    let probe=String::from_utf8(read_bounded(probe,8*1024*1024)?)?;
    let reference=serde_json::from_slice(&read_bounded(reference,8*1024*1024)?)?;
    let result=remote_session::run_probe(probe,reference,full_runtime).map_err(io_error)?;
    if let Some(matches)=result["matches"].as_array() {
        for name in matches {if let Some(name)=name.as_str(){println!("MATCH {name}");}}
    }
    println!("{}",serde_json::to_string_pretty(&result)?);
    result["nativeParity"].as_bool().ok_or_else(||"Invalid sandbox probe result".into())
}
