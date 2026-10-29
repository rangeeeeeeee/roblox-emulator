use mlua::{Error,Function,Lua,MultiValue,Result,Table,Value};
use mlua::chunk::Compiler;
use sha2::{Digest,Sha384};
use std::{cell::RefCell,collections::{BTreeMap,BTreeSet,HashMap},ffi::CString,rc::{Rc,Weak}};
#[path="regex_api.rs"] mod regex_api;

type ThreadHook = unsafe extern "C-unwind" fn(*mut mlua::ffi::lua_State, *mut mlua::ffi::lua_State);
struct ThreadIdentities {
    values: RefCell<HashMap<usize, i64>>,
    owners: RefCell<HashMap<usize, bool>>,
    origin: Option<Rc<dyn Fn(usize)->Option<String>>>,
    previous: Option<ThreadHook>,
}
struct IdentityHandle(Rc<ThreadIdentities>);
thread_local! { static IDENTITIES: RefCell<HashMap<usize,Weak<ThreadIdentities>>> = RefCell::new(HashMap::new()); }

unsafe extern "C-unwind" fn identity_thread(parent:*mut mlua::ffi::lua_State,child:*mut mlua::ffi::lua_State) {
    let key = unsafe { mlua::ffi::lua_callbacks(child) } as usize;
    let identity = IDENTITIES.try_with(|all|all.try_borrow().ok().and_then(|all|all.get(&key).and_then(Weak::upgrade))).ok().flatten();
    if let Some(identity)=identity {
        let _=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||{
            if let Ok(mut owners)=identity.owners.try_borrow_mut() {
                if parent.is_null() { owners.remove(&(child as usize)); }
                else {
                    let inherited=identity.origin.as_ref().and_then(|origin|origin(parent as usize)).map(|origin|origin=="executor")
                        .or_else(||owners.get(&(parent as usize)).copied()).unwrap_or(false);
                    owners.insert(child as usize,inherited);
                }
            }
            if let Ok(mut values)=identity.values.try_borrow_mut() {
                if parent.is_null() { values.remove(&(child as usize)); }
                else {
                    let inherited=values.get(&(parent as usize)).copied().or_else(||identity.origin.as_ref().and_then(|origin|origin(parent as usize)).map(|origin|if origin=="executor"{8}else{2}));
                    if let Some(value)=inherited { values.insert(child as usize,value); }
                }
            }
        }));
        if let Some(previous)=identity.previous { unsafe { previous(parent,child) }; }
    }
}

fn install_identities(lua:&Lua) {
    if lua.app_data_ref::<IdentityHandle>().is_some(){return}
    let origin=crate::scheduler::origin_resolver(lua);
    let identity=unsafe{lua.exec_raw_lua(|raw|{
        let callbacks=mlua::ffi::lua_callbacks(raw.state());
        let state=Rc::new(ThreadIdentities{values:RefCell::new(HashMap::new()),owners:RefCell::new(HashMap::new()),origin,previous:(*callbacks).userthread});
        IDENTITIES.with(|all|{let mut all=all.borrow_mut();all.retain(|_,value|value.strong_count()!=0);all.insert(callbacks as usize,Rc::downgrade(&state));});
        (*callbacks).userthread=Some(identity_thread);
        state
    })};
    lua.set_app_data(IdentityHandle(identity));
}

pub(crate) fn record_thread_origin(lua:&Lua,thread:&mlua::Thread,origin:&str) {
    if let Some(handle)=lua.app_data_ref::<IdentityHandle>() {
        if let Ok(mut owners)=handle.0.owners.try_borrow_mut() {owners.insert(thread.state() as usize,origin=="executor");}
    }
}
fn is_executor_thread(lua:&Lua,thread:mlua::Thread)->bool {
    lua.app_data_ref::<IdentityHandle>().and_then(|handle|handle.0.owners.try_borrow().ok()
        .and_then(|owners|owners.get(&(thread.state() as usize)).copied())).unwrap_or(false)
}

pub(crate) fn thread_identity(lua:&Lua)->i64 {
    let explicit=lua.app_data_ref::<IdentityHandle>().and_then(|handle|handle.0.values.try_borrow().ok().and_then(|values|values.get(&(lua.state() as usize)).copied()));
    explicit.unwrap_or_else(||if crate::scheduler::current_origin(lua)=="executor"{8}else{2})
}

#[derive(Default)]
struct VirtualIo {files:BTreeMap<String,Vec<u8>>,folders:BTreeSet<String>,clipboard:Vec<u8>}
type Io=Rc<RefCell<VirtualIo>>;
#[derive(Clone)]
struct VirtualIoHandle(Io);

