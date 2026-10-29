use mlua::{Error, Function, IntoLua, Lua, MetaMethod, Result, Table, UserData, UserDataMethods, Value};
use std::{cell::RefCell, collections::BTreeMap, ffi::CString, rc::Rc};

// There is no renderer in the emulator. These objects retain typed virtual state only.
const MAX_OBJECTS: usize = 4096;
const MAX_BYTES: usize = 16 * 1024 * 1024;
const METHODS: &str = "emulator.drawing.methods";

#[derive(Clone, Copy)]
enum Shape { Line, Text, Image, Circle, Square, Quad, Triangle }

impl Shape {
    fn parse(name: &str) -> Result<Self> {
        match name {
            "Line" => Ok(Self::Line), "Text" => Ok(Self::Text), "Image" => Ok(Self::Image),
            "Circle" => Ok(Self::Circle), "Square" => Ok(Self::Square),
            "Quad" => Ok(Self::Quad), "Triangle" => Ok(Self::Triangle),
            _ => Err(Error::lua_string(format!("Unsupported Drawing type: {name}"))),
        }
    }
}

#[derive(Clone)]
enum Property {
    Boolean(bool), Number(f64), Vector(crate::types::Vector2), Color(crate::types::Color3), Bytes(Vec<u8>),
}

impl Property {
    fn bytes(&self) -> usize { if let Self::Bytes(bytes) = self { bytes.len() } else { 0 } }

    fn to_lua(&self, lua: &Lua) -> Result<Value> {
        match self {
            Self::Boolean(value) => Ok(Value::Boolean(*value)),
            Self::Number(value) => Ok(Value::Number(*value)),
            Self::Vector(value) => (*value).into_lua(lua),
            Self::Color(value) => (*value).into_lua(lua),
            Self::Bytes(value) => Ok(Value::String(lua.create_string(value)?)),
        }
    }

    fn from_lua(&self, value: Value, name: &str) -> Result<Self> {
        let error = || Error::lua_string(format!("Invalid value for Drawing.{name}"));
        match (self, value) {
            (Self::Boolean(_), Value::Boolean(value)) => Ok(Self::Boolean(value)),
            (Self::Number(_), Value::Integer(value)) => Ok(Self::Number(value as f64)),
            (Self::Number(_), Value::Number(value)) if value.is_finite() => Ok(Self::Number(value)),
            (Self::Vector(_), Value::UserData(value)) => value.borrow::<crate::types::Vector2>().map(|v| Self::Vector(*v)).map_err(|_| error()),
            (Self::Color(_), Value::UserData(value)) => value.borrow::<crate::types::Color3>().map(|v| Self::Color(*v)).map_err(|_| error()),
            (Self::Bytes(_), Value::String(value)) => Ok(Self::Bytes(value.as_bytes().to_vec())),
            _ => Err(error()),
        }
    }
}

struct Object { fields: BTreeMap<&'static str, Property> }
#[derive(Default)]
struct State { next: u64, bytes: usize, objects: BTreeMap<u64, Object> }

impl State {
    fn remove(&mut self, id: u64) {
        if let Some(object) = self.objects.remove(&id) {
            self.bytes -= object.fields.values().map(Property::bytes).sum::<usize>();
        }
    }
}

#[derive(Clone)]
struct Drawing { state: Rc<RefCell<State>>, id: u64 }

impl Drawing {
    fn exists(&self) -> bool { self.state.borrow().objects.contains_key(&self.id) }

    fn read(&self, lua: &Lua, name: &str) -> Result<Value> {
        if name == "__OBJECT_EXISTS" { return Ok(Value::Boolean(self.exists())); }
        let state = self.state.borrow();
        let object = state.objects.get(&self.id).ok_or_else(|| Error::lua_string("Drawing object has been removed"))?;
        if name == "TextBounds" && object.fields.contains_key("Text") {
            return Err(Error::lua_string("UnsupportedApi: Drawing.TextBounds requires text measurement"));
        }
        object.fields.get(name).ok_or_else(|| Error::lua_string(format!("Unsupported Drawing property: {name}")))?.to_lua(lua)
    }

