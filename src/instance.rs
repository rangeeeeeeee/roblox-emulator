use mlua::{AnyUserData, Error, Function, Lua, MetaMethod, MultiValue, ObjectLike, Result, Table, UserData, UserDataFields, UserDataMethods, UserDataRegistry, Value, ffi};
use std::{cell::RefCell, collections::HashMap, rc::{Rc, Weak}, sync::OnceLock};

pub(crate) type Link = Rc<RefCell<Node>>;

#[derive(Clone, PartialEq)]
pub(crate) enum Attribute { Boolean(bool), Number(f64), String(String) }

pub(crate) struct Node {
    pub(crate) id: String,
    pub(crate) class: String,
    pub(crate) name: String,
    pub(crate) raw_name: Option<Vec<u8>>,
    pub(crate) parent: Weak<RefCell<Node>>,
    pub(crate) children: Vec<Link>,
    pub(crate) attributes: HashMap<String, Attribute>,
    pub(crate) archivable: bool,
    pub(crate) destroyed: bool,
    pub(crate) linked_source: String,
    pub(crate) source: String,
    pub(crate) properties: HashMap<String, Value>,
    pub(crate) signals: HashMap<String, AnyUserData>,
    pub(crate) callback_environments: HashMap<String, Table>,
}

#[derive(Clone)]
pub(crate) struct Instance(pub(crate) Link);

const CACHE: &str = "emulator.native.instances";
pub(crate) const METHODS: &str = "emulator.native.instance.methods";
pub(crate) const SERVICES: &[&str] = &["Workspace", "Players", "ReplicatedStorage", "ReplicatedFirst",
    "ServerStorage", "ServerScriptService", "RunService", "LogService", "HttpService", "CoreGui",
    "UserInputService", "GuiService", "VRService", "TextService", "ContentProvider", "NetworkClient",
    "StarterGui", "StarterPlayer", "EncodingService", "CollectionService", "Stats", "TweenService", "Chat", "ScriptContext",
    "CommerceService", "MarketplaceService", "HttpRbxApiService", "Lighting", "TextChatService", "ContextActionService", "SoundService"];

#[derive(serde::Deserialize)]
pub(crate) struct ClassInfo{superclass:Option<String>,pub(crate) service:bool,pub(crate) creatable:bool}
pub(crate) fn class_info(class:&str)->Option<&'static ClassInfo>{
    static CATALOG:OnceLock<HashMap<String,ClassInfo>>=OnceLock::new();
    CATALOG.get_or_init(||serde_json::from_str(include_str!("../native/classes.json")).expect("Studio class metadata")).get(class)
}

pub(crate) fn superclass(class: &str) -> Option<&'static str> {
    class_info(class).and_then(|info|info.superclass.as_deref())
}
pub(crate) fn implemented(class:&str)->bool{
    SERVICES.contains(&class)||matches!(class,
        "ServiceProvider" | "Folder" | "Configuration" | "BindableEvent" | "BindableFunction" |
        "RemoteEvent" | "RemoteFunction" | "Players" | "Player" | "ReplicatedStorage" |
        "ReplicatedFirst" | "ServerStorage" | "ServerScriptService" | "RunService" |
        "LogService" | "HttpService" | "CoreGui" | "PlayerGui" | "PlayerScripts" |
        "Backpack" | "LuaSourceContainer" | "PVInstance" | "UserInputService" | "GuiService" |
        "VRService" | "TextService" | "ContentProvider" | "NetworkClient" | "StarterGui" |
        "StarterPlayer" | "EncodingService" | "CollectionService" | "Stats" | "TweenService" | "Chat" | "ScriptContext" |
        "StarterPlayerScripts" | "StarterCharacterScripts" | "Humanoid" | "Animator" | "Animation" | "AnimationTrack" | "AnimationController" | "ValueBase" |
        "GuiBase2d" | "StringValue" | "NumberValue" | "IntValue" | "BoolValue" | "ObjectValue" |
        "GuiObject" | "LayerCollector" | "Frame" | "Path2D" | "Tween" | "TextLabel" | "TextButton" | "ImageLabel" |
        "ImageButton" | "ScreenGui" | "UnreliableRemoteEvent" | "DataModel" | "Model" | "BasePart" |
        "Camera" | "WorldRoot" | "Actor" | "Tool" | "Workspace" | "WorldModel" | "Part" | "MeshPart" |
        "Terrain" | "BaseScript" | "ModuleScript" | "Script" | "LocalScript" | "Instance")
}

