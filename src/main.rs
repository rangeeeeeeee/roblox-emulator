use mlua::{Error as LuaError, Function, Lua, LuaSerdeExt, Result as LuaResult, Table, Value, VmState};
use serde::Deserialize;
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use std::{cell::{Cell, RefCell}, collections::{HashMap, VecDeque}, env, fs, path::{Path, PathBuf}, rc::Rc, time::{Duration, Instant}};
mod native;
mod instance;
mod types;
mod scheduler;
mod model;
mod executor;
mod diagnostics;
mod session;
mod web;

pub(crate) fn internal<T: mlua::FromLua>(lua:&Lua,name:&str)->LuaResult<T> {
    if let Ok(table)=lua.named_registry_value::<Table>("emulator.internal") { table.get(name) } else {lua.globals().get(name)}
}

fn hide_internals(lua:&Lua)->LuaResult<()> {
    let private=lua.create_table()?;
    let globals=lua.globals();
    let keys=globals.clone().pairs::<Value,Value>().filter_map(|entry|match entry{Ok((Value::String(k),v)) if k.to_string_lossy().starts_with("__")=>Some(Ok((k,v))),Ok(_)=>None,Err(e)=>Some(Err(e))}).collect::<LuaResult<Vec<_>>>()?;
    for (key,value) in keys {private.raw_set(key.clone(),value)?;globals.raw_set(key,Value::Nil)?;}
    lua.set_named_registry_value("emulator.internal",private)?;
    globals.set("_G",lua.create_table()?)?;
    globals.set_safeenv(true);
    Ok(())
}

