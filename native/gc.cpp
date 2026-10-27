#include "lua.h"
#include "lualib.h"
#include "lapi.h"
#include "lgc.h"
#include "lmem.h"
#include "lstate.h"
#include "lstring.h"
#include "Luau/BytecodeUtils.h"
#include <climits>
#include <cmath>
#include <cstdint>
#include <cstring>
#include <initializer_list>

extern "C" int emu_is_protected_function(lua_CFunction function);
extern "C" int emu_is_reference_thread(lua_State* state, lua_State* thread);

static char executor_profile_key = 0;
static constexpr uint32_t executor_profile_magic = 0x45505246;
struct ExecutorProfile { uint32_t magic; int id; };

static int executor_profile(lua_State* state)
{
    lua_rawgetp(state, LUA_REGISTRYINDEX, &executor_profile_key);
    if (lua_isnil(state, -1)) { lua_pop(state, 1); return 0; }
    if (lua_type(state, -1) != LUA_TUSERDATA || lua_objlen(state, -1) != sizeof(ExecutorProfile))
        luaL_error(state, "ExecutorProfileError: invalid native profile record");
    const ExecutorProfile* profile = static_cast<const ExecutorProfile*>(lua_touserdata(state, -1));
    if (profile->magic != executor_profile_magic || (profile->id != 0 && profile->id != 1))
        luaL_error(state, "ExecutorProfileError: invalid native profile record");
    const int id = profile->id;
    lua_pop(state, 1);
    return id;
}

// Rust host configuration only; this setter is never registered in a script environment.
extern "C" void emu_set_executor_profile(lua_State* state, int id)
{
    if (id != 0 && id != 1) luaL_error(state, "ExecutorProfileError: unknown native profile");
    ExecutorProfile* profile = static_cast<ExecutorProfile*>(lua_newuserdata(state, sizeof(ExecutorProfile)));
    profile->magic = executor_profile_magic;
    profile->id = id;
    lua_rawsetp(state, LUA_REGISTRYINDEX, &executor_profile_key);
}

extern "C" void emu_setfunctionname(lua_State* state, const char* name)
{
    luaL_checktype(state, -1, LUA_TFUNCTION);
    const TValue* value = luaA_toobject(state, -1);
    if (!iscfunction(value))
        luaL_error(state, "native debug names require a C closure");
    Closure* closure = clvalue(value);
    TString* debugname = name ? luaS_new(state, name) : nullptr;
    closure->c.debugname = debugname;
    if (debugname)
        luaC_objbarrier(state, closure, debugname);
}

struct GcPause
{
    global_State* global;
    size_t threshold;
    explicit GcPause(lua_State* state): global(state->global), threshold(global->GCthreshold)
    {
        global->GCthreshold = SIZE_MAX;
    }
    ~GcPause() { global->GCthreshold = threshold; }
};

struct Scan
{
    lua_State* state;
    bool tables;
    bool extended;
    bool buffers;
    bool fill;
    int count;
    const void* result;
    const void* inspector;
    const void* inspector_metatable;
    const void* inspector_scan;
    const void* registry;
    const void* host_tables[10] = {};
    int host_table_count = 0;
};

static bool visit(void* context, lua_Page*, GCObject* object)
{
    Scan& scan = *static_cast<Scan*>(context);
    for (int i = 0; i < scan.host_table_count; ++i)
        if (static_cast<const void*>(object) == scan.host_tables[i]) return false;
    if (isdead(scan.state->global, object) || static_cast<const void*>(object) == scan.result
        || static_cast<const void*>(object) == scan.inspector || static_cast<const void*>(object) == scan.inspector_metatable
        || static_cast<const void*>(object) == scan.inspector_scan || static_cast<const void*>(object) == scan.registry)
        return false;
    int type = object->gch.tt;
    if (type != LUA_TFUNCTION && type != LUA_TUSERDATA && !(scan.tables && type == LUA_TTABLE)
        && !(scan.extended && (type == LUA_TSTRING || type == LUA_TTHREAD))
        && !(scan.buffers && type == LUA_TBUFFER))
        return false;
    // This host-only thread stores mlua reference slots and cannot be resumed/reset by scripts.
    if (type == LUA_TTHREAD && emu_is_reference_thread(scan.state, gco2th(object)))
        return false;
    if (++scan.count > 1000000)
        luaL_error(scan.state, "GcEnumerationLimit: more than 1000000 objects");
    if (scan.fill)
    {
        TValue value = {};
        value.value.gc = object;
        value.tt = type;
        luaA_pushvalue(scan.state, &value);
        lua_rawseti(scan.state, -2, scan.count);
    }
    return false;
}