pub(crate) fn is_a(node: &Node, expected: &str) -> bool {
    let mut class = Some(node.class.as_str());
    while let Some(value) = class {
        if value == expected { return true; }
        class = superclass(value);
    }
    false
}

pub(crate) fn new_node(class: &str) -> Result<Link> {
    if superclass(class).is_none() || matches!(class, "Instance" | "LuaSourceContainer" | "BaseScript" | "PVInstance" | "BasePart" | "ServiceProvider" | "WorldRoot") {
        return Err(Error::lua_string(format!("Unable to create an Instance of type \"{class}\"")));
    }
    if !implemented(class){return Err(Error::runtime(format!("UnsupportedApi: class {class}")))}
    Ok(Rc::new(RefCell::new(Node { id: String::new(), class: class.into(), name: class.into(), raw_name: None, parent: Weak::new(),
        children: Vec::new(), attributes: HashMap::new(), archivable: true, destroyed: false,
        linked_source: String::new(), source: String::new(), properties: HashMap::new(), signals: HashMap::new(), callback_environments: HashMap::new() })))
}
pub(crate) fn create_node(lua:&Lua,class:&str)->Result<Link>{
    if class_info(class).is_some()&&!implemented(class){return Err(crate::model::unsupported(lua,&format!("class {class}")))}
    new_node(class)
}

pub(crate) fn wrapper(lua: &Lua, node: Link) -> Result<AnyUserData> {
    let cache: Table = lua.named_registry_value(CACHE)?;
    let key = Rc::as_ptr(&node) as usize as i64;
    if let Value::UserData(value) = cache.raw_get(key)? { return Ok(value); }
    let value = lua.create_userdata(Instance(node))?;
    cache.raw_set(key, value.clone())?;
    Ok(value)
}

pub(crate) fn set_parent(node: &Link, target: Option<Link>) -> Result<()> {
    let previous = node.borrow().parent.upgrade();
    if previous.as_ref().zip(target.as_ref()).is_some_and(|(a, b)| Rc::ptr_eq(a, b))
        || previous.is_none() && target.is_none() { return Ok(()); }
    if node.borrow().destroyed && target.is_some() {
        let current = previous.as_ref().map(|parent| parent.borrow().name.clone()).unwrap_or_else(|| "NULL".into());
        let next = target.as_ref().map(|parent| parent.borrow().name.clone()).unwrap_or_else(|| "NULL".into());
        return Err(Error::lua_string(format!("The Parent property of {} is locked, current parent: {current}, new parent {next}", node.borrow().name)));
    }
    let mut cursor = target.clone();
    while let Some(parent) = cursor {
        if Rc::ptr_eq(node, &parent) { return Err(Error::runtime("Parent would create a circular reference")); }
        if parent.borrow().destroyed { return Err(Error::runtime("Cannot parent to a destroyed Instance")); }
        cursor = parent.borrow().parent.upgrade();
    }
    if let Some(parent) = previous { parent.borrow_mut().children.retain(|child| !Rc::ptr_eq(child, node)); }
    node.borrow_mut().parent = target.as_ref().map(Rc::downgrade).unwrap_or_default();
    if let Some(parent) = target { parent.borrow_mut().children.push(node.clone()); }
    Ok(())
}