#[derive(Clone, Deserialize)]
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExpectedError {
    realm: String,
    message: String,
    max_count: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Scenario {
    name: String,
    #[serde(default = "default_engine")] engine: String,
    #[serde(default = "default_signal_behavior")] signal_behavior: String,
    #[serde(default)] trace_errors: bool,
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
struct Message { from: String, kind: String, payload: Json, at: f64, bytes: usize }
struct Realm { config: RealmConfig, lua: Lua, deadline: Rc<Cell<Instant>>, wall_time_exceeded: Rc<Cell<bool>>, native: bool }
struct Host {
    messages: Rc<RefCell<VecDeque<Message>>>,
    logs: Rc<RefCell<Vec<Json>>>,
    clock: Rc<Cell<f64>>,
    emitted: Rc<Cell<usize>>,
    pending_bytes: Rc<Cell<usize>>,
    log_sequence: Rc<Cell<u64>>,
    interactive: bool,
}

impl Host {
    fn new() -> Self {
        Self { messages: Default::default(), logs: Default::default(), clock: Default::default(), emitted: Default::default(), pending_bytes: Default::default(), log_sequence: Default::default(), interactive: false }
    }

    fn interactive() -> Self { Self { interactive: true, ..Self::new() } }

    fn realm(&self, cfg: &RealmConfig, scenario: &Scenario) -> LuaResult<Realm> {
        let lua = Lua::new();
        native::install(&lua)?;
        native::set_executor_profile(&lua,&cfg.executor_profile)?;
        lua.set_memory_limit(scenario.memory_mb * 1024 * 1024)?;
        let deadline = Rc::new(Cell::new(Instant::now() + Duration::from_millis(scenario.cpu_ms)));
        let guard = deadline.clone();
        let wall_time_exceeded=Rc::new(Cell::new(false));
        let exceeded=wall_time_exceeded.clone();
        lua.set_interrupt(move |_| {
            if Instant::now() > guard.get() { exceeded.set(true); Err(LuaError::runtime("BudgetExceeded: Luau wall-time budget")) }
            else { Ok(VmState::Continue) }
        });
        let globals = lua.globals();
        let mut config = json!({"realmId":cfg.id,"side":cfg.side,"executor":cfg.executor,"maxResumes":10000,"maxTasks":10000,"signalBehavior":scenario.signal_behavior});
        if let Some(id) = &cfg.client_id { config["clientId"] = json!(id); }
        if let Some(id) = &cfg.actor_id { config["actorId"] = json!(id); }
        let config = lua.to_value(&config)?;
        globals.set("__config", config)?;
        let logs = self.logs.clone();
        let clock = self.clock.clone();
        let realm_id = cfg.id.clone();
        let native_mode=scenario.engine=="native";
        let log_sequence = self.log_sequence.clone();
        let interactive = self.interactive;
        let side = cfg.side.clone();
        globals.set("__host_log", lua.create_function(move |lua, (level, message): (String, String)| {
            if logs.borrow().len() >= 4000 {
                if interactive { logs.borrow_mut().drain(..2000); }
                else { return Err(LuaError::runtime("BudgetExceeded: console log limit")); }
            }
            let text: String = message.chars().take(4096).collect();
            if !interactive { println!("[{:.3}][{}][{}] {}", clock.get(), realm_id, level, text); }
            log_sequence.set(log_sequence.get().saturating_add(1));
            let context = if native_mode && scheduler::current_origin(lua)=="executor" { "executor" } else { side.as_str() };
            logs.borrow_mut().push(json!({"seq":log_sequence.get(),"time":clock.get(),"realm":realm_id,"context":context,"level":level,"message":text}));
            if native_mode { model::log(lua,&level,&text)?; }
            Ok(())
        })?)?;
        let messages = self.messages.clone();
        let emitted = self.emitted.clone();
        let pending_bytes = self.pending_bytes.clone();
        let clock = self.clock.clone();
        let realm_id = cfg.id.clone();
        let latency = scenario.latency;
        let emit=Rc::new(move |kind:String,payload:Json| -> LuaResult<()> {
            let bytes = serde_json::to_vec(&payload).map_err(LuaError::external)?.len();
            if bytes > 1024 * 1024 {
                return Err(LuaError::runtime("BudgetExceeded: message size"));
            }
            if (!interactive && emitted.get() >= 20000) || messages.borrow().len() >= 20000 { return Err(LuaError::runtime("BudgetExceeded: message count")); }
            if pending_bytes.get().saturating_add(bytes) > 32 * 1024 * 1024 { return Err(LuaError::runtime("BudgetExceeded: pending message bytes")); }
            emitted.set(emitted.get().saturating_add(1));
            pending_bytes.set(pending_bytes.get() + bytes);
            let delay = if kind.starts_with("remote_") { latency } else { 0.0 };
            messages.borrow_mut().push_back(Message { from: realm_id.clone(), kind, payload, at: clock.get() + delay, bytes });
            Ok(())
        });
        if native_mode {lua.set_app_data(model::MessageSink(emit.clone()));}
        globals.set("__host_emit", lua.create_function(move |lua, (kind, payload): (String, Value)| emit(kind,lua.from_value(payload)?))?)?;
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
        if scenario.engine == "native" {
            scheduler::install(&lua)?;
            types::install(&lua)?;
            model::install(&lua)?;
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
                    let merged=model::merge_replica(previous,n);
                    if previous.is_none(){
                        outgoing.push(if from.config.side=="server"{server_nodes.get(id).cloned().unwrap_or_else(||merged.clone())}else{merged.clone()});
                    }else{outgoing.push(n.clone());}
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
    let count = host.messages.borrow().len();
    for _ in 0..count {
        let Some(mut msg) = host.messages.borrow_mut().pop_front() else { break };
        if msg.at > host.clock.get() { host.messages.borrow_mut().push_back(msg); continue; }
        host.pending_bytes.set(host.pending_bytes.get().saturating_sub(msg.bytes));
        let Some(from) = realms.iter().find(|r| r.config.id == msg.from) else { continue };
        msg.payload["sourceRealm"] = json!(msg.from);
        msg.payload["clientId"] = json!(from.config.client_id);
        let target = msg.payload.get("target").and_then(Json::as_str).unwrap_or("").to_owned();
        for to in realms {
            let selected = match msg.kind.as_str() {
                "actor_message" => same_machine(&from.config, &to.config) && to.config.actor_id.as_deref() == msg.payload.get("actorId").and_then(Json::as_str),
                "remote_event" | "remote_call" => {
                    if target == "server" { from.config.side == "client" && to.config.side == "server" }
                    else { from.config.side == "server" && to.config.side == "client" && (target == "*" || to.config.client_id.as_deref() == Some(target.as_str())) }
                }
                "remote_return" => to.config.id == target,
                _ => return Err(LuaError::runtime(format!("UnsupportedMessage: {}", msg.kind))),
            };
            if selected { to.arm(scenario.cpu_ms); to.deliver(&msg.kind, &msg.payload)?; }
        }
    }
    Ok(())
}

fn read_source(path:&Path, files:&mut Vec<Json>) -> Result<String,Box<dyn std::error::Error>> {
    if fs::metadata(path)?.len()>8*1024*1024 {return Err(format!("Source too large: {}",path.display()).into());}
    let bytes=fs::read(path)?;
    if bytes.len()>8*1024*1024 {return Err(format!("Source too large: {}",path.display()).into());}
    files.push(json!({"path":path.to_string_lossy(),"bytes":bytes.len(),"sha256":format!("{:x}",Sha256::digest(&bytes))}));
    Ok(String::from_utf8(bytes)?.replace("\r\n","\n"))
}

fn load_nodes(nodes: &mut [Json], base: &Path) -> Result<Vec<Json>, Box<dyn std::error::Error>> {
    let mut snapshots=Vec::new();
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
                    source = source.replacen(marker, &format!("{marker}\n{piece}\n"), 1);
                }
            }
            if let Some(paths) = node.get("sourcePrependPaths").and_then(Json::as_array) {
                let mut prelude = String::new();
                for piece in paths { prelude.push_str(&read_source(&base.join(piece.as_str().ok_or("sourcePrependPaths must contain strings")?),&mut files)?); prelude.push('\n'); }
                prelude.push_str(&source); source = prelude;
            }
            let prefix = node.get("sourcePrefix").and_then(Json::as_str).unwrap_or("");
            let suffix = node.get("sourceSuffix").and_then(Json::as_str).unwrap_or("");
            node["source"] = json!(format!("{prefix}\n{source}\n{suffix}"));
        }
        if node_id(node).is_empty() { return Err("Every node needs a nonempty id".into()); }
        if let Some(source)=node.get("source").and_then(Json::as_str) {
            if source.len()>8*1024*1024 {return Err("Assembled source exceeds 8 MiB".into());}
            snapshots.push(json!({"nodeId":node_id(node),"assembledBytes":source.len(),"assembledSha256":format!("{:x}",Sha256::digest(source.as_bytes())),"files":files}));
        }
    }
    Ok(snapshots)
}