static Scan prepare_scan(lua_State* state, bool tables, bool extended)
{
    // Only these host bookkeeping tables are excluded; their user values remain visible.
    lua_rawgetfield(state, LUA_REGISTRYINDEX, "emulator.inspector.values");
    const void* inspector = lua_topointer(state, -1);
    const void* inspector_metatable = nullptr;
    if (lua_getmetatable(state, -1)) { inspector_metatable = lua_topointer(state, -1); lua_pop(state, 1); }
    lua_pop(state, 1);
    lua_rawgetfield(state, LUA_REGISTRYINDEX, "emulator.inspector.scan");
    const void* inspector_scan = lua_topointer(state, -1);
    lua_pop(state, 1);
    Scan scan{state, tables, extended, false, false, 0, nullptr, inspector, inspector_metatable,
        inspector_scan, lua_topointer(state, LUA_REGISTRYINDEX)};
    lua_rawgetfield(state, LUA_REGISTRYINDEX, "emulator.internal");
    if (lua_istable(state, -1))
    {
        scan.host_tables[scan.host_table_count++] = lua_topointer(state, -1);
        for (const char* name : {"__config", "__emu", "__rt", "__model", "__types"})
        {
            lua_rawgetfield(state, -1, name);
            if (lua_istable(state, -1)) scan.host_tables[scan.host_table_count++] = lua_topointer(state, -1);
            lua_pop(state, 1);
        }
        lua_rawgetfield(state, -1, "__emu");
        if (lua_istable(state, -1))
            for (const char* name : {"capabilities", "unsupported"})
            {
                lua_rawgetfield(state, -1, name);
                if (lua_istable(state, -1)) scan.host_tables[scan.host_table_count++] = lua_topointer(state, -1);
                lua_pop(state, 1);
            }
        lua_pop(state, 1);
    }
    lua_pop(state, 1);
    return scan;
}

static void scan_objects(Scan& scan)
{
    lua_State* state = scan.state;
    luaM_visitgco(state, &scan, visit);
    // The real main thread is allocated with global_State, outside collectible pages.
    if (scan.extended) visit(&scan, nullptr, obj2gco(state->global->mainthread));
}

extern "C" int emu_getgc(lua_State* state)
{
    if (!lua_isnoneornil(state, 1))
        luaL_checktype(state, 1, LUA_TBOOLEAN);
    bool tables = lua_toboolean(state, 1) != 0;
    const bool extended = executor_profile(state) == 1;
    lua_settop(state, 0);
    luaL_checkstack(state, 3, "getgc");
    GcPause pause(state);
    Scan scan = prepare_scan(state, tables, extended);
    scan_objects(scan);
    lua_createtable(state, scan.count, 0);
    scan.result = lua_topointer(state, -1);
    scan.count = 0;
    scan.fill = true;
    scan_objects(scan);
    return 1;
}

// Host-only snapshot: the inspector clears this reused scratch after serialization.
extern "C" int emu_inspectgc(lua_State* state)
{
    lua_settop(state, 0);
    luaL_checkstack(state, 3, "inspectgc");
    lua_rawgetfield(state, LUA_REGISTRYINDEX, "emulator.inspector.scan");
    if (!lua_istable(state, -1)) luaL_error(state, "Inspector scan table is not initialized");
    lua_cleartable(state, -1);
    GcPause pause(state);
    // The viewer inspects the actual VM heap, independent of getgc profile filters.
    Scan scan = prepare_scan(state, true, true);
    scan.buffers = true;
    scan.result = lua_topointer(state, -1);
    scan.fill = true;
    scan_objects(scan);
    return 1;
}

extern "C" int emu_iscclosure(lua_State* state)
{
    luaL_checktype(state, 1, LUA_TFUNCTION);
    lua_pushboolean(state, lua_iscfunction(state, 1));
    return 1;
}

extern "C" int emu_collectgarbage(lua_State* state)
{
    const char* option = luaL_checkstring(state, 1);
    if (std::strcmp(option, "count") != 0)
        luaL_error(state, "collectgarbage must be called with 'count'; use gcinfo() instead");
    const int kilobytes = lua_gc(state, LUA_GCCOUNT, 0);
    const int remainder = lua_gc(state, LUA_GCCOUNTB, 0);
    lua_pushnumber(state, kilobytes + remainder / 1024.0);
    return 1;
}