pub(crate) fn name_bytes(node: &Node) -> &[u8] { node.raw_name.as_deref().unwrap_or_else(|| node.name.as_bytes()) }
pub(crate) fn set_name(node: &mut Node, bytes: &[u8]) -> bool {
    if name_bytes(node) == bytes { return false; }
    match std::str::from_utf8(bytes) {
        Ok(name) => { node.name = name.into(); node.raw_name = None; }
        Err(_) => {
            // The display/source label is escaped; Lua identity and lookups use raw_name.
            node.name = bytes.iter().flat_map(|byte| std::ascii::escape_default(*byte)).map(char::from).collect();
            node.raw_name = Some(bytes.to_vec());
        }
    }
    true
}

pub(crate) fn find_child(node: &Link, name: impl AsRef<[u8]>, recursive: bool) -> Option<Link> {
    let name = name.as_ref();
    let children = node.borrow().children.clone();
    if let Some(found) = children.iter().find(|child| name_bytes(&child.borrow()) == name) { return Some(found.clone()); }
    if recursive {
        let mut stack: Vec<Link> = children.into_iter().rev().collect();
        while let Some(current) = stack.pop() {
            let children = current.borrow().children.clone();
            if let Some(found) = children.iter().find(|child| name_bytes(&child.borrow()) == name) { return Some(found.clone()); }
            stack.extend(children.into_iter().rev());
        }
    }
    None
}

pub(crate) fn descendants(node: &Link) -> Vec<Link> {
    let mut result = Vec::new();
    let mut stack: Vec<Link> = node.borrow().children.iter().rev().cloned().collect();
    while let Some(current) = stack.pop() {
        stack.extend(current.borrow().children.iter().rev().cloned());
        result.push(current);
    }
    result
}
pub(crate) fn full_name(node:&Link)->String{
    let mut names=Vec::new();let mut cursor=Some(node.clone());
    while let Some(node)=cursor{let node=node.borrow();if node.class=="DataModel"{break}names.push(node.name.clone());cursor=node.parent.upgrade();}
    names.reverse();names.join(".")
}
fn full_name_bytes(node:&Link)->Vec<u8>{
    let mut names=Vec::new();let mut cursor=Some(node.clone());
    while let Some(node)=cursor{let node=node.borrow();if node.class=="DataModel"{break}names.push(name_bytes(&node).to_vec());cursor=node.parent.upgrade();}
    let mut result=Vec::new();for(index,name)in names.into_iter().rev().enumerate(){if index>0{result.push(b'.');}result.extend(name);}result
}

pub(crate) fn destroy(node: &Link) -> Result<()> {
    if node.borrow().destroyed { return Ok(()); }
    let mut all = descendants(node);
    all.push(node.clone());
    for item in all {
        set_parent(&item, None)?;
        let mut value = item.borrow_mut();
        value.children.clear();
        value.destroyed = true;
    }
    Ok(())
}

pub(crate) fn attribute_value(lua: &Lua, value: &Attribute) -> Result<Value> {
    Ok(match value {
        Attribute::Boolean(value) => Value::Boolean(*value),
        Attribute::Number(value) => Value::Number(*value),
        Attribute::String(value) => Value::String(lua.create_string(value)?),
    })
}

pub(crate) fn instance_list(lua: &Lua, nodes: Vec<Link>) -> Result<Table> {
    let result = lua.create_table_with_capacity(nodes.len(), 0)?;
    for (index, node) in nodes.into_iter().enumerate() { result.raw_set(index + 1, wrapper(lua, node)?)?; }
    Ok(result)
}

pub(crate) fn read_property(lua: &Lua, this: &Instance, key: &str) -> Result<Value> {
    if let Some(value) = crate::model::read_property(lua, &this.0, key)? { return Ok(value); }
    let node = this.0.borrow();
    match key {
        "Name" => Ok(Value::String(lua.create_string(name_bytes(&node))?)),
        "ClassName" => Ok(Value::String(lua.create_string(&node.class)?)),
        "Archivable" => Ok(Value::Boolean(node.archivable)),
        "Parent" => match node.parent.upgrade() { Some(parent) => Ok(Value::UserData(wrapper(lua, parent)?)), None => Ok(Value::Nil) },
        "LinkedSource" if is_a(&node, "LuaSourceContainer") => Ok(Value::String(lua.create_string(&node.linked_source)?)),
        _ => {
            if let Some(child) = node.children.iter().find(|child| name_bytes(&child.borrow()) == key.as_bytes()) {
                return Ok(Value::UserData(wrapper(lua, child.clone())?));
            }
            Err(missing_member(&node,key))
        }
    }
}

