#include "lua.h"
#include "lualib.h"
#include "lapi.h"
#include "lfunc.h"
#include "lgc.h"
#include "lmem.h"
#include "lstate.h"
#include "lstring.h"
#include "ltable.h"
#include "Luau/BytecodeUtils.h"
#if defined(EMULATOR_LOCAL_TEST)
#include "Luau/JitInliner.h"
#endif
#include <climits>
#include <cmath>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <initializer_list>

extern "C" int emu_is_protected_function(lua_CFunction function);
extern "C" int emu_is_reference_thread(lua_State* state, lua_State* thread);
extern "C" const void* emu_private_registry_key(const char* name);
// Registry keys are opaque host addresses, not interned Luau strings. A script
// may still create an identical string and discover it through its own GC view.
static char hook_registry_key;
static char hook_entries_registry_key;

static void private_registry_get(lua_State* state, const char* name)
{
    const void* key = emu_private_registry_key(name);
    if (!key) luaL_error(state, "Unknown private registry key");
    lua_rawgetp(state, LUA_REGISTRYINDEX, const_cast<void*>(key));
}

#if defined(EMULATOR_LOCAL_TEST)
// Test the real Luau bytecode inliner, not a simulated CMPPROTO branch.
extern "C" void emu_test_enable_inliner(lua_State* state) { Luau::JitInliner::setup(state); }
extern "C" void emu_test_push_hook_registry(lua_State* state, int entries)
{
    lua_rawgetp(state, LUA_REGISTRYINDEX, entries ? &hook_entries_registry_key : &hook_registry_key);
}
extern "C" int emu_test_has_inlined_guard(lua_State* state)
{
    const TValue* value = luaA_toobject(state, 1);
    if (!ttisfunction(value) || clvalue(value)->isC) return 0;
    Proto* proto = clvalue(value)->l.p;
    while (proto->optimized) proto = proto->optimized;
    for (int pc = 0; pc < proto->sizecode;)
    {
        auto op = LuauOpcode(LUAU_INSN_OP(proto->code[pc]));
        if (op == LOP_CMPPROTO) return 1;
        pc += Luau::getOpLength(op);
    }
    return 0;
}
#endif

// Feed a canonical, bounded representation of a Luau Proto into the host hash.
// Runtime inline caches, debug information, closure identity and upvalues are
// intentionally excluded. No Lua callbacks or Lua allocations occur here.
using HashFeed = void (*)(void*, const uint8_t*, size_t);
struct ProtoHash
{
    void* context;
    HashFeed feed;
    size_t remaining = 8000000;
    const Proto* parents[64] = {};

    void u8(uint8_t value) { feed(context, &value, 1); }
    void u32(uint32_t value)
    {
        uint8_t data[4];
        for (int i = 0; i < 4; ++i) data[i] = uint8_t(value >> (i * 8));
        feed(context, data, sizeof(data));
    }
    void u64(uint64_t value)
    {
        uint8_t data[8];
        for (int i = 0; i < 8; ++i) data[i] = uint8_t(value >> (i * 8));
        feed(context, data, sizeof(data));
    }
    bool take(size_t amount)
    {
        if (amount > remaining) return false;
        remaining -= amount;
        return true;
    }
};

