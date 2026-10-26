use mlua::{AnyUserData, Error, FromLuaMulti, Function, IntoLuaMulti, Lua, MultiValue, Result, Table, Thread, UserData, UserDataFields, Value, ffi};
use mlua::state::NativeCall;
use std::{cell::RefCell, collections::{HashMap, VecDeque}, ffi::CStr, rc::{Rc, Weak}, time::Instant};

unsafe extern "C-unwind" { fn emu_getmemcat(state: *mut ffi::lua_State) -> i32; }
const MEMORY_ENVIRONMENTS: &str = "emulator.memory.default.environments";
const MEMORY_THREADS: &str = "emulator.memory.default.threads";

pub type Completion = Rc<dyn Fn(&Lua, std::result::Result<MultiValue, Value>) -> Result<()>>;
pub type YieldObserver = Rc<dyn Fn(&Lua, MultiValue) -> Result<()>>;
type Shared = Rc<RefCell<Scheduler>>;

#[derive(serde::Serialize)]
pub struct Status { pub pending: usize, pub parked: usize, pub errors: usize }

struct Scheduler {
    now: f64,
    tick: u64,
    sequence: u64,
    resumes: usize,
    resume_depth: usize,
    max_resume_depth: usize,
    max_tasks: usize,
    max_resumes: usize,
    actor: bool,
    deferred_signals: bool,
    tasks: HashMap<usize, Task>,
    queue: Vec<Job>,
    rt: Table,
    errors: Table,
    view_methods: Table,
    profile_threads: Table,
    profiles_begun: u64,
    profiles_completed: u64,
    profile_samples: VecDeque<ProfileSample>,
    memory_names: Vec<mlua::LuaString>,
    memory_ids: HashMap<mlua::LuaString, u8>,
}

struct ProfileScope { label: String, started: Instant }
struct ProfileSample { label: String, elapsed: f64 }
struct ProfileStack(Vec<ProfileScope>);
impl UserData for ProfileStack {}

struct Task {
    thread: Thread,
    mode: String,
    origin: String,
    parked: bool,
    waiter: Option<Weak<RefCell<SignalData>>>,
    completion: Option<Completion>,
    yield_observer: Option<YieldObserver>,
    pending_connection: Option<Rc<RefCell<ConnectionData>>>,
}

struct Job {
    thread: Thread,
    args: MultiValue,
    error: Option<Value>,
    at: f64,
    tick: u64,
    sequence: u64,
    started: Option<f64>,
}

#[derive(Clone)]
struct Signal(Rc<RefCell<SignalData>>);
#[derive(Default)]
struct SignalData {
    label: String,
    handlers: Vec<Rc<RefCell<ConnectionData>>>,
    waiters: Vec<Thread>,
}
#[derive(Clone)]
struct Connection(Rc<RefCell<ConnectionData>>);
#[derive(Clone)]
struct ConnectionView(Rc<RefCell<ConnectionData>>);
struct ConnectionData {
    signal: Weak<RefCell<SignalData>>,
    callback: Value,
    environment: Table,
    mode: String,
    origin: String,
    connected: bool,
    enabled: bool,
    once: bool,
}

impl UserData for Signal {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) { fields.add_meta_field("__type", "RBXScriptSignal"); }
}
impl UserData for Connection {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) { fields.add_meta_field("__type", "RBXScriptConnection"); }
}
impl UserData for ConnectionView {}

fn fail(message: impl Into<String>) -> Error { Error::lua_string(message) }
fn shared(lua: &Lua) -> Result<Shared> {
    lua.app_data_ref::<Shared>().map(|value| value.clone()).ok_or_else(|| fail("scheduler is not installed"))
}
pub fn status(lua: &Lua) -> Result<Status> {
    let shared = shared(lua)?;
    let state = shared.borrow();
    Ok(Status {
        pending: state.queue.len(),
        parked: state.tasks.values().filter(|task| task.parked && task.thread.is_resumable()).count(),
        errors: state.errors.raw_len(),
    })
}
fn key(thread: &Thread) -> usize { thread.state() as usize }
fn clear_profile(lua: &Lua, thread: &Thread) -> Result<()> {
    let profiles = shared(lua)?.borrow().profile_threads.clone();
    if !profiles.raw_get::<Value>(thread.clone())?.is_nil() { profiles.raw_set(thread.clone(), Value::Nil)?; }
    Ok(())
}

fn memory_category(lua: &Lua, name: mlua::LuaString) -> Result<u8> {
    let shared = shared(lua)?;
    let mut state = shared.borrow_mut();
    if let Some(category) = state.memory_ids.get(&name) { return Ok(*category); }
    if state.memory_names.len() >= 256 { return Err(fail("MemoryCategoryLimit: Luau allocation categories exhausted")); }
    let category = state.memory_names.len() as u8;
    state.memory_ids.insert(name.clone(), category);
    state.memory_names.push(name);
    Ok(category)
}

pub fn set_script_category(lua: &Lua, thread: &Thread, function: &Function, name: &str) -> Result<()> {
    let category = memory_category(lua, lua.create_string(name)?)?;
    lua.named_registry_value::<Table>(MEMORY_THREADS)?.raw_set(thread.clone(), category)?;
    if let Some(environment) = function.environment() {
        lua.named_registry_value::<Table>(MEMORY_ENVIRONMENTS)?.raw_set(environment, category)?;
    }
    unsafe { ffi::lua_setmemcat(thread.state(), category as i32); }
    Ok(())
}

pub fn set_thread_environment(lua: &Lua, thread: &Thread, environment: &Table) -> Result<()> {
    let mut assigned = false;
    unsafe {
        lua.exec_raw::<()>((thread.clone(), environment.clone()), |state| {
            assigned = ffi::lua_setfenv(state, 1) != 0;
            ffi::lua_settop(state, 0);
        })?;
    }
    if assigned { Ok(()) } else { Err(fail("Unable to assign thread environment")) }
}

