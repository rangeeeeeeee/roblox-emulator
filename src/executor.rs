use mlua::{Error,Function,Lua,MultiValue,Result,Table,Value};
use std::{cell::RefCell,collections::{BTreeMap,BTreeSet},ffi::CString,rc::Rc};

#[derive(Default)]
struct VirtualIo {files:BTreeMap<String,String>,folders:BTreeSet<String>,clipboard:String}
type Io=Rc<RefCell<VirtualIo>>;
fn instance_arg(value:Value,name:&str,index:usize)->Result<crate::instance::Link>{
    if let Value::UserData(value)=value{if let Ok(instance)=value.borrow::<crate::instance::Instance>(){return Ok(instance.0.clone())}}
    Err(Error::lua_string(format!("invalid argument #{index} to '{name}' (Instance expected)")))
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
fn write(io:&Io,name:&str,data:String)->Result<()>{let name=path(name)?;let mut io=io.borrow_mut();if name.is_empty()||io.folders.contains(&name){return Err(Error::lua_string("path is a directory"));}if !io.folders.contains(parent(&name)){return Err(Error::lua_string("virtual parent directory does not exist"));}let used:usize=io.files.values().map(String::len).sum();if used-io.files.get(&name).map_or(0,String::len)+data.len()>1_048_576||(!io.files.contains_key(&name)&&io.files.len()>=256){return Err(Error::lua_string("VirtualFilesystemLimit: storage limit exceeded"));}io.files.insert(name,data);Ok(())}
fn set<A:mlua::FromLuaMulti,R:mlua::IntoLuaMulti>(lua:&Lua,env:&Table,name:&str,f:impl Fn(&Lua,A)->Result<R>+'static)->Result<()>{let function=lua.create_function(f)?;crate::native::name_function(lua,&function,&CString::new(name).unwrap())?;env.set(name,function)}
pub fn install(lua:&Lua)->Result<()> {
    let game=lua.globals();let env=lua.create_table()?;let mt=lua.create_table()?;mt.set("__index",game.clone())?;env.set_metatable(Some(mt))?;
    env.set("_G",env.clone())?;env.set("shared",lua.create_table()?)?;
    let emu=lua.create_table()?;let caps=lua.create_table()?;let unsupported=lua.create_table()?;
    emu.set("executorEnvironment",env.clone())?;emu.set("capabilities",caps.clone())?;emu.set("unsupported",unsupported.clone())?;
    let executor_env=env.clone();set(lua,&env,"getgenv",move|_,()|{executor_env.set_safeenv(false);Ok(executor_env.clone())})?;
    let game_env=game.clone();set(lua,&env,"getrenv",move|_,()|Ok(game_env.clone()))?;
    set(lua,&env,"checkcaller",|lua,()|Ok(crate::scheduler::current_origin(lua)=="executor"))?;
    set(lua,&env,"identifyexecutor",|_,()|Ok(("RobloxEmulator","0.2.0")))?;
    let rt:Table=game.get("__rt")?;env.set("getconnections",rt.get::<Function>("getConnections")?)?;
    for (name,target) in [("getinstances","getInstances"),("getnilinstances","getNilInstances"),("getsenv","getEnvironment")]{set(lua,&env,name,move|lua,args:MultiValue|crate::internal::<Table>(lua,"__model")?.get::<Function>(target)?.call::<MultiValue>(args))?;}
    set(lua,&env,"cloneref",|lua,value:Value|lua.create_userdata(crate::instance::Instance(instance_arg(value,"cloneref",1)?)))?;
    set(lua,&env,"compareinstances",|_,(left,right):(Value,Value)|Ok(Rc::ptr_eq(&instance_arg(left,"compareinstances",1)?,&instance_arg(right,"compareinstances",2)?)))?;
    set(lua,&env,"getcallingscript",|lua,()|crate::model::calling_script(lua))?;
    set(lua,&env,"getscripts",|lua,()|crate::model::scripts(lua,false))?;
    set(lua,&env,"getloadedmodules",|lua,()|crate::model::scripts(lua,true))?;
    for name in ["cloneref","compareinstances","getcallingscript","getscripts","getloadedmodules"]{caps.set(name,"native: isolated DataModel and current thread environment")?;}
    for (name,host) in [("getgc","__host_getgc"),("iscclosure","__host_iscclosure"),("islclosure","__host_islclosure"),("newcclosure","__host_newcclosure"),("clonefunction","__host_clonefunction"),("getrawmetatable","__host_getrawmetatable"),("setrawmetatable","__host_setrawmetatable"),("setreadonly","__host_setreadonly"),("isreadonly","__host_isreadonly"),("getnamecallmethod","__host_getnamecallmethod"),("hookfunction","__host_hookfunction"),("restorefunction","__host_restorefunction"),("hookmetamethod","__host_hookmetamethod")]{env.set(name,game.get::<Function>(host)?)?;caps.set(name,"native: isolated Luau VM")?;}
    env.set("filtergc",crate::native::bind_filtergc(lua,&env,&unsupported)?)?;caps.set("filtergc","native: Name, Constants, Upvalues, IgnoreExecutor, Keys, Values, KeyValuePairs, Metatable; Hash unsupported")?;
    let debug=lua.create_table()?;for pair in game.get::<Table>("debug")?.pairs::<Value,Value>(){let(k,v)=pair?;debug.set(k,v)?;}
    for(name,host)in[("getupvalue","__host_getupvalue"),("getupvalues","__host_getupvalues"),("setupvalue","__host_setupvalue")]{let function=game.get::<Function>(host)?;crate::native::mark_executor_function(lua,&function,&env)?;debug.set(name,function)?;}env.set("debug",debug)?;
    let io=Rc::new(RefCell::new(VirtualIo::default()));io.borrow_mut().folders.insert(String::new());
    let s=io.clone();set(lua,&env,"writefile",move|_,(name,data):(String,String)|write(&s,&name,data))?;
    let s=io.clone();set(lua,&env,"appendfile",move|_,(name,data):(String,String)|{let name=path(&name)?;let mut previous=s.borrow().files.get(&name).cloned().unwrap_or_default();previous.push_str(&data);write(&s,&name,previous)})?;
    let s=io.clone();set(lua,&env,"readfile",move|_,name:String|s.borrow().files.get(&path(&name)?).cloned().ok_or_else(||Error::lua_string("virtual file does not exist")))?;
    let s=io.clone();set(lua,&env,"makefolder",move|_,name:String|{let name=path(&name)?;let mut s=s.borrow_mut();if s.files.contains_key(&name)||!s.folders.contains(parent(&name)){return Err(Error::lua_string("invalid virtual directory"));}if s.folders.len()>=256&&!s.folders.contains(&name){return Err(Error::lua_string("VirtualFilesystemLimit: directory limit exceeded"));}s.folders.insert(name);Ok(())})?;
    let s=io.clone();set(lua,&env,"isfile",move|_,name:String|Ok(s.borrow().files.contains_key(&path(&name)?)))?;
    let s=io.clone();set(lua,&env,"isfolder",move|_,name:String|Ok(s.borrow().folders.contains(&path(&name)?)))?;
    let s=io.clone();set(lua,&env,"listfiles",move|_,name:String|{let name=path(&name)?;let s=s.borrow();if !s.folders.contains(&name){return Err(Error::lua_string("virtual directory does not exist"));}Ok(s.files.keys().chain(s.folders.iter()).filter(|p|!p.is_empty()&&parent(p)==name).cloned().collect::<Vec<_>>())})?;
    let s=io.clone();set(lua,&env,"delfile",move|_,name:String|{if s.borrow_mut().files.remove(&path(&name)?).is_none(){return Err(Error::lua_string("virtual file does not exist"));}Ok(())})?;
    let s=io.clone();set(lua,&env,"delfolder",move|_,name:String|{let name=path(&name)?;let mut s=s.borrow_mut();if name.is_empty()||!s.folders.contains(&name){return Err(Error::lua_string("invalid virtual directory"));}let prefix=name.clone()+"/";s.files.retain(|n,_|!n.starts_with(&prefix));s.folders.retain(|n|n!=&name&&!n.starts_with(&prefix));Ok(())})?;
    let s=io.clone();set(lua,&env,"setclipboard",move|_,data:String|{if data.len()>65536{return Err(Error::lua_string("virtual clipboard size limit exceeded"));}s.borrow_mut().clipboard=data;Ok(())})?;
    let s=io.clone();set(lua,&env,"getclipboard",move|_,()|Ok(s.borrow().clipboard.clone()))?;
    for(name,level)in[("rconsoleprint","executor"),("rconsolewarn","executor_warn"),("rconsoleerr","executor_error")]{set(lua,&env,name,move|lua,message:Value|{let tostring:Function=lua.globals().get("tostring")?;let message:String=tostring.call(message)?;crate::internal::<Function>(lua,"__host_log")?.call::<()>((level,message))})?;}
    let e=env.clone();set(lua,&env,"loadstring",move|lua,(source,name):(String,Option<String>)|{if source.len()>8*1024*1024{return Err(Error::lua_string("source size limit exceeded"));}e.set_safeenv(false);match lua.load(&source).set_mode(mlua::chunk::ChunkMode::Text).set_name(name.as_deref().unwrap_or("=executor")).set_environment(e.clone()).into_function(){Ok(f)=>{crate::native::mark_executor_function(lua,&f,&e)?;Ok((Value::Function(f),Value::Nil))},Err(e)=>Ok((Value::Nil,Value::String(lua.create_string(e.to_string())?)))}})?;
    let e=env.clone();let s=io.clone();set(lua,&env,"loadfile",move|lua,name:String|{let name=path(&name)?;let data=s.borrow().files.get(&name).cloned().ok_or_else(||Error::lua_string("virtual file does not exist"))?;e.set_safeenv(false);let f=lua.load(data).set_mode(mlua::chunk::ChunkMode::Text).set_name(format!("@virtual/{name}")).set_environment(e.clone()).into_function()?;crate::native::mark_executor_function(lua,&f,&e)?;Ok(f)})?;
    for name in ["getreg","isexecutorclosure","setnamecallmethod","getthreadidentity","setthreadidentity","gethiddenproperty","sethiddenproperty","getscriptbytecode","getscriptclosure","request","http_request","queue_on_teleport","firetouchinterest","fireclickdetector","fireproximityprompt","setfpscap","getfpscap","shutdown"] {
        let unsupported=unsupported.clone();caps.set(name,"unsupported")?;set(lua,&env,name,move|lua,_:MultiValue|{if unsupported.raw_len()<1000{let event=lua.create_table()?;event.set("api",name)?;unsupported.raw_push(event)?;}Err::<(),_>(Error::lua_string(format!("UnsupportedApi: {name} is not implemented by this emulator")))})?;
    }
    set(lua,&emu,"runExecutor",|lua,(source,name):(String,Option<String>)|run(lua,&source,name.as_deref().unwrap_or("=executor")))?;
    set(lua,&emu,"virtualState",move|lua,()|{let s=io.borrow();let t=lua.create_table()?;t.set("fileCount",s.files.len())?;t.set("fileBytes",s.files.values().map(String::len).sum::<usize>())?;t.set("clipboardBytes",s.clipboard.len())?;Ok(t)})?;
    for entry in env.pairs::<Value,Value>(){let(_,value)=entry?;if let Value::Function(function)=value{crate::native::mark_executor_function(lua,&function,&env)?;}}
    env.set_safeenv(true);game.set("__emu",emu)
}

#[cfg(all(test, emulator_local_tests))]
#[path = "../local_tests/executor.rs"]
mod tests;
