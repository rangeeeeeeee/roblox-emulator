local nativeTypeof = typeof
local unpack = table.unpack
local config = __config or {}
local queue, tasks, kinds = {}, setmetatable({}, { __mode = "k" }), setmetatable({}, { __mode = "k" })
local signals, connections = setmetatable({}, { __mode = "k" }), setmetatable({}, { __mode = "k" })
local WAIT, PARK, SWITCH = {}, {}, {}
local stepIndex, sequence, resumes = 0, 0, 0
local maxTasks = config.maxTasks or 10000
local maxResumes = config.maxResumes or 10000
local rt = { now = 0, errors = {} }
local mainMetadata = { mode = "serial", origin = "game", cancelled = false }
__rt = rt

local function fail(message)
    error(message, 3)
end

local function duration(value)
    if value == nil then return 0 end
    if type(value) ~= "number" or value ~= value or math.abs(value) == math.huge then
        fail("duration must be a finite number")
    end
    return math.max(value, 0)
end

local function metadata(thread)
    if thread == nil then return mainMetadata end
    local data = tasks[thread]
    if not data then
        data = { mode = "serial", origin = "game", cancelled = false }
        tasks[thread] = data
    end
    return data
end

local function currentMetadata()
    return metadata(coroutine.running())
end

local function report(message)
    local record = { time = rt.now, message = tostring(message) }
    table.insert(rt.errors, record)
    __host_log("error", record.message)
end

local function enqueue(thread, args, at, minTick, started)
    if #queue >= maxTasks then fail("SchedulerLimit: queued task limit exceeded") end
    sequence += 1
    table.insert(queue, { thread = thread, args = args, at = at, tick = minTick, order = sequence, started = started })
end

local resumeTask
resumeTask = function(thread, args)
    local data = metadata(thread)
    if data.cancelled or coroutine.status(thread) == "dead" then return end
    resumes += 1
    if resumes > maxResumes then error("SchedulerLimit: resumption budget exceeded", 0) end
    data.parked = false
    local result = table.pack(coroutine.resume(thread, unpack(args, 1, args.n)))
    if not result[1] then
        data.failed = true
        report(result[2])
        return
    end
    if coroutine.status(thread) == "dead" then return end
    if result[2] == WAIT then
        enqueue(thread, nil, rt.now + result[3], stepIndex + 1, rt.now)
    elseif result[2] == SWITCH then
        data.mode = result[3]
        enqueue(thread, table.pack(), rt.now, stepIndex + 1)
    else
        data.parked = true
    end
end

local function toThread(callable)
    local thread
    if type(callable) == "function" then
        thread = coroutine.create(callable)
        local parent = currentMetadata()
        tasks[thread] = { mode = parent.mode, origin = parent.origin, cancelled = false }
    elseif type(callable) == "thread" then
        thread = callable
        if coroutine.status(thread) ~= "suspended" then fail("thread is not suspended") end
        metadata(thread)
    else
        fail("expected function or thread")
    end
    return thread
end

function rt.spawn(callable, ...)
    local thread = toThread(callable)
    resumeTask(thread, table.pack(...))
    return thread
end

function rt.spawnWith(callable, mode, origin, ...)
    local thread = toThread(callable)
    local data = metadata(thread)
    data.mode, data.origin = mode or "serial", origin or "game"
    resumeTask(thread, table.pack(...))
    return thread
end

function rt.defer(callable, ...)
    local thread = toThread(callable)
    enqueue(thread, table.pack(...), rt.now, stepIndex + 1)
    return thread
end

function rt.delay(seconds, callable, ...)
    local thread = toThread(callable)
    enqueue(thread, table.pack(...), rt.now + duration(seconds), stepIndex + 1)
    return thread
end

function rt.wait(seconds)
    return coroutine.yield(WAIT, duration(seconds))
end

function rt.park()
    return coroutine.yield(PARK)
end

function rt.schedule(thread, args, mode)
    local data = metadata(thread)
    if data.cancelled or coroutine.status(thread) == "dead" then return end
    if mode then data.mode = mode end
    enqueue(thread, args or table.pack(), rt.now, stepIndex + 1)
end

function rt.cancel(thread)
    if type(thread) ~= "thread" then fail("expected thread") end
    if thread == coroutine.running() then fail("cannot cancel the running thread") end
    local data = metadata(thread)
    data.cancelled = true
    if data.onCancel then data.onCancel(); data.onCancel = nil end
    for i = #queue, 1, -1 do
        if queue[i].thread == thread then table.remove(queue, i) end
    end
    if coroutine.status(thread) == "suspended" then coroutine.close(thread) end
end

function rt.getMode() return currentMetadata().mode end
function rt.getOrigin() return currentMetadata().origin end
function rt.setMode(mode) currentMetadata().mode = mode end

function rt.step(now)
    if type(now) ~= "number" or now ~= now or now < rt.now or now == math.huge then
        fail("scheduler time must be finite and nondecreasing")
    end
    rt.now, stepIndex, resumes = now, stepIndex + 1, 0
    table.sort(queue, function(a, b)
        if a.at ~= b.at then return a.at < b.at end
        return a.order < b.order
    end)
    local ready, pending = {}, {}
    for _, job in queue do
        if job.at <= now and job.tick <= stepIndex then table.insert(ready, job) else table.insert(pending, job) end
    end
    queue = pending
    for _, job in ready do
        local args = job.args or table.pack(now - job.started)
        resumeTask(job.thread, args)
    end
    return #queue