// Called in the original native frame before Rust borrows. Unmanaged coroutines
// recover their Script default from the real function environment, independently
// of the active allocation category they inherited from coroutine.create.
unsafe fn private_memory_category(state: *mut ffi::lua_State) -> Option<i32> {
    unsafe {
        if ffi::lua_type(state, -1) == ffi::LUA_TNIL { return None; }
        if ffi::lua_type(state, -1) == ffi::LUA_TNUMBER {
            let value = ffi::lua_tonumber(state, -1);
            if value.is_finite() && (0.0..256.0).contains(&value) && value.fract() == 0.0 { return Some(value as i32); }
        }
        ffi::lua_pushstring(state, c"MemoryCategoryError: invalid private allocation category".as_ptr());
        ffi::lua_error(state)
    }
}
unsafe fn default_memory_category(state: *mut ffi::lua_State) -> i32 {
    unsafe {
        ffi::luaL_checkstack(state, 5, c"memory category lookup".as_ptr());
        let top = ffi::lua_gettop(state);
        ffi::lua_rawgetfield(state, ffi::LUA_REGISTRYINDEX, c"emulator.memory.default.threads".as_ptr());
        if ffi::lua_type(state, -1) != ffi::LUA_TTABLE {
            ffi::lua_pushstring(state, c"MemoryCategoryError: private thread map is not a table".as_ptr());
            ffi::lua_error(state);
        }
        let threads = top + 1;
        ffi::lua_pushthread(state);
        ffi::lua_rawget(state, threads);
        if let Some(category) = private_memory_category(state) {
            ffi::lua_settop(state, top);
            return category;
        }
        ffi::lua_pop(state, 1);
        ffi::lua_rawgetfield(state, ffi::LUA_REGISTRYINDEX, c"emulator.memory.default.environments".as_ptr());
        if ffi::lua_type(state, -1) != ffi::LUA_TTABLE {
            ffi::lua_pushstring(state, c"MemoryCategoryError: private environment map is not a table".as_ptr());
            ffi::lua_error(state);
        }
        let environments = top + 2;
        let mut category = 0;
        for level in 0..20_000 {
            let mut debug: ffi::lua_Debug = std::mem::zeroed();
            if ffi::lua_getinfo(state, level, c"f".as_ptr(), &mut debug) == 0 { break; }
            ffi::lua_getfenv(state, -1);
            ffi::lua_rawget(state, environments);
            if let Some(value) = private_memory_category(state) {
                category = value;
                break;
            }
            ffi::lua_settop(state, environments);
        }
        ffi::lua_settop(state, environments);
        ffi::lua_pushthread(state);
        ffi::lua_pushinteger(state, category.into());
        ffi::lua_rawset(state, threads);
        ffi::lua_settop(state, top);
        category
    }
}
fn duration(value: Value) -> Result<f64> {
    let value = match value { Value::Nil => 0.0, Value::Number(value) => value, Value::Integer(value) => value as f64, _ => return Err(fail("duration must be a finite number")) };
    if !value.is_finite() { return Err(fail("duration must be a finite number")); }
    Ok(value.max(0.0))
}
fn take(args: &mut MultiValue) -> Value { args.pop_front().unwrap_or(Value::Nil) }
fn string(value: Value) -> Result<String> {
    match value { Value::String(value) => Ok(value.to_str()?.to_string()), _ => Err(fail("expected string")) }
}
fn optional_string(value: Value) -> Result<Option<String>> {
    if value.is_nil() { Ok(None) } else { string(value).map(Some) }
}
fn thread(value: Value) -> Result<Thread> {
    match value { Value::Thread(value) => Ok(value), _ => Err(fail("expected thread")) }
}
fn userdata(value: Value) -> Result<AnyUserData> {
    match value { Value::UserData(value) => Ok(value), _ => Err(fail("expected userdata")) }
}
fn signal_data(signal: &AnyUserData) -> Result<Rc<RefCell<SignalData>>> {
    signal.borrow::<Signal>().map(|signal| signal.0.clone()).map_err(|_| fail("expected RBXScriptSignal"))
}
fn connection_data(value: &AnyUserData) -> Result<Rc<RefCell<ConnectionData>>> {
    if let Ok(connection) = value.borrow::<Connection>() { return Ok(connection.0.clone()); }
    value.borrow::<ConnectionView>().map(|connection| connection.0.clone()).map_err(|_| fail("expected RBXScriptConnection"))
}
fn one(value: Value) -> MultiValue { MultiValue::from_vec(vec![value]) }
fn returned(value: Value) -> NativeCall { NativeCall::Return(one(value)) }
fn empty() -> NativeCall { NativeCall::Return(MultiValue::new()) }

pub fn current_mode(lua: &Lua) -> String {
    shared(lua).ok().and_then(|state| state.borrow().tasks.get(&(lua.state() as usize)).map(|task| task.mode.clone())).unwrap_or_else(|| "serial".into())
}
pub fn current_origin(lua: &Lua) -> String {
    shared(lua).ok().and_then(|state| state.borrow().tasks.get(&(lua.state() as usize)).map(|task| task.origin.clone())).unwrap_or_else(|| "game".into())
}

fn ensure_task(lua: &Lua, thread: &Thread, mode: Option<&str>, origin: Option<&str>) -> Result<()> {
    let mode = mode.map(str::to_owned).unwrap_or_else(|| current_mode(lua));
    let origin = origin.map(str::to_owned).unwrap_or_else(|| current_origin(lua));
    let shared = shared(lua)?;
    let mut state = shared.borrow_mut();
    if !state.tasks.contains_key(&key(thread)) {
        if state.tasks.len() >= state.max_tasks { return Err(fail("SchedulerLimit: task limit exceeded")); }
        state.tasks.insert(key(thread), Task { thread: thread.clone(), mode, origin, parked: false, waiter: None, completion: None, yield_observer: None, pending_connection: None });
    }
    Ok(())
}
fn to_thread(lua: &Lua, callable: Value, mode: Option<&str>, origin: Option<&str>) -> Result<Thread> {
    let thread = match callable {
        Value::Function(function) => {
            let defaults: Table = lua.named_registry_value(MEMORY_THREADS)?;
            let parent_default: Option<u8> = defaults.raw_get(lua.current_thread())?;
            let category = if let Some(category) = parent_default { category } else {
                function.environment().map(|environment| lua.named_registry_value::<Table>(MEMORY_ENVIRONMENTS)?.raw_get::<Option<u8>>(environment)).transpose()?.flatten().unwrap_or(0)
            };
            let thread = lua.create_thread(function)?;
            defaults.raw_set(thread.clone(), category)?;
            thread
        }
        Value::Thread(thread) if thread.is_resumable() => thread,
        Value::Thread(_) => return Err(fail("thread is not suspended")),
        _ => return Err(fail("expected function or thread")),
    };
    ensure_task(lua, &thread, mode, origin)?;
    if mode.is_some() || origin.is_some() {
        if let Some(task) = shared(lua)?.borrow_mut().tasks.get_mut(&key(&thread)) {
            if let Some(mode) = mode { task.mode = mode.into(); }
            if let Some(origin) = origin { task.origin = origin.into(); }
        }
    }
    Ok(thread)
}

fn enqueue(lua: &Lua, thread: Thread, args: MultiValue, error: Option<Value>, delay: f64, started: Option<f64>) -> Result<()> {
    let shared = shared(lua)?;
    let mut state = shared.borrow_mut();
    if state.queue.len() >= state.max_tasks { return Err(fail("SchedulerLimit: queued task limit exceeded")); }
    state.sequence += 1;
    let job = Job { thread, args, error, at: state.now + delay, tick: state.tick + 1, sequence: state.sequence, started };
    state.queue.push(job);
    Ok(())
}