extern "C" int emu_islclosure(lua_State* state)
{
    luaL_checktype(state, 1, LUA_TFUNCTION);
    lua_pushboolean(state, lua_isLfunction(state, 1));
    return 1;
}

static Closure* check_lua_closure(lua_State* state)
{
    if (lua_type(state, 1) == LUA_TNUMBER)
    {
        const double level = lua_tonumber(state, 1);
        if (!std::isfinite(level) || level < 0 || level > INT_MAX || std::floor(level) != level)
            luaL_argerror(state, 1, "invalid stack level");
        lua_Debug info = {};
        if (!lua_getinfo(state, int(level), "f", &info)) luaL_argerror(state, 1, "stack level out of range");
        lua_replace(state, 1);
    }
    else if (!lua_isfunction(state, 1)) luaL_argerror(state, 1, "function or stack level expected");
    if (!lua_isLfunction(state, 1))
        luaL_error(state, "C closures are not supported");
    return clvalue(luaA_toobject(state, 1));
}

struct ProtoScan
{
    lua_State* state;
    Proto* proto;
    int count;
    bool fill;
};

static int check_index(lua_State* state, int argument, int count, const char* message)
{
    const double index = luaL_checknumber(state, argument);
    if (!std::isfinite(index) || index < 1 || index > count || std::floor(index) != index)
        luaL_argerror(state, argument, message);
    return int(index);
}

static bool visit_proto(void* context, lua_Page*, GCObject* object)
{
    ProtoScan& scan = *static_cast<ProtoScan*>(context);
    if (isdead(scan.state->global, object) || object->gch.tt != LUA_TFUNCTION) return false;
    Closure* closure = gco2cl(object);
    // A preloaded DUPCLOSURE with unbound captures is not an active function.
    if (closure->isC || closure->preload || closure->l.p != scan.proto) return false;
    if (++scan.count > 1000000) luaL_error(scan.state, "GcEnumerationLimit: more than 1000000 active closures");
    if (scan.fill)
    {
        TValue value;
        setclvalue(scan.state, &value, closure);
        luaA_pushvalue(scan.state, &value);
        lua_rawseti(scan.state, -2, scan.count);
    }
    return false;
}

extern "C" int emu_getproto(lua_State* state)
{
    Proto* parent = check_lua_closure(state)->l.p;
    const int index = check_index(state, 2, parent->sizep, "prototype index out of range");
    if (!lua_isnoneornil(state, 3)) luaL_checktype(state, 3, LUA_TBOOLEAN);
    if (!lua_toboolean(state, 3))
        luaL_error(state, "UnsupportedApi: inactive prototypes need a VM call guard; use getproto(function, index, true)");
    Proto* proto = parent->p[index - 1];
    lua_settop(state, 1);
    luaL_checkstack(state, 2, "getproto");
    GcPause pause(state);
    ProtoScan scan{state, proto, 0, false};
    luaM_visitgco(state, &scan, visit_proto);
    lua_createtable(state, scan.count, 0);
    scan.count = 0;
    scan.fill = true;
    luaM_visitgco(state, &scan, visit_proto);
    return 1;
}

extern "C" int emu_getprotos(lua_State* state)
{
    check_lua_closure(state);
    luaL_error(state, "UnsupportedApi: inactive prototypes need a VM call guard; use getproto(function, index, true)");
}

static CallInfo* check_stack_frame(lua_State* state)
{
    const double level = luaL_checknumber(state, 1);
    if (!std::isfinite(level) || level < 0 || level > INT_MAX || std::floor(level) != level)
        luaL_argerror(state, 1, "invalid stack level");
    if (level >= state->ci - state->base_ci) luaL_argerror(state, 1, "stack level out of range");
    CallInfo* frame = state->ci - int(level);
    if (!frame->p) luaL_error(state, "C closures are not supported");
    if (frame->flags & LUA_CALLINFO_NATIVE)
        luaL_error(state, "UnsupportedApi: native machine-code stack frames are not accessible");
    return frame;
}

