use crate::{Host, Realm, Scenario, internal, load_nodes, model, native, node_id, route, scheduler, sync_realms, visible};
use mlua::{AnyUserData, Error, Function, MultiValue, Result, Table, Value};
use serde_json::{Value as Json, json};
use std::{cell::{Cell, RefCell}, collections::{HashMap, HashSet, VecDeque}, fs, path::Path, rc::Rc, time::{Duration, Instant}};

const MAX_SOURCE: usize = 256 * 1024;
const MAX_COMMANDS: usize = 128;
const MAX_ADVANCE: f64 = 60.0;
const MAX_TIME: f64 = 3600.0;

#[derive(Clone)]
struct CommandEnvironment { table: Table, name: String }

#[derive(Clone)]
struct Output {
    logs: Rc<RefCell<Vec<Json>>>,
    sequence: Rc<Cell<u64>>,
    clock: Rc<Cell<f64>>,
}
impl Output {
    fn log(&self, realm: &str, context: &str, level: &str, message: &str) {
        let mut logs = self.logs.borrow_mut();
        if logs.len() >= 4000 { logs.drain(..2000); }
        let seq = self.sequence.get() + 1;
        self.sequence.set(seq);
        logs.push(json!({"seq":seq,"time":self.clock.get(),"realm":realm,"context":context,"level":level,
            "message":message.chars().take(4096).collect::<String>()}));
    }
    fn log_values(&self, realm: &str, context: &str, level: &str, values: &[Json]) {
        self.log(realm, context, level, &serde_json::to_string(values).unwrap_or_default());
        if let Some(log) = self.logs.borrow_mut().last_mut() { log["arguments"] = json!(values); }
    }
}

pub struct Session {
    host: Host,
    realms: Vec<Realm>,
    scenario: Scenario,
    cpu_budget: Rc<Cell<u64>>,
    server_nodes: HashMap<String, Json>,
    known: Vec<HashMap<String, Json>>,
    environments: HashMap<(usize, String), CommandEnvironment>,
    commands: VecDeque<Rc<RefCell<Json>>>,
    next_command: u64,
    output: Output,
    origin: Option<String>,
}

fn failure(message: impl Into<String>) -> Error { Error::runtime(message.into()) }
fn valid_profile(profile: &str) -> Result<()> {
    if matches!(profile, "generic" | "macsploit-744.2") { Ok(()) }
    else { Err(failure("Unknown executor profile")) }
}

impl Session {
    pub fn new(profile: &str) -> Result<Self> {
        valid_profile(profile)?;
        let scenario: Scenario = serde_json::from_value(json!({
            "name":"Interactive session", "engine":"native", "duration":0.0,
            "nodes":[
                {"id":"player1","class":"Player","name":"LocalPlayer","parent":"Players","properties":{"UserId":1}},
                {"id":"playerScripts1","class":"PlayerScripts","name":"PlayerScripts","parent":"player1"},
                {"id":"actor1","class":"Actor","name":"ConsoleActor","parent":"playerScripts1"}
            ],
            "realms":[
                {"id":"server","side":"server"},
                {"id":"client1","side":"client","clientId":"player1","executor":true,"executorProfile":profile},
                {"id":"actor1","side":"client","clientId":"player1","actorId":"actor1"}
            ]
        })).map_err(Error::external)?;
        Self::from_scenario(scenario)
    }