pub fn schedule(lua: &Lua, thread: Thread, args: MultiValue, mode: Option<&str>) -> Result<()> {
    if thread.is_finished() || thread.is_error() { return Ok(()); }
    ensure_task(lua, &thread, None, None)?;
    let waiter = if let Some(task) = shared(lua)?.borrow_mut().tasks.get_mut(&key(&thread)) {
        task.parked = false;
        if let Some(mode) = mode { task.mode = mode.into(); }
        task.waiter.take()
    } else { None };
    detach_waiter(&thread, waiter);
    enqueue(lua, thread, args, None, 0.0, None)
}
pub fn schedule_error(lua: &Lua, thread: Thread, error: Value) -> Result<()> {
    if thread.is_finished() || thread.is_error() { return Ok(()); }
    ensure_task(lua, &thread, None, None)?;
    let waiter = if let Some(task) = shared(lua)?.borrow_mut().tasks.get_mut(&key(&thread)) { task.parked = false; task.waiter.take() } else { None };
    detach_waiter(&thread, waiter);
    enqueue(lua, thread, MultiValue::new(), Some(error), 0.0, None)
}

fn detach_waiter(thread: &Thread, waiter: Option<Weak<RefCell<SignalData>>>) {
    if let Some(signal) = waiter.and_then(|signal| signal.upgrade()) {
        signal.borrow_mut().waiters.retain(|candidate| key(candidate) != key(thread));
    }
}

struct StackRestore(*mut ffi::lua_State, i32);
impl Drop for StackRestore { fn drop(&mut self) { unsafe { ffi::lua_settop(self.0, self.1); } } }

// Direct protected resume preserves Lua-thrown table/userdata identity and adds no VM adapter frame.
fn resume_raw(lua: &Lua, thread: &Thread, args: MultiValue, error: Option<Value>) -> Result<(bool, std::result::Result<MultiValue, Value>)> {
    lua.exec_raw_lua(|raw| unsafe {
        let caller = raw.state();
        let target = thread.state();
        let _restore = StackRestore(caller, ffi::lua_gettop(caller));
        let is_error = error.is_some();
        let values = error.map(one).unwrap_or(args);
        let count = values.push_into_stack_multi(raw)?;
        if ffi::lua_checkstack(target, count + 1) == 0 { return Err(Error::StackError); }
        ffi::lua_xmove(caller, target, count);
        // Engine-owned tasks enter independently. User coroutine.resume keeps Luau's own caller accounting.
        let status = if is_error { ffi::lua_resumeerror(target, std::ptr::null_mut()) } else { ffi::lua_resume_(target, std::ptr::null_mut(), count) };
        let count = if status == ffi::LUA_OK || status == ffi::LUA_YIELD { ffi::lua_gettop(target) } else { 1 };
        if ffi::lua_checkstack(caller, count + 1) == 0 { ffi::lua_settop(target, 0); return Err(Error::StackError); }
        ffi::lua_xmove(target, caller, count);
        let values = MultiValue::from_stack_multi(count, raw)?;
        if status == ffi::LUA_OK || status == ffi::LUA_YIELD { Ok((status == ffi::LUA_YIELD, Ok(values))) }
        else { Ok((false, Err(values.into_iter().next().unwrap_or(Value::Nil)))) }
    })
}

fn error_text(lua: &Lua, error: Value) -> Result<String> {
    const NO_OUTPUT: &str = "Error occurred, no output from Luau.";
    match error {
        Value::String(message) => Ok(message.to_string_lossy()),
        number @ (Value::Integer(_) | Value::Number(_)) => Ok(lua.coerce_string(number)?.map(|message| message.to_string_lossy()).unwrap_or_else(|| NO_OUTPUT.into())),
        _ => Ok(NO_OUTPUT.into()),
    }
}
fn report(lua: &Lua, message: String, thread: &Thread) -> Result<()> {
    let (errors, now) = { let state = shared(lua)?; let state = state.borrow(); (state.errors.clone(), state.now) };
    if errors.raw_len() >= 1000 { return Err(fail("SchedulerLimit: error log limit exceeded")); }
    let record = lua.create_table()?;
    record.raw_set("time", now)?;
    record.raw_set("message", message.clone())?;
    errors.raw_set(errors.raw_len() + 1, record)?;
    if let Ok(logger) = crate::internal::<Function>(lua, "__host_log") { logger.call::<()>(("error", message.as_str()))?; }
    crate::model::script_error(lua, &message, thread)
}
fn resume_task(lua: &Lua, thread: Thread, args: MultiValue, error: Option<Value>) -> Result<()> {
    if !thread.is_resumable() { return Ok(()); }
    {
        let shared = shared(lua)?;
        let mut state = shared.borrow_mut();
        if !state.tasks.contains_key(&key(&thread)) { return Ok(()); }
        if state.resume_depth >= state.max_resume_depth {
            state.tasks.remove(&key(&thread));
            return Err(fail("SchedulerLimit: synchronous engine resumption depth exceeded"));
        }
        state.resumes += 1;
        if state.resumes > state.max_resumes { return Err(fail("SchedulerLimit: resumption budget exceeded")); }
        state.resume_depth += 1;
        if let Some(task) = state.tasks.get_mut(&key(&thread)) { task.parked = true; task.pending_connection = None; }
    }
    let depth_guard = ResumeDepth(shared(lua)?);
    let resumed = resume_raw(lua, &thread, args, error);
    drop(depth_guard);
    match resumed {
        Ok((true, Ok(values))) => {
            let observer = shared(lua)?.borrow().tasks.get(&key(&thread)).and_then(|task| task.yield_observer.clone());
            if let Some(observer) = observer { observer(lua, values)?; }
            Ok(())
        }
        result => {
            let task = shared(lua)?.borrow_mut().tasks.remove(&key(&thread));
            clear_profile(lua, &thread)?;
            let outcome = match result {
                Ok((false, result)) => result,
                Err(error) => Err(Value::Error(Box::new(error))),
                _ => unreachable!(),
            };
            if let Some(completion) = task.and_then(|task| task.completion) { completion(lua, outcome) }
            else if let Err(error) = outcome { report(lua, error_text(lua, error)?, &thread) }
            else { Ok(()) }
        }
    }
}

struct ResumeDepth(Shared);
impl Drop for ResumeDepth { fn drop(&mut self) { self.0.borrow_mut().resume_depth -= 1; } }

pub fn spawn_with(lua: &Lua, callable: Value, args: MultiValue, mode: Option<&str>, origin: Option<&str>) -> Result<Thread> {
    let thread = to_thread(lua, callable, mode, origin)?;
    resume_task(lua, thread.clone(), args, None)?;
    Ok(thread)
}
pub fn spawn_tracked(lua: &Lua, callable: Value, args: MultiValue, mode: Option<&str>, origin: Option<&str>, completion: Completion) -> Result<Thread> {
    spawn_observed(lua, callable, args, mode, origin, completion, None)
}
pub fn spawn_observed(lua: &Lua, callable: Value, args: MultiValue, mode: Option<&str>, origin: Option<&str>, completion: Completion, observer: Option<YieldObserver>) -> Result<Thread> {
    let thread = to_thread(lua, callable, mode, origin)?;
    {
        let shared = shared(lua)?;
        let mut state = shared.borrow_mut();
        let task = state.tasks.get_mut(&key(&thread)).unwrap();
        task.completion = Some(completion);
        task.yield_observer = observer;
    }
    resume_task(lua, thread.clone(), args, None)?;
    Ok(thread)
}