// 0 = success, 1 = non-Lua function, 2 = unsupported complex constant,
// 3 = malformed/cyclic Proto, 4 = budget/allocation exhausted.
static int hash_proto(ProtoHash& hash, const Proto* proto, size_t depth)
{
    if (!proto || depth >= 64 || proto->sizecode < 0 || proto->sizek < 0 || proto->sizep < 0)
        return 3;
    for (size_t i = 0; i < depth; ++i) if (hash.parents[i] == proto) return 3;
    if (!hash.take(size_t(proto->sizecode) + size_t(proto->sizek) + size_t(proto->sizep))) return 4;
    if ((proto->sizecode && !proto->code) || (proto->sizek && !proto->k) || (proto->sizep && !proto->p)) return 3;
    hash.parents[depth] = proto;

    struct ImportMask
    {
        uint8_t* data;
        ~ImportMask() { std::free(data); }
    } imports{static_cast<uint8_t*>(std::calloc(size_t(proto->sizek), 1))};
    if (proto->sizek && !imports.data) return 4;
    // GETIMPORT's destination constant is a runtime cache; the import's name
    // and path still appear as other constants and in the instruction AUX.
    for (int pc = 0; pc < proto->sizecode;)
    {
        Instruction word = proto->code[pc];
        unsigned op = LUAU_INSN_OP(word);
        if (op == LOP_BREAK && proto->debuginsn) op = proto->debuginsn[pc];
        if (op >= LOP__COUNT) return 3;
        int length = Luau::getOpLength(LuauOpcode(op));
        if (length > proto->sizecode - pc) return 3;
        if (op == LOP_GETIMPORT)
        {
            int index = LUAU_INSN_D(word);
            if (index < 0 || index >= proto->sizek) return 3;
            imports.data[index] = 1;
        }
        pc += length;
    }

    hash.u8(proto->numparams);
    hash.u8(proto->is_vararg);
    hash.u8(proto->nups);
    hash.u32(uint32_t(proto->sizek));
    for (int i = 0; i < proto->sizek; ++i)
    {
        const TValue* value = &proto->k[i];
        if (imports.data[i]) { hash.u8(0); continue; }
        if (ttisnil(value)) { hash.u8(1); }
        else if (ttisboolean(value)) { hash.u8(2); hash.u8(bvalue(value) != 0); }
        else if (ttisnumber(value))
        {
            hash.u8(3);
            uint64_t bits;
            double number = nvalue(value);
            std::memcpy(&bits, &number, sizeof(bits));
            hash.u64(bits);
        }
        else if (ttisinteger(value)) { hash.u8(4); hash.u64(uint64_t(lvalue(value))); }
        else if (ttisstring(value))
        {
            TString* string = tsvalue(value);
            if (!hash.take(string->len)) return 4;
            hash.u8(5); hash.u32(string->len);
            hash.feed(hash.context, reinterpret_cast<const uint8_t*>(getstr(string)), string->len);
        }
        else if (ttisvector(value))
        {
            hash.u8(6);
            for (int component = 0; component < LUA_VECTOR_SIZE; ++component)
            {
                auto number = vvalue(value)[component];
#if LUA_VECTOR_DOUBLE == 1
                uint64_t bits; std::memcpy(&bits, &number, sizeof(bits)); hash.u64(bits);
#else
                uint32_t bits; std::memcpy(&bits, &number, sizeof(bits)); hash.u32(bits);
#endif
            }
        }
        else if (ttisfunction(value) && !clvalue(value)->isC)
        {
            hash.u8(7);
            int result = hash_proto(hash, clvalue(value)->l.p, depth + 1);
            if (result) return result;
        }
        else return 2;
    }

    hash.u32(uint32_t(proto->sizecode));
    for (int pc = 0; pc < proto->sizecode;)
    {
        uint32_t word = proto->code[pc];
        unsigned op = LUAU_INSN_OP(word);
        if (op == LOP_BREAK && proto->debuginsn)
        {
            op = proto->debuginsn[pc];
            word = (word & ~0xffu) | op;
        }
        int length = Luau::getOpLength(LuauOpcode(op));
        bool userdata = op == LOP_GETUDATAKS || op == LOP_SETUDATAKS || op == LOP_NAMECALLUDATA;
        if (userdata)
        {
            unsigned normalized = op == LOP_GETUDATAKS ? LOP_GETTABLEKS : op == LOP_SETUDATAKS ? LOP_SETTABLEKS : LOP_NAMECALL;
            word = (word & ~0xffu) | normalized;
        }
        if (userdata || op == LOP_GETGLOBAL || op == LOP_SETGLOBAL || op == LOP_GETTABLEKS || op == LOP_SETTABLEKS || op == LOP_NAMECALL)
            word &= 0x00ffffffu;
        if (op == LOP_COVERAGE) word &= 0xffu;
        hash.u32(word);
        if (length == 2)
        {
            uint32_t aux = proto->code[pc + 1];
            if (userdata) aux &= 0xffffu;
            if (op == LOP_CALLFB) aux = 0;
            hash.u32(aux);
        }
        pc += length;
    }
    hash.u32(uint32_t(proto->sizep));
    for (int i = 0; i < proto->sizep; ++i)
    {
        int result = hash_proto(hash, proto->p[i], depth + 1);
        if (result) return result;
    }
    return 0;
}

