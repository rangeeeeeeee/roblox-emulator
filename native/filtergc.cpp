#include "lua.h"
#include "lualib.h"
#include "lapi.h"
#include "lgc.h"
#include "lstate.h"
#include <cstring>

extern "C" int emu_getgc(lua_State* state);

static char executor_proto_marker;

extern "C" int emu_is_executor_prototype(const Proto* proto)
{
    return proto && proto->userdata == &executor_proto_marker;
}

static void mark_prototypes(lua_State* state, Proto* proto, int depth, int& remaining)
{
    if (depth > 1000 || --remaining < 0)
        luaL_error(state, "ExecutorProvenanceLimit: prototype tree is too large");
    if (proto->userdata && proto->userdata != &executor_proto_marker)
        luaL_error(state, "UnsupportedApi: executor prototype metadata is owned by another embedder");
    proto->userdata = &executor_proto_marker;
    for (int i = 0; i < proto->sizep; ++i)
        mark_prototypes(state, proto->p[i], depth + 1, remaining);
}

extern "C" int emu_mark_executor(lua_State* state)
{
    luaL_checktype(state, 1, LUA_TFUNCTION);
    luaL_checktype(state, 2, LUA_TTABLE);
    Closure* closure = clvalue(luaA_toobject(state, 1));
    if (closure->isC)
    {
        closure->env = hvalue(luaA_toobject(state, 2));
        luaC_objbarrier(state, closure, closure->env);
    }
    else
    {
        int remaining = 100000;
        mark_prototypes(state, closure->l.p, 0, remaining);
    }
    return 0;
}

static void unsupported(lua_State* state, const char* option)
{
    if (lua_istable(state, lua_upvalueindex(2)))
    {
        int count = lua_objlen(state, lua_upvalueindex(2));
        if (count < 1000)
        {
            lua_createtable(state, 0, 1);
            lua_pushfstring(state, "filtergc.%s", option);
            lua_setfield(state, -2, "api");
            lua_rawseti(state, lua_upvalueindex(2), count + 1);
        }
    }
    luaL_error(state, "UnsupportedApi: filtergc option '%s' is not implemented", option);
}

struct Budget
{
    lua_State* state;
    int remaining = 10000000;
    void step()
    {
        if (--remaining < 0)
            luaL_error(state, "GcFilterLimit: more than 10000000 comparisons");
    }
};

static int option(lua_State* state, const char* name, int type)
{
    lua_pushstring(state, name);
    lua_rawget(state, 2);
    if (lua_isnil(state, -1)) return 0;
    if (lua_type(state, -1) != type)
        luaL_error(state, "invalid filtergc option '%s' (%s expected)", name, lua_typename(state, type));
    return lua_gettop(state);
}

static void validate_options(lua_State* state, bool functions)
{
    int count = 0;
    lua_pushnil(state);
    while (lua_next(state, 2))
    {
        if (++count > 32) luaL_error(state, "GcFilterLimit: too many filter options");
        if (lua_type(state, -2) != LUA_TSTRING)
            luaL_error(state, "filtergc option names must be strings");
        const char* name = lua_tostring(state, -2);
        bool known = functions
            ? std::strcmp(name, "Name") == 0 || std::strcmp(name, "IgnoreExecutor") == 0
                || std::strcmp(name, "Constants") == 0 || std::strcmp(name, "Upvalues") == 0
            : std::strcmp(name, "Keys") == 0 || std::strcmp(name, "Values") == 0
                || std::strcmp(name, "KeyValuePairs") == 0 || std::strcmp(name, "Metatable") == 0;
        if (!known) unsupported(state, name);
        lua_pop(state, 1);
    }
}

static bool contains_value(lua_State* state, int table, int wanted, Budget& budget)
{
    wanted = lua_absindex(state, wanted);
    lua_pushnil(state);
    while (lua_next(state, table))
    {
        budget.step();
        bool equal = lua_rawequal(state, wanted, -1) != 0;
        lua_pop(state, 1);
        if (equal) { lua_pop(state, 1); return true; }
    }
    return false;
}

static bool table_members(lua_State* state, int candidate, int requested, int mode, Budget& budget)
{
    if (!requested) return true;
    int count = 0;
    lua_pushnil(state);
    while (lua_next(state, requested))
    {
        budget.step();
        if (++count > 256) luaL_error(state, "GcFilterLimit: more than 256 criteria in one option");
        bool match;
        if (mode == 1)
            match = contains_value(state, candidate, -1, budget);
        else
        {
            lua_pushvalue(state, mode == 2 ? -2 : -1);
            lua_rawget(state, candidate);
            match = mode == 2 ? lua_rawequal(state, -1, -2) != 0 : !lua_isnil(state, -1);
            lua_pop(state, 1);
        }
        lua_pop(state, 1);
        if (!match) { lua_pop(state, 1); return false; }
    }
    return true;
}

