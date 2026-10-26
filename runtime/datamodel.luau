local rt, config = __rt, __config
local records = setmetatable({}, { __mode = "v" })
local wrappers = setmetatable({}, { __mode = "v" })
local objects = setmetatable({}, { __mode = "k" })
local environments = setmetatable({}, { __mode = "k" })
local changes, moduleCache, pendingCalls, logs = {}, {}, {}, {}
local sequence, requestSequence, applying = 0, 0, false
local baseTypeof = typeof
local enumValues = setmetatable({}, { __mode = "k" })
local enumFamilies = {}

local enumDefinitions = {
    RunContext = { Legacy = 0, Server = 1, Client = 2, Plugin = 3 },
    MessageType = { MessageOutput = 0, MessageInfo = 1, MessageWarning = 2, MessageError = 3 },
}

if not Enum then
    Enum = {}
end
for family, values in enumDefinitions do
    if not Enum[family] then
        local items = {}
        for name, value in values do
            local item = newproxy(true)
            enumValues[item] = { family = family, name = name, value = value }
            getmetatable(item).__index = function(_, key)
                if key == "Name" then return name end
                if key == "Value" then return value end
                if key == "EnumType" then return items end
                error(key .. " is not a valid member of EnumItem", 2)
            end
            getmetatable(item).__tostring = function() return "Enum." .. family .. "." .. name end
            getmetatable(item).__metatable = "The metatable is locked"
            items[name] = item
        end
        enumFamilies[family] = items
        Enum[family] = table.freeze(items)
    end
end

local parents = {
    Instance = "Object", DataModel = "Instance", Folder = "Instance", Model = "PVInstance",
    PVInstance = "Instance", Actor = "Model", LuaSourceContainer = "Instance",
    BaseScript = "LuaSourceContainer", Script = "BaseScript", LocalScript = "BaseScript",
    ModuleScript = "LuaSourceContainer", BindableEvent = "Instance", BindableFunction = "Instance",
    RemoteEvent = "Instance", UnreliableRemoteEvent = "RemoteEvent", RemoteFunction = "Instance",
    Players = "Instance", Player = "Instance", ReplicatedStorage = "Instance", ReplicatedFirst = "Instance",
    ServerStorage = "Instance", ServerScriptService = "Instance", RunService = "Instance",
    LogService = "Instance", HttpService = "Instance", Workspace = "Model", StarterPlayer = "Instance",
    StarterPlayerScripts = "Instance", StarterCharacterScripts = "Instance", StarterGui = "Instance",
    PlayerGui = "Instance", PlayerScripts = "Instance", Backpack = "Instance", CoreGui = "Instance",
    GuiBase2d = "Instance", GuiObject = "GuiBase2d", Frame = "GuiObject", TextLabel = "GuiObject",
    TextButton = "GuiObject", ImageLabel = "GuiObject", ImageButton = "GuiObject",
    LayerCollector = "GuiBase2d", ScreenGui = "LayerCollector", BasePart = "PVInstance",
    Part = "BasePart", MeshPart = "BasePart", Camera = "Instance", Humanoid = "Instance",
    ValueBase = "Instance", StringValue = "ValueBase", BoolValue = "ValueBase", NumberValue = "ValueBase",
    IntValue = "ValueBase", ObjectValue = "ValueBase", Configuration = "Instance", Tool = "Model",
}
local serviceNames = {
    Workspace = true, Players = true, ReplicatedStorage = true, ReplicatedFirst = true,
    ServerStorage = true, ServerScriptService = true, RunService = true, LogService = true,
    HttpService = true, StarterPlayer = true, StarterGui = true, CoreGui = true,
}
local defaults = {
    Instance = { Archivable = true },
    DataModel = { PlaceId = 0, GameId = 0, JobId = "emulator", PlaceVersion = 0, CreatorId = 0 },
    BaseScript = { Enabled = true, Disabled = false, LinkedSource = "" },
    LuaSourceContainer = { LinkedSource = "" },
    Script = { RunContext = Enum.RunContext.Legacy },
    Player = { UserId = 0, DisplayName = "Player", AccountAge = 0, Character = false },
    Players = { MaxPlayers = 100, RespawnTime = 5, CharacterAutoLoads = true },
    Workspace = { Gravity = 196.2, DistributedGameTime = 0, CurrentCamera = false },
    BasePart = { Anchored = false, CanCollide = true, CanTouch = true, CanQuery = true, Transparency = 0, Massless = false },
    GuiObject = { Visible = true, Active = false, BackgroundTransparency = 0, ZIndex = 1 },
    LayerCollector = { Enabled = true, ResetOnSpawn = true },
    ScreenGui = { IgnoreGuiInset = false, DisplayOrder = 0 },
    TextLabel = { Text = "Label", TextSize = 14, TextTransparency = 0 },
    TextButton = { Text = "Button", TextSize = 14, TextTransparency = 0 },
    ImageLabel = { Image = "", ImageTransparency = 0 },
    ImageButton = { Image = "", ImageTransparency = 0 },
    Humanoid = { Health = 100, MaxHealth = 100, WalkSpeed = 16, JumpPower = 50, HipHeight = 0 },
    StringValue = { Value = "" }, BoolValue = { Value = false }, NumberValue = { Value = 0 },
    IntValue = { Value = 0 }, ObjectValue = { Value = false },
}
local nilProperties = { Character = true, CurrentCamera = true, PrimaryPart = true, OnInvoke = true, OnServerInvoke = true, OnClientInvoke = true }
local eventClasses = {
    ChildAdded = "Instance", ChildRemoved = "Instance", DescendantAdded = "Instance",
    DescendantRemoving = "Instance", AncestryChanged = "Instance", Changed = "Instance",
    Destroying = "Instance", AttributeChanged = "Instance", Event = "BindableEvent",
    OnServerEvent = "RemoteEvent", OnClientEvent = "RemoteEvent", PlayerAdded = "Players",
    PlayerRemoving = "Players", CharacterAdded = "Player", CharacterRemoving = "Player",
    Heartbeat = "RunService", Stepped = "RunService", RenderStepped = "RunService",
    PreSimulation = "RunService", PostSimulation = "RunService", PreRender = "RunService",
    MessageOut = "LogService", Died = "Humanoid",
}
local methodClasses = {
    GetService = "DataModel", FindService = "DataModel", IsLoaded = "DataModel",
    GetPlayers = "Players", GetPlayerFromCharacter = "Players", IsClient = "RunService",
    IsServer = "RunService", IsStudio = "RunService", IsRunning = "RunService",
    GetLogHistory = "LogService", JSONEncode = "HttpService", JSONDecode = "HttpService",
    Fire = "BindableEvent", Invoke = "BindableFunction", FireServer = "RemoteEvent",
    FireClient = "RemoteEvent", FireAllClients = "RemoteEvent", InvokeServer = "RemoteFunction",
    InvokeClient = "RemoteFunction", SendMessage = "Actor", BindToMessage = "Actor",
    BindToMessageParallel = "Actor",
}