fn run(path: &Path, report_path: Option<&Path>) -> Result<bool, Box<dyn std::error::Error>> {
    let mut scenario: Scenario = serde_json::from_str(&fs::read_to_string(path)?)?;
    if !matches!(scenario.engine.as_str(), "native"|"behavioral") { return Err("engine must be native or behavioral".into()); }
    if !matches!(scenario.signal_behavior.as_str(), "immediate"|"deferred") { return Err("signalBehavior must be immediate or deferred".into()); }
    println!("{} MODE: offline scenario; only measured API comparisons establish Roblox parity.", scenario.engine.to_uppercase());
    if !scenario.duration.is_finite() || !(0.0..=3600.0).contains(&scenario.duration) || !scenario.step.is_finite() || !(0.001..=1.0).contains(&scenario.step) || !scenario.latency.is_finite() || !(0.0..=60.0).contains(&scenario.latency) || !(8..=512).contains(&scenario.memory_mb) || !(10..=10000).contains(&scenario.cpu_ms) || scenario.realms.is_empty() || scenario.realms.len()>16 {
        return Err("Invalid scenario limits".into());
    }
    let mut ids = std::collections::HashSet::new();
    for r in &scenario.realms {
        if !ids.insert(r.id.clone()) || !matches!(r.side.as_str(), "server"|"client") || (r.side == "client" && r.client_id.is_none()) { return Err("Invalid/duplicate realm identity".into()); }
        if !matches!(r.executor_profile.as_str(),"generic"|"macsploit-744.2") {return Err("Unknown executor profile".into());}
        if scenario.engine!="native" && r.executor_profile!="generic" {return Err("Executor compatibility profiles require the native engine".into());}
    }
    let mut error_rules = std::collections::HashSet::new();
    for rule in &scenario.expected_errors {
        if !ids.contains(&rule.realm) || !(1..=1000).contains(&rule.max_count)
            || !error_rules.insert((rule.realm.as_str(),rule.message.as_str())) {return Err("Invalid/duplicate expected error rule".into());}
    }
    let source_snapshots=load_nodes(&mut scenario.nodes, path.parent().unwrap_or(Path::new(".")))?;
    let host = Host::new();
    let mut realms = Vec::new();
    let mut server_nodes: HashMap<_,_> = scenario.nodes.iter().map(|n|(node_id(n).to_owned(),n.clone())).collect();
    if server_nodes.len() != scenario.nodes.len() { return Err("Duplicate node IDs".into()); }
    let mut known = Vec::new();
    for cfg in &scenario.realms {
        let realm = host.realm(cfg, &scenario)?;
        let nodes: Vec<_> = scenario.nodes.iter().filter(|n| cfg.side=="server" || visible(n,&server_nodes)).cloned().collect();
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
        let status: Value = internal::<Table>(&realm.lua,"__rt")?.get::<Function>("status")?.call(())?;
        let status: Json = realm.lua.from_value(status)?;
        let errors = status.get("errors").and_then(Json::as_u64).unwrap_or_else(||status.get("errors").and_then(Json::as_array).map_or(0,|x|x.len() as u64));
        scheduler_errors.push((realm.config.id.clone(),errors));
        let unsupported = if realm.native { model::unsupported_calls(&realm.lua)? } else {Default::default()};
        if !unsupported.is_empty() { failed=true; println!("Unsupported engine API in {}: {}",realm.config.id,serde_json::to_string(&unsupported)?); }
        let unsupported_executor = if realm.native { executor::unsupported_calls(&realm.lua)? } else {Default::default()};
        if !unsupported_executor.is_empty() { failed=true; println!("Unsupported executor API in {}: {}",realm.config.id,serde_json::to_string(&unsupported_executor)?); }
        let trace=if scenario.trace_errors {diagnostics::snapshot(&realm.lua)?} else {Json::Null};
        if realm.wall_time_exceeded.get() {failed=true;println!("BudgetExceeded: Luau wall-time budget in {}",realm.config.id);}
        let profiles = if realm.native {
            let value: Value = internal::<Table>(&realm.lua,"__rt")?.get::<Function>("profileStatus")?.call(())?;
            realm.lua.from_value::<Json>(value)?
        } else { Json::Null };
        let memory_categories=if realm.native {
            let value:Value=internal::<Table>(&realm.lua,"__rt")?.get::<Function>("memoryStatus")?.call(())?;
            realm.lua.from_value::<Json>(value)?
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
    if let Some(p) = report_path { fs::write(p, serde_json::to_string_pretty(&report)?)?; }
    println!("{}: {} ({} realms, {} messages, {} log entries)", if failed {"FAIL"} else {"PASS"}, scenario.name, realms.len(), host.emitted.get(), logs.len());
    if !missing.is_empty() { println!("Missing expectations: {}", serde_json::to_string(&missing)?); }
    if !rejected.is_empty() {println!("Rejected messages: {}",serde_json::to_string(&rejected)?);}
    Ok(!failed)
}

fn main() {
    for flag in ["LuauFastpcall", "LuauCompileFastpcall", "LuauFastpcallInterrupt"] {
        if Lua::set_fflag(flag, true).is_err() { eprintln!("Required {flag} flag is unavailable"); std::process::exit(2); }
    }
    let args: Vec<_> = env::args_os().skip(1).collect();
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

fn native_probe(probe: &Path, reference: &Path, full_runtime: bool) -> Result<bool, Box<dyn std::error::Error>> {
    let lua = Lua::new();
    lua.set_memory_limit(64 * 1024 * 1024)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    lua.set_interrupt(move |_| if Instant::now() > deadline { Err(LuaError::runtime("BudgetExceeded: native probe")) } else { Ok(VmState::Continue) });
    native::install(&lua)?;
    types::install(&lua)?;
    if full_runtime {
        lua.globals().set("__config",lua.to_value(&json!({"realmId":"probe","side":"server","isStudio":true,"executor":false,"signalBehavior":"deferred"}))?)?;
        lua.globals().set("__host_log",lua.create_function(|_,(level,message):(String,String)|{eprintln!("[probe][{level}] {message}");Ok(())})?)?;
        scheduler::install(&lua)?;
        model::install(&lua)?;
        hide_internals(&lua)?;
    } else { instance::install(&lua)?; }
    lua.globals().set_safeenv(true);
    let entry = lua.load(fs::read_to_string(probe)?).set_mode(mlua::chunk::ChunkMode::Text).set_name("=NativeParity").into_function()?;
    let thread=lua.create_thread(entry.clone())?;
    if full_runtime { scheduler::set_script_category(&lua,&thread,&entry,"NativeParity")?; }
    let result: Value = thread.resume(())?;
    let mut actual: Json = lua.from_value(result)?;
    let mut expected: Json = serde_json::from_str(&fs::read_to_string(reference)?)?;
    normalize_probe(&mut actual);
    normalize_probe(&mut expected);
    let mut mismatches = Vec::new();
    for (name, value) in expected.as_object().ok_or("Invalid reference")? {
        if actual.get(name) != Some(value) { mismatches.push(json!({"check":name,"studio":value,"emulator":actual.get(name)})); }
        else { println!("MATCH {name}"); }
    }
    println!("{}",serde_json::to_string_pretty(&json!({"nativeParity":mismatches.is_empty(),"mismatches":mismatches,"note":"Probe chunk names and empty-stack JSON array shape normalized. Stack depth, native names, error text and frame layout compared unchanged. Results cover only the supplied probe and reference."}))?);
    Ok(mismatches.is_empty())
}
