local config = __config or {}
local gameEnvironment = getfenv(0)
local executorEnvironment = setmetatable({}, { __index = gameEnvironment })
local files, folders, clipboard = {}, { [""] = true }, ""
local byteLimit = config.virtualFileBytes or 1048576
local fileLimit = config.virtualFileCount or 256
local capabilities = {
    scheduler = "approximate: virtual ticks and configured immediate/deferred signals",
    actorParallelism = "approximate: isolated VM and phase rules; cooperative host scheduling",
    gc = "native Luau garbage collector",
    getgenv = "implemented: executor environment within this realm",
    getrenv = "implemented: game global environment within this realm",
    checkcaller = "approximate: originating scheduled task and signal connection",
    getconnections = "implemented: same-VM emulator signals",
    filesystem = "virtual: memory only, bounded bytes and file count",
    clipboard = "virtual: memory only",
    getgc = "unsupported",
    hookfunction = "unsupported",
    hookmetamethod = "unsupported",
    newcclosure = "unsupported",
    request = "unsupported: no external network",
}
__emu = { capabilities = capabilities, executorEnvironment = executorEnvironment, unsupported = {} }

local function unavailable(name)
    return function(...)
        table.insert(__emu.unsupported, { api = name, time = __rt.now })
        error("UnsupportedApi: " .. name .. " is not implemented by this emulator", 2)
    end
end

local function modelFunction(name)
    return function(...)
        local fn = __model and __model[name]
        if not fn then return unavailable(name)(...) end
        return fn(...)
    end
end

local function path(value)
    if type(value) ~= "string" then error("path must be a string", 3) end
    local normalized = value:gsub("\\", "/"):gsub("/+", "/"):gsub("/$", "")
    if #normalized > 240 or normalized:sub(1, 1) == "/" or normalized:find(":", 1, true) or normalized:find("\0", 1, true) then
        error("invalid virtual path", 3)
    end
    for component in normalized:gmatch("[^/]+") do
        if component == "." or component == ".." then error("invalid virtual path", 3) end
    end
    return normalized
end

local function parent(value) return value:match("^(.*)/[^/]+$") or "" end
local function usage()
    local count, bytes = 0, 0
    for _, contents in files do count += 1; bytes += #contents end
    return count, bytes
end

local function writefile(name, contents)
    name = path(name)
    if name == "" or folders[name] then error("path is a directory", 2) end
    if type(contents) ~= "string" then error("file contents must be a string", 2) end
    if not folders[parent(name)] then error("virtual parent directory does not exist", 2) end
    local count, bytes = usage()
    if bytes - #(files[name] or "") + #contents > byteLimit or (not files[name] and count >= fileLimit) then
        error("VirtualFilesystemLimit: storage limit exceeded", 2)
    end
    files[name] = contents
end

local function readfile(name)
    name = path(name)
    local contents = files[name]
    if not contents then error("virtual file does not exist: " .. name, 2) end
    return contents
end

local function makefolder(name)
    name = path(name)
    if files[name] then error("path is a file", 2) end
    if not folders[parent(name)] then error("virtual parent directory does not exist", 2) end
    local count = 0
    for _ in folders do count += 1 end
    if not folders[name] and count >= fileLimit then error("VirtualFilesystemLimit: directory limit exceeded", 2) end
    folders[name] = true
end

local function listfiles(name)
    name = path(name)
    if not folders[name] then error("virtual directory does not exist", 2) end
    local result = {}
    for filename in files do if parent(filename) == name then table.insert(result, filename) end end
    for folder in folders do if folder ~= "" and parent(folder) == name then table.insert(result, folder) end end
    table.sort(result)
    return result
end