local methods, proxy, makeRecord, setParent
local function isA(record, class)
    local cursor = record.class
    while cursor do
        if cursor == class then return true end
        cursor = parents[cursor]
    end
    return false
end
local function recordOf(instance)
    local record = objects[instance]
    if not record then error("Instance expected", 3) end
    return record
end
local function mark(record)
    if not applying then changes[record.id] = record end
end
local function assertWritable()
    if not applying and rt.getMode and rt.getMode() == "parallel" then
        error("Instance mutation is unavailable in parallel", 3)
    end
end
local function signal(record, key)
    if not record.signals[key] then record.signals[key] = rt.newSignal() end
    return record.signals[key]
end
local function fire(record, key, ...)
    local event = record.signals[key]
    if event then event:Fire(...) end
end
local function descendants(record, result)
    for _, child in record.children do
        table.insert(result, child)
        descendants(child, result)
    end
end
local function default(record, key)
    local cursor = record.class
    while cursor do
        local value = defaults[cursor] and defaults[cursor][key]
        if value ~= nil then return value, true end
        cursor = parents[cursor]
    end
    return nil, false
end
local function propertyChanged(record, key, value)
    if isA(record, "ValueBase") and key == "Value" then
        fire(record, "Changed", value)
    else
        fire(record, "Changed", key)
    end
    fire(record, "property:" .. key)
    mark(record)
end