pub(crate) fn missing_member(node: &Node, key: &str) -> Error {
    Error::lua_string(format!("{key} is not a valid member of {} \"{}\"",node.class,node.name))
}
pub(crate) fn method_class(name: &str) -> Option<&'static str> {
    match name {
        "GetService" | "FindService" => Some("ServiceProvider"),
        "IsA" | "GetChildren" | "GetDescendants" | "FindFirstChild" | "FindFirstChildOfClass" | "FindFirstChildWhichIsA" |
        "FindFirstAncestor" | "FindFirstAncestorOfClass" | "FindFirstAncestorWhichIsA" |
        "GetFullName" | "GetActor" | "IsDescendantOf" | "Destroy" | "ClearAllChildren" |
        "GetAttribute" | "GetAttributes" | "SetAttribute" => Some("Instance"),
        _ => crate::model::method_class(name),
    }
}
pub(crate) fn validate_method(node: &Node, name: &str) -> Result<()> {
    if method_class(name).is_some_and(|class|class.split('|').any(|class|is_a(node,class))){Ok(())}else{Err(missing_member(node,name))}
}
fn argument_error(error:Error)->Error{
    fn cast(error:&Error)->Option<String>{match error{
        Error::FromLuaConversionError{from,to,..}=>{
            let from=match *from{"table"=>"Array","boolean"=>"bool",other=>other};
            let to=match to.as_str(){"String"|"LuaString"=>"string","AnyUserData"=>"Instance","f32"|"f64"=>"float","bool"=>"bool",other=>other};
            Some(format!("Unable to cast {from} to {to}"))
        },_=>None,
    }}
    match &error{
        Error::BadArgument{pos,cause,..}=>{
            if matches!(cause.as_ref(),Error::FromLuaConversionError{from:"nil",..}){Error::lua_string(format!("Argument {pos} missing or nil"))}
            else if let Some(message)=cast(cause){Error::lua_string(message)}else{error}
        },_=>if let Some(message)=cast(&error){Error::lua_string(message)}else{error},
    }
}
fn receiver(values:&mut MultiValue,name:&str)->Result<Instance>{
    if let Some(Value::UserData(value))=values.pop_front(){if let Ok(instance)=value.borrow::<Instance>(){return Ok(instance.clone())}}
    Err(Error::lua_string(format!("Expected ':' not '.' calling member function {name}")))
}
fn property_key(lua:&Lua,value:Value)->Result<String>{
    let key=lua.coerce_string(value)?.ok_or_else(||Error::lua_string("invalid argument #2 (string expected)"))?;
    key.to_str().map(|key|key.to_string()).map_err(|_|Error::lua_string("invalid property name"))
}
unsafe extern "C-unwind" fn invalid_property_key(state:*mut ffi::lua_State)->std::ffi::c_int{
    unsafe{ffi::luaL_typeerror(state,2,c"string".as_ptr())}
}
pub(crate) unsafe extern "C-unwind" fn invalid_instance(state:*mut ffi::lua_State)->std::ffi::c_int{
    unsafe{ffi::luaL_typeerror(state,1,c"Instance".as_ptr())}
}
pub(crate) fn has_instance_self(values:&MultiValue)->bool{matches!(values.front(),Some(Value::UserData(value))if value.is::<Instance>())}
unsafe extern "C-unwind" fn raw_index(state:*mut ffi::lua_State)->std::ffi::c_int{unsafe{
    Lua::invoke_native_callback(state,|lua,mut values|{
        if !has_instance_self(&values){return Ok(mlua::state::NativeCall::Dispatch(invalid_instance))}
        if !matches!(values.get(1),Some(Value::String(_)|Value::Number(_)|Value::Integer(_))){return Ok(mlua::state::NativeCall::Dispatch(invalid_property_key))}
        let instance=receiver(&mut values,"__index")?;
        let raw=lua.coerce_string(values.pop_front().unwrap_or(Value::Nil))?.ok_or_else(||Error::lua_string("invalid property name"))?;
        let key=match raw.to_str(){Ok(key)=>key.to_string(),Err(_)=>{
            let child=find_child(&instance.0,raw.as_bytes(),false).ok_or_else(||missing_member(&instance.0.borrow(),&crate::console::display_bytes(&raw.as_bytes(),512)))?;
            return Ok(mlua::state::NativeCall::Return(MultiValue::from_vec(vec![Value::UserData(wrapper(lua,child)?)])))
        }};
        let method:Value=lua.named_registry_value::<Table>(METHODS)?.raw_get(key.as_str())?;
        let value=if !method.is_nil()&&validate_method(&instance.0.borrow(),&key).is_ok(){method}else{read_property(lua,&instance,&key)?};
        Ok(mlua::state::NativeCall::Return(MultiValue::from_vec(vec![value])))
    })
}}
unsafe extern "C-unwind" fn raw_newindex(state:*mut ffi::lua_State)->std::ffi::c_int{unsafe{
    let callback=ffi::lua_tocfunction(state,ffi::lua_upvalueindex(2)).expect("native newindex dispatcher");
    Lua::invoke_native_callback(state,|_,values|{
        if !has_instance_self(&values){return Ok(mlua::state::NativeCall::Dispatch(invalid_instance))}
        if !matches!(values.get(1),Some(Value::String(_)|Value::Number(_)|Value::Integer(_))){return Ok(mlua::state::NativeCall::Dispatch(invalid_property_key))}
        Ok(mlua::state::NativeCall::Dispatch(callback))
    })
}}
pub(crate) fn register_method<M,A,R,F>(methods:&mut M,name:&'static str,callback:F)
where M:UserDataMethods<Instance>,A:mlua::FromLuaMulti,R:mlua::IntoLuaMulti,
      F:Fn(&Lua,&Instance,A)->Result<R>+mlua::MaybeSend+'static {
    methods.add_function(name,move|lua,mut values:MultiValue|{
        let this=receiver(&mut values,name)?;validate_method(&this.0.borrow(),name)?;
        let args=A::from_lua_args(values,1,Some(name),lua).map_err(argument_error)?;
        callback(lua,&this,args).map_err(|error|if matches!(error,Error::UserDataTypeMismatch){Error::lua_string("Unable to cast userdata to Instance")}else{error})
    });
}