static bool function_members(lua_State* state, Closure* closure, int requested, bool constants, Budget& budget)
{
    if (!requested) return true;
    if (closure->isC) return false;
    int count = 0;
    lua_pushnil(state);
    while (lua_next(state, requested))
    {
        budget.step();
        if (++count > 256) luaL_error(state, "GcFilterLimit: more than 256 criteria in one option");
        const TValue* wanted = luaA_toobject(state, -1);
        bool found = false;
        int size = constants ? closure->l.p->sizek : closure->nupvalues;
        for (int i = 0; i < size; ++i)
        {
            budget.step();
            const TValue* value = constants ? &closure->l.p->k[i] : &closure->l.uprefs[i];
            if (!constants && ttisupval(value)) value = upvalue(value)->v;
            if (luaO_rawequalObj(value, wanted)) { found = true; break; }
        }
        lua_pop(state, 1);
        if (!found) { lua_pop(state, 1); return false; }
    }
    return true;
}

extern "C" int emu_filtergc(lua_State* state)
{
    luaL_checktype(state, 1, LUA_TSTRING);
    if (lua_type(state, 2) != LUA_TTABLE)
    {
        lua_pushliteral(state, "expected table for argument #2");
        lua_error(state);
    }
    if (!lua_isnoneornil(state, 3) && lua_type(state, 3) != LUA_TBOOLEAN)
    {
        lua_pushliteral(state, "expected boolean for argument #3");
        lua_error(state);
    }
    const char* kind = lua_tostring(state, 1);
    bool functions = std::strcmp(kind, "function") == 0;
    if (!functions && std::strcmp(kind, "table") != 0)
        luaL_argerror(state, 1, "'function' or 'table' expected");
    bool one = lua_toboolean(state, 3) != 0;
    lua_settop(state, 3);
    luaL_checkstack(state, 20, "filtergc");
    validate_options(state, functions);
    int name = 0, constants = 0, upvalues = 0, keys = 0, values = 0, pairs = 0, metatable = 0;
    bool ignore_executor = true;
    if (functions)
    {
        name = option(state, "Name", LUA_TSTRING);
        int ignore = option(state, "IgnoreExecutor", LUA_TBOOLEAN);
        if (ignore) ignore_executor = lua_toboolean(state, ignore) != 0;
        constants = option(state, "Constants", LUA_TTABLE);
        upvalues = option(state, "Upvalues", LUA_TTABLE);
    }
    else
    {
        keys = option(state, "Keys", LUA_TTABLE);
        values = option(state, "Values", LUA_TTABLE);
        pairs = option(state, "KeyValuePairs", LUA_TTABLE);
        metatable = option(state, "Metatable", LUA_TTABLE);
    }
    LuaTable* executor_env = lua_istable(state, lua_upvalueindex(1))
        ? hvalue(luaA_toobject(state, lua_upvalueindex(1))) : nullptr;
    lua_pushcfunction(state, emu_getgc, "getgc");
    lua_pushboolean(state, !functions);
    lua_call(state, 1, 1);
    int snapshot = lua_gettop(state);
    int length = lua_objlen(state, snapshot), matches = 0;
    lua_newtable(state);
    int result = lua_gettop(state);
    Budget budget{state};
    for (int i = 1; i <= length; ++i)
    {
        budget.step();
        lua_rawgeti(state, snapshot, i);
        int candidate = lua_gettop(state);
        bool match = false;
        if (functions && lua_isfunction(state, candidate))
        {
            Closure* closure = clvalue(luaA_toobject(state, candidate));
            bool executor = closure->isC ? executor_env && closure->env == executor_env
                : closure->l.p->userdata == &executor_proto_marker;
            match = !ignore_executor || !executor;
            if (match && name)
            {
                TString* debugname = closure->isC ? closure->c.debugname : closure->l.p->debugname;
                const TValue* wanted = luaA_toobject(state, name);
                match = debugname ? debugname == tsvalue(wanted) : tsvalue(wanted)->len == 0;
            }
            match = match && function_members(state, closure, constants, true, budget)
                && function_members(state, closure, upvalues, false, budget);
        }
        else if (!functions && lua_istable(state, candidate))
        {
            match = true;
            if (metatable)
            {
                if (lua_getmetatable(state, candidate))
                {
                    match = lua_rawequal(state, -1, metatable) != 0;
                    lua_pop(state, 1);
                }
                else match = false;
            }
            match = match && table_members(state, candidate, keys, 0, budget)
                && table_members(state, candidate, values, 1, budget)
                && table_members(state, candidate, pairs, 2, budget);
        }
        if (match)
        {
            if (one) return 1;
            lua_rawseti(state, result, ++matches);
        }
        else lua_pop(state, 1);
    }
    if (one) lua_pushnil(state);
    return 1;
}