pub(crate) fn virtual_files(lua:&Lua)->Result<Vec<(String,usize)>>{
    let handle=lua.app_data_ref::<VirtualIoHandle>().ok_or_else(||Error::lua_string("executor is not enabled in this VM"))?;
    let io=handle.0.borrow();
    Ok(io.files.iter().map(|(name,data)|(name.clone(),data.len())).collect())
}
pub(crate) fn virtual_file(lua:&Lua,name:&str)->Result<Vec<u8>>{
    let name=path(name)?;
    let handle=lua.app_data_ref::<VirtualIoHandle>().ok_or_else(||Error::lua_string("executor is not enabled in this VM"))?;
    let io=handle.0.borrow();
    io.files.get(&name).cloned().ok_or_else(||Error::lua_string("virtual file does not exist"))
}
fn instance_arg(value:Value,name:&str,index:usize)->Result<crate::instance::Link>{
    if let Value::UserData(value)=value{if let Ok(instance)=value.borrow::<crate::instance::Instance>(){return Ok(instance.0.clone())}}
    Err(Error::lua_string(format!("invalid argument #{index} to '{name}' (Instance expected)")))
}
fn script_bytecode(value:Value,name:&str)->Result<Option<Vec<u8>>>{
    let script=instance_arg(value,name,1)?;
    let Some(source)=crate::model::script_source(&script)? else{return Ok(None)};
    if !script.borrow().source_initialized {return Ok(None)}
    Ok(Compiler::new().set_mutable_globals(["_G","shared","script"]).compile(source).ok())
}
pub fn unsupported_calls(lua:&Lua)->Result<BTreeMap<String,u64>> {
    let entries:Table=crate::internal::<Table>(lua,"__emu")?.get("unsupported")?;
    let mut calls=BTreeMap::new();
    for entry in entries.sequence_values::<Table>() {*calls.entry(entry?.get::<String>("api")?).or_default()+=1;}
    Ok(calls)
}
pub fn run(lua:&Lua,source:&str,name:&str)->Result<mlua::Thread>{
    if !crate::internal::<Table>(lua,"__config")?.get::<bool>("executor")?{return Err(Error::lua_string("executor profile is disabled for this realm"));}
    let env:Table=crate::internal::<Table>(lua,"__emu")?.get("executorEnvironment")?;
    let f=lua.load(source).set_mode(mlua::chunk::ChunkMode::Text).set_name(name).set_environment(env.clone()).into_function()?;
    crate::native::mark_executor_function(lua,&f,&env)?;
    let thread=lua.create_thread(f)?;
    crate::scheduler::set_thread_environment(lua,&thread,&env)?;
    crate::scheduler::spawn_with(lua,Value::Thread(thread),MultiValue::new(),Some("serial"),Some("executor"))
}
fn path(value:&str)->Result<String>{let value=value.replace('\\',"/");if value.len()>240||value.starts_with('/')||value.contains(':')||value.contains('\0'){return Err(Error::lua_string("invalid virtual path"));}let parts:Vec<_>=value.split('/').filter(|p|!p.is_empty()).collect();if parts.iter().any(|p|*p=="."||*p==".."){return Err(Error::lua_string("invalid virtual path"));}Ok(parts.join("/"))}
fn parent(value:&str)->&str{value.rsplit_once('/').map(|(p,_)|p).unwrap_or("")}
fn write(io:&Io,name:&str,data:&[u8],append:bool)->Result<()>{
    let name=path(name)?;let mut io=io.borrow_mut();
    if name.is_empty()||io.folders.contains(&name){return Err(Error::lua_string("path is a directory"));}
    if !io.folders.contains(parent(&name)){return Err(Error::lua_string("virtual parent directory does not exist"));}
    let used:usize=io.files.values().map(Vec::len).sum();let old=io.files.get(&name).map_or(0,Vec::len);
    let new=if append{old.checked_add(data.len())}else{Some(data.len())};
    if new.is_none_or(|new|new>1_048_576-(used-old))||(!io.files.contains_key(&name)&&io.files.len()>=256){
        return Err(Error::lua_string("VirtualFilesystemLimit: storage limit exceeded"));
    }
    if append {io.files.entry(name).or_default().extend_from_slice(data);}
    else {io.files.insert(name,data.to_vec());}
    Ok(())
}
fn set<A:mlua::FromLuaMulti,R:mlua::IntoLuaMulti>(lua:&Lua,env:&Table,name:&str,f:impl Fn(&Lua,A)->Result<R>+'static)->Result<()>{let function=lua.create_function(f)?;crate::native::name_function(lua,&function,&CString::new(name).unwrap())?;env.set(name,function)}
pub fn install(lua:&Lua)->Result<()> {
    install_identities(lua);
    let mac_744_4=crate::native::is_macsploit_7444(lua);
    let game=lua.globals();let env=lua.create_table()?;let mt=lua.create_table()?;mt.set("__index",game.clone())?;env.set_metatable(Some(mt))?;
    env.set("_G",env.clone())?;env.set("shared",lua.create_table()?)?;
    let emu=lua.create_table()?;let caps=lua.create_table()?;let unsupported=lua.create_table()?;
    crate::encoding::install(lua,&env,&caps)?;
    regex_api::install(lua,&env,&caps)?;
    crate::drawing::install(lua,&env,&caps)?;
    crate::network::install(lua,&env,&caps)?;
    emu.set("executorEnvironment",env.clone())?;emu.set("capabilities",caps.clone())?;emu.set("unsupported",unsupported.clone())?;
    let executor_env=env.clone();set(lua,&env,"getgenv",move|_,()|{executor_env.set_safeenv(false);Ok(executor_env.clone())})?;
    let game_env=game.clone();set(lua,&env,"getrenv",move|_,()|Ok(game_env.clone()))?;
    set(lua,&env,"checkcaller",|lua,()|Ok(crate::scheduler::current_origin(lua)=="executor"))?;
    set(lua,&env,"isourthread",|lua,thread:mlua::Thread|Ok(is_executor_thread(lua,thread)))?;
    caps.set("isourthread","partial: creation provenance of threads in the current VM")?;
    set(lua,&env,"identifyexecutor",move|_,()|Ok(if mac_744_4{("MacSploit","744.4")}else{("RobloxEmulator","0.2.0")}))?;
    set(lua,&env,"isrbxactive",|_,()|Ok(false))?;
    env.set("isgameactive",env.get::<Function>("isrbxactive")?)?;
    caps.set("isrbxactive","native: no Roblox game window in headless mode")?;
    set(lua,&env,"getgcvalue",|lua,(viewer,snapshot,index):(String,u64,usize)|crate::inspector::gc_value(lua,&viewer,snapshot,index))?;
    caps.set("getgcvalue","emulator: current GCViewer snapshot; expires after refresh, close, reset or 10 minutes")?;
    let rt:Table=game.get("__rt")?;env.set("getconnections",rt.get::<Function>("getConnections")?)?;
    env.set("firesignal",rt.get::<Function>("fireExecutorSignal")?)?;
    env.set("getconnection",rt.get::<Function>("getConnection")?)?;
    for (name,info) in [("getsignalarguments",false),("getsignalargumentsinfo",true)] {
        set(lua,&env,name,move|lua,signal:mlua::AnyUserData|crate::scheduler::signal_arguments(lua,signal,info))?;
        caps.set(name,"partial: metadata for selected built-in signals; unknown signals report unsupported")?;
    }
    caps.set("firesignal","native: immediate local RBXScriptSignal callbacks")?;
    caps.set("getconnection","native: indexed live local signal connection")?;
    for (name,nil_only) in [("getinstances",false),("getnilinstances",true)]{set(lua,&env,name,move|lua,()|crate::model::instances(lua,nil_only))?;}
    set(lua,&env,"getsenv",|lua,value:Value|crate::model::active_script_environment(lua,&instance_arg(value,"getsenv",1)?))?;
    caps.set("getsenv","partial: live same-VM script or loaded module; foreign Actor returns nil")?;
    set(lua,&env,"cloneref",|lua,value:Value|lua.create_userdata(crate::instance::Instance(instance_arg(value,"cloneref",1)?)))?;
    set(lua,&env,"compareinstances",|_,(left,right):(Value,Value)|Ok(Rc::ptr_eq(&instance_arg(left,"compareinstances",1)?,&instance_arg(right,"compareinstances",2)?)))?;
    set(lua,&env,"getcallingscript",|lua,()|crate::model::calling_script(lua))?;
    set(lua,&env,"getscriptfromthread",|lua,thread:mlua::Thread|crate::model::script_from_thread(lua,thread))?;
    set(lua,&env,"getscriptthread",|lua,value:Value|crate::model::script_thread(lua,&instance_arg(value,"getscriptthread",1)?))?;
    caps.set("getscriptfromthread","partial: inferred from current thread environment; environment replacement and weak ownership are not modeled")?;
    caps.set("getscriptthread","partial: explicitly launched live root thread only; require-created ModuleScript and child threads are not tracked")?;
    let cache=lua.create_table()?;
    set(lua,&cache,"invalidate",|lua,value:mlua::AnyUserData|crate::instance::cache_invalidate(lua,value))?;
    set(lua,&cache,"iscached",|lua,value:mlua::AnyUserData|crate::instance::cache_is_cached(lua,value))?;
    set(lua,&cache,"replace",|lua,values:(mlua::AnyUserData,mlua::AnyUserData)|crate::instance::cache_replace(lua,values))?;
    for name in ["invalidate","iscached","replace"]{crate::native::mark_executor_function(lua,&cache.get::<Function>(name)?,&env)?;}
    env.set("cache",cache)?;
    caps.set("cache","native: per-VM weak Instance wrapper cache")?;
    set(lua,&env,"getscripts",|lua,()|crate::model::scripts(lua,false))?;
    set(lua,&env,"getloadedmodules",|lua,()|crate::model::scripts(lua,true))?;
    set(lua,&env,"getrunningscripts",|lua,()|crate::model::running_scripts(lua))?;
    for (name,hidden) in [("getproperties",false),("gethiddenproperties",true)] {
        set(lua,&env,name,move|lua,value:Value|crate::model::properties(lua,&instance_arg(value,name,1)?,hidden))?;
        caps.set(name,"partial: properties implemented by the emulator in the current VM")?;
    }
    set(lua,&env,"setfpscap",|lua,cap:f64|crate::model::set_fps_cap(lua,cap))?;
    set(lua,&env,"getfpscap",|lua,()|crate::model::fps_cap(lua))?;
    set(lua,&env,"getrendersteppedlist",|lua,()|crate::model::render_bindings(lua))?;
    caps.set("getrendersteppedlist","partial: named bindings in the current VM; duplicate names map to the last callback")?;
    caps.set("setfpscap","partial: caps simulated render signals in this VM; no real renderer")?;
    caps.set("getfpscap","partial: returns this VM's simulated render cap")?;
    set(lua,&env,"getcallbackvalue",|_,(object,property):(Value,String)|crate::model::get_callback(&instance_arg(object,"getcallbackvalue",1)?,&property))?;
    set(lua,&env,"fireclickdetector",|lua,(object,_distance,event):(Value,Option<f64>,Option<String>)|crate::model::fire_click_detector(lua,&instance_arg(object,"fireclickdetector",1)?,event.as_deref().unwrap_or("MouseClick")))?;
    caps.set("fireclickdetector","partial: local ClickDetector signals only; distance and cross-realm replication are not modeled")?;
    set(lua,&env,"fireproximityprompt",|lua,object:Value|crate::model::fire_proximity_prompt(lua,&instance_arg(object,"fireproximityprompt",1)?))?;
    caps.set("fireproximityprompt","partial: local Triggered signal only; visibility, physical distance and server replication are not modeled")?;
    set(lua,&env,"getproximitypromptduration",|lua,object:Value|{
        let prompt=instance_arg(object,"getproximitypromptduration",1)?;
        if !crate::instance::is_a(&prompt.borrow(),"ProximityPrompt"){return Err(Error::lua_string("ProximityPrompt expected"))}
        crate::model::read_property(lua,&prompt,"HoldDuration")?.ok_or_else(||Error::lua_string("HoldDuration unavailable"))
    })?;
    set(lua,&env,"setproximitypromptduration",|lua,(object,duration):(Value,f64)|{
        let prompt=instance_arg(object,"setproximitypromptduration",1)?;
        if !crate::instance::is_a(&prompt.borrow(),"ProximityPrompt"){return Err(Error::lua_string("ProximityPrompt expected"))}
        if !crate::model::write_property(lua,&prompt,"HoldDuration",&Value::Number(duration))?{return Err(Error::lua_string("HoldDuration unavailable"))}
        Ok(())
    })?;
    for name in ["getproximitypromptduration","setproximitypromptduration"]{caps.set(name,"partial: local headless ProximityPrompt HoldDuration property")?;}
    set(lua,&env,"firetouchinterest",|lua,(first,second,toggle):(Value,Value,Value)|{
        let begin=match toggle {Value::Boolean(value)=>value,Value::Integer(0)=>true,Value::Integer(1)=>false,Value::Number(value) if value==0.0=>true,Value::Number(value) if value==1.0=>false,_=>return Err(Error::lua_string("touch state must be boolean, 0 or 1"))};
        crate::model::fire_touch_interest(lua,&instance_arg(first,"firetouchinterest",1)?,&instance_arg(second,"firetouchinterest",2)?,begin)
    })?;
    caps.set("firetouchinterest","partial: local Touched/TouchEnded signals; physics and cross-realm replication are not modeled")?;
    set(lua,&env,"isscriptable",|lua,(object,property):(Value,String)|crate::model::scriptability(lua,&instance_arg(object,"isscriptable",1)?,&property))?;
    set(lua,&env,"setscriptable",|lua,(object,property,enabled):(Value,String,bool)|crate::model::set_scriptability(lua,&instance_arg(object,"setscriptable",1)?,&property,enabled))?;
    set(lua,&env,"gethiddenproperty",|lua,(object,property):(Value,String)|crate::model::get_hidden_property(lua,&instance_arg(object,"gethiddenproperty",1)?,&property))?;
    set(lua,&env,"sethiddenproperty",|lua,(object,property,value):(Value,String,Value)|crate::model::set_hidden_property(lua,&instance_arg(object,"sethiddenproperty",1)?,&property,&value))?;
    for name in ["isscriptable","setscriptable","gethiddenproperty","sethiddenproperty"]{caps.set(name,"partial: supported property metadata and per-instance scriptability")?;}
    set(lua,&env,"getsimulationradius",|lua,()|{
        let player=crate::model::local_player(lua)?;
        crate::model::get_hidden_property(lua,&player,"SimulationRadius").map(|(radius,_)|radius)
    })?;
    set(lua,&env,"setsimulationradius",|lua,(radius,maximum):(f64,Option<f64>)|{
        let player=crate::model::local_player(lua)?;
        if let Some(maximum)=maximum{crate::model::set_hidden_property(lua,&player,"MaximumSimulationRadius",&Value::Number(maximum))?;}
        crate::model::set_hidden_property(lua,&player,"SimulationRadius",&Value::Number(radius))?;
        Ok(())
    })?;
    for name in ["getsimulationradius","setsimulationradius"]{caps.set(name,"partial: local Player properties only; dynamic physics and ownership are not modeled")?;}
    let hidden_ui=Rc::new(RefCell::new(None::<mlua::AnyUserData>));
    set(lua,&env,"gethui",move|lua,()|{
        if let Some(ui)=hidden_ui.borrow().as_ref(){return Ok(ui.clone())}
        let new:Function=lua.globals().get::<Table>("Instance")?.get("new")?;
        let ui:mlua::AnyUserData=new.call("Folder")?;
        *hidden_ui.borrow_mut()=Some(ui.clone());
        Ok(ui)
    })?;
    set(lua,&env,"getscriptbytecode",|lua,value:Value|match script_bytecode(value,"getscriptbytecode")?{
        Some(bytes)=>Ok(Value::String(lua.create_string(bytes)?)),None=>Ok(Value::Nil)
    })?;
    set(lua,&env,"getscripthash",|lua,value:Value|match script_bytecode(value,"getscripthash")?{
        Some(bytes)=>Ok(Value::String(lua.create_string(format!("{:x}",Sha384::digest(bytes)))?)),None=>Ok(Value::Nil)
    })?;
    let executor_environment=env.clone();
    set(lua,&env,"getscriptclosure",move|lua,value:Value|{
        let script=instance_arg(value.clone(),"getscriptclosure",1)?;
        let Some(bytecode)=script_bytecode(value.clone(),"getscriptclosure")? else{return Ok(Value::Nil)};
        let script_env=crate::model::env(lua,&script)?;
        let name=format!("={}",crate::instance::full_name(&script));
        let f=lua.load(bytecode).set_mode(mlua::chunk::ChunkMode::Binary).set_name(name).set_environment(script_env).into_function().map_err(|e|Error::lua_string(e.to_string()))?;
        crate::native::mark_executor_function(lua,&f,&executor_environment)?;
        Ok(Value::Function(f))
    })?;
    caps.set("getscriptbytecode","native: source compiled with the VM's Luau compiler; Roblox bytecode packaging is not emulated")?;
    caps.set("getscripthash","native: SHA-384 of emulated Luau bytecode; Roblox bytecode packaging is not emulated")?;
    caps.set("getscriptclosure","native: fresh closure with the target script environment")?;
    caps.set("getcallbackvalue","native: callback property of the selected VM's Instance")?;
    caps.set("gethui","native: detached per-VM UI container")?;
    for name in ["cloneref","compareinstances","getcallingscript","getscripts","getloadedmodules","getrunningscripts"]{caps.set(name,"native: isolated DataModel and current thread environment")?;}
    for (name,host) in [("getgc","__host_getgc"),("iscclosure","__host_iscclosure"),("islclosure","__host_islclosure"),("newcclosure","__host_newcclosure"),("clonefunction","__host_clonefunction"),("getrawmetatable","__host_getrawmetatable"),("setrawmetatable","__host_setrawmetatable"),("setreadonly","__host_setreadonly"),("isreadonly","__host_isreadonly"),("getnamecallmethod","__host_getnamecallmethod"),("hookfunction","__host_hookfunction"),("restorefunction","__host_restorefunction"),("hookmetamethod","__host_hookmetamethod")]{env.set(name,game.get::<Function>(host)?)?;caps.set(name,"native: isolated Luau VM")?;}
    for (name,host) in [("isnewcclosure","__host_isnewcclosure"),("isfunctionhooked","__host_isfunctionhooked"),("makereadonly","__host_makereadonly"),("makewritable","__host_makewritable"),("setnamecallmethod","__host_setnamecallmethod")] {
        env.set(name,game.get::<Function>(host)?)?;caps.set(name,"native: isolated Luau VM")?;
    }
    caps.set("isfunctionhooked","partial: tracks hooks until restorefunction; re-hooking an original stays tracked")?;
    set(lua,&env,"getfunctionhash",|lua,function:Function|crate::native::function_hash(lua,&function))?;
    caps.set("getfunctionhash","partial: SHA-384 of normalized Luau instructions and primitive constants; complex constants unsupported; cross-executor hash bytes may differ")?;
    let lua_wrapper:Function=lua.load("return function(f) return function(...) return f(...) end end").set_name("=newlclosure").set_environment(env.clone()).eval()?;
    crate::native::mark_executor_function(lua,&lua_wrapper,&env)?;
    let owner=env.clone();
    set(lua,&env,"newlclosure",move|lua,target:Function|{
        let wrapped:Function=lua_wrapper.call(target)?;
        crate::native::mark_executor_function(lua,&wrapped,&owner)?;
        Ok(wrapped)
    })?;
    caps.set("newlclosure","native: Luau forwarding closure")?;
    let isexecutorclosure=crate::native::bind_isexecutorclosure(lua,&env)?;
    for name in ["isexecutorclosure","checkclosure","isourclosure"]{env.set(name,isexecutorclosure.clone())?;}
    caps.set("isexecutorclosure","native: closure provenance within the selected VM")?;
    env.set("filtergc",crate::native::bind_filtergc(lua,&env,&unsupported)?)?;caps.set("filtergc","native: Name, Constants, Upvalues, IgnoreExecutor, Keys, Values, KeyValuePairs, Metatable; Hash unsupported")?;
    let debug=lua.create_table()?;for pair in game.get::<Table>("debug")?.pairs::<Value,Value>(){let(k,v)=pair?;debug.set(k,v)?;}
    let isvalidlevel=game.get::<Function>("__host_isvalidlevel")?;
    crate::native::mark_executor_function(lua,&isvalidlevel,&env)?;
    debug.set("isvalidlevel",isvalidlevel)?;caps.set("debug.isvalidlevel","native: current VM stack")?;
    crate::native::bind_debug_helpers(lua,&env,&debug)?;
    for(name,detail)in[
        ("getcallstack","native: active Luau stack frames"),
        ("getsafeenv","partial: function, table, or thread environment flag"),
        ("setsafeenv","partial: function, table, or thread environment flag"),
        ("setname","partial: Lua prototype debug name; C closures unsupported"),
        ("setinfo","partial: Lua prototype name, source and definition line; active frame line tables unchanged"),
    ]{
        caps.set(format!("debug.{name}"),detail)?;
    }
    env.set("setinfo",debug.get::<Function>("setinfo")?)?;
    for(name,host)in[("getinfo","__host_getinfo"),("getupvalue","__host_getupvalue"),("getupvalues","__host_getupvalues"),("setupvalue","__host_setupvalue"),("getconstant","__host_getconstant"),("getconstants","__host_getconstants"),("setconstant","__host_setconstant"),("getproto","__host_getproto"),("getprotos","__host_getprotos"),("getstack","__host_getstack"),("setstack","__host_setstack")]{
        let function=game.get::<Function>(host)?;crate::native::mark_executor_function(lua,&function,&env)?;
        debug.set(name,function.clone())?;env.set(name,function)?;
        caps.set(name,match name {
            "setconstant"=>"native: Luau function or stack level; preserves primitive constant type",
            "getproto"=>"native: inspect-only inactive Luau prototypes or active closures (activated=true)",
            "getprotos"=>"native: inspect-only inactive Luau prototypes",
            "getstack"|"setstack"=>"native: interpreted Luau register slots; active call function slots cannot be overwritten",
            _=>"native: Luau function or stack level",
        })?;
    }env.set("debug",debug)?;
    let io=Rc::new(RefCell::new(VirtualIo::default()));io.borrow_mut().folders.insert(String::new());
    lua.set_app_data(VirtualIoHandle(io.clone()));
    let s=io.clone();set(lua,&env,"writefile",move|_,(name,data):(String,mlua::LuaString)|write(&s,&name,&data.as_bytes(),false))?;
    let s=io.clone();set(lua,&env,"appendfile",move|_,(name,data):(String,mlua::LuaString)|write(&s,&name,&data.as_bytes(),true))?;
    let s=io.clone();set(lua,&env,"readfile",move|lua,name:String|{let s=s.borrow();let data=s.files.get(&path(&name)?).ok_or_else(||Error::lua_string("virtual file does not exist"))?;lua.create_string(data)})?;
    let s=io.clone();set(lua,&env,"getcustomasset",move|lua,name:String|{let data=s.borrow().files.get(&path(&name)?).cloned().ok_or_else(||Error::lua_string("virtual file does not exist"))?;crate::model::register_custom_asset(lua,data)})?;
    caps.set("getcustomasset","partial: bounded in-memory assets; Sound preload validates PCM WAV; playback and other formats unsupported")?;
    let s=io.clone();set(lua,&env,"makefolder",move|_,name:String|{let name=path(&name)?;let mut s=s.borrow_mut();if s.files.contains_key(&name)||!s.folders.contains(parent(&name)){return Err(Error::lua_string("invalid virtual directory"));}if s.folders.len()>=256&&!s.folders.contains(&name){return Err(Error::lua_string("VirtualFilesystemLimit: directory limit exceeded"));}s.folders.insert(name);Ok(())})?;
    let s=io.clone();set(lua,&env,"isfile",move|_,name:String|Ok(s.borrow().files.contains_key(&path(&name)?)))?;
    let s=io.clone();set(lua,&env,"isfolder",move|_,name:String|Ok(s.borrow().folders.contains(&path(&name)?)))?;
    let s=io.clone();set(lua,&env,"listfiles",move|_,name:String|{let name=path(&name)?;let s=s.borrow();if !s.folders.contains(&name){return Err(Error::lua_string("virtual directory does not exist"));}Ok(s.files.keys().chain(s.folders.iter()).filter(|p|!p.is_empty()&&parent(p)==name).cloned().collect::<Vec<_>>())})?;
    let s=io.clone();set(lua,&env,"delfile",move|_,name:String|{if s.borrow_mut().files.remove(&path(&name)?).is_none(){return Err(Error::lua_string("virtual file does not exist"));}Ok(())})?;
    let s=io.clone();set(lua,&env,"delfolder",move|_,name:String|{let name=path(&name)?;let mut s=s.borrow_mut();if name.is_empty()||!s.folders.contains(&name){return Err(Error::lua_string("invalid virtual directory"));}let prefix=name.clone()+"/";s.files.retain(|n,_|!n.starts_with(&prefix));s.folders.retain(|n|n!=&name&&!n.starts_with(&prefix));Ok(())})?;
    let s=io.clone();set(lua,&env,"setclipboard",move|_,data:mlua::LuaString|{if data.as_bytes().len()>65536{return Err(Error::lua_string("virtual clipboard size limit exceeded"));}s.borrow_mut().clipboard=data.as_bytes().to_vec();Ok(())})?;
    let s=io.clone();set(lua,&env,"getclipboard",move|lua,()|lua.create_string(&s.borrow().clipboard))?;
    for(name,level)in[("rconsoleprint","executor"),("rconsolewarn","executor_warn"),("rconsoleerr","executor_error")]{set(lua,&env,name,move|lua,message:Value|{let tostring:Function=lua.globals().get("tostring")?;let message:mlua::LuaString=tostring.call(message)?;crate::host_log(lua,level,message)})?;}
    let e=env.clone();set(lua,&env,"loadstring",move|lua,(source,name):(mlua::LuaString,Option<String>)|{if source.as_bytes().len()>8*1024*1024{return Err(Error::lua_string("source size limit exceeded"));}e.set_safeenv(false);match lua.load(source.as_bytes().as_ref()).set_mode(mlua::chunk::ChunkMode::Text).set_name(name.as_deref().unwrap_or("=executor")).set_environment(e.clone()).into_function(){Ok(f)=>{crate::native::mark_executor_function(lua,&f,&e)?;Ok((Value::Function(f),Value::Nil))},Err(e)=>Ok((Value::Nil,Value::String(lua.create_string(e.to_string())?)))}})?;
    let e=env.clone();let s=io.clone();set(lua,&env,"loadfile",move|lua,name:String|{
        let name=path(&name)?;
        let data=s.borrow().files.get(&name).cloned().ok_or_else(||Error::lua_string(if mac_744_4{"the file you are trying to load quite simply does not exist lol."}else{"virtual file does not exist"}))?;
        e.set_safeenv(false);
        match lua.load(data).set_mode(mlua::chunk::ChunkMode::Text).set_name(format!("@virtual/{name}")).set_environment(e.clone()).into_function(){
            Ok(f)=>{crate::native::mark_executor_function(lua,&f,&e)?;Ok((Some(f),None::<String>))},
            Err(err) if mac_744_4=>Ok((None,Some(err.to_string()))),
            Err(err)=>Err(Error::lua_string(err.to_string())),
        }
    })?;
    let dofile_factory:Function=lua.load("return function(load) return function(path) local fn, err = load(path); if not fn then error(err, 2) end; return fn() end end")
        .set_name("=dofile").set_environment(env.clone()).eval()?;
    let dofile_body:Function=dofile_factory.call(env.get::<Function>("loadfile")?)?;
    crate::native::mark_executor_function(lua,&dofile_body,&env)?;
    env.set("dofile",crate::native::bind_dofile(lua,&dofile_body)?)?;
    caps.set("dofile","partial: execute bounded virtual files in the executor environment; no host filesystem")?;
    for (alias,original) in [("replaceclosure","hookfunction"),("getexecutorname","identifyexecutor"),("getscriptfunction","getscriptclosure"),("dumpstring","getscriptbytecode"),("toclipboard","setclipboard"),("setrbxclipboard","setclipboard"),("consoleprint","rconsoleprint")] {
        env.set(alias,env.get::<Function>(original)?)?;
    }
    set(lua,&env,"getthreadidentity",|lua,()|Ok(thread_identity(lua)))?;
    set(lua,&env,"setthreadidentity",|lua,value:Value|{
        let identity=match value {
            Value::Integer(value) if (0..=12).contains(&value)=>value,
            Value::Number(value) if value.is_finite()&&value.fract()==0.0&&(0.0..=12.0).contains(&value)=>value as i64,
            _=>return Err(Error::lua_string("setthreadidentity expects an integer identity from 0 to 12")),
        };
        let handle=lua.app_data_ref::<IdentityHandle>().ok_or_else(||Error::runtime("thread identity unavailable"))?;
        handle.0.values.borrow_mut().insert(lua.state() as usize,identity);
        Ok(())
    })?;
    for (alias,original) in [("getidentity","getthreadidentity"),("getthreadcontext","getthreadidentity"),("get_thread_identity","getthreadidentity"),("get_thread_context","getthreadidentity"),("setidentity","setthreadidentity"),("setthreadcontext","setthreadidentity"),("set_thread_identity","setthreadidentity"),("set_thread_context","setthreadidentity")] {
        env.set(alias,env.get::<Function>(original)?)?;
    }
    for name in ["getthreadidentity","setthreadidentity"] {caps.set(name,"partial: per-coroutine identity; only Source access at identities 2 and 8 is modeled")?;}
    for name in ["getreg","queue_on_teleport","shutdown"] {
        let unsupported=unsupported.clone();caps.set(name,"unsupported")?;set(lua,&env,name,move|lua,_:MultiValue|{if unsupported.raw_len()<1000{let event=lua.create_table()?;event.set("api",name)?;unsupported.raw_push(event)?;}Err::<(),_>(Error::lua_string(format!("UnsupportedApi: {name} is not implemented by this emulator")))})?;
    }
    set(lua,&emu,"runExecutor",|lua,(source,name):(String,Option<String>)|run(lua,&source,name.as_deref().unwrap_or("=executor")))?;
    set(lua,&emu,"virtualState",move|lua,()|{let s=io.borrow();let t=lua.create_table()?;t.set("fileCount",s.files.len())?;t.set("fileBytes",s.files.values().map(Vec::len).sum::<usize>())?;t.set("clipboardBytes",s.clipboard.len())?;Ok(t)})?;
    for entry in env.pairs::<Value,Value>(){let(_,value)=entry?;if let Value::Function(function)=value{crate::native::mark_executor_function(lua,&function,&env)?;}}
    env.set_safeenv(true);game.set("__emu",emu)
}

#[cfg(all(test, emulator_local_tests))]
#[path = "../local_tests/executor.rs"]
mod tests;