impl UserData for Instance {
    fn register(registry: &mut UserDataRegistry<Self>) {
        registry.enable_namecall();
        registry.add_meta_field("__type", "Instance");
        Self::add_methods(registry);
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::Index, |lua, this, key: String| {
            read_property(lua, this, &key)
        });
        methods.add_meta_function(MetaMethod::NewIndex, |lua, mut values:MultiValue| {
            let this=receiver(&mut values,"__newindex")?;let key=property_key(lua,values.pop_front().unwrap_or(Value::Nil))?;let value=values.pop_front().unwrap_or(Value::Nil);
            if key == "Parent" {
                let parent = match value {
                    Value::Nil => None,
                    Value::UserData(value) => Some(value.borrow::<Instance>().map_err(|_|Error::lua_string("Parent must be an Instance or nil"))?.0.clone()),
                    _ => return Err(Error::lua_string("Parent must be an Instance or nil")),
                };
                return crate::model::parent(lua, &this.0, parent);
            }
            if crate::model::write_property(lua, &this.0, &key, &value)? { return Ok(()); }
            let mut node = this.0.borrow_mut();
            match (key.as_str(), value) {
                ("Name", Value::String(value)) => { set_name(&mut node, &value.as_bytes()); },
                ("Archivable", Value::Boolean(value)) => node.archivable = value,
                ("LinkedSource", Value::String(value)) if is_a(&node, "LuaSourceContainer") => node.linked_source = value.to_str()?.to_string(),
                ("ClassName", _) => return Err(Error::lua_string("Unable to assign property ClassName. Property is read only")),
                _ => return Err(Error::lua_string(format!("Unsupported property or invalid value: {}.{key}", node.class))),
            }
            Ok(())
        });
        methods.add_meta_method(MetaMethod::ToString, |lua, this, ()| lua.create_string(name_bytes(&this.0.borrow())));
        methods.add_meta_method(MetaMethod::Eq, |_, this, other: AnyUserData| {
            Ok(other.borrow::<Instance>().is_ok_and(|other| std::ptr::eq(this, &*other)))
        });
        register_method(methods,"IsA", |_, this, class: Value| {
            match class {
                Value::String(class) => Ok(is_a(&this.0.borrow(), &class.to_string_lossy())),
                _ => Ok(false),
            }
        });
        register_method(methods,"GetChildren", |lua, this, ()| instance_list(lua, this.0.borrow().children.clone()));
        register_method(methods,"GetDescendants", |lua, this, ()| instance_list(lua, descendants(&this.0)));
        register_method(methods,"FindFirstChild", |lua, this, (name, recursive): (mlua::LuaString, Option<bool>)| {
            find_child(&this.0, name.as_bytes(), recursive.unwrap_or(false)).map(|node| wrapper(lua, node)).transpose()
        });
        register_method(methods,"FindFirstChildOfClass", |lua, this, class: String| {
            this.0.borrow().children.iter().find(|child| child.borrow().class == class)
                .cloned().map(|node| wrapper(lua, node)).transpose()
        });
        register_method(methods,"FindFirstChildWhichIsA", |lua, this, (class, recursive): (String, Option<bool>)| {
            let nodes = if recursive.unwrap_or(false) { descendants(&this.0) } else { this.0.borrow().children.clone() };
            nodes.into_iter().find(|node| is_a(&node.borrow(), &class)).map(|node| wrapper(lua, node)).transpose()
        });
        for method in ["FindFirstAncestor", "FindFirstAncestorOfClass", "FindFirstAncestorWhichIsA"] {
            register_method(methods,method,move|lua,this,query:mlua::LuaString| {
                let mut ancestor=this.0.borrow().parent.upgrade();
                while let Some(node)=ancestor {
                    let found={let n=node.borrow();match method {"FindFirstAncestor"=>name_bytes(&n)==query.as_bytes().as_ref(),"FindFirstAncestorOfClass"=>n.class.as_bytes()==query.as_bytes().as_ref(),_=>query.to_str().is_ok_and(|query|is_a(&n,&query))}};
                    if found{return wrapper(lua,node).map(Some)}
                    ancestor=node.borrow().parent.upgrade();
                }
                Ok(None)
            });
        }
        register_method(methods,"GetFullName", |lua, this, ()| lua.create_string(full_name_bytes(&this.0)));
        register_method(methods,"GetActor", |lua, this, ()| {
            let mut cursor = this.0.borrow().parent.upgrade();
            while let Some(node) = cursor {
                if node.borrow().class == "Actor" { return Ok(Some(wrapper(lua, node)?)); }
                cursor = node.borrow().parent.upgrade();
            }
            Ok(None)
        });
        register_method(methods,"IsDescendantOf", |_, this, other: AnyUserData| {
            let other = other.borrow::<Instance>()?.0.clone();
            let mut cursor = this.0.borrow().parent.upgrade();
            while let Some(node) = cursor {
                if Rc::ptr_eq(&node, &other) { return Ok(true); }
                cursor = node.borrow().parent.upgrade();
            }
            Ok(false)
        });
        register_method(methods,"GetService", |lua, this, name: String| {
            let name=name.split('\0').next().unwrap_or("");
            if this.0.borrow().class != "DataModel" { return Err(Error::runtime("GetService requires DataModel")); }
            if !class_info(name).is_some_and(|info|info.service){return Err(Error::lua_string(format!("'{name}' is not a valid Service name")))}
            if !SERVICES.contains(&name) { return Err(crate::model::unsupported(lua,&format!("service {name}"))); }
            let node = this.0.borrow().children.iter().find(|node|node.borrow().class==name).cloned().ok_or_else(|| Error::runtime("Service unavailable"))?;
            wrapper(lua, node)
        });
        register_method(methods,"FindService", |lua, this, name: String| {
            let name=name.split('\0').next().unwrap_or("");
            if this.0.borrow().class != "DataModel" { return Err(Error::runtime("FindService requires DataModel")); }
            if !class_info(name).is_some_and(|info|info.service){return Err(Error::lua_string(format!("'{name}' is not a valid Service name")))}
            let node=this.0.borrow().children.iter().find(|n|n.borrow().class==name).cloned();
            node.map(|node|wrapper(lua,node)).transpose()
        });
        register_method(methods,"Destroy", |lua, this, ()| crate::model::destroy(lua, &this.0));
        register_method(methods,"ClearAllChildren", |lua, this, ()| {
            let children = this.0.borrow().children.clone();
            for child in children { crate::model::destroy(lua, &child)?; }
            Ok(())
        });
        register_method(methods,"GetAttribute", |lua, this, name: String| {
            this.0.borrow().attributes.get(&name).map(|value| attribute_value(lua, value)).transpose().map(|v| v.unwrap_or(Value::Nil))
        });
        register_method(methods,"GetAttributes", |lua, this, ()| {
            let result = lua.create_table()?;
            for (key, value) in &this.0.borrow().attributes { result.raw_set(key.as_str(), attribute_value(lua, value)?)?; }
            Ok(result)
        });
        register_method(methods,"SetAttribute", |lua, this, (name, value): (String, Value)| {
            crate::model::guard_write(lua)?;
            if name.is_empty() { return Err(Error::runtime("Attribute name cannot be empty")); }
            let value = match value {
                Value::Nil => { let removed=this.0.borrow_mut().attributes.remove(&name); return if removed.is_some(){crate::model::attribute_changed(lua, &this.0, &name)}else{Ok(())}; }
                Value::Boolean(value) => Attribute::Boolean(value),
                Value::Integer(value) => Attribute::Number(value as f64),
                Value::Number(value) => Attribute::Number(value),
                Value::String(value) => Attribute::String(value.to_str()?.to_string()),
                _ => return Err(Error::runtime("Unsupported attribute type")),
            };
            if this.0.borrow().attributes.get(&name)==Some(&value){return Ok(())}
            this.0.borrow_mut().attributes.insert(name.clone(), value);
            crate::model::attribute_changed(lua, &this.0, &name)
        });
        crate::model::register_methods(methods);
    }
}

