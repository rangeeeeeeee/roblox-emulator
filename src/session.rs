use crate::{Host, Realm, Scenario, internal, load_nodes, model, native, node_id, route, scheduler, sync_realms, visible};
use mlua::{Error, MultiValue, Result, Table, Value};
#[cfg(all(test, emulator_local_tests))]
use mlua::{AnyUserData, Function};
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

enum Startup { Script(usize, String), Executor(usize, String) }
struct PumpContinuation { now: f64, dt: f64, timed: bool, index: usize, resume_step: bool }
impl Output {
    fn log(&self, realm: &str, context: &str, level: &str, message: &str) {
        let mut logs = self.logs.borrow_mut();
        if logs.len() >= 4000 { logs.drain(..2000); }
        let seq = self.sequence.get() + 1;
        self.sequence.set(seq);
        logs.push(json!({"seq":seq,"time":self.clock.get(),"realm":realm,"context":context,"level":level,
            "message":message.chars().take(4096).collect::<String>()}));
    }
    fn protected_error(&self, realm: &str, side: &str, entry: &crate::diagnostics::Entry) {
        let context = if entry.origin.as_deref() == Some("executor") { "executor" } else { side };
        let description = match &entry.message {
            Some(message) if entry.message_truncated => format!("Caught error: {message}…"),
            Some(message) => format!("Caught error: {message}"),
            None => format!("Caught error ({})", entry.value_type),
        };
        let Ok(mut logs) = self.logs.try_borrow_mut() else { return; };
        if logs.len() >= 4000 { logs.drain(..2000); }
        let seq = self.sequence.get().saturating_add(1); self.sequence.set(seq);
        let mut record = json!({"seq":seq,"time":self.clock.get(),"realm":realm,"context":context,"level":"debug",
            "category":"caughtError","message":description,"errorType":entry.value_type,"frames":entry.frames});
        if let Some(frame) = entry.frames.iter().find(|frame| frame.line > 0 && frame.source != "[C]") {
            record["source"] = json!(frame.source); record["sourceLine"] = json!(frame.line);
        }
        logs.push(record);
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
    startups: VecDeque<Startup>,
    interrupted_pump: Option<PumpContinuation>,
    advance_target: Option<f64>,
    network_resume_pending: bool,
    trace_enabled: bool,
    last_step_ms: Option<f64>,
    last_virtual_step_ms: Option<f64>,
    total_step_ms: f64,
    measured_steps: u64,
}

fn failure(message: impl Into<String>) -> Error { Error::runtime(message.into()) }
fn valid_profile(profile: &str) -> Result<()> {
    if matches!(profile, "generic" | "unc" | "macsploit-744.2" | "macsploit-744.4") { Ok(()) }
    else { Err(failure("Unknown executor profile")) }
}

pub(crate) fn restrict_source_paths(nodes:&mut [Json],scenario_path:&Path)->Result<()> {
    let base=scenario_path.parent().unwrap_or(Path::new("."));
    let root=if base.file_name().is_some_and(|name|name.eq_ignore_ascii_case("scenarios")) {
        base.parent().unwrap_or(base)
    } else {base};
    let root=fs::canonicalize(root).map_err(Error::external)?;
    let resolve=|value:&mut Json|->Result<()> {
        let name=value.as_str().ok_or_else(||failure("Scenario source path must be a string"))?;
        let path=fs::canonicalize(base.join(name)).map_err(Error::external)?;
        if !path.starts_with(&root) {return Err(failure("Scenario source path is outside its project directory"));}
        *value=json!(path.to_string_lossy());
        Ok(())
    };
    for node in nodes {
        let Some(fields)=node.as_object_mut() else {continue};
        if let Some(path)=fields.get_mut("sourcePath") {resolve(path)?;}
        if let Some(paths)=fields.get_mut("sourcePrependPaths") {
            for path in paths.as_array_mut().ok_or_else(||failure("sourcePrependPaths must be an array"))? {resolve(path)?;}
        }
        if let Some(insertions)=fields.get_mut("sourceInsertions") {
            for insertion in insertions.as_array_mut().ok_or_else(||failure("sourceInsertions must be an array"))? {
                resolve(insertion.get_mut("path").ok_or_else(||failure("sourceInsertions requires path"))?)?;
            }
        }
    }
    Ok(())
}

impl Session {
    /// An inactive session contains no Luau VMs; it exists only to serve place editing.
    pub(crate) fn stopped() -> Self {
        let scenario = Scenario {
            name: "Stopped".into(), engine: "native".into(), signal_behavior: "deferred".into(),
            trace_errors: false, default_animate: false, duration: 0.0, step: 1.0 / 60.0, memory_mb: 64, cpu_ms: 2000,
            latency: 0.0, nodes: Vec::new(), realms: Vec::new(), expect: Vec::new(),
            reject: Vec::new(), expected_errors: Vec::new(),
        };
        let host = Host::interactive();
        let output = Output { logs: host.logs.clone(), sequence: host.log_sequence.clone(), clock: host.clock.clone() };
        Self { host, realms: Vec::new(), scenario, cpu_budget: Rc::new(Cell::new(2000)),
            server_nodes: HashMap::new(), known: Vec::new(), environments: HashMap::new(),
            commands: VecDeque::new(), next_command: 0, output, origin: None, startups: VecDeque::new(),
            interrupted_pump: None, advance_target: None, network_resume_pending: false,
            trace_enabled: false, last_step_ms: None, last_virtual_step_ms: None, total_step_ms: 0.0, measured_steps: 0 }
    }