    pub fn from_path(path: &Path) -> Result<Self> { Self::load(path, None) }
    pub fn from_path_with_profile(path: &Path, profile: &str) -> Result<Self> {
        valid_profile(profile)?;
        Self::load(path, Some(profile))
    }
    fn load(path: &Path, profile: Option<&str>) -> Result<Self> {
        if fs::metadata(path).map_err(Error::external)?.len() > 16 * 1024 * 1024 { return Err(failure("Scenario size limit exceeded")); }
        let mut scenario: Scenario = serde_json::from_str(&fs::read_to_string(path).map_err(Error::external)?).map_err(Error::external)?;
        for realm in &mut scenario.realms {
            if realm.side == "client" && realm.actor_id.is_none() { realm.executor = true; }
        }
        if let Some(profile) = profile {
            for realm in &mut scenario.realms {
                if realm.side == "client" && realm.executor { realm.executor_profile = profile.into(); }
            }
        }
        load_nodes(&mut scenario.nodes, path.parent().unwrap_or(Path::new("."))).map_err(|error| failure(error.to_string()))?;
        let mut session = Self::from_scenario(scenario)?;
        session.origin = Some(path.to_string_lossy().into_owned());
        Ok(session)
    }

    fn from_scenario(scenario: Scenario) -> Result<Self> {
        if scenario.engine != "native" { return Err(failure("Interactive sessions require the native engine")); }
        if !matches!(scenario.signal_behavior.as_str(), "immediate" | "deferred")
            || !scenario.step.is_finite() || !(0.001..=1.0).contains(&scenario.step)
            || !scenario.latency.is_finite() || !(0.0..=60.0).contains(&scenario.latency)
            || !(8..=512).contains(&scenario.memory_mb) || !(10..=10000).contains(&scenario.cpu_ms)
            || scenario.realms.is_empty() || scenario.realms.len() > 16 || scenario.nodes.len() > 100000 {
            return Err(failure("Invalid session limits"));
        }
        let mut ids = HashSet::new();
        for realm in &scenario.realms {
            valid_profile(&realm.executor_profile)?;
            if realm.id.is_empty() || realm.id.len() > 128 || !ids.insert(realm.id.clone())
                || !matches!(realm.side.as_str(), "server" | "client")
                || realm.side == "client" && realm.client_id.as_ref().is_none_or(String::is_empty) {
                return Err(failure("Invalid or duplicate realm identity"));
            }
        }
        let server_nodes: HashMap<_, _> = scenario.nodes.iter().map(|node| (node_id(node).to_owned(), node.clone())).collect();
        if server_nodes.len() != scenario.nodes.len() || server_nodes.contains_key("") { return Err(failure("Invalid or duplicate node identity")); }
        let host = Host::interactive();
        let output = Output { logs: host.logs.clone(), sequence: host.log_sequence.clone(), clock: host.clock.clone() };
        let mut realms = Vec::new();
        let mut known = Vec::new();
        for config in &scenario.realms {
            let realm = host.realm(config, &scenario)?;
            let nodes: Vec<_> = scenario.nodes.iter().filter(|node| config.side == "server" || visible(node, &server_nodes)).cloned().collect();
            realm.seed(&nodes)?;
            realm.changes()?;
            known.push(nodes.iter().map(|node| (node_id(node).to_owned(), node.clone())).collect());
            realms.push(realm);
        }
        let cpu_budget = Rc::new(Cell::new(scenario.cpu_ms));
        let mut session = Self { host, realms, scenario, cpu_budget, server_nodes, known, environments: HashMap::new(),
            commands: VecDeque::new(), next_command: 0, output, origin: None };
        for realm in &session.realms {
            for id in &realm.config.entry {
                realm.arm(session.scenario.cpu_ms);
                if let Err(error) = realm.run_script(id) { session.output.log(&realm.config.id, &realm.config.side, "error", &error.to_string()); }
            }
            for id in &realm.config.executor_entry {
                let node = session.scenario.nodes.iter().find(|node| node_id(node) == id).ok_or_else(|| failure("Executor entry node missing"))?;
                let source = node["source"].as_str().ok_or_else(|| failure("Executor entry source missing"))?;
                realm.arm(session.scenario.cpu_ms);
                if let Err(error) = crate::executor::run(&realm.lua, source, &format!("@executor/{id}")) {
                    session.output.log(&realm.config.id, "executor", "error", &error.to_string());
                }
            }
        }
        session.pump(0.0, 0.0, false)?;
        Ok(session)
    }