extern "C" int emu_getstack(lua_State* state)
{
    CallInfo* frame = check_stack_frame(state);
    const int count = int(frame->top - frame->base);
    if (!lua_isnoneornil(state, 2))
    {
        const int index = check_index(state, 2, count, "stack index out of range");
        const TValue value = frame->base[index - 1];
        luaC_threadbarrier(state);
        luaA_pushvalue(state, &value);
        return 1;
    }
    // Snapshot before creating the result table: its temporary slots can overlap
    // the caller's register window. The scratch lives on the quota-accounted Lua stack.
    const ptrdiff_t base = frame->base - state->stack;
    const int scratch = lua_gettop(state) + 1;
    GcPause pause(state);
    luaL_checkstack(state, count + 2, "getstack");
    luaC_threadbarrier(state);
    std::memmove(state->top, state->stack + base, size_t(count) * sizeof(TValue));
    state->top += count;
    lua_createtable(state, count, 0);
    for (int index = 0; index < count; ++index)
    {
        lua_pushvalue(state, scratch + index);
        lua_rawseti(state, -2, index + 1);
    }
    return 1;
}

extern "C" int emu_setstack(lua_State* state)
{
    CallInfo* frame = check_stack_frame(state);
    const int index = check_index(state, 2, int(frame->top - frame->base), "stack index out of range");
    luaL_checkany(state, 3);
    TValue* slot = frame->base + index - 1;
    // A frame may overlap a callee's function slot. Replacing that slot while
    // the call is active would invalidate native continuation/closure metadata.
    for (CallInfo* active = frame + 1; active <= state->ci; ++active)
        if (slot == active->func)
            luaL_error(state, "UnsupportedApi: an active call's function slot cannot be overwritten");
    const TValue value = *luaA_toobject(state, 3);
    luaC_threadbarrier(state);
    setobj2s(state, slot, &value);
    return 0;
}

extern "C" int emu_getupvalue(lua_State* state)
{
    check_lua_closure(state);
    int index = luaL_checkinteger(state, 2);
    if (index < 1 || !lua_getupvalue(state, 1, index))
        luaL_error(state, "invalid upvalue index");
    return 1;
}

extern "C" int emu_getupvalues(lua_State* state)
{
    check_lua_closure(state);
    lua_settop(state, 1);
    lua_newtable(state);
    for (int index = 1; lua_getupvalue(state, 1, index); ++index)
        lua_rawseti(state, 2, index);
    return 1;
}

extern "C" int emu_setupvalue(lua_State* state)
{
    check_lua_closure(state);
    int index = luaL_checkinteger(state, 2);
    luaL_checkany(state, 3);
    lua_pushvalue(state, 3);
    if (index < 1 || !lua_setupvalue(state, 1, index))
        luaL_error(state, "invalid upvalue index");
    return 0;
}

static bool visible_constant(const TValue* value)
{
    return ttisnil(value) || ttisboolean(value) || ttisnumber(value) || ttisstring(value);
}

extern "C" int emu_getconstant(lua_State* state)
{
    Proto* proto = check_lua_closure(state)->l.p;
    const int index = luaL_checkinteger(state, 2);
    if (index < 1 || index > proto->sizek || !visible_constant(&proto->k[index - 1])) lua_pushnil(state);
    else luaA_pushvalue(state, &proto->k[index - 1]);
    return 1;
}

static void fill_constants(lua_State* state, Proto* proto)
{
    for (int index = 0; index < proto->sizek; ++index)
    {
        if (!visible_constant(&proto->k[index]) || ttisnil(&proto->k[index])) continue;
        luaA_pushvalue(state, &proto->k[index]);
        lua_rawseti(state, -2, index + 1);
    }
}

extern "C" int emu_getconstants(lua_State* state)
{
    Proto* proto = check_lua_closure(state)->l.p;
    lua_settop(state, 1);
    lua_createtable(state, proto->sizek, 0);
    fill_constants(state, proto);
    return 1;
}

extern "C" int emu_inspectconstants(lua_State* state)
{
    Proto* proto = check_lua_closure(state)->l.p;
    lua_settop(state, 1);
    lua_rawgetfield(state, LUA_REGISTRYINDEX, "emulator.inspector.scan");
    if (!lua_istable(state, -1)) luaL_error(state, "Inspector scan table is not initialized");
    lua_cleartable(state, -1);
    fill_constants(state, proto);
    return 1;
}