setParent = function(record, parent)
    assertWritable()
    if parent == record.parent then return end
    if record.destroyed and parent then error("The Parent property of " .. record.name .. " is locked", 3) end
    if parent and parent.destroyed then error("Cannot parent to a destroyed Instance", 3) end
    local cursor = parent
    while cursor do
        if cursor == record then error("Attempt to set Parent would result in circular reference", 3) end
        cursor = cursor.parent
    end
    local moved = { record }
    descendants(record, moved)
    local previous = record.parent
    cursor = previous
    while cursor do
        for _, item in moved do fire(cursor, "DescendantRemoving", proxy(item)) end
        cursor = cursor.parent
    end
    if previous then
        local index = table.find(previous.children, record)
        if index then table.remove(previous.children, index) end
    end
    record.parent = parent
    if parent then table.insert(parent.children, record) end
    if previous then fire(previous, "ChildRemoved", proxy(record)) end
    if parent then fire(parent, "ChildAdded", proxy(record)) end
    if record.class == "Player" then
        if previous and previous.class == "Players" then fire(previous, "PlayerRemoving", proxy(record)) end
        if parent and parent.class == "Players" then fire(parent, "PlayerAdded", proxy(record)) end
    end
    cursor = parent
    while cursor do
        for _, item in moved do fire(cursor, "DescendantAdded", proxy(item)) end
        cursor = cursor.parent
    end
    for _, item in moved do fire(item, "AncestryChanged", proxy(item), item.parent and proxy(item.parent) or nil) end
    propertyChanged(record, "Parent")
end

local function findChild(record, name, recursive)
    for _, child in record.children do if child.name == name then return child end end
    if recursive then
        for _, child in record.children do
            local found = findChild(child, name, true)
            if found then return found end
        end
    end
end

local function readProperty(record, key)
    if key == "Name" then return record.name end
    if key == "ClassName" then return record.class end
    if key == "Parent" then return record.parent and proxy(record.parent) or nil end
    if key == "Source" and isA(record, "LuaSourceContainer") then
        error("The current thread cannot read 'Source' (lacking capability PluginOrOpenCloud)", 3)
    end
    if key == "LocalPlayer" and record.class == "Players" then
        return config.side == "client" and records[config.clientId or ""] and proxy(records[config.clientId]) or nil
    end
    if key == "DistributedGameTime" and record.class == "Workspace" then return rt.now end
    if record.properties[key] ~= nil then
        local value = record.properties[key]
        if value == false and nilProperties[key] then return nil end
        return value
    end
    if nilProperties[key] then
        if (key == "OnInvoke" and record.class == "BindableFunction")
            or (key == "OnServerInvoke" or key == "OnClientInvoke") and record.class == "RemoteFunction"
            or key == "PrimaryPart" and isA(record, "Model") then return nil end
    end
    local value, exists = default(record, key)
    if exists then
        if value == false and (nilProperties[key] or key == "Value" and record.class == "ObjectValue") then return nil end
        return value
    end
    local requiredClass = eventClasses[key]
    if requiredClass and isA(record, requiredClass) then return signal(record, key) end
    if methods[key] and (not methodClasses[key] or isA(record, methodClasses[key])) then return methods[key] end
    local child = findChild(record, key, false)
    if child then return proxy(child) end
    error(key .. " is not a valid member of " .. record.class .. ' "' .. record.name .. '"', 3)
end

local function writeProperty(record, key, value)
    if key == "Name" then
        if type(value) ~= "string" then error("Name must be a string", 3) end
        if value ~= record.name then record.name = value; propertyChanged(record, key, value) end
        return
    end
    if key == "Parent" then setParent(record, value and recordOf(value)); return end
    if key == "ClassName" then error("ClassName is read only", 3) end
    if key == "Source" and isA(record, "LuaSourceContainer") then
        error("The current thread cannot write 'Source' (lacking capability PluginOrOpenCloud)", 3)
    end
    if (key == "OnInvoke" and record.class == "BindableFunction")
        or ((key == "OnServerInvoke" or key == "OnClientInvoke") and record.class == "RemoteFunction") then
        if value ~= nil and type(value) ~= "function" then error("Callback must be a function or nil", 3) end
        record.properties[key] = value
        return
    end
    local _, exists = default(record, key)
    if not exists and record.properties[key] == nil and not (key == "PrimaryPart" and isA(record, "Model")) then
        error(key .. " is not a valid member of " .. record.class, 3)
    end
    if record.properties[key] == value then return end
    local previous = record.properties[key]
    if key == "Character" and record.class == "Player" and previous and previous ~= false then
        fire(record, "CharacterRemoving", previous)
    end
    record.properties[key] = value
    if key == "Enabled" and isA(record, "BaseScript") then record.properties.Disabled = not value end
    if key == "Disabled" and isA(record, "BaseScript") then record.properties.Enabled = not value end
    propertyChanged(record, key, value)
    if key == "Character" and record.class == "Player" and value then fire(record, "CharacterAdded", value) end
    if key == "Health" and record.class == "Humanoid" and type(value) == "number" and value <= 0
        and (previous == nil or previous > 0) then fire(record, "Died") end