    fn environment(&mut self, index: usize, context: &str) -> Result<CommandEnvironment> {
        let key = (index, context.to_owned());
        if let Some(environment) = self.environments.get(&key) { return Ok(environment.clone()); }
        let realm = &self.realms[index];
        realm.arm(self.scenario.cpu_ms);
        if context == "executor" {
            let table = internal::<Table>(&realm.lua, "__emu")?.get("executorEnvironment")?;
            let environment = CommandEnvironment { table, name: format!("Console/{}/executor", realm.config.id) };
            self.environments.insert(key, environment.clone());
            return Ok(environment);
        }
        let model = realm.model()?;
        let get: Function = model.get("get")?;
        let config = &realm.config;
        let parent = if config.side == "server" { "ServerScriptService".to_owned() }
            else if let Some(actor) = &config.actor_id { actor.clone() }
            else if let Some(player) = &config.client_id {
                let player: AnyUserData = get.call(player.as_str())?;
                let node = player.borrow::<crate::instance::Instance>()?.0.clone();
                let existing = node.borrow().children.iter().find(|child| child.borrow().class == "PlayerScripts").map(|child|child.borrow().id.clone());
                if let Some(id) = existing { id } else {
                let player = node.borrow().id.clone();
                let id = format!("__console.PlayerScripts.{player}");
                if get.call::<Option<AnyUserData>>(id.clone())?.is_none() {
                    realm.seed(&[json!({"id":id,"class":"PlayerScripts","name":"PlayerScripts","parent":player})])?;
                }
                id
                }
            } else { return Err(failure("Client realm has no player")); };
        if get.call::<Option<AnyUserData>>(parent.clone())?.is_none() { return Err(failure("Command Script parent is unavailable")); }
        let mut id = format!("__console.{}.{}", config.id, context);
        while get.call::<Option<AnyUserData>>(id.clone())?.is_some() { id.push('_'); }
        let name = format!("Console_{}_{}", config.id, context);
        realm.seed(&[json!({"id":id,"class":if config.side=="server" {"Script"} else {"LocalScript"},"name":name,"parent":parent})])?;
        let script: AnyUserData = get.call(id)?;
        let table = model.get::<Function>("getEnvironment")?.call(script)?;
        let environment = CommandEnvironment { table, name };
        self.environments.insert(key, environment.clone());
        Ok(environment)
    }