    fn write(&self, name: &str, value: Value) -> Result<()> {
        if name == "__OBJECT_EXISTS" || name == "TextBounds" { return Err(Error::lua_string(format!("Drawing.{name} is read-only"))); }
        let mut state = self.state.borrow_mut();
        let object = state.objects.get(&self.id).ok_or_else(|| Error::lua_string("Drawing object has been removed"))?;
        let previous = object.fields.get(name).ok_or_else(|| Error::lua_string(format!("Unsupported Drawing property: {name}")))?;
        let old_size = previous.bytes();
        if let (Property::Bytes(_), Value::String(value)) = (previous, &value) {
            let length = value.as_bytes().len();
            if length > MAX_BYTES || state.bytes - old_size > MAX_BYTES - length {
                return Err(Error::lua_string("DrawingLimit: data budget exceeded"));
            }
        }
        let next = previous.from_lua(value, name)?;
        let next_size = next.bytes();
        if next_size > MAX_BYTES || state.bytes - old_size > MAX_BYTES - next_size {
            return Err(Error::lua_string("DrawingLimit: data budget exceeded"));
        }
        state.bytes = state.bytes - old_size + next_size;
        *state.objects.get_mut(&self.id).unwrap().fields.get_mut(name).unwrap() = next;
        Ok(())
    }
}

impl UserData for Drawing {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::Index, |lua, this, name: String| {
            if matches!(name.as_str(), "Destroy" | "Remove") {
                return crate::native::private_registry_value::<Table>(lua,METHODS)?.get::<Value>(name);
            }
            this.read(lua, &name)
        });
        methods.add_meta_method(MetaMethod::NewIndex, |_, this, (name, value): (String, Value)| this.write(&name, value));
    }
}

fn fields(shape: Shape) -> BTreeMap<&'static str, Property> {
    use Property::{Boolean as B, Bytes as S, Color as C, Number as N, Vector as V};
    let zero = crate::types::Vector([0., 0.]);
    let white = crate::types::Color3([1., 1., 1.]);
    let mut result = BTreeMap::from([
        ("Visible", B(false)), ("ZIndex", N(0.)), ("Transparency", N(0.)), ("Color", C(white)),
    ]);
    let values: Vec<(&'static str, Property)> = match shape {
        Shape::Line => vec![("From", V(zero)), ("To", V(zero)), ("Thickness", N(1.))],
        Shape::Text => vec![("Text", S(Vec::new())), ("Font", N(0.)), ("Size", N(13.)), ("Position", V(zero)), ("Center", B(false)), ("Outline", B(false)), ("OutlineColor", C(crate::types::Color3([0.,0.,0.])))],
        Shape::Image => vec![("Data", S(Vec::new())), ("Size", V(zero)), ("Position", V(zero)), ("Rounding", N(0.))],
        Shape::Circle => vec![("NumSides", N(32.)), ("Radius", N(0.)), ("Position", V(zero)), ("Thickness", N(1.)), ("Filled", B(false))],
        Shape::Square => vec![("Size", V(zero)), ("Position", V(zero)), ("Thickness", N(1.)), ("Filled", B(false))],
        Shape::Quad => vec![("PointA", V(zero)), ("PointB", V(zero)), ("PointC", V(zero)), ("PointD", V(zero)), ("Thickness", N(1.)), ("Filled", B(false))],
        Shape::Triangle => vec![("PointA", V(zero)), ("PointB", V(zero)), ("PointC", V(zero)), ("Thickness", N(1.)), ("Filled", B(false))],
    };
    result.extend(values);
    result
}

fn object(value: Value, state: &Rc<RefCell<State>>) -> Result<Drawing> {
    if let Value::UserData(value) = value {
        if let Ok(drawing) = value.borrow::<Drawing>() {
            if Rc::ptr_eq(&drawing.state, state) { return Ok(drawing.clone()); }
        }
    }
    Err(Error::lua_string("Drawing object expected"))
}