end

proxy = function(record)
    if wrappers[record.id] then return wrappers[record.id] end
    local object = newproxy(true)
    objects[object], wrappers[record.id] = record, object
    local mt = getmetatable(object)
    mt.__index = function(_, key) return readProperty(record, key) end
    mt.__newindex = function(_, key, value)
        assertWritable()
        writeProperty(record, key, value)
    end
    mt.__tostring = function() return record.name end
    mt.__eq = function(a, b) return objects[a] ~= nil and objects[a] == objects[b] end
    mt.__type = "Instance"
    mt.__metatable = "The metatable is locked"
    return object
end

makeRecord = function(class, name, id)
    assertWritable()
    if not parents[class] or class == "Instance" or class == "BaseScript" or class == "LuaSourceContainer" then
        error("UnsupportedApi: Instance.new(" .. tostring(class) .. ")", 3)
    end
    sequence += 1
    id = id or config.realmId .. ":" .. sequence
    if records[id] then return records[id] end
    local record = { id = id, class = class, name = name or class, parent = nil, children = {},
        properties = {}, attributes = {}, signals = {}, source = "", destroyed = false }
    records[id] = record
    mark(record)
    return record
end

local function encode(value, active)
    local valueType = type(value)
    if value == nil then return { tag = "nil" } end
    if objects[value] then return { tag = "instance", id = objects[value].id } end
    if enumValues[value] then
        local e = enumValues[value]
        return { tag = "enum", family = e.family, name = e.name }
    end
    if __types and __types.encode then
        local converted = __types.encode(value)
        if converted then return converted end
    end
    if valueType == "boolean" or valueType == "string" then return { tag = valueType, value = value } end
    if valueType == "number" then
        if value ~= value then return { tag = "number", special = "nan" } end
        if value == math.huge then return { tag = "number", special = "+inf" } end
        if value == -math.huge then return { tag = "number", special = "-inf" } end
        return { tag = "number", value = value }
    end
    if valueType ~= "table" then error("Cannot serialize " .. valueType .. " across VM boundary", 3) end
    active = active or {}
    if active[value] then error("Cannot serialize cyclic table", 3) end
    active[value] = true
    local entries = {}
    for key, item in value do
        local keyType = type(key)
        if keyType ~= "number" and keyType ~= "string" and keyType ~= "boolean" then
            error("Unsupported table key across VM boundary: " .. keyType, 3)
        end
        table.insert(entries, { key = encode(key, active), value = encode(item, active) })
    end
    active[value] = nil
    return { tag = "table", entries = entries }
end
local function decode(value)
    if value.tag == "nil" then return nil end
    if value.tag == "instance" then return records[value.id] and proxy(records[value.id]) or nil end
    if value.tag == "enum" then return Enum[value.family][value.name] end
    if value.tag == "number" then
        if value.special == "nan" then return 0 / 0 end
        if value.special == "+inf" then return math.huge end
        if value.special == "-inf" then return -math.huge end
        return value.value
    end
    if value.tag == "string" or value.tag == "boolean" then return value.value end
    if value.tag == "table" then
        local result = {}
        for _, entry in value.entries do result[decode(entry.key)] = decode(entry.value) end
        return result
    end
    if __types and __types.decode then return __types.decode(value) end
    error("Unknown wire type: " .. tostring(value.tag), 2)
end
local function encodeTuple(...)
    local args = table.pack(...)
    local encoded = { n = args.n, values = {} }
    for i = 1, args.n do encoded.values[i] = encode(args[i]) end
    return encoded
end
local function decodeTuple(args)
    local decoded = { n = args.n }
    for i = 1, args.n do decoded[i] = decode(args.values[i]) end
    return decoded
end

methods = {}
function methods:IsA(class) return isA(recordOf(self), class) end
function methods:GetChildren()
    local result = {}
    for _, child in recordOf(self).children do table.insert(result, proxy(child)) end
    return result
end
function methods:GetDescendants()
    local result = {}
    descendants(recordOf(self), result)
    for i, record in result do result[i] = proxy(record) end
    return result
end
function methods:FindFirstChild(name, recursive)
    local found = findChild(recordOf(self), name, recursive)
    return found and proxy(found) or nil