    pub fn execute(&mut self, realm: &str, context: &str, source: &str) -> Result<Json> {
        let index = self.realms.iter().position(|candidate| candidate.config.id == realm).ok_or_else(|| failure("Unknown realm"))?;
        let config = &self.realms[index].config;
        if context != config.side && !(context == "executor" && config.side == "client" && config.executor) {
            return Err(failure("Context is not available in the selected realm"));
        }
        if source.len() > MAX_SOURCE { return Err(failure("Command source exceeds 256 KiB")); }
        if self.commands.len() >= MAX_COMMANDS {
            if let Some(index) = self.commands.iter().position(|command| matches!(command.borrow()["status"].as_str(), Some("completed" | "error"))) {
                self.commands.remove(index);
            } else { return Err(failure("Pending command limit exceeded")); }
        }
        self.pump(self.host.clock.get(), 0.0, false)?;
        let environment = self.environment(index, context)?;
        environment.table.set_safeenv(false);
        self.next_command += 1;
        let command = Rc::new(RefCell::new(json!({"id":self.next_command,"realm":realm,"context":context,
            "status":"running","time":self.host.clock.get(),"sourceBytes":source.len(),"yieldCount":0})));
        self.commands.push_back(command.clone());
        let runtime = &self.realms[index];
        runtime.arm(self.scenario.cpu_ms);
        runtime.wall_time_exceeded.set(false);
        let function = runtime.lua.load(source).set_mode(mlua::chunk::ChunkMode::Text)
            .set_name(format!("=Console/{realm}/{context}/{}", self.next_command)).set_environment(environment.table.clone()).into_function();
        let function = match function {
            Ok(function) => function,
            Err(error) => {
                command.borrow_mut()["status"] = json!("error");
                command.borrow_mut()["error"] = json!({"type":"compile","message":error.to_string()});
                self.output.log(realm, context, "error", &error.to_string());
                return Ok(command.borrow().clone());
            }
        };
        if context == "executor" { native::mark_executor_function(&runtime.lua, &function, &environment.table)?; }
        let thread = runtime.lua.create_thread(function.clone())?;
        scheduler::set_thread_environment(&runtime.lua, &thread, &environment.table)?;
        scheduler::set_script_category(&runtime.lua, &thread, &function, &environment.name)?;
        let output = self.output.clone();
        let completed = command.clone();
        let realm_id = realm.to_owned();
        let command_context = context.to_owned();
        let deadline = runtime.deadline.clone();
        let exceeded = runtime.wall_time_exceeded.clone();
        let cpu_budget = self.cpu_budget.clone();
        let finished_thread = thread.clone();
        let completion: scheduler::Completion = Rc::new(move |lua, outcome| {
            let timed_out = exceeded.replace(false);
            deadline.set(Instant::now() + Duration::from_millis(cpu_budget.get()));
            match outcome {
                Ok(values) if !timed_out => {
                    let (count, values) = tuple_preview(lua, values);
                    let mut record = completed.borrow_mut();
                    record["status"] = json!("completed"); record["returnCount"] = json!(count); record["returns"] = json!(values);
                    output.log_values(&realm_id, &command_context, "return", &values);
                }
                outcome => {
                    let error = match outcome { Err(error) => error, _ => Value::Nil };
                    let game_message = error_message(&error);
                    let message = if timed_out { "BudgetExceeded: Luau wall-time budget".into() } else { game_message.chars().take(4096).collect::<String>() };
                    let truncated = !timed_out && match &error {
                        Value::String(value) => crate::console::render_bytes(&value.as_bytes(), 4096).1,
                        _ => message.len() < game_message.len(),
                    };
                    let mut record = completed.borrow_mut();
                    record["status"] = json!("error");
                    record["error"] = json!({"type":if timed_out {"budget"} else {error.type_name()},"message":message,"truncated":truncated});
                    drop(record);
                    output.log(&realm_id, &command_context, "error", &message);
                    let _ = model::log(lua, "error", game_message.chars().take(4096).collect::<String>().as_bytes());
                    let _ = model::script_error(lua, &game_message, &finished_thread);
                    let _ = scheduler::cancel(lua, finished_thread.clone());
                }
            }
            completed.borrow_mut()["completedAt"] = json!(output.clock.get());
            Ok(())
        });
        let output = self.output.clone();
        let yielded = command.clone();
        let realm_id = realm.to_owned();
        let command_context = context.to_owned();
        let observer: scheduler::YieldObserver = Rc::new(move |lua, values| {
            let (count, values) = tuple_preview(lua, values);
            let mut record = yielded.borrow_mut();
            record["status"] = json!("yielded");
            record["yieldCount"] = json!(record["yieldCount"].as_u64().unwrap_or(0) + 1);
            record["yieldArity"] = json!(count); record["yielded"] = json!(values);
            output.log_values(&realm_id, &command_context, "yield", &values);
            Ok(())
        });
        if let Err(error) = scheduler::spawn_observed(&runtime.lua, Value::Thread(thread.clone()), MultiValue::new(), Some("serial"),
            Some(if context == "executor" { "executor" } else { "game" }), completion, Some(observer)) {
            runtime.arm(self.scenario.cpu_ms);
            let _ = scheduler::cancel(&runtime.lua, thread);
            command.borrow_mut()["status"] = json!("error");
            command.borrow_mut()["error"] = json!({"type":"host","message":error.to_string()});
            self.output.log(realm, context, "error", &error.to_string());
        }
        if let Err(error) = self.pump(self.host.clock.get(), 0.0, false) { command.borrow_mut()["pumpError"] = json!(error.to_string()); }
        let result = command.borrow().clone();
        Ok(result)
    }

