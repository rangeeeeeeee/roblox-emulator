use mlua::{Function, Lua, Result, Table, ffi, chunk::Compiler};

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
    fn emu_set_executor_profile(state: *mut ffi::lua_State, profile: std::ffi::c_int);
    fn emu_getgc(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_filtergc(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_mark_executor(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_iscclosure(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_islclosure(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_getupvalue(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_getupvalues(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_setupvalue(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_newcclosure(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_clonefunction(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_getrawmetatable(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_setrawmetatable(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_setreadonly(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_isreadonly(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_getnamecallmethod(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_hookfunction(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_restorefunction(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_hookmetamethod(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_collectgarbage(state: *mut ffi::lua_State) -> std::ffi::c_int;
    fn emu_setfunctionname(state: *mut ffi::lua_State, name: *const std::ffi::c_char);
}

pub fn set_executor_profile(lua: &Lua, name: &str) -> Result<()> {
    let profile=match name {"generic"=>0,"macsploit-744.2"=>1,_=>return Err(mlua::Error::runtime("Unknown executor profile"))};
    unsafe {lua.exec_raw((),|state|emu_set_executor_profile(state,profile))}
}

pub fn name_function(lua: &Lua, function: &Function, name: &std::ffi::CStr) -> Result<()> {
    set_function_name(lua, function, name.as_ptr())
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
    let functions: [(&str, &std::ffi::CStr, ffi::lua_CFunction); 18] = [
        ("__host_getgc", c"getgc", emu_getgc),
        ("__host_filtergc", c"filtergc", emu_filtergc),
        ("__host_iscclosure", c"iscclosure", emu_iscclosure),
        ("__host_islclosure", c"islclosure", emu_islclosure),
        ("__host_getupvalue", c"getupvalue", emu_getupvalue),
        ("__host_getupvalues", c"getupvalues", emu_getupvalues),
        ("__host_setupvalue", c"setupvalue", emu_setupvalue),
        ("__host_newcclosure", c"newcclosure", emu_newcclosure),
        ("__host_clonefunction", c"clonefunction", emu_clonefunction),
        ("__host_getrawmetatable", c"getrawmetatable", emu_getrawmetatable),
        ("__host_setrawmetatable", c"setrawmetatable", emu_setrawmetatable),
        ("__host_setreadonly", c"setreadonly", emu_setreadonly),
        ("__host_isreadonly", c"isreadonly", emu_isreadonly),
        ("__host_getnamecallmethod", c"getnamecallmethod", emu_getnamecallmethod),
        ("__host_hookfunction", c"hookfunction", emu_hookfunction),
        ("__host_restorefunction", c"restorefunction", emu_restorefunction),
        ("__host_hookmetamethod", c"hookmetamethod", emu_hookmetamethod),
        ("collectgarbage", c"collectgarbage", emu_collectgarbage),
    ];
    for (name, debug_name, function) in functions {
        let closure: Function = unsafe {
            lua.exec_raw((), |state| ffi::lua_pushcfunctiond(state, function, debug_name.as_ptr()))?
        };
        lua.globals().set(name, closure)?;
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