end
function methods:FindFirstChildOfClass(class)
    for _, child in recordOf(self).children do if child.class == class then return proxy(child) end end
    return nil
end
function methods:FindFirstChildWhichIsA(class, recursive)
    local all = recursive and self:GetDescendants() or self:GetChildren()
    for _, child in all do if child:IsA(class) then return child end end
    return nil
end
function methods:FindFirstAncestor(name)
    local cursor = recordOf(self).parent
    while cursor do if cursor.name == name then return proxy(cursor) end; cursor = cursor.parent end
    return nil
end
function methods:FindFirstAncestorOfClass(class)
    local cursor = recordOf(self).parent
    while cursor do if cursor.class == class then return proxy(cursor) end; cursor = cursor.parent end
    return nil
end
function methods:FindFirstAncestorWhichIsA(class)
    local cursor = recordOf(self).parent
    while cursor do if isA(cursor, class) then return proxy(cursor) end; cursor = cursor.parent end
    return nil
end
function methods:IsDescendantOf(ancestor)
    local target, cursor = recordOf(ancestor), recordOf(self).parent
    while cursor do if cursor == target then return true end; cursor = cursor.parent end
    return false
end
function methods:IsAncestorOf(descendant) return descendant:IsDescendantOf(self) end
function methods:GetFullName()
    local pieces, cursor = {}, recordOf(self)
    while cursor and cursor.class ~= "DataModel" do table.insert(pieces, 1, cursor.name); cursor = cursor.parent end
    return table.concat(pieces, ".")
end
function methods:GetActor()
    local cursor = recordOf(self).parent
    while cursor do if cursor.class == "Actor" then return proxy(cursor) end; cursor = cursor.parent end
    return nil
end
function methods:WaitForChild(name, timeout)
    local existing = self:FindFirstChild(name)
    if existing then return existing end
    if timeout ~= nil and (type(timeout) ~= "number" or timeout < 0) then error("Invalid timeout", 2) end
    local ready, done = rt.newSignal(), false
    local connection = self.ChildAdded:Connect(function(child)
        if child.Name == name and not done then done = true; ready:Fire(child) end
    end)
    local timer
    if timeout then timer = rt.delay(timeout, function() if not done then done = true; ready:Fire(nil) end end) end
    local child = ready:Wait()
    connection:Disconnect()
    if timer then rt.cancel(timer) end
    return child
end
function methods:GetPropertyChangedSignal(name)
    readProperty(recordOf(self), name)
    return signal(recordOf(self), "property:" .. name)
end
function methods:GetAttribute(name) return recordOf(self).attributes[name] end
function methods:GetAttributes() return table.clone(recordOf(self).attributes) end
function methods:SetAttribute(name, value)
    assertWritable()
    if type(name) ~= "string" or name == "" then error("Invalid attribute name", 2) end
    local kind = type(value)
    if value ~= nil and kind ~= "number" and kind ~= "string" and kind ~= "boolean" then
        error("Unsupported attribute type: " .. kind, 2)
    end
    local record = recordOf(self)
    if record.attributes[name] == value then return end
    record.attributes[name] = value
    mark(record)
    fire(record, "AttributeChanged", name)
    fire(record, "attribute:" .. name)
end
function methods:GetAttributeChangedSignal(name) return signal(recordOf(self), "attribute:" .. name) end
function methods:Destroy()
    assertWritable()
    local record = recordOf(self)
    if record.destroyed then return end
    fire(record, "Destroying")
    local children = table.clone(record.children)
    for _, child in children do proxy(child):Destroy() end
    setParent(record, nil)
    record.destroyed = true
    for _, event in record.signals do
        if event.DisconnectAll then event:DisconnectAll() end
    end
    record.signals = {}
    mark(record)
end
function methods:ClearAllChildren()
    for _, child in self:GetChildren() do child:Destroy() end
end
function methods:Clone()
    assertWritable()
    local original = recordOf(self)
    if self.Archivable == false then return nil end
    local mapping = {}
    local function copy(source)
        local target = makeRecord(source.class, source.name)
        target.source = source.source
        target.properties = table.clone(source.properties)
        target.attributes = table.clone(source.attributes)
        mapping[source] = target
        for _, child in source.children do
            if readProperty(child, "Archivable") then setParent(copy(child), target) end
        end
        return target
    end
    local result = copy(original)
    for _, target in mapping do
        for key, value in target.properties do
            if objects[value] and mapping[objects[value]] then target.properties[key] = proxy(mapping[objects[value]]) end
        end
    end
    return proxy(result)