    fn pump(&mut self, now: f64, dt: f64, timed: bool) -> Result<()> {
        self.host.clock.set(now);
        sync_realms(&self.realms, &self.scenario, &mut self.server_nodes, &mut self.known)?;
        route(&self.host, &self.realms, &self.scenario)?;
        for realm in &self.realms {
            realm.arm(self.scenario.cpu_ms);
            let stepped = if timed { realm.step(now) } else { scheduler::flush(&realm.lua, now).map(|_| ()) };
            if let Err(error) = stepped { realm.wall_time_exceeded.set(false); self.output.log(&realm.config.id, &realm.config.side, "error", &error.to_string()); return Err(error); }
            if realm.wall_time_exceeded.replace(false) {
                let error = failure("BudgetExceeded: Luau wall-time budget");
                self.output.log(&realm.config.id, &realm.config.side, "error", &error.to_string()); return Err(error);
            }
            if dt > 0.0 {
                realm.arm(self.scenario.cpu_ms);
                if let Err(error) = realm.frame(dt) { realm.wall_time_exceeded.set(false); self.output.log(&realm.config.id, &realm.config.side, "error", &error.to_string()); return Err(error); }
                if realm.wall_time_exceeded.replace(false) {
                    let error = failure("BudgetExceeded: Luau wall-time budget");
                    self.output.log(&realm.config.id, &realm.config.side, "error", &error.to_string()); return Err(error);
                }
            }
        }
        sync_realms(&self.realms, &self.scenario, &mut self.server_nodes, &mut self.known)?;
        route(&self.host, &self.realms, &self.scenario)
    }

    pub fn advance(&mut self, seconds: f64) -> Result<Json> {
        if !seconds.is_finite() || !(0.0..=MAX_ADVANCE).contains(&seconds) || self.host.clock.get() + seconds > MAX_TIME {
            return Err(failure("Advance requires 0..60 seconds and session time at most 3600 seconds"));
        }
        let target = self.host.clock.get() + seconds;
        let started = Instant::now();
        let budget = Duration::from_millis(self.scenario.cpu_ms.clamp(1000, 10000));
        if seconds == 0.0 { self.pump(target, 0.0, true)?; }
        while self.host.clock.get() < target {
            if started.elapsed() > budget { return Err(failure("BudgetExceeded: advance request wall-time budget")); }
            let now = (self.host.clock.get() + self.scenario.step).min(target);
            let dt = now - self.host.clock.get();
            self.pump(now, dt, true)?;
        }
        self.snapshot()
    }

    pub fn snapshot(&self) -> Result<Json> {
        let mut realms = Vec::new();
        for realm in &self.realms {
            realm.arm(self.scenario.cpu_ms);
            let status = scheduler::status(&realm.lua)?;
            realms.push(json!({"id":realm.config.id,"side":realm.config.side,"clientId":realm.config.client_id,
                "actorId":realm.config.actor_id,"executor":realm.config.executor,"executorProfile":realm.config.executor_profile,
                "memoryBytes":realm.lua.used_memory(),"scheduler":status,
                "unsupportedApiCalls":model::unsupported_calls(&realm.lua)?,"unsupportedExecutorApiCalls":crate::executor::unsupported_calls(&realm.lua)?}));
        }
        Ok(json!({"name":self.scenario.name,"interactive":true,"origin":self.origin.as_ref().map(|path|json!({"kind":"scenario","path":path})).unwrap_or_else(||json!({"kind":"default"})),
            "time":self.host.clock.get(),"realms":realms,"logs":*self.host.logs.borrow(),
            "viewers":self.realms.iter().flat_map(|realm|crate::inspector::viewers(&realm.lua,&realm.config.id)).collect::<Vec<_>>(),
            "pendingMessages":self.host.messages.borrow().len(),"pendingMessageBytes":self.host.pending_bytes.get(),
            "commands":self.commands.iter().map(|command|command.borrow().clone()).collect::<Vec<_>>(),
            "limits":{"sourceBytes":MAX_SOURCE,"advanceSeconds":MAX_ADVANCE,"sessionSeconds":MAX_TIME,"memoryMb":self.scenario.memory_mb,"cpuMs":self.scenario.cpu_ms}}))
    }

