use mlua::{Function, Lua, Result, Table, ffi, chunk::Compiler};
use sha2::{Digest, Sha384};

// Private registry keys are host addresses, so their names never enter the
// Luau heap. The slots are shared as identifiers, not as registry values.
static PRIVATE_REGISTRY_KEYS: [u8; 19] = [0; 19];
fn private_registry_key(name:&[u8])->Option<*const std::ffi::c_void>{
    let index=match name {
        b"emulator.internal"=>0,
        b"emulator.inspector.values"=>1, b"emulator.inspector.scan"=>2,
        b"emulator.inspector.viewer.metatable"=>3,
        b"emulator.drawing.methods"=>4,
        b"emulator.native.instances"=>5, b"emulator.native.instance.methods"=>6,
        b"emulator.native.environments"=>7, b"emulator.native.environment.owners"=>8,
        b"emulator.native.script.threads"=>9, b"emulator.native.base.environment"=>10,
        b"emulator.memory.default.environments"=>11, b"emulator.memory.default.threads"=>12,
        b"emulator.signal.metatable"=>13, b"emulator.connection.metatable"=>14,
        b"emulator.connectionview.metatable"=>15, b"emulator.scheduler.tostring"=>16,
        b"emulator.inspector.objects"=>17, b"emulator.executor.unsupported"=>18,
        _=>return None,
    };
    Some((&PRIVATE_REGISTRY_KEYS[index] as *const u8).cast())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn emu_private_registry_key(name:*const std::ffi::c_char)->*const std::ffi::c_void{
    if name.is_null(){return std::ptr::null()}
    private_registry_key(unsafe{std::ffi::CStr::from_ptr(name)}.to_bytes()).unwrap_or(std::ptr::null())
}

pub(crate) fn private_registry_value<T:mlua::FromLua>(lua:&Lua,name:&str)->Result<T>{
    let key=private_registry_key(name.as_bytes()).ok_or_else(||mlua::Error::runtime("Unknown private registry key"))?;
    unsafe {lua.exec_raw((),|state|{ffi::lua_rawgetp(state,ffi::LUA_REGISTRYINDEX,key);})}
}
pub(crate) fn set_private_registry_value<T:mlua::IntoLua>(lua:&Lua,name:&str,value:T)->Result<()>{
    let key=private_registry_key(name.as_bytes()).ok_or_else(||mlua::Error::runtime("Unknown private registry key"))?;
    unsafe {lua.exec_raw(value,|state|{ffi::lua_rawsetp(state,ffi::LUA_REGISTRYINDEX,key);})}
}

pub(crate) unsafe fn raw_private_registry_get(state:*mut ffi::lua_State,name:&std::ffi::CStr){
    let key=private_registry_key(name.to_bytes()).unwrap_or(std::ptr::null());
    if key.is_null(){unsafe{ffi::luaL_error(state,c"Unknown private registry key".as_ptr())}}
    unsafe{ffi::lua_rawgetp(state,ffi::LUA_REGISTRYINDEX,key);}
}
pub(crate) unsafe fn raw_private_registry_set(state:*mut ffi::lua_State,name:&std::ffi::CStr){
    let key=private_registry_key(name.to_bytes()).unwrap_or(std::ptr::null());
    if key.is_null(){unsafe{ffi::luaL_error(state,c"Unknown private registry key".as_ptr())}}
    unsafe{ffi::lua_rawsetp(state,ffi::LUA_REGISTRYINDEX,key);}
}

pub(crate) fn register_gc_viewer_objects(lua:&Lua,viewer:&mlua::AnyUserData)->Result<()>{
    let objects:Option<Table>=private_registry_value(lua,"emulator.inspector.objects")?;
    let objects=match objects {
        Some(objects)=>objects,
        None=>{
            let objects=lua.create_table()?;let meta=lua.create_table()?;
            meta.raw_set("__mode","k")?;objects.set_metatable(Some(meta))?;
            set_private_registry_value(lua,"emulator.inspector.objects",objects.clone())?;objects
        },
    };
    unsafe {lua.exec_raw::<()>((viewer.clone(),objects),|state|emu_inspector_mark_objects(state))}
}

// Capability descriptions are host diagnostics, not part of the Lua heap.
#[derive(Clone,Default)]
pub(crate) struct Capabilities(std::rc::Rc<std::cell::RefCell<std::collections::BTreeMap<String,&'static str>>>);
impl Capabilities {
    pub(crate) fn set(&self,name:impl Into<String>,description:&'static str)->Result<()>{
        self.0.borrow_mut().insert(name.into(),description);Ok(())
    }
}

pub(crate) fn random_bytes(bytes:&mut [u8])->std::io::Result<()> {
    #[cfg(windows)] {
        #[link(name="bcrypt")]
        unsafe extern "system" {fn BCryptGenRandom(algorithm:*mut std::ffi::c_void,output:*mut u8,length:u32,flags:u32)->i32;}
        if unsafe{BCryptGenRandom(std::ptr::null_mut(),bytes.as_mut_ptr(),bytes.len() as u32,2)}<0 {
            return Err(std::io::Error::other("Cannot obtain operating-system randomness"));
        }
    }
    #[cfg(not(windows))] {use std::io::Read;std::fs::File::open("/dev/urandom")?.read_exact(bytes)?;}
    Ok(())
}

unsafe extern "C-unwind" {
    fn emu_spy_frame_pointer(state: *mut ffi::lua_State, level: std::ffi::c_int) -> *const std::ffi::c_void;
    fn emu_inspector_mark_objects(state: *mut ffi::lua_State);
    fn emu_set_executor_profile(state: *mut ffi::lua_State, profile: std::ffi::c_int);
    fn emu_getgc(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_inspectgc(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_filtergc(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_mark_executor(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_iscclosure(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_islclosure(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_isexecutorclosure(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_getinfo(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_setname(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_setinfo(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_getsafeenv(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_setsafeenv(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_getcallstack(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_bind_dofile(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_getupvalue(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_getupvalues(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_setupvalue(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_getconstant(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_getconstants(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_inspectconstants(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_setconstant(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_getproto(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_getprotos(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_getstack(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_setstack(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_newcclosure(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_isnewcclosure(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_isfunctionhooked(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_clonefunction(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_getrawmetatable(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_setrawmetatable(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_setreadonly(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_isreadonly(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_makereadonly(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_makewritable(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_getnamecallmethod(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_setnamecallmethod(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_isvalidlevel(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_hookfunction(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_restorefunction(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_hookmetamethod(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_collectgarbage(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_setfunctionname(state: *mut ffi::lua_State, name: *const std::ffi::c_char);
    fn emu_feed_function_hash(state: *mut ffi::lua_State, context: *mut std::ffi::c_void, feed: unsafe extern "C" fn(*mut std::ffi::c_void, *const u8, usize)) -> std::ffi::c_int;
}

pub(crate) fn spy_frame_pointer(lua: &Lua, level: i32) -> *const std::ffi::c_void {
    unsafe { emu_spy_frame_pointer(lua.state(), level) }
}

unsafe extern "C" fn feed_function_hash(context: *mut std::ffi::c_void, data: *const u8, len: usize) {
    let hash = unsafe { &mut *context.cast::<Sha384>() };
    if len != 0 { hash.update(unsafe { std::slice::from_raw_parts(data, len) }); }
}

pub(crate) fn function_hash(lua: &Lua, function: &Function) -> Result<String> {
    let mut hash = Sha384::new();
    let mut status = 0;
    unsafe {
        lua.exec_raw::<()>(function.clone(), |state| {
            status = emu_feed_function_hash(state, (&mut hash as *mut Sha384).cast(), feed_function_hash);
            ffi::lua_settop(state, 0);
        })?;
    }
    match status {
        0 => Ok(format!("{:x}", hash.finalize())),
        1 => Err(mlua::Error::lua_string("getfunctionhash expects a Lua closure")),
        2 => Err(mlua::Error::lua_string("UnsupportedApi: getfunctionhash cannot serialize this complex constant")),
        3 => Err(mlua::Error::lua_string("UnsupportedApi: getfunctionhash encountered a cyclic or invalid prototype")),
        _ => Err(mlua::Error::lua_string("UnsupportedApi: getfunctionhash exceeded its prototype budget")),
    }
}

struct ExecutorProfile(i32);
pub(crate) fn is_macsploit_7444(lua: &Lua) -> bool {
    lua.app_data_ref::<ExecutorProfile>().is_some_and(|profile| profile.0 == 2)
}
pub(crate) fn loadfile_returns_error_tuple(lua: &Lua) -> bool {
    lua.app_data_ref::<ExecutorProfile>().is_some_and(|profile| matches!(profile.0, 2 | 3))
}
pub(crate) fn is_unc_profile(lua: &Lua) -> bool {
    lua.app_data_ref::<ExecutorProfile>().is_some_and(|profile| profile.0 == 3)
}
pub(crate) fn executor_compiler(lua: &Lua) -> Compiler {
    let compiler=Compiler::new().set_mutable_globals(["_G", "shared", "script"]);
    if is_unc_profile(lua) {
        compiler.set_debug_level(2)
    } else {
        compiler
    }
}
pub fn set_executor_profile(lua: &Lua, name: &str) -> Result<()> {
    let (profile,native)=match name {"generic"=>(0,0),"macsploit-744.2"=>(1,1),"macsploit-744.4"=>(2,2),"unc"=>(3,0),_=>return Err(mlua::Error::runtime("Unknown executor profile"))};
    unsafe {lua.exec_raw::<()>((),|state|emu_set_executor_profile(state,native))?;}
    lua.set_app_data(ExecutorProfile(profile));
    Ok(())
}

pub fn name_function(lua: &Lua, function: &Function, name: &std::ffi::CStr) -> Result<()> {
    set_function_name(lua, function, name.as_ptr())
}

unsafe extern "C-unwind" fn unavailable_client_loadstring(state:*mut ffi::lua_State)->i32 {
    unsafe { ffi::lua_pushstring(state,c"loadstring() is not available".as_ptr());ffi::lua_error(state) }
}
pub(crate) fn disable_client_loadstring(lua:&Lua)->Result<()> {
    let function:Function=unsafe{lua.exec_raw((),|state|ffi::lua_pushcfunctiond(state,unavailable_client_loadstring,c"loadstring".as_ptr()))?};
    lua.globals().set("loadstring",function)
}

pub fn clear_function_name(lua: &Lua, function: &Function) -> Result<()> {
    set_function_name(lua, function, std::ptr::null())
}

pub fn mark_executor_function(lua: &Lua, function: &Function, environment: &Table) -> Result<()> {
    unsafe {
        lua.exec_raw((function.clone(), environment.clone()), |state| {
            emu_mark_executor(state);
            ffi::lua_settop(state, 0);
        })
    }
}

pub fn bind_filtergc(lua: &Lua, environment: &Table, unsupported: &Table) -> Result<Function> {
    unsafe {
        lua.exec_raw((environment.clone(), unsupported.clone()), |state| {
            ffi::lua_pushcclosured(state, emu_filtergc, c"filtergc".as_ptr(), 2);
        })
    }
}

pub(crate) fn bind_dofile(lua: &Lua, function: &Function) -> Result<Function> {
    unsafe { lua.exec_raw(function.clone(), |state| { emu_bind_dofile(state); }) }
}

pub(crate) fn bind_debug_helpers(lua: &Lua, environment: &Table, debug: &Table) -> Result<()> {
    for (name, debug_name, native) in [
        ("setname", c"setname", emu_setname as ffi::lua_CFunction),
        ("setinfo", c"setinfo", emu_setinfo as ffi::lua_CFunction),
        ("getsafeenv", c"getsafeenv", emu_getsafeenv as ffi::lua_CFunction),
        ("setsafeenv", c"setsafeenv", emu_setsafeenv as ffi::lua_CFunction),
        ("getcallstack", c"getcallstack", emu_getcallstack as ffi::lua_CFunction),
    ] {
        let function: Function = unsafe { lua.exec_raw((), |state| ffi::lua_pushcfunctiond(state, native, debug_name.as_ptr()))? };
        mark_executor_function(lua, &function, environment)?;
        debug.set(name, function)?;
    }
    Ok(())
}

pub fn bind_isexecutorclosure(lua: &Lua, environment: &Table) -> Result<Function> {
    unsafe {
        lua.exec_raw(environment.clone(), |state| {
            ffi::lua_pushcclosured(state, emu_isexecutorclosure, c"isexecutorclosure".as_ptr(), 1);
        })
    }
}

pub(crate) fn inspect_constants(lua:&Lua,function:&Function)->Result<Table>{
    unsafe{lua.exec_raw(function.clone(),|state|{emu_inspectconstants(state);ffi::lua_replace(state,1);ffi::lua_settop(state,1);})}
}

pub(crate) fn gc_snapshot(lua:&Lua)->Result<Table>{
    unsafe{lua.exec_raw((),|state|{emu_inspectgc(state);})}
}

fn set_function_name(lua: &Lua, function: &Function, name: *const std::ffi::c_char) -> Result<()> {
    unsafe {
        lua.exec_raw(function.clone(), |state| {
            emu_setfunctionname(state, name);
            ffi::lua_pop(state, 1);
        })
    }
}

pub fn install(lua: &Lua) -> Result<()> {
    lua.set_compiler(Compiler::new().set_mutable_globals(["_G", "shared", "script"]));
    let functions: [(&str, &std::ffi::CStr, ffi::lua_CFunction); 32] = [
        ("__host_getgc", c"getgc", emu_getgc),
        ("__host_filtergc", c"filtergc", emu_filtergc),
        ("__host_iscclosure", c"iscclosure", emu_iscclosure),
        ("__host_islclosure", c"islclosure", emu_islclosure),
        ("__host_getinfo", c"getinfo", emu_getinfo),
        ("__host_getupvalue", c"getupvalue", emu_getupvalue),
        ("__host_getupvalues", c"getupvalues", emu_getupvalues),
        ("__host_setupvalue", c"setupvalue", emu_setupvalue),
        ("__host_getconstant", c"getconstant", emu_getconstant),
        ("__host_getconstants", c"getconstants", emu_getconstants),
        ("__host_setconstant", c"setconstant", emu_setconstant),
        ("__host_getproto", c"getproto", emu_getproto),
        ("__host_getprotos", c"getprotos", emu_getprotos),
        ("__host_getstack", c"getstack", emu_getstack),
        ("__host_setstack", c"setstack", emu_setstack),
        ("__host_newcclosure", c"newcclosure", emu_newcclosure),
        ("__host_isnewcclosure", c"isnewcclosure", emu_isnewcclosure),
        ("__host_isfunctionhooked", c"isfunctionhooked", emu_isfunctionhooked),
        ("__host_clonefunction", c"clonefunction", emu_clonefunction),
        ("__host_getrawmetatable", c"getrawmetatable", emu_getrawmetatable),
        ("__host_setrawmetatable", c"setrawmetatable", emu_setrawmetatable),
        ("__host_setreadonly", c"setreadonly", emu_setreadonly),
        ("__host_isreadonly", c"isreadonly", emu_isreadonly),
        ("__host_makereadonly", c"makereadonly", emu_makereadonly),
        ("__host_makewritable", c"makewritable", emu_makewritable),
        ("__host_getnamecallmethod", c"getnamecallmethod", emu_getnamecallmethod),
        ("__host_setnamecallmethod", c"setnamecallmethod", emu_setnamecallmethod),
        ("__host_isvalidlevel", c"isvalidlevel", emu_isvalidlevel),
        ("__host_hookfunction", c"hookfunction", emu_hookfunction),
        ("__host_restorefunction", c"restorefunction", emu_restorefunction),
        ("__host_hookmetamethod", c"hookmetamethod", emu_hookmetamethod),
        ("collectgarbage", c"collectgarbage", emu_collectgarbage),
    ];
    for (name, debug_name, function) in functions {
        let closure: Function = unsafe {
            lua.exec_raw((), |state| ffi::lua_pushcfunctiond(state, function, debug_name.as_ptr()))?
        };
        if name.starts_with("__") {crate::set_internal(lua,name,closure)?;}
        else {lua.globals().set(name,closure)?;}
    }
    Ok(())
}

#[unsafe(no_mangle)]
pub extern "C" fn emu_is_protected_function(function: ffi::lua_CFunction) -> std::ffi::c_int {
    Lua::is_protected_c_function(function) as std::ffi::c_int
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn emu_is_reference_thread(state: *mut ffi::lua_State, thread: *mut ffi::lua_State) -> std::ffi::c_int {
    unsafe { Lua::is_reference_thread(state, thread) as std::ffi::c_int }
}

#[cfg(all(test, emulator_local_tests))]
#[path = "../local_tests/native.rs"]
mod tests;