    pub fn new(profile: &str) -> Result<Self> {
        Self::from_scenario(Self::default_scenario(profile)?)
    }

    pub(crate) fn default_scenario(profile: &str) -> Result<Scenario> {
        valid_profile(profile)?;
        let scenario: Scenario = serde_json::from_value(json!({
            "name":"Interactive session", "engine":"native", "duration":0.0, "defaultAnimate":true,
            "nodes":[
                {"id":"player1","class":"Player","name":"LocalPlayer","parent":"Players","properties":{"UserId":1,"Character":{"tag":"instance","id":"character1"}}},
                {"id":"playerScripts1","class":"PlayerScripts","name":"PlayerScripts","parent":"player1"},
                {"id":"actor1","class":"Actor","name":"ConsoleActor","parent":"playerScripts1"},
                {"id":"character1","class":"Model","name":"LocalPlayer","parent":"Workspace","properties":{"PrimaryPart":{"tag":"instance","id":"characterRoot1"}}},
                {"id":"characterRoot1","class":"Part","name":"HumanoidRootPart","parent":"character1"},
                {"id":"characterHead1","class":"Part","name":"Head","parent":"character1"},
                {"id":"characterHumanoid1","class":"Humanoid","name":"Humanoid","parent":"character1"},
                {"id":"characterAnimator1","class":"Animator","name":"Animator","parent":"characterHumanoid1"}
            ],
            "realms":[
                {"id":"server","side":"server"},
                {"id":"client1","side":"client","clientId":"player1","executor":true,"executorProfile":profile},
                {"id":"actor1","side":"client","clientId":"player1","actorId":"actor1"}
            ]
        })).map_err(Error::external)?;
        Ok(scenario)
    }