extern "C" int emu_setconstant(lua_State* state)
{
    Proto* proto = check_lua_closure(state)->l.p;
    const int requested = luaL_checkinteger(state, 2);
    luaL_checkany(state, 3);
    if (requested < 1 || requested > proto->sizek) luaL_argerror(state, 2, "constant index out of range");
    const int index = requested - 1;
    TValue* previous = &proto->k[index];
    const TValue* value = luaA_toobject(state, 3);
    if (!visible_constant(previous)) luaL_argerror(state, 2, "internal table/function constants are not editable");
    if (!visible_constant(value)) luaL_argerror(state, 3, "number, string, boolean, or nil expected");
    // Luau opcodes specialize on constant tags (e.g. GETGLOBAL requires a string).
    if (ttype(previous) != ttype(value)) luaL_error(state, "UnsupportedApi: setconstant currently preserves the constant type");
    if (proto->execdata) luaL_error(state, "UnsupportedApi: constants in native machine code are not editable");
    if (luaO_rawequalObj(previous, value)) return 0;
    setobj2n(state, previous, value);
    luaC_barrier(state, proto, previous);

    bool importedNameChanged = false;
    for (int pc = 0; pc < proto->sizecode;)
    {
        const Instruction instruction = proto->code[pc];
        const LuauOpcode op = LuauOpcode(LUAU_INSN_OP(instruction));
        if (op == LOP_GETIMPORT)
        {
            const uint32_t path = proto->code[pc + 1];
            const unsigned count = path >> 30;
            if (((path >> 20) & 1023) == unsigned(index) || (count > 1 && ((path >> 10) & 1023) == unsigned(index))
                || (count > 2 && (path & 1023) == unsigned(index)))
            {
                // Cached imports must be resolved again against the changed name.
                setnilvalue(&proto->k[LUAU_INSN_D(instruction)]);
                importedNameChanged = true;
            }
        }
        pc += Luau::getOpLength(op);
    }
    if (importedNameChanged)
    {
        // A specialized builtin may name the old import. Keep its ordinary fallback
        // bytecode and remove only this prototype's fast-call shortcut, with no wrapper.
        for (int pc = 0; pc < proto->sizecode;)
        {
            const LuauOpcode op = LuauOpcode(LUAU_INSN_OP(proto->code[pc]));
            const int length = Luau::getOpLength(op);
            if (Luau::isFastCall(op)) for (int word = 0; word < length; ++word) proto->code[pc + word] = LOP_NOP;
            pc += length;
        }
    }
    return 0;
}

static int closure_continuation(lua_State* state, int status)
{
    if (status != LUA_OK) lua_error(state);
    return lua_gettop(state);
}

static int closure_call(lua_State* state)
{
    int arguments = lua_gettop(state);
    lua_pushvalue(state, lua_upvalueindex(1));
    lua_insert(state, 1);
    return lua_callyieldable(state, arguments, LUA_MULTRET);
}

static int mac_closure_continuation(lua_State* state, int status)
{
    if (status != LUA_OK)
    {
        // lua_tolstring converts only strings/numbers, never invokes __tostring.
        // The protected boundary already discarded the inner Lua error frames.
        if (!lua_tolstring(state, -1, nullptr)) lua_pushliteral(state, "");
        lua_error(state);
    }
    return lua_gettop(state);
}

static int mac_closure_call(lua_State* state)
{
    const int arguments = lua_gettop(state);
    lua_pushvalue(state, lua_upvalueindex(1));
    lua_insert(state, 1);
    return lua_pcallyieldable(state, arguments, LUA_MULTRET, 0);
}

static constexpr const char* hook_registry = "emulator.executor.hooks";
static int hook_call(lua_State* state);

static void push_hooks(lua_State* state)
{
    lua_getfield(state, LUA_REGISTRYINDEX, hook_registry);
    if (lua_istable(state, -1)) return;
    lua_pop(state, 1);
    lua_newtable(state);
    lua_newtable(state);
    lua_pushliteral(state, "k");
    lua_setfield(state, -2, "__mode");
    lua_setmetatable(state, -2);
    lua_pushvalue(state, -1);
    lua_setfield(state, LUA_REGISTRYINDEX, hook_registry);
}

static int hook_call(lua_State* state)
{
    const int arguments = lua_gettop(state);
    push_hooks(state);
    luaA_pushvalue(state, state->ci->func);
    lua_rawget(state, -2);
    if (!lua_istable(state, -1)) luaL_error(state, "Hook state is unavailable");
    lua_getfield(state, -1, "replacement");
    lua_remove(state, -2);
    lua_remove(state, -2);
    lua_insert(state, 1);
    return lua_callyieldable(state, arguments, LUA_MULTRET);
}