extern "C" int emu_feed_function_hash(lua_State* state, void* context, HashFeed feed)
{
    const TValue* function = luaA_toobject(state, 1);
    if (!ttisfunction(function) || clvalue(function)->isC) return 1;
    ProtoHash hash{context, feed};
    static const uint8_t domain[] = "RobloxEmulator.ProtoHash.v1";
    feed(context, domain, sizeof(domain));
    return hash_proto(hash, clvalue(function)->l.p, 0);
}

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
    if (profile->magic != executor_profile_magic || profile->id < 0 || profile->id > 2)
        luaL_error(state, "ExecutorProfileError: invalid native profile record");
    const int id = profile->id;
    lua_pop(state, 1);
    return id;
}

// Rust host configuration only; this setter is never registered in a script environment.
extern "C" void emu_set_executor_profile(lua_State* state, int id)
{
    if (id < 0 || id > 2) luaL_error(state, "ExecutorProfileError: unknown native profile");
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
    LuaTable* hook_entries = nullptr;
    LuaTable* inspector_objects = nullptr;
    const void* host_tables[64] = {};
    int host_table_count = 0;
};

static void add_host_table(Scan& scan, const void* table)
{
    if (scan.host_table_count >= int(sizeof(scan.host_tables) / sizeof(scan.host_tables[0])))
        luaL_error(scan.state, "GcEnumerationLimit: too many host tables");
    scan.host_tables[scan.host_table_count++] = table;
}

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
    if (type == LUA_TTABLE && scan.hook_entries)
    {
        TValue key = {};
        key.value.gc = object;
        key.tt = LUA_TTABLE;
        if (!ttisnil(luaH_get(scan.hook_entries, &key))) return false;
    }
    if ((type == LUA_TTABLE || type == LUA_TFUNCTION) && scan.inspector_objects)
    {
        TValue key = {};
        key.value.gc = object;
        key.tt = type;
        if (!ttisnil(luaH_get(scan.inspector_objects, &key))) return false;
    }
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
    private_registry_get(state, "emulator.inspector.values");
    const void* inspector = lua_topointer(state, -1);
    const void* inspector_metatable = nullptr;
    if (lua_getmetatable(state, -1)) { inspector_metatable = lua_topointer(state, -1); lua_pop(state, 1); }
    lua_pop(state, 1);
    private_registry_get(state, "emulator.inspector.scan");
    const void* inspector_scan = lua_topointer(state, -1);
    lua_pop(state, 1);
    Scan scan{state, tables, extended, false, false, 0, nullptr, inspector, inspector_metatable,
        inspector_scan, lua_topointer(state, LUA_REGISTRYINDEX)};
    private_registry_get(state, "emulator.internal");
    if (lua_istable(state, -1))
    {
        add_host_table(scan, lua_topointer(state, -1));
        int private_table = lua_absindex(state, -1);
        lua_pushnil(state);
        while (lua_next(state, private_table))
        {
            if (lua_istable(state, -1)) add_host_table(scan, lua_topointer(state, -1));
            lua_pop(state, 1);
        }
    }
    lua_pop(state, 1);
    for (const char* name : {"emulator.drawing.methods", "emulator.native.instances",
        "emulator.native.instance.methods", "emulator.native.environments", "emulator.native.environment.owners",
        "emulator.native.script.threads", "emulator.memory.default.environments", "emulator.memory.default.threads",
        "emulator.inspector.viewer.metatable", "emulator.inspector.objects", "emulator.executor.unsupported"})
    {
        private_registry_get(state, name);
        if (lua_istable(state, -1))
        {
            add_host_table(scan, lua_topointer(state, -1));
            if (lua_getmetatable(state, -1)) { add_host_table(scan, lua_topointer(state, -1)); lua_pop(state, 1); }
        }
        lua_pop(state, 1);
    }
    for (void* key : {&hook_registry_key, &hook_entries_registry_key})
    {
        lua_rawgetp(state, LUA_REGISTRYINDEX, key);
        if (lua_istable(state, -1))
        {
            add_host_table(scan, lua_topointer(state, -1));
            if (lua_getmetatable(state, -1)) { add_host_table(scan, lua_topointer(state, -1)); lua_pop(state, 1); }
        }
        lua_pop(state, 1);
    }
    lua_rawgetp(state, LUA_REGISTRYINDEX, &hook_entries_registry_key);
    if (lua_istable(state, -1)) scan.hook_entries = hvalue(luaA_toobject(state, -1));
    lua_pop(state, 1);
    private_registry_get(state, "emulator.inspector.objects");
    if (lua_istable(state, -1)) scan.inspector_objects = hvalue(luaA_toobject(state, -1));
    lua_pop(state, 1);
    return scan;
}

