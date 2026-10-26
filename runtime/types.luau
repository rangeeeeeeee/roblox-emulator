local originalTypeof = typeof
local types = setmetatable({}, {__mode = "k"})
function typeof(value)
    return types[value] or originalTypeof(value)
end
local function vectorType(name, fields)
    local api = {}
    local storage = setmetatable({}, {__mode = "k"})
    local function equal(a,b)
        if not storage[a] or not storage[b] then return false end
        for _,field in fields do if a[field]~=b[field] then return false end end
        return true
    end
    function api.new(...)
        local values = table.pack(...)
        local object = newproxy(true)
        local data = {}
        for i, field in fields do
            local value = values[i] or 0
            assert(type(value) == "number", "Expected number")
            data[field] = value
        end
        storage[object] = data
        types[object] = name
        local mt = getmetatable(object)
        mt.__index = function(_, key)
            if data[key] ~= nil then return data[key] end
            if key == "Magnitude" then local n=0; for _,field in fields do n += data[field]^2 end; return math.sqrt(n) end
            if key == "Unit" then return object / object.Magnitude end
            if key == "Dot" then return function(_, other) local n=0; for _,field in fields do n += data[field]*other[field] end; return n end end
            if key == "Lerp" then return function(_,other,alpha) return object+(other-object)*alpha end end
            if key == "Cross" and #fields == 3 then return function(_,o) return api.new(data.Y*o.Z-data.Z*o.Y,data.Z*o.X-data.X*o.Z,data.X*o.Y-data.Y*o.X) end end
            if key == "FuzzyEq" then return function(_,other,epsilon) return (object-other).Magnitude <= (epsilon or 1e-5) end end
            error(key.." is not a valid member of "..name,2)
        end
        mt.__newindex = function() error(name.." is immutable",2) end
        local function operation(a,b,fn)
            local result={}
            for i,field in fields do result[i]=fn(type(a)=="number" and a or a[field], type(b)=="number" and b or b[field]) end
            return api.new(table.unpack(result))
        end
        mt.__add=function(a,b)return operation(a,b,function(x,y)return x+y end)end
        mt.__sub=function(a,b)return operation(a,b,function(x,y)return x-y end)end
        mt.__mul=function(a,b)return operation(a,b,function(x,y)return x*y end)end
        mt.__div=function(a,b)return operation(a,b,function(x,y)return x/y end)end
        mt.__unm=function(a)return a * -1 end
        mt.__eq=equal
        mt.__tostring=function() local v={};for _,field in fields do table.insert(v,tostring(data[field]))end;return table.concat(v,", ")end
        mt.__metatable=name
        return object
    end
    api.zero=api.new()
    api.one=api.new(table.unpack(table.create(#fields,1)))
    return table.freeze(api)
end
Vector2=vectorType("Vector2",{"X","Y"})
Vector3=vectorType("Vector3",{"X","Y","Z"})
__types = {}
function __types.encode(value)
    local kind=types[value]
    if kind=="Vector2" then return {tag="Vector2",x=value.X,y=value.Y} end
    if kind=="Vector3" then return {tag="Vector3",x=value.X,y=value.Y,z=value.Z} end
    return nil
end
function __types.decode(value)
    if value.tag=="Vector2" then return Vector2.new(value.x,value.y) end
    if value.tag=="Vector3" then return Vector3.new(value.x,value.y,value.z) end
    error("Unsupported wire datatype: "..tostring(value.tag),2)
end