pub fn install(lua: &Lua) -> Result<()> {
    let cache = lua.create_table()?;
    let weak = lua.create_table()?;
    weak.set("__mode", "v")?;
    cache.set_metatable(Some(weak))?;
    lua.set_named_registry_value(CACHE, cache)?;
    let instance = lua.create_table()?;
    let constructor = lua.create_function(|lua, values:MultiValue| {
        let (class,parent)=<(String,Option<AnyUserData>)as mlua::FromLuaMulti>::from_lua_args(values,1,Some("new"),lua).map_err(argument_error)?;
        if class=="AnimationTrack" {return Err(Error::lua_string("Unable to create an Instance of type AnimationTrack"))}
        if class == "GCViewer" && crate::scheduler::current_origin(lua) == "executor" {
            if parent.is_some() { return Err(Error::lua_string("GCViewer is an emulator window and has no DataModel parent")); }
            return crate::inspector::create_viewer(lua);
        }
        if class_info(&class).is_some_and(|info|!info.creatable){return Err(Error::lua_string(format!("Unable to create an Instance of type \"{class}\"")))}
        let node = create_node(lua,&class)?;
        crate::model::created(lua, &node)?;
        if let Some(parent) = parent { crate::model::parent(lua, &node, Some(parent.borrow::<Instance>().map_err(|_|Error::lua_string("Parent must be an Instance or nil"))?.0.clone()))?; }
        wrapper(lua, node)
    })?;
    crate::native::name_function(lua, &constructor, c"new")?;
    instance.set("new", constructor)?;
    instance.set_readonly(true);
    lua.globals().set("Instance", instance)?;
    let game = new_node("DataModel")?;
    game.borrow_mut().name = "Game".into();
    for service in SERVICES { set_parent(&new_node(service)?, Some(game.clone()))?; }
    let workspace = find_child(&game, "Workspace", false).unwrap();
    let game = wrapper(lua, game)?;
    let native_methods = lua.create_table()?;
    for (name, debug_name) in [("IsA", c"IsA"), ("GetChildren", c"GetChildren"),
        ("GetDescendants", c"GetDescendants"), ("FindFirstChild", c"FindFirstChild"),
        ("FindFirstChildOfClass", c"FindFirstChildOfClass"), ("FindFirstChildWhichIsA",c"FindFirstChildWhichIsA"),
        ("FindFirstAncestor",c"FindFirstAncestor"), ("FindFirstAncestorOfClass",c"FindFirstAncestorOfClass"),
        ("FindFirstAncestorWhichIsA",c"FindFirstAncestorWhichIsA"), ("GetFullName", c"GetFullName"),
        ("GetActor", c"GetActor"), ("IsDescendantOf", c"IsDescendantOf"), ("GetService", c"GetService"), ("FindService",c"FindService"),
        ("Destroy", c"Destroy"), ("ClearAllChildren", c"ClearAllChildren"),
        ("GetAttribute", c"GetAttribute"), ("GetAttributes", c"GetAttributes"), ("SetAttribute", c"SetAttribute")] {
        let method = game.get::<Function>(name)?;
        crate::native::name_function(lua, &method, debug_name)?;
        native_methods.raw_set(name, method)?;
    }
    for (name, debug_name) in crate::model::method_names() {
        let method = game.get::<Function>(name)?;
        crate::native::name_function(lua, &method, debug_name)?;
        native_methods.raw_set(name, method)?;
    }
    crate::model::install_yielding_methods(lua, &game, &native_methods)?;
    crate::native::clear_function_name(lua, &game.metatable()?.get::<Function>("__namecall")?)?;
    lua.set_named_registry_value(METHODS,native_methods)?;
    let index:Function=unsafe{lua.exec_raw((),|state|ffi::lua_pushcfunction(state,raw_index))?};
    let original:Function=game.metatable()?.get("__newindex")?;
    let newindex:Function=unsafe{lua.exec_raw(original,|state|{
        ffi::lua_getupvalue(state,1,1);ffi::lua_pushvalue(state,1);
        ffi::lua_pushcclosurek(state,raw_newindex,std::ptr::null(),2,None);ffi::lua_remove(state,1);
    })?};
    unsafe {
        lua.exec_raw::<()>((game.clone(), index, newindex), |state| {
            ffi::lua_getmetatable(state, 1);
            ffi::lua_pushvalue(state, 2);
            ffi::lua_rawsetfield(state, -2, c"__index".as_ptr());
            ffi::lua_pushvalue(state, 3);
            ffi::lua_rawsetfield(state, -2, c"__newindex".as_ptr());
            ffi::lua_pushstring(state,c"The metatable is locked".as_ptr());
            ffi::lua_rawsetfield(state,-2,c"__metatable".as_ptr());
            ffi::lua_settop(state, 0);
        })?;
    }
    lua.globals().set("game", game)?;
    lua.globals().set("workspace", wrapper(lua, workspace)?)?;
    Ok(())
}

#[cfg(all(test, emulator_local_tests))]
#[path = "../local_tests/instance.rs"]
mod tests;