local api = {
    getgenv = function() return executorEnvironment end,
    getrenv = function() return gameEnvironment end,
    checkcaller = function() return __rt.getOrigin() == "executor" end,
    identifyexecutor = function() return "RobloxEmulator", "0.1.0" end,
    getconnections = __rt.getConnections,
    getinstances = modelFunction("getInstances"),
    getnilinstances = modelFunction("getNilInstances"),
    getsenv = modelFunction("getEnvironment"),
    writefile = writefile,
    readfile = readfile,
    appendfile = function(name, contents)
        if type(contents) ~= "string" then error("file contents must be a string", 2) end
        writefile(name, (files[path(name)] or "") .. contents)
    end,
    makefolder = makefolder,
    isfile = function(name) return files[path(name)] ~= nil end,
    isfolder = function(name) return folders[path(name)] == true end,
    listfiles = listfiles,
    delfile = function(name)
        name = path(name)
        if not files[name] then error("virtual file does not exist", 2) end
        files[name] = nil
    end,
    delfolder = function(name)
        name = path(name)
        if name == "" then error("cannot delete virtual root", 2) end
        if not folders[name] then error("virtual directory does not exist", 2) end
        local prefix = name .. "/"
        for filename in files do if filename:sub(1, #prefix) == prefix then files[filename] = nil end end
        for folder in folders do if folder == name or folder:sub(1, #prefix) == prefix then folders[folder] = nil end end
    end,
    setclipboard = function(value)
        if type(value) ~= "string" or #value > 65536 then error("virtual clipboard requires a string of at most 65536 bytes", 2) end
        clipboard = value
    end,
    getclipboard = function() return clipboard end,
    rconsoleprint = function(value) __host_log("executor", tostring(value)) end,
    rconsolewarn = function(value) __host_log("executor_warn", tostring(value)) end,
    rconsoleerr = function(value) __host_log("executor_error", tostring(value)) end,
}

for _, name in {
    "getgc", "filtergc", "getreg", "hookfunction", "hookmetamethod", "restorefunction", "clonefunction",
    "newcclosure", "iscclosure", "islclosure", "isexecutorclosure", "getrawmetatable", "setrawmetatable",
    "getnamecallmethod", "setnamecallmethod", "setreadonly", "isreadonly", "getthreadidentity", "setthreadidentity",
    "gethiddenproperty", "sethiddenproperty", "cloneref", "compareinstances", "getscriptbytecode", "getscriptclosure",
    "getcallingscript", "getloadedmodules", "getscripts", "request", "http_request", "queue_on_teleport",
    "firetouchinterest", "fireclickdetector", "fireproximityprompt", "setfpscap", "getfpscap", "shutdown",
} do
    api[name] = unavailable(name)
    if not capabilities[name] then capabilities[name] = "unsupported" end
end

api.loadstring = function(source, name)
    if type(source) ~= "string" then error("source must be a string", 2) end
    local ok, result = pcall(__host_compile, source, name or "=executor", executorEnvironment)
    if ok then return result end
    return nil, tostring(result)
end
api.loadfile = function(name) return api.loadstring(readfile(name), "@virtual/" .. path(name)) end
if __host_getgc then
    api.getgc = __host_getgc
    api.iscclosure = __host_iscclosure
    api.islclosure = __host_islclosure
    capabilities.getgc = "native: Luau740 same-VM non-dead functions/userdata and optional tables"
    capabilities.iscclosure = "native Luau closure classification"
    capabilities.islclosure = "native Luau closure classification"
    api.debug = table.clone(gameEnvironment.debug)
    api.debug.getupvalue = __host_getupvalue
    api.debug.getupvalues = __host_getupvalues
    api.debug.setupvalue = __host_setupvalue
    capabilities["debug.getupvalue"] = "native: Lua closure upvalues"
    capabilities["debug.getupvalues"] = "native: Lua closure upvalues"
    capabilities["debug.setupvalue"] = "native: Lua closure upvalues"
end
executorEnvironment._G = executorEnvironment
executorEnvironment.shared = {}
for name, value in api do executorEnvironment[name] = value end

function __emu.runExecutor(source, name)
    if not config.executor then error("executor profile is disabled for this realm", 2) end
    local fn = __host_compile(source, name or "=executor", executorEnvironment)
    return __rt.spawnWith(fn, "serial", "executor")
end

function __emu.virtualState()
    local count, bytes = usage()
    return { fileCount = count, fileBytes = bytes, clipboardBytes = #clipboard }
end