    pub fn completions(&self, realm: &str, context: &str, path: &[String]) -> Result<Json> {
        let realm = self.realms.iter().find(|candidate|candidate.config.id == realm).ok_or_else(||failure("Unknown realm"))?;
        if context != realm.config.side && !(context == "executor" && realm.config.side == "client" && realm.config.executor) {
            return Err(failure("Context is not available in the selected realm"));
        }
        if path.len() > 16 || path.iter().any(|segment| segment.is_empty() || segment.len() > 256 || segment.contains('\0')) {
            return Err(failure("Completion path requires at most 16 segments of 1..256 bytes"));
        }
        Ok(json!({"items":model::completions(&realm.lua,path)?}))
    }

    pub fn clear_output(&mut self) { self.host.logs.borrow_mut().clear(); }

    pub fn inspect(&self, body: &Json) -> Result<Json> {
        let text=|key:&str|->Result<&str>{body.get(key).and_then(Json::as_str).filter(|s|s.len()<=512).ok_or_else(||failure(format!("Invalid inspector {key}")))};
        let realm_id=text("realm")?;
        let context=text("context")?;
        let realm=self.realms.iter().find(|realm|realm.config.id==realm_id).ok_or_else(||failure("Unknown realm"))?;
        if context != realm.config.side && !(context=="executor" && realm.config.side=="client" && realm.config.executor) {
            return Err(failure("Context is not available in the selected realm"));
        }
        realm.arm(self.scenario.cpu_ms);
        let offset=match body.get("offset") {None=>0,Some(value)=>value.as_u64().and_then(|n|usize::try_from(n).ok()).ok_or_else(||failure("Invalid inspector offset"))?};
        match text("kind")? {
            "table"=>crate::inspector::table_page(&realm.lua,text("handle")?,offset),
            "function"=>crate::inspector::function_page(&realm.lua,text("handle")?,text("section")?,offset),
            "tree"=>model::inspect_tree(&realm.lua,match body.get("parent"){None|Some(Json::Null)=>None,_=>Some(text("parent")?)},offset),
            "instance"=>model::inspect_node(&realm.lua,text("id")?),
            "gc"|"closeGc" if context!="executor"=>Err(failure("GC Viewer requires Executor context")),
            "gc"=>crate::inspector::gc_page(&realm.lua,text("viewer")?,body.get("refresh").and_then(Json::as_bool).unwrap_or(false),offset,
                if body.get("query").is_some(){text("query")?}else{""},if body.get("valueType").is_some(){text("valueType")?}else{""}),
            "closeGc"=>crate::inspector::close_viewer(&realm.lua,text("viewer")?),
            _=>Err(failure("Unknown inspector operation")),
        }
    }

    pub fn set_cpu_budget(&mut self, ms: u64) -> Result<()> {
        if !(10..=10000).contains(&ms) { return Err(failure("CPU budget requires 10..10000 milliseconds")); }
        self.scenario.cpu_ms = ms;
        self.cpu_budget.set(ms);
        for realm in &self.realms { realm.arm(ms); }
        Ok(())
    }
}

fn error_message(error: &Value) -> String {
    match error {
        Value::String(value) => crate::console::display_bytes(&value.as_bytes(), 4096),
        Value::Integer(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::Error(error) => error.to_string(),
        _ => "Error occurred, no output from Luau.".into(),
    }
}

fn tuple_preview(lua: &mlua::Lua, values: MultiValue) -> (usize, Vec<Json>) {
    (values.len(), crate::console::snapshot(lua, values.iter()))
}

#[cfg(all(test, emulator_local_tests))]
#[path = "../local_tests/session.rs"]
mod tests;