extern "C" int emu_newcclosure(lua_State* state)
{
    luaL_checktype(state, 1, LUA_TFUNCTION);
    if (lua_iscfunction(state, 1)) { lua_pushvalue(state, 1); return 1; }
    lua_pushvalue(state, 1);
    if (executor_profile(state) == 1)
        lua_pushcclosurek(state, mac_closure_call, "newcclosure_handler", 1, mac_closure_continuation);
    else
        lua_pushcclosurek(state, closure_call, nullptr, 1, closure_continuation);
    return 1;
}

extern "C" int emu_clonefunction(lua_State* state)
{
    luaL_checktype(state, 1, LUA_TFUNCTION);
    if (lua_isLfunction(state, 1))
    {
        lua_clonefunction(state, 1);
        lua_getfenv(state, 1);
        lua_setfenv(state, -2);
        return 1;
    }
    Closure* closure = clvalue(luaA_toobject(state, 1));
    if (closure->c.f == hook_call)
    {
        push_hooks(state);
        lua_pushvalue(state, 1);
        lua_rawget(state, -2);
        lua_getfield(state, -1, "replacement");
        const char* name = closure->c.debugname ? getstr(closure->c.debugname) : nullptr;
        lua_pushcclosurek(state, closure_call, name, 1, closure_continuation);
        return 1;
    }
    luaL_checkstack(state, closure->nupvalues + 1, "clonefunction");
    for (int i = 1; i <= closure->nupvalues; ++i) lua_getupvalue(state, 1, i);
    const char* name = closure->c.debugname ? getstr(closure->c.debugname) : nullptr;
    lua_pushcclosurek(state, closure->c.f, name, closure->nupvalues, closure->c.cont);
    lua_getfenv(state, 1);
    lua_setfenv(state, -2);
    return 1;
}

extern "C" int emu_getrawmetatable(lua_State* state)
{
    luaL_checkany(state, 1);
    if (!lua_getmetatable(state, 1)) lua_pushnil(state);
    return 1;
}

extern "C" int emu_setrawmetatable(lua_State* state)
{
    luaL_checkany(state, 1);
    if (!lua_isnil(state, 2)) luaL_checktype(state, 2, LUA_TTABLE);
    lua_settop(state, 2);
    lua_setmetatable(state, 1);
    lua_pushvalue(state, 1);
    return 1;
}

extern "C" int emu_setreadonly(lua_State* state)
{
    luaL_checktype(state, 1, LUA_TTABLE);
    luaL_checktype(state, 2, LUA_TBOOLEAN);
    lua_setreadonly(state, 1, lua_toboolean(state, 2));
    return 0;
}

extern "C" int emu_isreadonly(lua_State* state)
{
    luaL_checktype(state, 1, LUA_TTABLE);
    lua_pushboolean(state, lua_getreadonly(state, 1));
    return 1;
}

extern "C" int emu_getnamecallmethod(lua_State* state)
{
    const char* method = lua_namecallatom(state, nullptr);
    if (method) lua_pushstring(state, method); else lua_pushnil(state);
    return 1;
}