end

function rt.status()
    local parked = 0
    for thread, data in tasks do
        if data.parked and not data.cancelled and coroutine.status(thread) ~= "dead" then parked += 1 end
    end
    return { pending = #queue, parked = parked, errors = #rt.errors }
end

local function disconnect(connection)
    local item = connections[connection]
    if item and item.connected then
        item.connected = false
        for i, candidate in item.signal.handlers do
            if candidate == connection then table.remove(item.signal.handlers, i); break end
        end
    end
end

function rt.newSignal()
    local signal = newproxy(true)
    local data = { handlers = {}, waiters = {} }
    signals[signal], kinds[signal] = data, "RBXScriptSignal"
    local methods = {}
    local function connect(callback, mode, once)
        if type(callback) ~= "function" then fail("callback must be a function") end
        if mode == "parallel" and not config.actorId then fail("ConnectParallel requires an Actor") end
        local connection = newproxy(true)
        local item = { callback = callback, mode = mode, origin = rt.getOrigin(), connected = true, enabled = true, once = once, signal = data }
        connections[connection], kinds[connection] = item, "RBXScriptConnection"
        local mt = getmetatable(connection)
        mt.__index = function(_, key)
            if key == "Connected" then return item.connected end
            if key == "Disconnect" then return disconnect end
            error(key .. " is not a valid member of RBXScriptConnection", 2)
        end
        mt.__newindex = function() error("RBXScriptConnection is read-only", 2) end
        mt.__tostring = function() return "Connection" end
        mt.__metatable = "The metatable is locked"
        table.insert(data.handlers, connection)
        return connection
    end
    function methods:Connect(callback) return connect(callback, "serial", false) end
    function methods:ConnectParallel(callback) return connect(callback, "parallel", false) end
    function methods:Once(callback) return connect(callback, "serial", true) end
    function methods:Wait()
        local thread = coroutine.running()
        table.insert(data.waiters, thread)
        metadata(thread).onCancel = function()
            for i, candidate in data.waiters do
                if candidate == thread then table.remove(data.waiters, i); break end
            end
        end
        return rt.park()
    end
    function methods:Fire(...)
        local args = table.pack(...)
        local waiters = data.waiters
        data.waiters = {}
        for _, thread in waiters do
            metadata(thread).onCancel = nil
            rt.schedule(thread, args)
        end
        local handlers = table.clone(data.handlers)
        for _, connection in handlers do
            local item = connections[connection]
            if item.connected and item.enabled then
                if item.once then disconnect(connection) end
                if config.signalBehavior == "deferred" then
                    local thread = toThread(item.callback)
                    local state = metadata(thread)
                    state.mode, state.origin = item.mode, item.origin
                    enqueue(thread, args, rt.now, stepIndex + 1)
                else
                    rt.spawnWith(item.callback, item.mode, item.origin, unpack(args, 1, args.n))
                end
            end
        end
    end
    function methods:DisconnectAll()
        for _, connection in table.clone(data.handlers) do disconnect(connection) end
    end
    local mt = getmetatable(signal)
    mt.__index = methods
    mt.__newindex = function() error("RBXScriptSignal is read-only", 2) end
    mt.__tostring = function() return "Signal" end
    mt.__metatable = "The metatable is locked"
    return signal
end

function rt.getConnections(signal)
    local data = signals[signal]
    if not data then fail("expected RBXScriptSignal") end
    local result = {}
    for _, connection in data.handlers do
        local item = connections[connection]
        local wrapper = {}
        function wrapper:Disable() item.enabled = false end
        function wrapper:Enable() item.enabled = true end
        function wrapper:Disconnect() disconnect(connection) end
        function wrapper:Fire(...) return rt.spawnWith(item.callback, item.mode, item.origin, ...) end
        function wrapper:Defer(...) return rt.defer(function(...) rt.spawnWith(item.callback, item.mode, item.origin, ...) end, ...) end
        setmetatable(wrapper, { __index = function(_, key)
            if key == "Enabled" then return item.connected and item.enabled end
            if key == "Function" then return item.callback end
            if key == "ForeignState" then return false end
            if key == "LuaConnection" then return true end
        end })
        table.insert(result, wrapper)
    end
    return result
end

function typeof(value) return kinds[value] or nativeTypeof(value) end

task = {
    spawn = rt.spawn, defer = rt.defer, delay = rt.delay, wait = rt.wait, cancel = rt.cancel,
    desynchronize = function()
        if not config.actorId then fail("task.desynchronize requires an Actor") end
        return coroutine.yield(SWITCH, "parallel")
    end,
    synchronize = function()
        if not config.actorId then fail("task.synchronize requires an Actor") end
        return coroutine.yield(SWITCH, "serial")
    end,
}

local function output(level, ...)
    local values = table.pack(...)
    for i = 1, values.n do values[i] = tostring(values[i]) end
    __host_log(level, table.concat(values, "\t", 1, values.n))
end
function print(...) output("info", ...) end
function warn(...) output("warn", ...) end
function time() return rt.now end
function elapsedTime() return rt.now end
function tick() return 1700000000 + rt.now end