pub fn park_current(lua: &Lua) -> Result<()> {
    if unsafe { ffi::lua_isyieldable(lua.state()) } == 0 { return Err(fail("attempt to yield across metamethod/C-call boundary")); }
    let thread = lua.current_thread();
    ensure_task(lua, &thread, None, None)?;
    shared(lua)?.borrow_mut().tasks.get_mut(&key(&thread)).unwrap().parked = true;
    Ok(())
}
fn wait(lua: &Lua, seconds: f64) -> Result<()> {
    park_current(lua)?;
    let now = shared(lua)?.borrow().now;
    let thread = lua.current_thread();
    shared(lua)?.borrow_mut().tasks.get_mut(&key(&thread)).unwrap().parked = false;
    enqueue(lua, thread, MultiValue::new(), None, seconds, Some(now))
}
pub fn cancel(lua: &Lua, thread: Thread) -> Result<()> {
    if thread.state() == lua.state() || thread.is_normal() { return Err(fail("cannot cancel the running thread")); }
    let task = {
        let shared = shared(lua)?;
        let mut state = shared.borrow_mut();
        state.queue.retain(|job| key(&job.thread) != key(&thread));
        state.tasks.remove(&key(&thread))
    };
    clear_profile(lua, &thread)?;
    detach_waiter(&thread, task.and_then(|task| task.waiter));
    if thread.is_resumable() { unsafe { ffi::lua_resetthread(thread.state()); } }
    Ok(())
}
pub fn step(lua: &Lua, now: f64) -> Result<usize> {
    step_mode(lua, now, true)
}
pub fn flush(lua: &Lua, now: f64) -> Result<usize> {
    step_mode(lua, now, false)
}
fn step_mode(lua: &Lua, now: f64, timed_waits: bool) -> Result<usize> {
    let ready = {
        let shared = shared(lua)?;
        let mut state = shared.borrow_mut();
        if !now.is_finite() || now < state.now { return Err(fail("scheduler time must be finite and nondecreasing")); }
        state.now = now;
        state.tick += 1;
        state.resumes = 0;
        state.rt.raw_set("now", now)?;
        state.queue.sort_by(|a, b| a.at.total_cmp(&b.at).then(a.sequence.cmp(&b.sequence)));
        let tick = state.tick;
        let (ready, pending) = std::mem::take(&mut state.queue).into_iter().partition::<Vec<_>, _>(|job| job.at <= now && job.tick <= tick && (timed_waits || job.started.is_none()));
        state.queue = pending;
        ready
    };
    for job in ready {
        let args = job.started.map(|started| one(Value::Number(now - started))).unwrap_or(job.args);
        resume_task(lua, job.thread, args, job.error)?;
    }
    Ok(shared(lua)?.borrow().queue.len())
}

pub fn new_signal(lua: &Lua) -> Result<AnyUserData> { new_named_signal(lua, "") }
pub fn new_named_signal(lua: &Lua, label: &str) -> Result<AnyUserData> {
    lua.create_userdata(Signal(Rc::new(RefCell::new(SignalData { label: label.into(), ..SignalData::default() }))))
}
pub fn wait_signal(lua: &Lua, signal: &AnyUserData) -> Result<()> {
    let data = signal_data(signal)?;
    park_current(lua)?;
    let thread = lua.current_thread();
    data.borrow_mut().waiters.push(thread.clone());
    shared(lua)?.borrow_mut().tasks.get_mut(&key(&thread)).unwrap().waiter = Some(Rc::downgrade(&data));
    Ok(())
}
pub fn connect_signal(lua: &Lua, signal: &AnyUserData, callback: Function, parallel: bool, once: bool) -> Result<AnyUserData> {
    connect_value(lua, signal, Value::Function(callback), parallel, once)
}
fn connect_value(lua: &Lua, signal: &AnyUserData, callback: Value, parallel: bool, once: bool) -> Result<AnyUserData> {
    let data = signal_data(signal)?;
    let state = shared(lua)?;
    if parallel && !state.borrow().actor { return Err(fail("ConnectParallel requires an Actor")); }
    if data.borrow().handlers.len() >= state.borrow().max_tasks { return Err(fail("SchedulerLimit: signal connection limit exceeded")); }
    let connection = Rc::new(RefCell::new(ConnectionData {
        signal: Rc::downgrade(&data), callback, mode: if parallel { "parallel" } else { "serial" }.into(),
        environment: lua.globals(), origin: current_origin(lua), connected: true, enabled: true, once,
    }));
    let value = lua.create_userdata(Connection(connection.clone()))?;
    data.borrow_mut().handlers.push(connection);
    Ok(value)
}
fn disconnect(lua: &Lua, connection: &Rc<RefCell<ConnectionData>>, drop_pending: bool) -> Result<()> {
    let signal = { let mut connection = connection.borrow_mut(); connection.connected = false; connection.signal.upgrade() };
    if let Some(signal) = signal { signal.borrow_mut().handlers.retain(|candidate| !Rc::ptr_eq(candidate, connection)); }
    if drop_pending {
        let pending: Vec<_> = shared(lua)?.borrow().tasks.values()
            .filter(|task| task.pending_connection.as_ref().is_some_and(|candidate| Rc::ptr_eq(candidate, connection)))
            .map(|task| task.thread.clone()).collect();
        for thread in pending { cancel(lua, thread)?; }
    }
    Ok(())
}
pub fn disconnect_all(_lua: &Lua, signal: &AnyUserData) -> Result<()> {
    let data = signal_data(signal)?;
    let handlers = std::mem::take(&mut data.borrow_mut().handlers);
    for handler in handlers { handler.borrow_mut().connected = false; }
    Ok(())
}
pub fn fire_signal(lua: &Lua, signal: &AnyUserData, args: MultiValue) -> Result<()> {
    let data = signal_data(signal)?;
    let (waiters, handlers) = { let mut data = data.borrow_mut(); (std::mem::take(&mut data.waiters), data.handlers.clone()) };
    for waiter in waiters { schedule(lua, waiter, args.clone(), None)?; }
    let deferred = shared(lua)?.borrow().deferred_signals;
    for connection in handlers {
        let (callback, environment, mode, origin, once) = {
            let connection = connection.borrow();
            if !connection.connected || !connection.enabled { continue; }
            (connection.callback.clone(), connection.environment.clone(), connection.mode.clone(), connection.origin.clone(), connection.once)
        };
        if once { disconnect(lua, &connection, false)?; }
        let callback = callable_callback(lua, callback)?;
        let thread = to_thread(lua, Value::Function(callback), Some(&mode), Some(&origin))?;
        set_thread_environment(lua, &thread, &environment)?;
        if deferred {
            shared(lua)?.borrow_mut().tasks.get_mut(&key(&thread)).unwrap().pending_connection = Some(connection);
            enqueue(lua, thread, args.clone(), None, 0.0, None)?;
        } else { resume_task(lua, thread, args.clone(), None)?; }
    }
    Ok(())
}