struct ActiveClosure { Closure* target; bool active; };
static void active_thread(ActiveClosure& check, lua_State* thread)
{
    if (!thread || !thread->stack || !thread->base_ci || !thread->ci) return;
    // Failed coroutines keep traceback frames, but cannot resume those calls.
    if (thread->status != LUA_OK && thread->status != LUA_YIELD && thread->status != LUA_BREAK) return;
    for (CallInfo* frame = thread->base_ci; frame <= thread->ci; ++frame)
        if (ttisfunction(frame->func) && clvalue(frame->func) == check.target) check.active = true;
}
static bool active_closure(void* context, lua_Page*, GCObject* object)
{
    ActiveClosure& check = *static_cast<ActiveClosure*>(context);
    if (object->gch.tt != LUA_TTHREAD) return false;
    active_thread(check, &object->th);
    return false;
}
static bool invalidate_builtin_assumptions(void*, lua_Page*, GCObject* object)
{
    if (object->gch.tt == LUA_TTABLE) object->h.safeenv = false;
    return false;
}
static void check_inactive(lua_State* state, Closure* target)
{
    ActiveClosure check{target, false};
    active_thread(check, state);
    active_thread(check, state->global->mainthread);
    luaM_visitgco(state, &check, active_closure);
    if (check.active) luaL_error(state, "UnsupportedApi: cannot replace a closure with an active frame");
}
static void copy_lua_closure(lua_State* state, Closure* target, Closure* source)
{
    target->l.p = source->l.p;
    target->env = source->env;
    target->stacksize = source->stacksize;
    target->preload = source->preload;
    target->nupvalues = source->nupvalues;
    luaC_objbarrier(state, target, source->l.p);
    luaC_objbarrier(state, target, source->env);
    for (int i = 0; i < source->nupvalues; ++i)
    {
        setobj2n(state, &target->l.uprefs[i], &source->l.uprefs[i]);
        luaC_barrier(state, target, &target->l.uprefs[i]);
    }
}
extern "C" int emu_hookfunction(lua_State* state)
{
    luaL_checktype(state, 1, LUA_TFUNCTION);
    luaL_checktype(state, 2, LUA_TFUNCTION);
    lua_settop(state, 2);
    Closure* target = clvalue(luaA_toobject(state, 1));
    Closure* replacement = clvalue(luaA_toobject(state, 2));
    if (target->isC && emu_is_protected_function(target->c.f))
        luaL_error(state, "HostInternalClosure: cannot hook a host protected-call entry");
    if (target == replacement) return emu_clonefunction(state);
    check_inactive(state, target);
    push_hooks(state);
    const int hooks = lua_gettop(state);
    lua_pushvalue(state, 1);
    lua_rawget(state, hooks);
    bool first = lua_isnil(state, -1);
    if (first) { lua_pop(state, 1); lua_newtable(state); lua_pushinteger(state, target->nupvalues); lua_setfield(state, -2, "capacity"); }
    const int entry = lua_gettop(state);
    lua_getfield(state, entry, "capacity");
    const int capacity = lua_tointeger(state, -1);
    lua_pop(state, 1);
    if (!target->isC && (replacement->isC || replacement->nupvalues > capacity))
        luaL_error(state, "UnsupportedApi: Lua hook requires a Lua replacement within the original upvalue capacity");
    emu_clonefunction(state);
    const int previous = lua_gettop(state);
    if (first) { lua_pushvalue(state, previous); lua_setfield(state, entry, "original"); }
    lua_pushvalue(state, 2);
    lua_setfield(state, entry, "replacement");
    lua_pushvalue(state, 1);
    lua_pushvalue(state, entry);
    lua_rawset(state, hooks);
    luaM_visitgco(state, nullptr, invalidate_builtin_assumptions);
    if (target->isC) { target->c.f = hook_call; target->c.cont = closure_continuation; }
    else copy_lua_closure(state, target, replacement);
    lua_pushvalue(state, previous);
    return 1;
}
extern "C" int emu_restorefunction(lua_State* state)
{
    luaL_checktype(state, 1, LUA_TFUNCTION);
    Closure* target = clvalue(luaA_toobject(state, 1));
    if (target->isC && emu_is_protected_function(target->c.f))
        luaL_error(state, "HostInternalClosure: cannot restore a host protected-call entry");
    check_inactive(state, target);
    push_hooks(state);
    const int hooks = lua_gettop(state);
    lua_pushvalue(state, 1); lua_rawget(state, hooks);
    if (lua_isnil(state, -1)) return 0;
    lua_getfield(state, -1, "original");
    Closure* original = clvalue(luaA_toobject(state, -1));
    if (target->isC) { target->c.f = original->c.f; target->c.cont = original->c.cont; }
    else copy_lua_closure(state, target, original);
    lua_pushvalue(state, 1); lua_pushnil(state); lua_rawset(state, hooks);
    return 0;
}

extern "C" int emu_hookmetamethod(lua_State* state)
{
    luaL_checkany(state, 1);
    const char* method = luaL_checkstring(state, 2);
    luaL_checktype(state, 3, LUA_TFUNCTION);
    if (!lua_getmetatable(state, 1)) luaL_error(state, "Object has no metatable");
    lua_getfield(state, -1, method);
    if (!lua_isfunction(state, -1)) luaL_error(state, "Metamethod is not a function");
    lua_replace(state, 1);
    lua_pushvalue(state, 3);
    lua_replace(state, 2);
    lua_settop(state, 2);
    return emu_hookfunction(state);
}

extern "C" int emu_getmemcat(lua_State* state)
{
    return state->activememcat;
}