end
function methods:GetService(name)
    if recordOf(self).class ~= "DataModel" then error("GetService requires DataModel", 2) end
    if name == "workspace" then name = "Workspace" end
    if not serviceNames[name] then error("UnsupportedApi: service " .. tostring(name), 2) end
    return proxy(records[name])
end
function methods:FindService(name)
    if recordOf(self).class ~= "DataModel" then error("FindService requires DataModel", 2) end
    return serviceNames[name] and records[name] and proxy(records[name]) or nil
end
function methods:IsLoaded() if recordOf(self).class ~= "DataModel" then error("IsLoaded requires DataModel", 2) end; return true end
function methods:GetPlayers()
    if recordOf(self).class ~= "Players" then error("GetPlayers requires Players", 2) end
    local result = {}
    for _, child in recordOf(self).children do if child.class == "Player" then table.insert(result, proxy(child)) end end
    return result
end
function methods:GetPlayerFromCharacter(character)
    for _, player in self:GetPlayers() do if player.Character == character then return player end end
    return nil
end
function methods:IsClient() if recordOf(self).class ~= "RunService" then error("IsClient requires RunService", 2) end; return config.side == "client" end
function methods:IsServer() if recordOf(self).class ~= "RunService" then error("IsServer requires RunService", 2) end; return config.side == "server" end
function methods:IsStudio() if recordOf(self).class ~= "RunService" then error("IsStudio requires RunService", 2) end; return config.isStudio == true end
function methods:IsRunning() if recordOf(self).class ~= "RunService" then error("IsRunning requires RunService", 2) end; return true end
function methods:GetLogHistory() if recordOf(self).class ~= "LogService" then error("GetLogHistory requires LogService", 2) end; return table.clone(logs) end
function methods:JSONEncode(value)
    if recordOf(self).class ~= "HttpService" or not __host_json_encode then error("UnsupportedApi: JSONEncode", 2) end
    return __host_json_encode(value)
end
function methods:JSONDecode(value)
    if recordOf(self).class ~= "HttpService" or not __host_json_decode then error("UnsupportedApi: JSONDecode", 2) end
    return __host_json_decode(value)
end
function methods:Fire(...)
    local record = recordOf(self)
    if record.class ~= "BindableEvent" then error("Fire requires BindableEvent", 2) end
    signal(record, "Event"):Fire(...)
end
function methods:Invoke(...)
    local record = recordOf(self)
    if record.class ~= "BindableFunction" then error("Invoke requires BindableFunction", 2) end
    while not record.properties.OnInvoke do rt.wait() end
    return record.properties.OnInvoke(...)
end
local function checkRemote(self, class, side)
    local record = recordOf(self)
    if not isA(record, class) then error(class .. " expected", 3) end
    if config.side ~= side then error("This method can only be called from the " .. side, 3) end
    return record
end
function methods:FireServer(...)
    local record = checkRemote(self, "RemoteEvent", "client")
    __host_emit("remote_event", { remoteId = record.id, target = "server", args = encodeTuple(...) })
end
function methods:FireClient(player, ...)
    local record = checkRemote(self, "RemoteEvent", "server")
    local target = recordOf(player)
    if target.class ~= "Player" then error("Player expected", 2) end
    __host_emit("remote_event", { remoteId = record.id, target = target.id, args = encodeTuple(...) })
end
function methods:FireAllClients(...)
    local record = checkRemote(self, "RemoteEvent", "server")
    __host_emit("remote_event", { remoteId = record.id, target = "*", args = encodeTuple(...) })
end
local function invokeRemote(record, target, ...)
    requestSequence += 1
    local requestId = config.realmId .. ":request:" .. requestSequence
    local response = rt.newSignal()
    pendingCalls[requestId] = response
    __host_emit("remote_call", { remoteId = record.id, target = target, requestId = requestId, args = encodeTuple(...) })
    local result = table.pack(response:Wait())
    pendingCalls[requestId] = nil
    if not result[1] then error(result[2], 3) end
    return table.unpack(result, 2, result.n)
end
function methods:InvokeServer(...)
    return invokeRemote(checkRemote(self, "RemoteFunction", "client"), "server", ...)
end
function methods:InvokeClient(player, ...)
    local record = checkRemote(self, "RemoteFunction", "server")
    local target = recordOf(player)
    if target.class ~= "Player" then error("Player expected", 2) end
    return invokeRemote(record, target.id, ...)