// Roblox accepts arbitrary Connect values. The VM performs the eventual call (including __call/error).
unsafe extern "C-unwind" fn call_value(state: *mut ffi::lua_State) -> i32 {
    unsafe {
        let arguments = ffi::lua_gettop(state);
        ffi::lua_pushvalue(state, ffi::lua_upvalueindex(1));
        ffi::lua_insert(state, 1);
        ffi::lua_callyieldable(state, arguments, ffi::LUA_MULTRET)
    }
}
unsafe extern "C-unwind" fn continue_value(state: *mut ffi::lua_State, status: i32) -> i32 {
    unsafe { if status != ffi::LUA_OK { ffi::lua_error(state) } else { ffi::lua_gettop(state) } }
}
fn callable_callback(lua: &Lua, value: Value) -> Result<Function> {
    if let Value::Function(function) = value { return Ok(function); }
    unsafe { lua.exec_raw(value, |state| ffi::lua_pushcclosurek(state, call_value, std::ptr::null(), 1, Some(continue_value))) }
}

#[repr(i32)]
#[derive(Clone, Copy)]
enum Op {
    Spawn, SpawnWith, Defer, Delay, Wait, Cancel, Park, Schedule, Step, Status, GetMode, GetOrigin, SetMode,
    NewSignal, GetConnections, Synchronize, Desynchronize, Print, Warn, Time, Tick,
    SignalIndex, SignalString, SignalReadOnly, Connect, ConnectParallel, Once, SignalWait, Fire, DisconnectAll,
    ConnectionIndex, ConnectionString, ConnectionReadOnly, Disconnect, ViewIndex, Enable, Disable, ConnectionFire, ConnectionDefer,
    ProfileBegin, ProfileEnd, ProfileStatus,
    GetMemoryCategory, SetMemoryCategory, ResetMemoryCategory, MemoryStatus,
    DumpCodeSize, DumpHeap, DumpRefs, TraceRefs, LoadModule,
}
unsafe fn is_native_userdata(state: *mut ffi::lua_State, registry: &CStr) -> bool {
    unsafe {
        if ffi::lua_type(state, 1) != ffi::LUA_TUSERDATA || ffi::lua_getmetatable(state, 1) == 0 { return false; }
        ffi::lua_rawgetfield(state, ffi::LUA_REGISTRYINDEX, registry.as_ptr());
        let matches = ffi::lua_rawequal(state, -1, -2) != 0;
        ffi::lua_pop(state, 2);
        matches
    }
}
unsafe extern "C-unwind" fn dispatch(state: *mut ffi::lua_State) -> i32 {
    let opcode = unsafe { ffi::lua_tointeger(state, ffi::lua_upvalueindex(1)) as i32 };
    if opcode == Op::ProfileBegin as i32 {
        unsafe {
            if ffi::lua_isnoneornil(state, 1) != 0 {
                ffi::lua_pushstring(state, c"debug.profilebegin(string) parameter must be string, got an unexpected type. Not opening profile block.".as_ptr());
                ffi::lua_error(state);
            }
            ffi::luaL_checklstring(state, 1, std::ptr::null_mut());
        }
    }
    unsafe {
        if (Op::SignalIndex as i32..=Op::DisconnectAll as i32).contains(&opcode) {
            ffi::luaL_checkstack(state, 2, c"signal receiver".as_ptr());
            if !is_native_userdata(state, c"emulator.signal.metatable") { ffi::luaL_typeerror(state, 1, c"RBXScriptSignal".as_ptr()); }
        }
        if (Op::ConnectionIndex as i32..=Op::ConnectionDefer as i32).contains(&opcode) {
            ffi::luaL_checkstack(state, 2, c"connection receiver".as_ptr());
            if !is_native_userdata(state, c"emulator.connection.metatable") && !is_native_userdata(state, c"emulator.connectionview.metatable") {
                ffi::luaL_typeerror(state, 1, c"RBXScriptConnection".as_ptr());
            }
        }
        if [Op::SignalIndex as i32, Op::ConnectionIndex as i32, Op::ViewIndex as i32].contains(&opcode) {
            ffi::luaL_checklstring(state, 2, std::ptr::null_mut());
        }
        if opcode == Op::SetMemoryCategory as i32 { ffi::luaL_checklstring(state, 1, std::ptr::null_mut()); }
        if [Op::Spawn as i32, Op::SpawnWith as i32, Op::Defer as i32, Op::Delay as i32,
            Op::GetMemoryCategory as i32, Op::SetMemoryCategory as i32, Op::ResetMemoryCategory as i32].contains(&opcode) {
            let category = default_memory_category(state);
            if opcode == Op::ResetMemoryCategory as i32 { ffi::lua_setmemcat(state, category); }
        }
        let denied = match opcode {
            value if value == Op::DumpCodeSize as i32 => Some(c"dumpcodesize can only be called from CommandBar"),
            value if value == Op::DumpHeap as i32 => Some(c"debug.dumpheap is not enabled."),
            value if value == Op::DumpRefs as i32 => Some(c"debug.dumprefs is not enabled."),
            value if value == Op::TraceRefs as i32 => Some(c"debug.tracerefs is not enabled."),
            value if value == Op::LoadModule as i32 => Some(c"debug.loadmodule is not enabled."),
            _ => None,
        };
        if let Some(message) = denied { ffi::lua_pushstring(state, message.as_ptr()); ffi::lua_error(state); }
    }
    unsafe { Lua::invoke_native_callback(state, |lua, args| invoke(lua, opcode, args)) }
}
fn native_function(lua: &Lua, name: Option<&'static CStr>, op: Op) -> Result<Function> {
    unsafe { lua.exec_raw(op as i32, |state| ffi::lua_pushcclosurek(state, dispatch, name.map_or(std::ptr::null(), CStr::as_ptr), 1, None)) }
}
fn invoke(lua: &Lua, op: i32, mut args: MultiValue) -> Result<NativeCall> {
    let op = match op { value if (Op::Spawn as i32..=Op::LoadModule as i32).contains(&value) => unsafe { std::mem::transmute::<i32, Op>(value) }, _ => return Err(fail("invalid native scheduler opcode")) };
    match op {
        Op::Spawn | Op::SpawnWith => {
            let callable = take(&mut args);
            let (mode, origin) = if matches!(op, Op::SpawnWith) { (optional_string(take(&mut args))?, optional_string(take(&mut args))?) } else { (None, None) };
            Ok(returned(Value::Thread(spawn_with(lua, callable, args, mode.as_deref(), origin.as_deref())?)))
        }
        Op::Defer | Op::Delay => {
            let delay = if matches!(op, Op::Delay) { duration(take(&mut args))? } else { 0.0 };
            let thread = to_thread(lua, take(&mut args), None, None)?;
            enqueue(lua, thread.clone(), args, None, delay, None)?;
            Ok(returned(Value::Thread(thread)))
        }
        Op::Wait => { wait(lua, duration(take(&mut args))?)?; Ok(NativeCall::Yield(MultiValue::new())) }
        Op::Park => { park_current(lua)?; Ok(NativeCall::Yield(MultiValue::new())) }
        Op::Cancel => { cancel(lua, thread(take(&mut args))?)?; Ok(empty()) }
        Op::Schedule => {
            let thread = thread(take(&mut args))?;
            let packed = take(&mut args);
            let mode = optional_string(take(&mut args))?;
            let mut values = MultiValue::new();
            if let Value::Table(table) = packed {
                let count = table.raw_get::<Option<usize>>("n")?.unwrap_or(table.raw_len());
                if count > 10000 { return Err(fail("SchedulerLimit: argument count exceeded")); }
                for index in 1..=count { values.push_back(table.raw_get(index)?); }
            } else if !packed.is_nil() { return Err(fail("expected packed argument table")); }
            schedule(lua, thread, values, mode.as_deref())?;
            Ok(empty())
        }
        Op::Step => {
            let now = match take(&mut args) { Value::Number(value) => value, Value::Integer(value) => value as f64, _ => return Err(fail("scheduler time must be a number")) };
            Ok(returned(Value::Integer(step(lua, now)? as i64)))
        }
        Op::Status => {
            let current = status(lua)?;
            let table = lua.create_table()?;
            table.raw_set("pending", current.pending)?;
            table.raw_set("parked", current.parked)?;
            table.raw_set("errors", current.errors)?;
            Ok(returned(Value::Table(table)))
        }
        Op::GetMode => Ok(returned(Value::String(lua.create_string(current_mode(lua))?))),
        Op::GetOrigin => Ok(returned(Value::String(lua.create_string(current_origin(lua))?))),
        Op::SetMode => {
            let mode = string(take(&mut args))?;
            let current = lua.current_thread();
            ensure_task(lua, &current, Some(&mode), None)?;
            shared(lua)?.borrow_mut().tasks.get_mut(&key(&current)).unwrap().mode = mode;
            Ok(empty())
        }
        Op::Synchronize | Op::Desynchronize => {
            if !shared(lua)?.borrow().actor { return Err(fail("task phase switching requires an Actor")); }
            let mode = if matches!(op, Op::Synchronize) { "serial" } else { "parallel" };
            if current_mode(lua) == mode { return Ok(empty()); }
            park_current(lua)?;
            schedule(lua, lua.current_thread(), MultiValue::new(), Some(mode))?;
            Ok(NativeCall::Yield(MultiValue::new()))
        }
        Op::NewSignal => Ok(returned(Value::UserData(new_signal(lua)?))),
        Op::GetConnections => {
            let signal = signal_data(&userdata(take(&mut args))?)?;
            let handlers = signal.borrow().handlers.clone();
            let result = lua.create_table()?;
            for (index, handler) in handlers.into_iter().enumerate() { result.raw_set(index + 1, lua.create_userdata(ConnectionView(handler))?)?; }
            Ok(returned(Value::Table(result)))
        }
        Op::Print | Op::Warn => {
            let tostring: Function = lua.globals().get("tostring")?;
            let mut bytes = Vec::new();
            let mut strings = Vec::with_capacity(args.len());
            for (index, value) in args.iter().enumerate() {
                if index > 0 { bytes.push(b'\t'); }
                let text = tostring.call::<mlua::LuaString>(value.clone())?;
                bytes.extend_from_slice(&text.as_bytes());
                strings.push(text);
            }
            if let Ok(logger) = crate::internal::<Function>(lua, "__host_log") {
                let _capture = crate::console::stage(lua, &args, &strings);
                logger.call::<()>((if matches!(op, Op::Print) { "info" } else { "warn" }, lua.create_string(&bytes)?))?;
            }
            Ok(empty())
        }
        Op::Time | Op::Tick => Ok(returned(Value::Number(shared(lua)?.borrow().now + if matches!(op, Op::Tick) { 1700000000.0 } else { 0.0 }))),
        Op::GetMemoryCategory | Op::SetMemoryCategory => {
            let current = unsafe { emu_getmemcat(lua.state()) } as usize;
            let previous = shared(lua)?.borrow().memory_names.get(current).cloned().ok_or_else(|| fail("Unknown native allocation category"))?;
            if matches!(op, Op::SetMemoryCategory) {
                let Value::String(name) = take(&mut args) else { return Err(fail("memory category conversion failed")); };
                let category = memory_category(lua, name)?;
                unsafe { ffi::lua_setmemcat(lua.state(), category as i32); }
            }
            Ok(returned(Value::String(previous)))
        }
        Op::ResetMemoryCategory => Ok(empty()),
        Op::MemoryStatus => {
            let names = shared(lua)?.borrow().memory_names.clone();
            let result = lua.create_table()?;
            for (category, name) in names.into_iter().enumerate() {
                let record = lua.create_table()?;
                record.raw_set("name", name)?;
                record.raw_set("category", category)?;
                record.raw_set("bytes", unsafe { ffi::lua_totalbytes(lua.state(), category as i32) })?;
                result.raw_set(category + 1, record)?;
            }
            Ok(returned(Value::Table(result)))
        }
        Op::DumpCodeSize | Op::DumpHeap | Op::DumpRefs | Op::TraceRefs | Op::LoadModule => unreachable!("permission checked in native entry"),
        Op::ProfileBegin => {
            let Value::String(label) = take(&mut args) else { return Err(fail("profile label conversion failed")); };
            let thread = lua.current_thread();
            let profiles = shared(lua)?.borrow().profile_threads.clone();
            let stack = match profiles.raw_get::<Option<AnyUserData>>(thread.clone())? {
                Some(stack) => stack,
                None => {
                    let count = profiles.clone().pairs::<Value, Value>().try_fold(0, |count, entry| entry.map(|_| count + 1))?;
                    if count >= 256 { return Err(fail("ProfilerLimit: active thread limit exceeded")); }
                    let stack = lua.create_userdata(ProfileStack(Vec::new()))?;
                    profiles.raw_set(thread, stack.clone())?;
                    stack
                }
            };
            let mut stack = stack.borrow_mut::<ProfileStack>()?;
            if stack.0.len() >= 128 { return Err(fail("ProfilerLimit: scope nesting limit exceeded")); }
            let bytes = label.as_bytes();
            let label = String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]).into_owned();
            stack.0.push(ProfileScope { label, started: Instant::now() });
            let state = shared(lua)?;
            let mut state = state.borrow_mut();
            state.profiles_begun = state.profiles_begun.saturating_add(1);
            Ok(empty())
        }
        Op::ProfileEnd => {
            let thread = lua.current_thread();
            let profiles = shared(lua)?.borrow().profile_threads.clone();
            if let Some(stack) = profiles.raw_get::<Option<AnyUserData>>(thread.clone())? {
                let (scope, empty) = { let mut stack = stack.borrow_mut::<ProfileStack>()?; (stack.0.pop(), stack.0.is_empty()) };
                if empty { profiles.raw_set(thread, Value::Nil)?; }
                if let Some(scope) = scope {
                    let sample = ProfileSample { label: scope.label, elapsed: scope.started.elapsed().as_secs_f64() };
                    let state = shared(lua)?;
                    let mut state = state.borrow_mut();
                    state.profiles_completed = state.profiles_completed.saturating_add(1);
                    if state.profile_samples.len() == 512 { state.profile_samples.pop_front(); }
                    state.profile_samples.push_back(sample);
                }
            }
            Ok(empty())
        }
        Op::ProfileStatus => {
            let state = shared(lua)?;
            let state = state.borrow();
            let result = lua.create_table()?;
            result.raw_set("begun", state.profiles_begun)?;
            result.raw_set("completed", state.profiles_completed)?;
            let (mut threads, mut scopes) = (0, 0);
            for entry in state.profile_threads.clone().pairs::<Value, AnyUserData>() { let (_, stack) = entry?; threads += 1; scopes += stack.borrow::<ProfileStack>()?.0.len(); }
            result.raw_set("activeThreads", threads)?;
            result.raw_set("activeScopes", scopes)?;
            let samples = lua.create_table()?;
            for (index, sample) in state.profile_samples.iter().enumerate() {
                let record = lua.create_table()?;
                record.raw_set("label", sample.label.as_str())?;
                record.raw_set("elapsed", sample.elapsed)?;
                samples.raw_set(index + 1, record)?;
            }
            result.raw_set("samples", samples)?;
            Ok(returned(Value::Table(result)))
        }
        Op::SignalIndex => {
            signal_data(&userdata(take(&mut args))?)?;
            let name = string(take(&mut args))?;
            let (debug_name, method) = match name.split('\0').next().unwrap_or("") {
                "Connect" | "connect" => (c"Connect", Op::Connect),
                "ConnectParallel" => (c"ConnectParallel", Op::ConnectParallel),
                "Once" => (c"Once", Op::Once),
                "Wait" => (c"Wait", Op::SignalWait),
                "Fire" => (c"Fire", Op::Fire),
                "DisconnectAll" => (c"DisconnectAll", Op::DisconnectAll),
                _ => return Err(fail(format!("{name} is not a valid member of RBXScriptSignal"))),
            };
            Ok(returned(Value::Function(native_function(lua, Some(debug_name), method)?)))
        }
        Op::SignalString => {
            let signal = signal_data(&userdata(take(&mut args))?)?;
            let label = signal.borrow().label.clone();
            Ok(returned(Value::String(lua.create_string(if label.is_empty() { "Signal".into() } else { format!("Signal {label}") })?)))
        }
        Op::SignalReadOnly => { take(&mut args); Err(fail(format!("{} is not a valid member of RBXScriptSignal", string(take(&mut args))?))) }
        Op::ConnectionString => Ok(returned(Value::String(lua.create_string("Connection")?))),
        Op::ConnectionReadOnly => { take(&mut args); Err(fail(format!("{} is not a valid member of RBXScriptConnection", string(take(&mut args))?))) }
        Op::Connect | Op::ConnectParallel | Op::Once => {
            let signal = userdata(take(&mut args))?;
            let callback = take(&mut args);
            Ok(returned(Value::UserData(connect_value(lua, &signal, callback, matches!(op, Op::ConnectParallel), matches!(op, Op::Once))?)))
        }
        Op::SignalWait => { wait_signal(lua, &userdata(take(&mut args))?)?; Ok(NativeCall::Yield(MultiValue::new())) }
        Op::Fire => { fire_signal(lua, &userdata(take(&mut args))?, args)?; Ok(empty()) }
        Op::DisconnectAll => { disconnect_all(lua, &userdata(take(&mut args))?)?; Ok(empty()) }
        Op::ConnectionIndex | Op::ViewIndex => {
            let connection = connection_data(&userdata(take(&mut args))?)?;
            let name = string(take(&mut args))?;
            let data = connection.borrow();
            let view = matches!(op, Op::ViewIndex);
            let value = match name.split('\0').next().unwrap_or("") {
                "Connected" | "connected" => Value::Boolean(data.connected),
                "Disconnect" | "disconnect" => Value::Function(native_function(lua, Some(c"Disconnect"), Op::Disconnect)?),
                "Enabled" if view => Value::Boolean(data.connected && data.enabled),
                "Function" if view => data.callback.clone(),
                "ForeignState" if view => Value::Boolean(false),
                "LuaConnection" if view => Value::Boolean(true),
                _ => {
                    if !view { return Err(fail(format!("{name} is not a valid member of RBXScriptConnection"))); }
                    let state = shared(lua)?;
                    let state = state.borrow();
                    let value = state.view_methods.raw_get::<Value>(name.as_str())?;
                    if value.is_nil() { return Err(fail(format!("{name} is not a valid member of RBXScriptConnection"))); }
                    value
                }
            };
            Ok(returned(value))
        }
        Op::Disconnect | Op::Enable | Op::Disable | Op::ConnectionFire | Op::ConnectionDefer => {
            let connection = connection_data(&userdata(take(&mut args))?)?;
            match op {
                Op::Disconnect => disconnect(lua, &connection, true)?,
                Op::Enable => connection.borrow_mut().enabled = true,
                Op::Disable => connection.borrow_mut().enabled = false,
                _ => {
                    let (function, environment, mode, origin) = { let connection = connection.borrow(); (connection.callback.clone(), connection.environment.clone(), connection.mode.clone(), connection.origin.clone()) };
                    let thread = to_thread(lua, Value::Function(callable_callback(lua, function)?), Some(&mode), Some(&origin))?;
                    set_thread_environment(lua, &thread, &environment)?;
                    if matches!(op, Op::ConnectionDefer) { enqueue(lua, thread, args, None, 0.0, None)?; }
                    else { resume_task(lua, thread, args, None)?; }
                }
            }
            Ok(empty())
        }
    }
}