// The viewer's generated index/newindex closures capture private getter,
// setter and method tables. Mark only these concrete host objects; user tables
// with the same fields and global functions captured alongside them stay visible.
extern "C" void emu_inspector_mark_objects(lua_State* state)
{
    luaL_checktype(state, 1, LUA_TUSERDATA);
    luaL_checktype(state, 2, LUA_TTABLE);
    luaL_checkstack(state, 8, "inspector metadata");
    if (!lua_getmetatable(state, 1)) return;
    int meta = lua_absindex(state, -1);
    for (const char* name : {"__index", "__newindex"})
    {
        lua_rawgetfield(state, meta, name);
        if (!lua_isfunction(state, -1)) { lua_pop(state, 1); continue; }
        int closure = lua_absindex(state, -1);
        lua_pushvalue(state, closure);
        lua_pushboolean(state, true);
        lua_rawset(state, 2);
        if (!lua_iscfunction(state, closure))
        {
            for (int index = 1; lua_getupvalue(state, closure, index); ++index)
            {
                if (lua_istable(state, -1))
                {
                    int table = lua_absindex(state, -1);
                    lua_pushvalue(state, table);
                    lua_pushboolean(state, true);
                    lua_rawset(state, 2);
                    lua_pushnil(state);
                    while (lua_next(state, table))
                    {
                        if (lua_isfunction(state, -1))
                        {
                            lua_pushvalue(state, -1);
                            lua_pushboolean(state, true);
                            lua_rawset(state, 2);
                        }
                        lua_pop(state, 1);
                    }
                }
                lua_pop(state, 1);
            }
        }
        lua_pop(state, 1);
    }
    lua_settop(state, 0);
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
    const bool extended = executor_profile(state) != 0;
    lua_settop(state, 0);
    luaL_checkstack(state, 8, "getgc");
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
    luaL_checkstack(state, 8, "inspectgc");
    private_registry_get(state, "emulator.inspector.scan");
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

// The environment and a marked Lua reference are captured by the host when
// binding this function. The reference supplies the same prototype marker as
// filtergc without exposing or relying on the marker's address to Lua code.
extern "C" int emu_isexecutorclosure(lua_State* state)
{
    luaL_checktype(state, 1, LUA_TFUNCTION);
    Closure* target = clvalue(luaA_toobject(state, 1));
    Closure* reference = clvalue(luaA_toobject(state, lua_upvalueindex(2)));
    bool owned = target->isC
        ? target->env == hvalue(luaA_toobject(state, lua_upvalueindex(1)))
        : target->l.p->userdata != nullptr && target->l.p->userdata == reference->l.p->userdata;
    lua_pushboolean(state, owned);
    return 1;
}

extern "C" int emu_getinfo(lua_State* state)
{
    luaL_checkstack(state, 3, "getinfo");
    int level;
    if (lua_type(state, 1) == LUA_TNUMBER)
    {
        double input = lua_tonumber(state, 1);
        if (!std::isfinite(input) || input < 0 || input > INT_MAX || std::floor(input) != input)
            luaL_argerror(state, 1, "invalid stack level");
        level = int(input);
    }
    else
    {
        luaL_checktype(state, 1, LUA_TFUNCTION);
        lua_pushvalue(state, 1);
        level = -1;
    }
    lua_Debug info = {};
    if (!lua_getinfo(state, level, "slnuaf", &info))
        luaL_argerror(state, 1, "stack level out of range");
    // lua_getinfo(..., "f") places the actual closure on the stack.
    lua_createtable(state, 0, 10);
    lua_pushstring(state, info.source ? info.source : ""); lua_setfield(state, -2, "source");
    lua_pushstring(state, info.short_src ? info.short_src : ""); lua_setfield(state, -2, "short_src");
    lua_pushvalue(state, -2); lua_setfield(state, -2, "func");
    lua_pushstring(state, info.what ? info.what : ""); lua_setfield(state, -2, "what");
    lua_pushinteger(state, info.currentline); lua_setfield(state, -2, "currentline");
    lua_pushinteger(state, info.linedefined); lua_setfield(state, -2, "linedefined");
    lua_pushstring(state, info.name ? info.name : ""); lua_setfield(state, -2, "name");
    lua_pushinteger(state, info.nupvals); lua_setfield(state, -2, "nups");
    lua_pushinteger(state, info.nparams); lua_setfield(state, -2, "numparams");
    lua_pushinteger(state, info.isvararg ? 1 : 0); lua_setfield(state, -2, "is_vararg");
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

extern "C" int emu_setname(lua_State* state)
{
    Closure* closure = check_lua_closure(state);
    luaL_checktype(state, 2, LUA_TSTRING);
    size_t length = 0;
    const char* name = luaL_checklstring(state, 2, &length);
    if (std::memchr(name, '\0', length)) luaL_argerror(state, 2, "name cannot contain NUL");
    TString* string = luaS_newlstr(state, name, length);
    closure->l.p->debugname = string;
    luaC_objbarrier(state, closure->l.p, string);
    return 0;
}

extern "C" int emu_setinfo(lua_State* state)
{
    Closure* closure = check_lua_closure(state);
    luaL_checktype(state, 2, LUA_TTABLE);
    luaL_checkstack(state, 6, "setinfo");
    lua_settop(state, 2);
    for (const char* field : {"name", "source", "short_src", "currentline"})
        lua_rawgetfield(state, 2, field);
    for (int index : {3, 4, 5})
    {
        if (lua_isnil(state, index)) continue;
        if (lua_type(state, index) != LUA_TSTRING)
            luaL_argerror(state, 2, "name, source and short_src must be strings");
        size_t length = 0;
        const char* text = lua_tolstring(state, index, &length);
        if (length > 4096 || std::memchr(text, '\0', length))
            luaL_argerror(state, 2, "debug metadata string is too long or contains NUL");
    }
    if (!lua_isnil(state, 6))
    {
        if (lua_type(state, 6) != LUA_TNUMBER && lua_type(state, 6) != LUA_TINTEGER)
            luaL_argerror(state, 2, "currentline must be an integer");
        const double line = lua_tonumber(state, 6);
        if (!std::isfinite(line) || line < -1 || line > INT_MAX || std::floor(line) != line)
            luaL_argerror(state, 2, "currentline must be an integer from -1 to INT_MAX");
    }
    Proto* proto = closure->l.p;
    if (!lua_isnil(state, 3))
    {
        proto->debugname = tsvalue(luaA_toobject(state, 3));
        luaC_objbarrier(state, proto, proto->debugname);
    }
    if (!lua_isnil(state, 4) || !lua_isnil(state, 5))
    {
        int source = 4;
        if (lua_isnil(state, source))
        {
            // short_src is displayed as a chunk ID: prefix @ to preserve the given text.
            lua_pushliteral(state, "@");
            lua_pushvalue(state, 5);
            lua_concat(state, 2);
            source = lua_gettop(state);
        }
        proto->source = tsvalue(luaA_toobject(state, source));
        luaC_objbarrier(state, proto, proto->source);
    }
    // Function queries use linedefined; active-frame instruction line tables are unchanged.
    if (!lua_isnil(state, 6)) proto->linedefined = int(lua_tonumber(state, 6));
    return 0;
}

static void push_safe_environment(lua_State* state)
{
    if (lua_istable(state, 1)) lua_pushvalue(state, 1);
    else if (lua_isfunction(state, 1) || lua_isthread(state, 1)) lua_getfenv(state, 1);
    else luaL_argerror(state, 1, "function, thread, or table expected");
}

extern "C" int emu_getsafeenv(lua_State* state)
{
    luaL_checkstack(state, 2, "getsafeenv");
    push_safe_environment(state);
    lua_pushboolean(state, hvalue(luaA_toobject(state, -1))->safeenv != 0);
    return 1;
}

extern "C" int emu_setsafeenv(lua_State* state)
{
    luaL_checktype(state, 2, LUA_TBOOLEAN);
    luaL_checkstack(state, 1, "setsafeenv");
    push_safe_environment(state);
    lua_setsafeenv(state, -1, lua_toboolean(state, 2));
    return 0;
}

extern "C" int emu_getcallstack(lua_State* state)
{
    luaL_checkstack(state, 5, "getcallstack");
    lua_settop(state, 0);
    lua_newtable(state);
    int count = 0;
    for (int level = 1; level <= 20000; ++level)
    {
        lua_Debug info = {};
        if (!lua_getinfo(state, level, "slnf", &info)) break;
        lua_createtable(state, 0, 4);
        lua_pushvalue(state, -2); lua_setfield(state, -2, "func");
        lua_pushinteger(state, info.currentline); lua_setfield(state, -2, "line");
        lua_pushstring(state, info.name ? info.name : ""); lua_setfield(state, -2, "name");
        lua_pushstring(state, info.source ? info.source : ""); lua_setfield(state, -2, "source");
        lua_rawseti(state, 1, ++count);
        lua_pop(state, 1);
    }
    return 1;
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

static void push_inactive_proto(lua_State* state, Closure* parent, Proto* proto)
{
    luaL_checkstack(state, 1, "getproto");
    luaC_checkGC(state);
    luaC_threadbarrier(state);
    Closure* closure = luaF_newLclosure(state, proto->nups, parent->env, proto);
    closure->preload = LUAU_INACTIVE_PROTO_CLOSURE;
    TValue value;
    setclvalue(state, &value, closure);
    luaA_pushvalue(state, &value);
}

extern "C" int emu_getproto(lua_State* state)
{
    Closure* closure = check_lua_closure(state);
    Proto* parent = closure->l.p;
    const int index = check_index(state, 2, parent->sizep, "prototype index out of range");
    if (!lua_isnoneornil(state, 3)) luaL_checktype(state, 3, LUA_TBOOLEAN);
    if (!lua_toboolean(state, 3))
    {
        lua_settop(state, 1);
        push_inactive_proto(state, closure, parent->p[index - 1]);
        return 1;
    }
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
    Closure* parent = check_lua_closure(state);
    Proto* proto = parent->l.p;
    lua_settop(state, 1);
    luaL_checkstack(state, 2, "getprotos");
    lua_createtable(state, proto->sizep, 0);
    for (int i = 0; i < proto->sizep; ++i)
    {
        push_inactive_proto(state, parent, proto->p[i]);
        lua_rawseti(state, 2, i + 1);
    }
    return 1;
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
    private_registry_get(state, "emulator.inspector.scan");
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

extern "C" int emu_bind_dofile(lua_State* state)
{
    luaL_checktype(state, 1, LUA_TFUNCTION);
    lua_settop(state, 1);
    lua_pushcclosurek(state, closure_call, "dofile", 1, closure_continuation);
    return 1;
}

// Keep this callback distinct from other host forwarding wrappers, so the
// executor can identify closures that actually came from newcclosure.
static int newcclosure_call(lua_State* state)
{
    return closure_call(state);
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

static int hook_call(lua_State* state);

static void mark_hook_entry(lua_State* state, int entry)
{
    lua_rawgetp(state, LUA_REGISTRYINDEX, &hook_entries_registry_key);
    if (!lua_istable(state, -1))
    {
        lua_pop(state, 1);
        lua_newtable(state);
        lua_newtable(state);
        lua_pushliteral(state, "k");
        lua_setfield(state, -2, "__mode");
        lua_setmetatable(state, -2);
        lua_pushvalue(state, -1);
        lua_rawsetp(state, LUA_REGISTRYINDEX, &hook_entries_registry_key);
    }
    lua_pushvalue(state, entry);
    lua_pushboolean(state, true);
    lua_rawset(state, -3);
    lua_pop(state, 1);
}

static void push_hooks(lua_State* state)
{
    lua_rawgetp(state, LUA_REGISTRYINDEX, &hook_registry_key);
    if (lua_istable(state, -1)) return;
    lua_pop(state, 1);
    lua_newtable(state);
    lua_newtable(state);
    lua_pushliteral(state, "k");
    lua_setfield(state, -2, "__mode");
    lua_setmetatable(state, -2);
    lua_pushvalue(state, -1);
    lua_rawsetp(state, LUA_REGISTRYINDEX, &hook_registry_key);
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
    const int profile = executor_profile(state);
    if (profile != 2 && lua_iscfunction(state, 1)) { lua_pushvalue(state, 1); return 1; }
    lua_pushvalue(state, 1);
    if (profile != 0)
        lua_pushcclosurek(state, mac_closure_call, "newcclosure_handler", 1, mac_closure_continuation);
    else
        lua_pushcclosurek(state, newcclosure_call, nullptr, 1, closure_continuation);
    return 1;
}

extern "C" int emu_isnewcclosure(lua_State* state)
{
    luaL_checktype(state, 1, LUA_TFUNCTION);
    Closure* closure = clvalue(luaA_toobject(state, 1));
    if (closure->isC && closure->c.f == hook_call)
    {
        lua_rawgetp(state, LUA_REGISTRYINDEX, &hook_registry_key);
        if (lua_istable(state, -1))
        {
            lua_pushvalue(state, 1);
            lua_rawget(state, -2);
            if (lua_istable(state, -1))
            {
                lua_getfield(state, -1, "original");
                if (lua_iscfunction(state, -1)) closure = clvalue(luaA_toobject(state, -1));
            }
        }
    }
    lua_pushboolean(state, closure->isC && (closure->c.f == newcclosure_call || closure->c.f == mac_closure_call));
    return 1;
}

extern "C" int emu_isfunctionhooked(lua_State* state)
{
    luaL_checktype(state, 1, LUA_TFUNCTION);
    lua_rawgetp(state, LUA_REGISTRYINDEX, &hook_registry_key);
    if (!lua_istable(state, -1)) { lua_pushboolean(state, false); return 1; }
    lua_pushvalue(state, 1);
    lua_rawget(state, -2);
    lua_pushboolean(state, !lua_isnil(state, -1));
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

extern "C" int emu_makereadonly(lua_State* state)
{
    luaL_checktype(state, 1, LUA_TTABLE);
    lua_setreadonly(state, 1, true);
    return 0;
}

extern "C" int emu_makewritable(lua_State* state)
{
    luaL_checktype(state, 1, LUA_TTABLE);
    lua_setreadonly(state, 1, false);
    return 0;
}

extern "C" int emu_getnamecallmethod(lua_State* state)
{
    const char* method = lua_namecallatom(state, nullptr);
    if (method) lua_pushstring(state, method); else lua_pushnil(state);
    return 1;
}

extern "C" int emu_setnamecallmethod(lua_State* state)
{
    luaL_checktype(state, 1, LUA_TSTRING);
    size_t length = 0;
    const char* name = lua_tolstring(state, 1, &length);
    if (std::memchr(name, '\0', length)) luaL_argerror(state, 1, "name cannot contain NUL");
    luaC_threadbarrier(state);
    state->namecall = tsvalue(luaA_toobject(state, 1));
    return 0;
}

extern "C" int emu_isvalidlevel(lua_State* state)
{
    luaL_checktype(state, 1, LUA_TNUMBER);
    const double level = lua_tonumber(state, 1);
    if (!std::isfinite(level) || level < 0 || level > INT_MAX || std::floor(level) != level)
    {
        lua_pushboolean(state, false);
        return 1;
    }
    lua_Debug info = {};
    lua_pushboolean(state, lua_getinfo(state, int(level), "", &info));
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
    luaL_checkstack(state, 8, "hookfunction bookkeeping");
    luaL_checktype(state, 1, LUA_TFUNCTION);
    luaL_checktype(state, 2, LUA_TFUNCTION);
    lua_settop(state, 2);
    Closure* target = clvalue(luaA_toobject(state, 1));
    Closure* replacement = clvalue(luaA_toobject(state, 2));
    if (luau_isunboundclosure(target) || luau_isunboundclosure(replacement))
        luaL_error(state, "inactive prototype cannot be hooked");
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
    if (first) mark_hook_entry(state, entry);
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
    if (luau_isunboundclosure(target)) luaL_error(state, "inactive prototype cannot be restored");
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