end
function methods:SendMessage(topic, ...)
    local record = recordOf(self)
    if record.class ~= "Actor" then error("SendMessage requires Actor", 2) end
    __host_emit("actor_message", { actorId = record.id, topic = topic, args = encodeTuple(...) })
end
function methods:BindToMessage(topic, callback)
    local record = recordOf(self)
    if record.class ~= "Actor" or config.actorId ~= record.id then error("Only scripts owned by this Actor can bind messages", 2) end
    return signal(record, "message:" .. topic):Connect(callback)
end
function methods:BindToMessageParallel(topic, callback)
    local record = recordOf(self)
    if record.class ~= "Actor" or config.actorId ~= record.id then error("Only scripts owned by this Actor can bind messages", 2) end
    return signal(record, "message:" .. topic):ConnectParallel(callback)
end

Instance = {}
function Instance.new(class, parent)
    local record = makeRecord(class)
    if parent then setParent(record, recordOf(parent)) end
    return proxy(record)
end
table.freeze(Instance)
typeof = function(value)
    if objects[value] then return "Instance" end
    if enumValues[value] then return "EnumItem" end
    return baseTypeof(value)
end

local gameRecord = makeRecord("DataModel", "Game", "game")
for name in serviceNames do setParent(makeRecord(name, name, name), gameRecord) end
game, workspace = proxy(gameRecord), proxy(records.Workspace)
shared = shared or {}

local function scriptEnvironment(record)
    local object = proxy(record)
    if environments[object] then return environments[object] end
    local env = setmetatable({ script = object }, { __index = getfenv(0) })
    environments[object] = env
    return env
end
local function requireModule(module)
    if rt.getMode and rt.getMode() == "parallel" then error("require cannot be called in parallel", 2) end
    local record = recordOf(module)
    if record.class ~= "ModuleScript" then error("ModuleScript expected", 2) end
    local cached = moduleCache[record.id]
    if cached then
        if cached.loading then
            if cached.owner == coroutine.running() then error("Requested module was required recursively", 2) end
            cached.ready:Wait()
        end
        if cached.error then error(cached.error, 2) end
        return cached.value
    end
    cached = { loading = true, ready = rt.newSignal(), owner = coroutine.running() }
    moduleCache[record.id] = cached
    local results = table.pack(pcall(function()
        local fn = __host_compile(record.source, record.name, scriptEnvironment(record))
        return fn()
    end))
    cached.loading, cached.owner = false, nil
    if not results[1] then cached.error = tostring(results[2])
    elseif results.n ~= 2 or results[2] == nil then cached.error = "Module code did not return exactly one value"
    else cached.value = results[2] end
    cached.ready:Fire()
    if cached.error then error(cached.error, 2) end
    return cached.value
end
require = requireModule

local model = {}
function model.get(id) return records[id] and proxy(records[id]) or nil end
function model.seed(nodes)
    local held = {}
    for _, node in nodes do
        local record = makeRecord(node.class, node.name, node.id)
        table.insert(held, record)
        record.name, record.source = node.name or node.class, node.source or record.source
        for key, value in node.properties or {} do record.properties[key] = value end
        for key, value in node.attributes or {} do record.attributes[key] = value end
        mark(record)
    end
    for _, node in nodes do
        if node.parent then
            local parent = records[node.parent]
            if not parent then error("Unknown parent: " .. node.parent) end
            setParent(records[node.id], parent)
        end
    end
end
function model.exportChanges()
    local result = {}
    for id, record in changes do
        local properties = {}
        for key, value in record.properties do if type(value) ~= "function" then properties[key] = value end end
        table.insert(result, { id = id, class = record.class, name = record.name,
            parent = record.parent and record.parent.id or nil, source = record.source,
            properties = encode(properties), attributes = encode(record.attributes), destroyed = record.destroyed })
    end
    table.sort(result, function(a, b) return a.id < b.id end)
    changes = {}
    return result