fn set_meta(lua: &Lua, sample: AnyUserData, index: Op, tostring: Op, readonly: Op) -> Result<()> {
    let registry = match index { Op::SignalIndex => c"emulator.signal.metatable", Op::ConnectionIndex => c"emulator.connection.metatable", _ => c"emulator.connectionview.metatable" };
    let index = native_function(lua, None, index)?;
    let tostring = native_function(lua, None, tostring)?;
    let readonly = native_function(lua, None, readonly)?;
    unsafe {
        lua.exec_raw::<()>((sample, index, tostring, readonly), |state| {
            ffi::lua_getmetatable(state, 1);
            ffi::lua_pushvalue(state, -1);
            ffi::lua_rawsetfield(state, ffi::LUA_REGISTRYINDEX, registry.as_ptr());
            for (argument, name) in [(2, c"__index"), (3, c"__tostring"), (4, c"__newindex")] {
                ffi::lua_pushvalue(state, argument);
                ffi::lua_rawsetfield(state, -2, name.as_ptr());
            }
            ffi::lua_pushstring(state, c"The metatable is locked".as_ptr());
            ffi::lua_rawsetfield(state, -2, c"__metatable".as_ptr());
            ffi::lua_settop(state, 0);
        })
    }
}

pub fn install(lua: &Lua) -> Result<()> {
    let config = crate::internal::<Option<Table>>(lua, "__config")?;
    let max_tasks = config.as_ref().and_then(|config| config.get::<usize>("maxTasks").ok()).unwrap_or(10000).clamp(1, 100000);
    let max_resumes = config.as_ref().and_then(|config| config.get::<usize>("maxResumes").ok()).unwrap_or(10000).clamp(1, 100000);
    let max_resume_depth = config.as_ref().and_then(|config| config.get::<usize>("maxResumeDepth").ok()).unwrap_or(64).clamp(1, 128);
    let actor = config.as_ref().is_some_and(|config| config.get::<Value>("actorId").is_ok_and(|value| !value.is_nil()));
    let deferred_signals = config.as_ref().is_some_and(|config| config.get::<String>("signalBehavior").is_ok_and(|value| value == "deferred"));
    let rt = lua.create_table()?;
    let errors = lua.create_table()?;
    rt.raw_set("now", 0.0)?;
    rt.raw_set("errors", errors.clone())?;
    let view_methods = lua.create_table()?;
    let profile_threads = lua.create_table()?;
    let profile_meta = lua.create_table()?;
    profile_meta.raw_set("__mode", "k")?;
    profile_threads.set_metatable(Some(profile_meta))?;
    for name in [MEMORY_THREADS, MEMORY_ENVIRONMENTS] {
        let table = lua.create_table()?;
        let meta = lua.create_table()?;
        meta.raw_set("__mode", "k")?;
        table.set_metatable(Some(meta))?;
        lua.set_named_registry_value(name, table)?;
    }
    let default_category = lua.create_string("Default")?;
    lua.set_app_data(Rc::new(RefCell::new(Scheduler {
        now: 0.0, tick: 0, sequence: 0, resumes: 0, resume_depth: 0, max_resume_depth, max_tasks, max_resumes, actor, deferred_signals,
        tasks: HashMap::new(), queue: Vec::new(), rt: rt.clone(), errors,
        view_methods: view_methods.clone(),
        profile_threads, profiles_begun: 0, profiles_completed: 0, profile_samples: VecDeque::new(),
        memory_names: vec![default_category.clone()], memory_ids: HashMap::from([(default_category, 0)]),
    })));
    let task = lua.create_table()?;
    for (name, debug_name, op) in [("spawn", c"spawn", Op::Spawn), ("defer", c"defer", Op::Defer),
        ("delay", c"delay", Op::Delay), ("wait", c"wait", Op::Wait), ("cancel", c"cancel", Op::Cancel),
        ("synchronize", c"synchronize", Op::Synchronize), ("desynchronize", c"desynchronize", Op::Desynchronize)] {
        let function = native_function(lua, Some(debug_name), op)?;
        task.raw_set(name, function.clone())?;
        rt.raw_set(name, function)?;
    }
    for (name, op) in [("spawnWith", Op::SpawnWith), ("park", Op::Park), ("schedule", Op::Schedule), ("step", Op::Step),
        ("status", Op::Status), ("getMode", Op::GetMode), ("getOrigin", Op::GetOrigin), ("setMode", Op::SetMode),
        ("newSignal", Op::NewSignal), ("getConnections", Op::GetConnections), ("profileStatus", Op::ProfileStatus),
        ("memoryStatus", Op::MemoryStatus)] { rt.raw_set(name, native_function(lua, None, op)?)?; }
    for (name, debug_name, op) in [("Enable", c"Enable", Op::Enable), ("Disable", c"Disable", Op::Disable),
        ("Fire", c"Fire", Op::ConnectionFire), ("Defer", c"Defer", Op::ConnectionDefer)] { view_methods.raw_set(name, native_function(lua, Some(debug_name), op)?)?; }
    let sample = new_signal(lua)?;
    set_meta(lua, sample.clone(), Op::SignalIndex, Op::SignalString, Op::SignalReadOnly)?;
    let callback = native_function(lua, None, Op::Time)?;
    let sample_connection = connect_signal(lua, &sample, callback, false, false)?;
    set_meta(lua, sample_connection.clone(), Op::ConnectionIndex, Op::ConnectionString, Op::ConnectionReadOnly)?;
    let connection = connection_data(&sample_connection)?;
    set_meta(lua, lua.create_userdata(ConnectionView(connection))?, Op::ViewIndex, Op::ConnectionString, Op::ConnectionReadOnly)?;
    disconnect_all(lua, &sample)?;
    lua.globals().set("__rt", rt)?;
    task.set_readonly(true);
    lua.globals().set("task", task)?;
    for (name, debug_name, op) in [("print", c"print", Op::Print), ("warn", c"warn", Op::Warn), ("time", c"time", Op::Time),
        ("elapsedTime", c"elapsedTime", Op::Time), ("tick", c"tick", Op::Tick)] { lua.globals().set(name, native_function(lua, Some(debug_name), op)?)?; }
    let clock = native_function(lua, Some(c"clock"), Op::Time)?;
    let os: Table = lua.globals().get("os")?;
    os.set_readonly(false);
    let result = os.raw_set("clock", clock);
    os.set_readonly(true);
    result?;
    let original_debug: Table = lua.globals().get("debug")?;
    let debug = lua.create_table()?;
    for entry in original_debug.pairs::<Value, Value>() { let (key, value) = entry?; debug.raw_set(key, value)?; }
    debug.raw_set("profilebegin", native_function(lua, Some(c"profilebegin"), Op::ProfileBegin)?)?;
    debug.raw_set("profileend", native_function(lua, Some(c"profileend"), Op::ProfileEnd)?)?;
    for (name, debug_name, op) in [("getmemorycategory", c"getmemorycategory", Op::GetMemoryCategory),
        ("setmemorycategory", c"setmemorycategory", Op::SetMemoryCategory), ("resetmemorycategory", c"resetmemorycategory", Op::ResetMemoryCategory),
        ("dumpcodesize", c"dumpcodesize", Op::DumpCodeSize), ("dumpheap", c"dumpheap", Op::DumpHeap),
        ("dumprefs", c"dumprefs", Op::DumpRefs), ("tracerefs", c"tracerefs", Op::TraceRefs), ("loadmodule", c"loadmodule", Op::LoadModule)] {
        debug.raw_set(name, native_function(lua, Some(debug_name), op)?)?;
    }
    debug.set_readonly(true);
    lua.globals().set("debug", debug)?;
    Ok(())
}

#[cfg(all(test, emulator_local_tests))]
#[path = "../local_tests/scheduler.rs"]
mod tests;