    pub fn from_path(path: &Path) -> Result<Self> { Self::load(path, None, None) }
    pub(crate) fn from_place(place: &crate::place::Place) -> Result<Self> {
        Self::from_scenario(place.scenario().map_err(failure)?)
    }
    pub(crate) fn from_place_with_trace(place: &crate::place::Place, enabled: bool) -> Result<Self> {
        let mut scenario = place.scenario().map_err(failure)?;
        scenario.trace_errors = enabled;
        Self::from_scenario(scenario)
    }
    pub fn from_path_with_profile(path: &Path, profile: &str) -> Result<Self> {
        valid_profile(profile)?;
        Self::load(path, Some(profile), None)
    }
    pub(crate) fn from_path_with_profile_and_trace(path: &Path, profile: &str, enabled: bool) -> Result<Self> {
        valid_profile(profile)?;
        Self::load(path, Some(profile), Some(enabled))
    }
    fn load(path: &Path, profile: Option<&str>, trace: Option<bool>) -> Result<Self> {
        let scenario = Self::prepare_scenario(path,profile,trace)?;
        let mut session = Self::from_scenario(scenario)?;
        session.origin = Some(path.to_string_lossy().into_owned());
        Ok(session)
    }

    // Resolve trusted CLI scenario files before starting the restricted worker.
    pub(crate) fn prepare_scenario(path: &Path, profile: Option<&str>, trace: Option<bool>) -> Result<Scenario> {
        let bytes=crate::read_bounded(path,16*1024*1024).map_err(|error|failure(error.to_string()))?;
        let mut scenario: Scenario = serde_json::from_slice(&bytes).map_err(Error::external)?;
        for realm in &mut scenario.realms {
            if realm.side == "client" && realm.actor_id.is_none() { realm.executor = true; }
        }
        if let Some(profile) = profile {
            for realm in &mut scenario.realms {
                if realm.side == "client" && realm.executor { realm.executor_profile = profile.into(); }
            }
        }
        restrict_source_paths(&mut scenario.nodes,path)?;
        load_nodes(&mut scenario.nodes, path.parent().unwrap_or(Path::new("."))).map_err(|error| failure(error.to_string()))?;
        if let Some(enabled) = trace { scenario.trace_errors = enabled; }
        for node in &mut scenario.nodes {
            if let Some(fields)=node.as_object_mut() {
                for key in ["sourcePath","sourceInsertions","sourcePrependPaths","sourcePrefix","sourceSuffix"] {fields.remove(key);}
            }
        }
        Ok(scenario)
    }

    pub(crate) fn set_origin(&mut self, origin: Option<String>) { self.origin=origin; }

    pub(crate) fn from_scenario(mut scenario: Scenario) -> Result<Self> {
        if scenario.engine != "native" { return Err(failure("Interactive sessions require the native engine")); }
        crate::prepare_default_animations(&mut scenario);
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
            let nodes: Vec<_> = scenario.nodes.iter().filter(|node| config.side == "server" || visible(node, &server_nodes)).cloned().map(|mut node|{
                if config.side=="client" {crate::redact_client_source(&mut node,&scenario);}node
            }).collect();
            realm.seed(&nodes)?;
            realm.changes()?;
            known.push(nodes.iter().map(|node| (node_id(node).to_owned(), node.clone())).collect());
            realms.push(realm);
        }
        let cpu_budget = Rc::new(Cell::new(scenario.cpu_ms));
        let mut startups = VecDeque::new();
        for (index, realm) in realms.iter().enumerate() {
            startups.extend(realm.config.entry.iter().cloned().map(|id|Startup::Script(index,id)));
            startups.extend(realm.config.executor_entry.iter().cloned().map(|id|Startup::Executor(index,id)));
        }
        let trace_enabled = scenario.trace_errors;
        let mut session = Self { host, realms, scenario, cpu_budget, server_nodes, known, environments: HashMap::new(),
            commands: VecDeque::new(), next_command: 0, output, origin: None, startups, interrupted_pump: None, advance_target: None,
            network_resume_pending: false, trace_enabled: false, last_step_ms: None, last_virtual_step_ms: None, total_step_ms: 0.0, measured_steps: 0 };
        if trace_enabled { session.set_trace_errors(true)?; }
        session.start_pending()?;
        session.pump(0.0, 0.0, false)?;
        Ok(session)
    }