end
function model.applyChanges(nodes)
    applying = true
    local ok, message = pcall(function()
        local held = {}
        for _, node in nodes do table.insert(held, makeRecord(node.class, node.name, node.id)) end
        for _, node in nodes do
            local record = records[node.id]
            if record.name ~= node.name then record.name = node.name; propertyChanged(record, "Name", node.name) end
            record.source = node.source or record.source
            local incoming = decode(node.properties)
            for key, value in table.clone(record.properties) do
                if incoming[key] == nil and type(value) ~= "function" then
                    record.properties[key] = nil
                    propertyChanged(record, key, nil)
                end
            end
            for key, value in incoming do
                if record.properties[key] ~= value then record.properties[key] = value; propertyChanged(record, key, value) end
            end
            local attributes = decode(node.attributes)
            local attributeKeys = table.clone(record.attributes)
            for key, value in attributes do attributeKeys[key] = value end
            for key in attributeKeys do
                if record.attributes[key] ~= attributes[key] then
                    record.attributes[key] = attributes[key]
                    fire(record, "AttributeChanged", key)
                    fire(record, "attribute:" .. key)
                end
            end
            if node.destroyed then proxy(record):Destroy()
            else setParent(record, node.parent and records[node.parent] or nil) end
        end
    end)
    applying = false
    if not ok then error(message) end
end
function model.runScript(id)
    local record = assert(records[id], "Unknown script " .. id)
    if not isA(record, "BaseScript") then error("Executable Script or LocalScript expected") end
    if readProperty(record, "Disabled") or record.destroyed then return nil end
    local fn = __host_compile(record.source, record.name, scriptEnvironment(record))
    return rt.spawn(fn)
end
function model.deliver(kind, payload)
    if kind == "remote_return" then
        local response = pendingCalls[payload.requestId]
        if response then
            local args = decodeTuple(payload.args)
            response:Fire(payload.ok, table.unpack(args, 1, args.n))
        end
        return
    end
    local record = records[payload.remoteId or payload.actorId or ""]
    if not record or record.destroyed then error("Remote/Actor unavailable: " .. tostring(payload.remoteId or payload.actorId)) end
    local args = decodeTuple(payload.args)
    if kind == "actor_message" then
        signal(record, "message:" .. payload.topic):Fire(table.unpack(args, 1, args.n))
    elseif kind == "remote_event" then
        if config.side == "server" then
            local player = records[payload.sender or payload.clientId or ""]
            if not player then error("Unknown remote sender") end
            signal(record, "OnServerEvent"):Fire(proxy(player), table.unpack(args, 1, args.n))
        else
            signal(record, "OnClientEvent"):Fire(table.unpack(args, 1, args.n))
        end
    elseif kind == "remote_call" then
        rt.spawn(function()
            local callbackName = config.side == "server" and "OnServerInvoke" or "OnClientInvoke"
            local result = table.pack(pcall(function()
                while not record.properties[callbackName] do rt.wait() end
                if config.side == "server" then
                    local player = records[payload.sender or payload.clientId or ""]
                    if not player then error("Unknown remote sender") end
                    return record.properties[callbackName](proxy(player), table.unpack(args, 1, args.n))
                end
                return record.properties[callbackName](table.unpack(args, 1, args.n))
            end))
            __host_emit("remote_return", { requestId = payload.requestId, target = payload.sourceRealm or payload.replyTo,
                ok = result[1], args = encodeTuple(table.unpack(result, 2, result.n)) })
        end)
    else error("Unsupported message kind: " .. tostring(kind)) end
end
function model.getInstances()
    local result = {}
    for _, record in records do if not record.destroyed then table.insert(result, proxy(record)) end end
    return result
end
function model.getNilInstances()
    local result = {}
    for _, record in records do if not record.parent and not record.destroyed then table.insert(result, proxy(record)) end end
    return result
end
function model.getEnvironment(instance) return scriptEnvironment(recordOf(instance)) end
function model.log(level, message)
    local names = { print = "MessageOutput", info = "MessageInfo", warn = "MessageWarning", error = "MessageError" }
    local messageType = Enum.MessageType[names[level] or "MessageOutput"]
    table.insert(logs, { message = message, messageType = messageType, timestamp = rt.now })
    if #logs > 1000 then table.remove(logs, 1) end
    fire(records.LogService, "MessageOut", message, messageType)
end
function model.frame(dt)
    local run = records.RunService
    fire(run, "PreSimulation", dt)
    fire(run, "Stepped", rt.now, dt)
    fire(run, "PostSimulation", dt)
    fire(run, "Heartbeat", dt)
    if config.side == "client" then fire(run, "PreRender", dt); fire(run, "RenderStepped", dt) end
end
model.encode, model.decode, model.encodeTuple, model.decodeTuple = encode, decode, encodeTuple, decodeTuple
__model = model
local hostLog = __host_log
__host_log = function(level, message)
    hostLog(level, message)
    model.log(level, tostring(message))
end
changes = {}