fn named<A: mlua::FromLuaMulti, R: mlua::IntoLuaMulti>(lua: &Lua, env: &Table, name: &str, callback: impl Fn(&Lua,A)->Result<R> + 'static) -> Result<Function> {
    let function = lua.create_function(callback)?;
    crate::native::name_function(lua, &function, &CString::new(name).unwrap())?;
    crate::native::mark_executor_function(lua, &function, env)?;
    Ok(function)
}

pub(crate) fn install(lua: &Lua, env: &Table, caps: &crate::native::Capabilities) -> Result<()> {
    let state = Rc::new(RefCell::new(State::default()));
    let methods = lua.create_table()?;
    let state_for_destroy = state.clone();
    let destroy = named(lua, env, "Destroy", move |_, value: Value| {
        let drawing = object(value, &state_for_destroy)?;
        drawing.state.borrow_mut().remove(drawing.id);
        Ok(())
    })?;
    methods.set("Destroy", destroy.clone())?;
    methods.set("Remove", destroy)?;
    crate::native::set_private_registry_value(lua,METHODS, methods)?;

    let drawing = lua.create_table()?;
    let state_for_new = state.clone();
    drawing.set("new", named(lua, env, "new", move |lua, name: String| {
        let shape = Shape::parse(&name)?;
        let mut state = state_for_new.borrow_mut();
        if state.objects.len() >= MAX_OBJECTS { return Err(Error::lua_string("DrawingLimit: object limit exceeded")); }
        state.next = state.next.checked_add(1).ok_or_else(|| Error::lua_string("DrawingLimit: object identifiers exhausted"))?;
        let id = state.next;
        let userdata = lua.create_userdata(Drawing { state: state_for_new.clone(), id })?;
        state.objects.insert(id, Object { fields: fields(shape) });
        Ok(userdata)
    })?)?;
    let fonts = lua.create_table()?;
    for (name, value) in [("UI",0),("System",1),("Plex",2),("Monospace",3)] { fonts.set(name,value)?; }
    fonts.set_readonly(true);
    drawing.set("Fonts", fonts)?;
    env.set("Drawing", drawing)?;

    let state_for_is = state.clone();
    env.set("isrenderobj", named(lua, env, "isrenderobj", move |_, value: Value| {
        Ok(object(value, &state_for_is).is_ok_and(|drawing| drawing.exists()))
    })?)?;
    let state_for_get = state.clone();
    env.set("getrenderproperty", named(lua, env, "getrenderproperty", move |lua, (value, name): (Value,String)| {
        object(value, &state_for_get)?.read(lua, &name)
    })?)?;
    let state_for_set = state.clone();
    env.set("setrenderproperty", named(lua, env, "setrenderproperty", move |_, (value, name, property): (Value,String,Value)| {
        object(value, &state_for_set)?.write(&name, property)
    })?)?;
    env.set("cleardrawcache", named(lua, env, "cleardrawcache", move |_, ()| {
        let mut state = state.borrow_mut();
        state.objects.clear(); state.bytes = 0;
        Ok(())
    })?)?;
    for name in ["Drawing", "Drawing.new", "isrenderobj", "getrenderproperty", "setrenderproperty", "cleardrawcache"] {
        caps.set(name, "partial: VM-local headless Drawing state; no renderer or TextBounds")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> Result<(Lua,Table)> {
        let lua = Lua::new();
        crate::native::install(&lua)?;
        crate::types::install(&lua)?;
        let env = lua.create_table()?;
        let mt = lua.create_table()?;
        mt.set("__index", lua.globals())?;
        env.set_metatable(Some(mt))?;
        install(&lua, &env, &crate::native::Capabilities::default())?;
        Ok((lua,env))
    }

    #[test]
    fn host_method_registry_is_not_visible_through_getgc() -> Result<()> {
        let (lua, _) = runtime()?;
        let methods:Table=crate::native::private_registry_value(&lua,METHODS)?;
        let getgc:Function=lua.globals().get("__host_getgc")?;
        let objects:Table=getgc.call(true)?;
        for object in objects.sequence_values::<Value>() {
            if let Value::Table(table)=object? {assert_ne!(table.to_pointer(),methods.to_pointer())}
        }
        Ok(())
    }

    #[test]
    fn typed_shapes_properties_and_lifetime() -> Result<()> {
        let (lua, env) = runtime()?;
        lua.load(r#"
            assert(Drawing.Fonts.UI==0 and Drawing.Fonts.System==1 and Drawing.Fonts.Plex==2 and Drawing.Fonts.Monospace==3)
            for _, kind in {'Line','Text','Image','Circle','Square','Quad','Triangle'} do
                local d=Drawing.new(kind)
                assert(type(d)=='userdata' and isrenderobj(d) and d.__OBJECT_EXISTS and not d.Visible)
                d.Color=Color3.new(0.25,0.5,0.75); d.Transparency=0.25; d.ZIndex=3
                setrenderproperty(d,'Visible',true)
                assert(getrenderproperty(d,'Visible')==true and d.Color.G==0.5 and d.Transparency==0.25 and d.ZIndex==3)
                assert(not pcall(function()d.Visible='yes'end))
                assert(not pcall(setrenderproperty,d,'Color',Vector2.new(1,2)))
                assert(not pcall(getrenderproperty,d,'UnknownProperty'))
                assert(not pcall(function()d.__OBJECT_EXISTS=false end))
                d:Destroy()
                assert(not d.__OBJECT_EXISTS and not isrenderobj(d))
                assert(not pcall(getrenderproperty,d,'Visible'))
            end
            local s=Drawing.new('Square')
            s.Size=Vector2.new(10,20); s.Position=Vector2.new(3,4); s.Filled=true
            assert(s.Size.X==10 and s.Size.Y==20 and s.Position.Y==4 and s.Filled)
            assert(not pcall(function()s.Radius=3 end))
            local t=Drawing.new('Text'); t.Text='abc'; t.Font=Drawing.Fonts.Plex
            assert(t.Text=='abc' and t.Font==2 and not pcall(getrenderproperty,t,'TextBounds'))
            local image=Drawing.new('Image');image.Data=string.char(0,255,0)
            assert(image.Data==string.char(0,255,0))
            s:Remove()
            cleardrawcache()
            assert(not s.__OBJECT_EXISTS and not t.__OBJECT_EXISTS and not image.__OBJECT_EXISTS)
            assert(not isrenderobj(newproxy()) and not isrenderobj({}))
            assert(not pcall(Drawing.new,'invalid'))
        "#).set_environment(env).exec()?;
        Ok(())
    }

    #[test]
    fn cache_is_per_vm_and_limits_remain_recoverable() -> Result<()> {
        let (first, env) = runtime()?;
        let (second, other) = runtime()?;
        first.load("x=Drawing.new('Line'); assert(x.__OBJECT_EXISTS)").set_environment(env.clone()).exec()?;
        second.load("cleardrawcache()").set_environment(other).exec()?;
        first.load(r#"
            assert(x.__OBJECT_EXISTS)
            for _=2,4096 do Drawing.new('Line') end
            assert(not pcall(Drawing.new,'Line'))
            x:Destroy()
            local replacement=Drawing.new('Image')
            replacement.Data=string.rep('a',16*1024*1024)
            assert(not pcall(function()replacement.Data=string.rep('a',16*1024*1024+1)end))
            assert(#replacement.Data==16*1024*1024)
            cleardrawcache()
            assert(not replacement.__OBJECT_EXISTS)
            assert(Drawing.new('Line').__OBJECT_EXISTS)
        "#).set_environment(env).exec()?;
        Ok(())
    }
}