    fn start_pending(&mut self) -> Result<()> {
        while !self.network_blocked() {
            let Some(startup) = self.startups.pop_front() else { break };
            match startup {
                Startup::Script(index, id) => {
                    let realm = &self.realms[index];
                    realm.arm(self.scenario.cpu_ms);
                    if let Err(error) = scheduler::with_network_root(&realm.lua,||realm.run_script(&id)) { self.output.log(&realm.config.id, &realm.config.side, "error", &error.to_string()); }
                }
                Startup::Executor(index, id) => {
                    let realm = &self.realms[index];
                    let node = self.scenario.nodes.iter().find(|node| node_id(node) == id).ok_or_else(|| failure("Executor entry node missing"))?;
                    let source = node["source"].as_str().ok_or_else(|| failure("Executor entry source missing"))?;
                    realm.arm(self.scenario.cpu_ms);
                    if let Err(error) = crate::executor::run(&realm.lua, source, &format!("@executor/{id}")) {
                        self.output.log(&realm.config.id, "executor", "error", &error.to_string());
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) fn primary_profile(&self) -> &str {
        self.realms.iter().find(|realm| realm.config.side == "client" && realm.config.executor)
            .map(|realm|realm.config.executor_profile.as_str()).unwrap_or("generic")
    }

    pub(crate) fn scenario_template(&self) -> &Scenario { &self.scenario }
    pub(crate) fn trace_errors_enabled(&self) -> bool { self.trace_enabled }

    pub(crate) fn set_trace_errors(&mut self, enabled: bool) -> Result<()> {
        for realm in &self.realms {
            let output = self.output.clone();
            let realm_id = realm.config.id.clone();
            let side = realm.config.side.clone();
            let sink = enabled.then(|| Rc::new(move |entry: &crate::diagnostics::Entry| {
                output.protected_error(&realm_id, &side, entry);
            }) as Rc<dyn Fn(&crate::diagnostics::Entry)>);
            crate::diagnostics::configure(&realm.lua, enabled, sink)?;
        }
        self.trace_enabled = enabled;
        self.scenario.trace_errors = enabled;
        Ok(())
    }

    pub(crate) fn network_pending(&self) -> Option<Json> { self.host.network.as_ref().and_then(|network|network.pending()) }
    pub(crate) fn network_blocked(&self) -> bool { self.host.network.as_ref().is_some_and(|network|network.has_pending()) }
    pub(crate) fn network_decide(&mut self, id: &str, allow: bool) -> Result<()> {
        self.host.network.as_ref().ok_or_else(||failure("Network approval is unavailable"))?.decide(id,allow).map_err(failure)?;
        self.poll_network()
    }
    pub(crate) fn network_complete_external(&mut self, id: &str, response: Json) -> Result<()> {
        self.host.network.as_ref().ok_or_else(||failure("Network approval is unavailable"))?
            .complete_external(id,response).map_err(failure)?;
        self.poll_network()
    }
    pub(crate) fn websocket_actions(&self)->Vec<Json>{self.host.network.as_ref().map_or_else(Vec::new,|network|network.socket_actions())}
    pub(crate) fn websocket_event_external(&mut self,id:&str,event:Json)->Result<()> {
        self.host.network.as_ref().ok_or_else(||failure("Network approval is unavailable"))?
            .socket_event_external(id,event).map_err(failure)?;
        self.poll_network()
    }
    pub(crate) fn poll_network(&mut self) -> Result<()> {
        if self.network_blocked() { return Ok(()); }
        let Some(network) = self.host.network.clone() else { return Ok(()) };
        let mut ready = 0;
        for realm in &self.realms { ready += network.poll(&realm.lua, &realm.config.id)?; }
        if ready > 0 { self.network_resume_pending = true; }
        self.start_pending()?;
        if self.network_blocked() { return Ok(()); }
        if let Some(frame) = self.interrupted_pump.take() { self.finish_pump(frame)?; }
        if self.network_blocked() { return Ok(()); }
        if self.network_resume_pending {
            self.network_resume_pending = false;
            self.pump(self.host.clock.get(), 0.0, false)?;
        }
        if self.network_blocked() { return Ok(()); }
        if let Some(target) = self.advance_target {
            let started = Instant::now();
            let budget = Duration::from_millis(self.scenario.cpu_ms.clamp(1000, 10000));
            while self.host.clock.get() < target && !self.network_blocked() {
                if started.elapsed() > budget { return Err(failure("BudgetExceeded: advance request wall-time budget")); }
                let now = (self.host.clock.get() + self.scenario.step).min(target);
                let dt = now - self.host.clock.get();
                self.pump(now,dt,true)?;
            }
            if !self.network_blocked() { self.advance_target = None; }
        }
        Ok(())
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
        let config = &realm.config;
        let parent = if config.side == "server" { "ServerScriptService".to_owned() }
            else if let Some(actor) = &config.actor_id { actor.clone() }
            else if let Some(player) = &config.client_id {
                let player = realm.get_instance(player)?.ok_or_else(||failure("Client player is unavailable"))?;
                let node = player.borrow::<crate::instance::Instance>()?.0.clone();
                let existing = node.borrow().children.iter().find(|child| child.borrow().class == "PlayerScripts").map(|child|child.borrow().id.clone());
                if let Some(id) = existing { id } else {
                let player = node.borrow().id.clone();
                let id = format!("__console.PlayerScripts.{player}");
                if realm.get_instance(&id)?.is_none() {
                    realm.seed(&[json!({"id":id,"class":"PlayerScripts","name":"PlayerScripts","parent":player})])?;
                }
                id
                }
            } else { return Err(failure("Client realm has no player")); };
        if realm.get_instance(&parent)?.is_none() { return Err(failure("Command Script parent is unavailable")); }
        let mut id = format!("__console.{}.{}", config.id, context);
        while realm.get_instance(&id)?.is_some() { id.push('_'); }
        let name = format!("Console_{}_{}", config.id, context);
        realm.seed(&[json!({"id":id,"class":if config.side=="server" {"Script"} else {"LocalScript"},"name":name,"parent":parent})])?;
        let script=realm.get_instance(&id)?.ok_or_else(||failure("Command Script is unavailable"))?;
        let table = realm.script_environment(script)?;
        let environment = CommandEnvironment { table, name };
        self.environments.insert(key, environment.clone());
        Ok(environment)
    }

    pub fn execute(&mut self, realm: &str, context: &str, source: &str) -> Result<Json> {
        self.execute_input(realm, context, source, "source")
    }

    pub fn execute_input(&mut self, realm: &str, context: &str, source: &str, format: &str) -> Result<Json> {
        if self.network_blocked() { return Err(failure("VMs are paused for network approval")); }
        if !matches!(format, "source" | "bytecode-base64") { return Err(failure("Unknown input format")); }
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
        if self.network_blocked() { return Err(failure("VMs are paused for network approval")); }
        let environment = self.environment(index, context)?;
        environment.table.set_safeenv(false);
        self.next_command += 1;
        let command = Rc::new(RefCell::new(json!({"id":self.next_command,"realm":realm,"context":context,
            "status":"running","time":self.host.clock.get(),"sourceBytes":source.len(),"format":format,"yieldCount":0})));
        self.commands.push_back(command.clone());
        let runtime = &self.realms[index];
        runtime.arm(self.scenario.cpu_ms);
        runtime.wall_time_exceeded.set(false);
        let input = if format == "bytecode-base64" {
            crate::bytecode::decode(source).map(|(bytes, encoding)| {
                command.borrow_mut()["bytecodeEncoding"] = json!(encoding);
                command.borrow_mut()["bytecodeBytes"] = json!(bytes.len());
                bytes
            })
        } else { Ok(source.as_bytes().to_vec()) };
        let function = input.and_then(|bytes| {
            let chunk = runtime.lua.load(bytes)
                .set_mode(if format == "source" { mlua::chunk::ChunkMode::Text } else { mlua::chunk::ChunkMode::Binary })
                .set_name(format!("=Console/{realm}/{context}/{}", self.next_command)).set_environment(environment.table.clone());
            if context == "executor" { chunk.set_compiler(crate::native::executor_compiler(&runtime.lua)).into_function() }
            else { chunk.into_function() }
        });
        let function = match function {
            Ok(function) => function,
            Err(error) => {
                command.borrow_mut()["status"] = json!("error");
                command.borrow_mut()["error"] = json!({"type":if format == "source" {"compile"} else {"bytecode"},"message":error.to_string()});
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
        if let Err(error) = scheduler::with_network_root(&runtime.lua,||scheduler::spawn_observed(&runtime.lua, Value::Thread(thread.clone()), MultiValue::new(), Some("serial"),
            Some(if context == "executor" { "executor" } else { "game" }), completion, Some(observer))) {
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
        if self.network_blocked() { return Ok(()); }
        if self.interrupted_pump.is_some() { return Err(failure("A paused scheduler frame must be resumed first")); }
        let started = Instant::now();
        let result = (|| {
            self.host.clock.set(now);
            sync_realms(&self.realms, &self.scenario, &mut self.server_nodes, &mut self.known)?;
            if self.network_blocked() { return Ok(()); }
            route(&self.host, &self.realms, &self.scenario)?;
            if self.network_blocked() { return Ok(()); }
            self.finish_pump(PumpContinuation { now, dt, timed, index: 0, resume_step: false })
        })();
        // An HTTP approval can interrupt a frame; don't report a partial pump
        // or include the user's decision time in the measured host duration.
        if timed && dt >= 0.000_001 && result.is_ok() && !self.network_blocked() && self.interrupted_pump.is_none() {
            let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
            self.last_step_ms = Some(elapsed_ms);
            self.last_virtual_step_ms = Some(dt * 1000.0);
            self.total_step_ms += elapsed_ms;
            self.measured_steps = self.measured_steps.saturating_add(1);
        }
        result
    }

    fn finish_pump(&mut self, mut frame: PumpContinuation) -> Result<()> {
        while frame.index < self.realms.len() {
            let realm = &self.realms[frame.index];
            realm.arm(self.scenario.cpu_ms);
            let stepped = if frame.resume_step { scheduler::resume_step(&realm.lua).map(|_| ()) }
                else if frame.timed { realm.step(frame.now) } else { scheduler::flush(&realm.lua, frame.now).map(|_| ()) };
            if let Err(error) = stepped { realm.wall_time_exceeded.set(false); self.output.log(&realm.config.id, &realm.config.side, "error", &error.to_string()); return Err(error); }
            if realm.wall_time_exceeded.replace(false) {
                let error = failure("BudgetExceeded: Luau wall-time budget");
                self.output.log(&realm.config.id, &realm.config.side, "error", &error.to_string()); return Err(error);
            }
            if self.network_blocked() { frame.resume_step = true; self.interrupted_pump = Some(frame); return Ok(()); }
            frame.resume_step = false;
            if frame.dt > 0.0 {
                realm.arm(self.scenario.cpu_ms);
                if let Err(error) = realm.frame(frame.dt) { realm.wall_time_exceeded.set(false); self.output.log(&realm.config.id, &realm.config.side, "error", &error.to_string()); return Err(error); }
                if realm.wall_time_exceeded.replace(false) {
                    let error = failure("BudgetExceeded: Luau wall-time budget");
                    self.output.log(&realm.config.id, &realm.config.side, "error", &error.to_string()); return Err(error);
                }
                if self.network_blocked() { frame.index += 1; self.interrupted_pump = Some(frame); return Ok(()); }
            }
            frame.index += 1;
        }
        sync_realms(&self.realms, &self.scenario, &mut self.server_nodes, &mut self.known)?;
        if self.network_blocked() { return Ok(()); }
        route(&self.host, &self.realms, &self.scenario)
    }

    pub(crate) fn create_gc_viewer(&mut self, realm: &str) -> Result<()> {
        if self.network_blocked() { return Err(failure("VMs are paused for network approval")); }
        let realm=self.realms.iter().find(|candidate|candidate.config.id==realm && candidate.config.side=="client" && candidate.config.executor)
            .ok_or_else(||failure("Select a client VM with executor enabled"))?;
        realm.arm(self.scenario.cpu_ms);
        crate::inspector::create_host_viewer(&realm.lua)
    }

    pub(crate) fn keyboard_input(&mut self, realm: &str, key: &str, down: bool) -> Result<()> {
        if self.network_blocked() { return Err(failure("VMs are paused for network approval")); }
        let config=&self.realms.iter().find(|candidate|candidate.config.id==realm && candidate.config.side=="client")
            .ok_or_else(||failure("Select a client VM for keyboard input"))?.config;
        let client=config.client_id.clone();
        for candidate in self.realms.iter().filter(|candidate|candidate.config.side=="client" && candidate.config.client_id==client) {
            candidate.arm(self.scenario.cpu_ms);
            model::keyboard_input(&candidate.lua,key,down)?;
        }
        self.pump(self.host.clock.get(),0.0,false)
    }

    pub fn advance(&mut self, seconds: f64) -> Result<Json> {
        if self.network_blocked() { return self.snapshot(); }
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
            if self.network_blocked() { self.advance_target = Some(target); break; }
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
            "traceCaughtErrors":self.trace_enabled,
            "metrics":{"lastStepMs":self.last_step_ms,"meanStepMs":if self.measured_steps>0 {Some(self.total_step_ms/self.measured_steps as f64)} else {None},
                "stepCount":self.measured_steps,"virtualStepMs":self.last_virtual_step_ms,"configuredStepMs":self.scenario.step*1000.0,"vmCount":self.realms.len(),"pendingMessages":self.host.messages.borrow().len()},
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

    pub(crate) fn packet_spy(&mut self, body:&Json)->Result<Json> {
        if self.network_blocked(){return Err(failure("VMs are paused for network approval"));}
        let action=body.get("action").and_then(Json::as_str).ok_or_else(||failure("Missing remote spy action"))?;
        let realm=body.get("realm").and_then(Json::as_str).ok_or_else(||failure("Select a VM"))?;
        let vm=self.realms.iter().find(|item|item.config.id==realm).ok_or_else(||failure("Select an available VM"))?;
        let mut spy=self.host.packet_spy.borrow_mut();
        if matches!(action,"block"|"unblock") {
            let id=body.get("remoteId").and_then(Json::as_str).filter(|id|!id.is_empty()).ok_or_else(||failure("Select a RemoteEvent"))?;
            let direction=body.get("direction").and_then(Json::as_str).ok_or_else(||failure("Select incoming or outgoing"))?;
            let identity=if action=="block" {model::remote_spy_event_identity(&vm.lua,id)} else {None};
            spy.set_block(realm,id,direction,identity,action=="block").map_err(failure)?;
        } else {spy.control(action,realm).map_err(failure)?;}
        spy.prune_blocks(&vm.lua,realm);
        Ok(spy.snapshot(realm))
    }

    pub(crate) fn workspace_files(&self, realm: &str) -> Result<Json> {
        let realm=self.realms.iter().find(|candidate|candidate.config.id==realm && candidate.config.side=="client" && candidate.config.executor)
            .ok_or_else(||failure("Select a client VM with an executor"))?;
        let files=crate::executor::virtual_files(&realm.lua)?.into_iter()
            .map(|(name,size)|json!({"name":name,"size":size})).collect::<Vec<_>>();
        Ok(json!({"files":files}))
    }
    pub(crate) fn workspace_file(&self, realm: &str, name: &str) -> Result<Json> {
        let realm=self.realms.iter().find(|candidate|candidate.config.id==realm && candidate.config.side=="client" && candidate.config.executor)
            .ok_or_else(||failure("Select a client VM with an executor"))?;
        let bytes=crate::executor::virtual_file(&realm.lua,name)?;
        Ok(json!({"name":name,"base64":base64::encode(&bytes)}))
    }

    pub fn clear_output(&mut self) { self.host.logs.borrow_mut().clear(); }
    pub(crate) fn last_output(&self) -> Json {json!(*self.host.logs.borrow())}

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
            "tree"=>{
                let mut page=model::inspect_tree(&realm.lua,match body.get("parent"){None|Some(Json::Null)=>None,_=>Some(text("parent")?)},offset)?;
                page["inspectorId"]=json!(crate::inspector::identity(&realm.lua)?);Ok(page)
            },
            "gameRoot"=>{
                let mut page=model::inspect_game_root(&realm.lua)?;
                page["inspectorId"]=json!(crate::inspector::identity(&realm.lua)?);Ok(page)
            },
            "nilInstances"=>{
                let mut page=model::inspect_nil_instances(&realm.lua,offset)?;
                page["inspectorId"]=json!(crate::inspector::identity(&realm.lua)?);Ok(page)
            },
            "instance"=>model::inspect_node(&realm.lua,text("id")?),
            "source"=>{
                let identity=crate::inspector::identity(&realm.lua)?;
                if text("inspectorId")?!=identity {return Err(failure("Inspector belongs to another VM or an expired session"))}
                let mut result=model::inspect_source(&realm.lua,text("id")?,text("objectId")?)?;
                result["inspectorId"]=json!(identity);Ok(result)
            },
            "gc"|"closeGc" if context!="executor"=>Err(failure("GC Viewer requires Executor context")),
            "gc"=>crate::inspector::gc_page(&realm.lua,text("viewer")?,body.get("refresh").and_then(Json::as_bool).unwrap_or(false),offset,
                if body.get("query").is_some(){text("query")?}else{""},if body.get("valueType").is_some(){text("valueType")?}else{""}),
            "closeGc"=>crate::inspector::close_viewer(&realm.lua,text("viewer")?),
            _=>Err(failure("Unknown inspector operation")),
        }
    }

    pub(crate) fn runtime_update(&mut self, body: &Json, running: bool) -> Result<Json> {
        if self.network_blocked() { return Err(failure("VMs are paused for network approval")); }
        let text = |key: &str| -> Result<&str> {
            body.get(key).and_then(Json::as_str).filter(|s| s.len() <= 512)
                .ok_or_else(|| failure(format!("Invalid runtime {key}")))
        };
        let realm_id = text("realm")?;
        let context = text("context")?;
        let result = {
            let realm = self.realms.iter().find(|realm| realm.config.id == realm_id).ok_or_else(|| failure("Unknown realm"))?;
            if context != realm.config.side && !(context == "executor" && realm.config.side == "client" && realm.config.executor) {
                return Err(failure("Context is not available in the selected realm"));
            }
            if text("inspectorId")? != crate::inspector::identity(&realm.lua)? {
                return Err(failure("Inspector belongs to another VM or an expired session"));
            }
            realm.arm(self.scenario.cpu_ms);
            if running { model::runtime_update(&realm.lua, body)? }
            else { scheduler::with_deferred_signals(&realm.lua, || model::runtime_update(&realm.lua, body))? }
        };
        // A paused edit changes only this VM immediately. Its queued signals and
        // replication are delivered by the next scheduler step after resuming.
        if running { self.pump(self.host.clock.get(), 0.0, false)?; }
        Ok(result)
    }

    pub fn set_cpu_budget(&mut self, ms: u64) -> Result<()> {
        if !(10..=10000).contains(&ms) { return Err(failure("CPU budget requires 10..10000 milliseconds")); }
        self.scenario.cpu_ms = ms;
        self.cpu_budget.set(ms);
        for realm in &self.realms { realm.arm(ms); }
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(network) = &self.host.network { network.cancel_all(); }
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
