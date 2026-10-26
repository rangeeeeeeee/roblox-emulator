use crate::instance::{self, Attribute, Instance, Link};
use mlua::{AnyUserData, Error, Function, IntoLua, Lua, LuaSerdeExt, MultiValue, Result, Table, Thread, UserDataMethods, Value, ffi, state::NativeCall};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use std::{cell::{Cell, RefCell}, collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque}, ffi::CStr, rc::{Rc, Weak}};

#[derive(Default)]
struct Model {
    nodes: HashMap<String, Weak<RefCell<instance::Node>>>,
    changes: BTreeMap<String, Change>,
    sequence: u64,
    requests: u64,
    applying: bool,
    realm: String,
    side: String,
    client: String,
    actor: String,
    studio: bool,
    pending: HashMap<String, Thread>,
    modules: HashMap<String, Module>,
    logs: VecDeque<(String, Vec<u8>, f64)>,
    log_bytes: usize,
    children_waiting: Vec<ChildWait>,
    invokes_waiting: Vec<(String, Thread, MultiValue)>,
    unsupported: BTreeMap<String, u64>,
    assets: HashMap<String, AssetFetch>,
    pending_assets: usize,
    actions: HashMap<String, BoundAction>,
    action_order: u64,
}
struct BoundAction { _callback:Function, inputs:Vec<Value>, priority:i64, order:u64 }
#[derive(Clone, Copy, PartialEq)]
enum AssetFetch { Loading, Failure }
#[derive(Clone)]
struct AssetResource { key: String, content: String }
struct Change { node: Link, keys: Option<BTreeMap<&'static str, BTreeSet<String>>> }
struct ChildWait { parent: String, name: String, thread: Thread, deadline: Option<f64> }
enum Module { Loading { owner: Thread, waiters: Vec<Thread> }, Ready(Value), Failed(Value) }
type State = Rc<RefCell<Model>>;
#[derive(Clone)]
pub(crate) struct MessageSink(pub Rc<dyn Fn(String,Json)->Result<()>>);
const ENVS: &str = "emulator.native.environments";
const ENV_OWNERS: &str = "emulator.native.environment.owners";
const BASE_ENV: &str = "emulator.native.base.environment";
const MAX_NODES: usize = 100_000;
const MAX_SOURCE: usize = 8 * 1024 * 1024;
const MAX_WAITERS: usize = 10_000;
const MAX_ASSETS: usize = 4096;

fn state(lua: &Lua) -> Option<State> { lua.app_data_ref::<State>().map(|s| s.clone()) }
fn required(lua: &Lua) -> Result<State> { state(lua).ok_or_else(|| Error::runtime("UnsupportedApi: model runtime not installed")) }
fn lookup(lua: &Lua, id: &str) -> Option<Link> { state(lua)?.borrow().nodes.get(id)?.upgrade() }
fn now(lua: &Lua) -> f64 { crate::internal::<Table>(lua,"__rt").and_then(|t| t.get("now")).unwrap_or(0.0) }
fn reserve_node(state: &mut Model) -> Result<()> {
    if state.nodes.len() >= MAX_NODES { state.nodes.retain(|_, node| node.strong_count() > 0); }
    if state.nodes.len() >= MAX_NODES { return Err(Error::runtime("BudgetExceeded: live Instance count")); }
    Ok(())
}
fn reserve_waiter(state: &mut Model) -> Result<()> {
    let alive=|thread:&Thread|!thread.is_finished()&&!thread.is_error();
    state.pending.retain(|_,thread|alive(thread));
    state.children_waiting.retain(|wait|alive(&wait.thread));
    state.invokes_waiting.retain(|(_,thread,_)|alive(thread));
    let mut count=state.pending.len()+state.children_waiting.len()+state.invokes_waiting.len();
    for module in state.modules.values_mut(){if let Module::Loading{waiters,..}=module{waiters.retain(alive);count+=waiters.len();}}
    if count>=MAX_WAITERS{return Err(Error::runtime("BudgetExceeded: pending Instance waiters"))}
    Ok(())
}
pub(crate) fn unsupported(lua: &Lua, api: &str) -> Error {
    if let Some(state) = state(lua) { *state.borrow_mut().unsupported.entry(api.into()).or_default() += 1; }
    Error::runtime(format!("UnsupportedApi: {api}"))
}
pub fn unsupported_calls(lua: &Lua) -> Result<BTreeMap<String, u64>> { Ok(required(lua)?.borrow().unsupported.clone()) }
pub(crate) fn calling_script(lua:&Lua)->Result<Value>{
    lua.named_registry_value::<Table>(ENV_OWNERS)?.raw_get(lua.globals())
}
pub(crate) fn scripts(lua:&Lua,loaded_only:bool)->Result<Table>{
    let state=required(lua)?;
    let nodes={let state=state.borrow();state.nodes.iter().filter_map(|(id,node)|{
        let node=node.upgrade()?;
        let present=if loaded_only{matches!(state.modules.get(id),Some(Module::Ready(_)))}
            else{matches!(node.borrow().class.as_str(),"Script"|"LocalScript"|"ModuleScript")};
        present.then_some(node)
    }).collect()};
    instance::instance_list(lua,nodes)
}
pub(crate) fn guard_write(lua: &Lua) -> Result<()> {
    if state(lua).is_some() && crate::scheduler::current_mode(lua) == "parallel" {
        return Err(Error::lua_string("Instance mutation is unavailable in parallel"));
    }
    Ok(())
}
fn mark(lua: &Lua, node: &Link) {
    if let Some(state) = state(lua) {
        let mut state = state.borrow_mut();
        if !state.applying { state.changes.insert(node.borrow().id.clone(), Change{node:node.clone(),keys:None}); }
    }
}
fn mark_key(lua:&Lua,node:&Link,key:&str,attribute:bool){
    if let Some(state)=state(lua){
        let mut state=state.borrow_mut();if state.applying{return}
        if !attribute&&matches!(key,"OnInvoke"|"OnServerInvoke"|"OnClientInvoke"){return}
        let (group,key)=if attribute{("attributes",key)}else{match key{
            "Name"=>("fields","name"),"Parent"=>("fields","parent"),"LinkedSource"=>("fields","linkedSource"),"Destroyed"=>("fields","destroyed"),_=>("properties",key),
        }};
        let change=state.changes.entry(node.borrow().id.clone()).or_insert_with(||Change{node:node.clone(),keys:Some(BTreeMap::new())});
        if let Some(keys)=&mut change.keys{let keys=keys.entry(group).or_default();keys.insert(key.into());if !attribute{match key{"Enabled"=>{keys.insert("Disabled".into());},"Disabled"=>{keys.insert("Enabled".into());},_=>{}}}}
    }
}
fn value(lua: &Lua, node: Link) -> Result<Value> { Ok(Value::UserData(instance::wrapper(lua, node)?)) }
fn args(items: impl IntoIterator<Item=Value>) -> MultiValue { MultiValue::from_iter(items) }
fn str_value(lua: &Lua, text: &str) -> Result<Value> { Ok(Value::String(lua.create_string(text)?)) }
fn signal(lua: &Lua, node: &Link, name: &str) -> Result<AnyUserData> {
    if let Some(event) = node.borrow().signals.get(name) { return Ok(event.clone()); }
    let label=if let Some(name)=name.strip_prefix("property:"){format!("{name}Changed")}else if let Some(name)=name.strip_prefix("attribute:"){format!("{name}AttributeChanged")}else{name.into()};
    let event = crate::scheduler::new_named_signal(lua,&label)?;
    node.borrow_mut().signals.insert(name.into(), event.clone());
    Ok(event)
}
fn fire(lua: &Lua, node: &Link, name: &str, args: MultiValue) -> Result<()> {
    let event = node.borrow().signals.get(name).cloned();
    if let Some(event) = event { crate::scheduler::fire_signal(lua, &event, args)?; }
    Ok(())
}
fn changed(lua: &Lua, node: &Link, key: &str) -> Result<()> {
    mark_key(lua,node,key,false);
    let changed_value = if key == "Value" && instance::is_a(&node.borrow(), "ValueBase") {
        node.borrow().properties.get(key).cloned().unwrap_or(Value::Nil)
    } else { str_value(lua, key)? };
    fire(lua, node, "Changed", args([changed_value]))?;
    fire(lua, node, &format!("property:{key}"), MultiValue::new())
}
pub(crate) fn attribute_changed(lua: &Lua, node: &Link, name: &str) -> Result<()> {
    mark_key(lua,node,name,true);
    fire(lua, node, "AttributeChanged", args([str_value(lua, name)?]))?;
    fire(lua, node, &format!("attribute:{name}"), MultiValue::new())
}

pub(crate) fn created(lua: &Lua, node: &Link) -> Result<()> {
    guard_write(lua)?;
    if let Some(state) = state(lua) {
        let mut state = state.borrow_mut();
        reserve_node(&mut state)?;
        state.sequence += 1;
        let id = format!("{}:{}", state.realm, state.sequence);
        node.borrow_mut().id = id.clone();
        state.nodes.insert(id.clone(), Rc::downgrade(node));
        if !state.applying { state.changes.insert(id, Change{node:node.clone(),keys:None}); }
    }
    Ok(())
}

pub(crate) fn parent(lua: &Lua, node: &Link, target: Option<Link>) -> Result<()> {
    guard_write(lua)?;
    let previous = node.borrow().parent.upgrade();
    if previous.as_ref().zip(target.as_ref()).is_some_and(|(a,b)| Rc::ptr_eq(a,b)) || previous.is_none() && target.is_none() { return Ok(()); }
    let descendants = instance::descendants(node);
    instance::set_parent(node, target.clone())?;
    if let Some(old) = &previous { fire(lua, old, "ChildRemoved", args([value(lua,node.clone())?]))?; }
    if let Some(new) = &target { fire(lua, new, "ChildAdded", args([value(lua,node.clone())?]))?; }
    for (ancestor, event_name) in [(previous.clone(),"DescendantRemoving"),(target.clone(),"DescendantAdded")] {
        let mut cursor = ancestor;
        while let Some(parent) = cursor {
            for item in std::iter::once(node).chain(descendants.iter()) { fire(lua,&parent,event_name,args([value(lua,item.clone())?]))?; }
            cursor = parent.borrow().parent.upgrade();
        }
    }
    for item in std::iter::once(node).chain(descendants.iter()) {
        let parent = item.borrow().parent.upgrade().map(|n| value(lua,n)).transpose()?.unwrap_or(Value::Nil);
        fire(lua,item,"AncestryChanged",args([value(lua,item.clone())?,parent]))?;
    }
    changed(lua,node,"Parent")?;
    if state(lua).is_some(){poll_children(lua)?;}
    Ok(())
}

pub(crate) fn destroy(lua: &Lua, node: &Link) -> Result<()> {
    guard_write(lua)?;
    if node.borrow().destroyed { return Ok(()); }
    fire(lua,node,"Destroying",MultiValue::new())?;
    let children = node.borrow().children.clone();
    for child in children { destroy(lua,&child)?; }
    parent(lua,node,None)?;
    let events: Vec<_> = node.borrow().signals.values().cloned().collect();
    for event in events { crate::scheduler::disconnect_all(lua,&event)?; }
    let mut n = node.borrow_mut();
    n.signals.clear();
    n.callback_environments.clear();
    n.properties.retain(|_,v| !matches!(v,Value::Function(_)));
    n.destroyed = true;
    drop(n);
    mark_key(lua,node,"Destroyed",false);
    Ok(())
}

fn event_class(name: &str) -> Option<&'static str> {
    match name {
        "ChildAdded"|"ChildRemoved"|"DescendantAdded"|"DescendantRemoving"|"AncestryChanged"|"Changed"|"Destroying"|"AttributeChanged" => Some("Instance"),
        "Loaded" => Some("DataModel"), "Event" => Some("BindableEvent"), "OnServerEvent"|"OnClientEvent" => Some("BaseRemoteEvent"),
        "PlayerAdded"|"PlayerRemoving" => Some("Players"), "CharacterAdded"|"CharacterRemoving"|"Idled" => Some("Player"),
        "Heartbeat"|"Stepped"|"RenderStepped"|"PreSimulation"|"PostSimulation"|"PreRender" => Some("RunService"),
        "MessageOut" => Some("LogService"), "Died"|"StateChanged" => Some("Humanoid"),
        "Error" => Some("ScriptContext"), "AssetFetchFailed" => Some("ContentProvider"),
        "InputBegan"|"InputEnded"|"InputChanged"|"WindowFocused"|"WindowFocusReleased" => Some("UserInputService"),
        _=>None,
    }
}

fn default(lua:&Lua,node:&Link,key:&str)->Result<Option<Value>> {
    let n=node.borrow();
    let is=|class| instance::is_a(&n,class);
    let result=match key {
        "Enabled" if is("BaseScript")||is("LayerCollector") => Some(Value::Boolean(true)),
        "Disabled" if is("BaseScript") => Some(Value::Boolean(false)),
        "OnInvoke" if is("BindableFunction") => Some(Value::Nil),
        "OnServerInvoke"|"OnClientInvoke" if is("RemoteFunction")=>Some(Value::Nil),
        "PrimaryPart" if is("Model")=>Some(Value::Nil),
        "Character" if is("Player")=>Some(Value::Nil),
        "UserId"|"AccountAge" if is("Player")=>Some(Value::Integer(0)),
        "DisplayName" if is("Player")=>Some(str_value(lua,&n.name)?),
        "CurrentCamera" if is("Workspace")=>Some(Value::Nil),
        "ViewportSize" if is("Camera")=>Some(crate::types::Vector([1.0,1.0]).into_lua(lua)?),
        "NearPlaneZ" if is("Camera")=>Some(Value::Number(-0.5)),
        "FieldOfView" if is("Camera")=>Some(Value::Number(70.0)),
        "FieldOfViewMode" if is("Camera")=>crate::types::decode_value(lua,&json!({"tag":"EnumItem","enum":"FieldOfViewMode","name":"Vertical"}))?,
        "CFrame" if is("Camera")=>Some(crate::types::CFrame{p:crate::types::Vector([0.0;3]),r:[[1.0,0.0,0.0],[0.0,1.0,0.0],[0.0,0.0,1.0]]}.into_lua(lua)?),
        "Gravity" if is("Workspace")=>Some(Value::Number(196.2)),
        "PlaceId"|"GameId"|"PlaceVersion"|"CreatorId" if is("DataModel")=>Some(Value::Integer(0)),
        "JobId" if is("DataModel")=>Some(str_value(lua,"emulator")?),
        "Value" if is("StringValue")=>Some(str_value(lua,"")?),
        "Value" if is("BoolValue")=>Some(Value::Boolean(false)),
        "Value" if is("NumberValue")||is("IntValue")=>Some(Value::Number(0.0)),
        "Value" if is("ObjectValue")=>Some(Value::Nil),
        "Anchored"|"Massless" if is("BasePart")=>Some(Value::Boolean(false)),
        "CanCollide"|"CanTouch"|"CanQuery" if is("BasePart")=>Some(Value::Boolean(true)),
        "Transparency" if is("BasePart")=>Some(Value::Number(0.0)),
        "Health"|"MaxHealth" if is("Humanoid")=>Some(Value::Number(100.0)),
        "WalkSpeed" if is("Humanoid")=>Some(Value::Number(16.0)),
        "JumpPower" if is("Humanoid")=>Some(Value::Number(50.0)),
        "HipHeight" if is("Humanoid")=>Some(Value::Number(0.0)),
        "JumpHeight" if is("Humanoid")=>Some(Value::Number(7.2f32 as f64)),
        "UseJumpPower" if is("Humanoid")=>Some(Value::Boolean(true)),
        "Visible" if is("GuiObject")=>Some(Value::Boolean(true)),
        "Active" if is("GuiObject")=>Some(Value::Boolean(false)),
        "Size"|"Position" if is("GuiObject")=>Some(crate::types::UDim2::default().into_lua(lua)?),
        "BackgroundTransparency" if is("GuiObject")=>Some(Value::Number(0.0)),
        "Text" if is("TextLabel")||is("TextButton")=>Some(str_value(lua,"")?),
        "Image" if is("ImageLabel")||is("ImageButton")=>Some(str_value(lua,"")?),
        "IsLoaded" if is("ImageLabel")||is("ImageButton")=>Some(Value::Boolean(false)),
        "ImageTransparency" if is("ImageLabel")||is("ImageButton")=>Some(Value::Number(0.0)),
        "ZIndex" if is("GuiObject")=>Some(Value::Integer(1)),
        "ResetOnSpawn" if is("LayerCollector")=>Some(Value::Boolean(true)),
        "DisplayOrder" if is("ScreenGui")=>Some(Value::Integer(0)),
        "TouchEnabled"|"GamepadEnabled"|"VREnabled" if is("UserInputService")||is("VRService")=>Some(Value::Boolean(false)),
        "KeyboardEnabled"|"MouseEnabled" if is("UserInputService")=>Some(Value::Boolean(true)),
        "MenuIsOpen" if is("GuiService")=>Some(Value::Boolean(false)),
        "IsWindows" if is("GuiService")=>Some(Value::Boolean(cfg!(windows))),
        "MemoryTrackingEnabled" if is("Stats")=>Some(Value::Boolean(true)),
        "Brightness" if is("Lighting")=>Some(Value::Number(3.0)),
        "ClockTime" if is("Lighting")=>Some(Value::Number(14.5)),
        "FogStart"|"ExposureCompensation" if is("Lighting")=>Some(Value::Number(0.0)),
        "FogEnd" if is("Lighting")=>Some(Value::Number(100000.0)),
        "GlobalShadows" if is("Lighting")=>Some(Value::Boolean(true)),
        "Ambient"|"OutdoorAmbient" if is("Lighting")=>Some(crate::types::Color3([70.0/255.0;3]).into_lua(lua)?),
        "ChatVersion" if is("TextChatService")=>crate::types::decode_value(lua,&json!({"tag":"EnumItem","enum":"ChatVersion","name":"TextChatService"}))?,
        _=>None,
    };
    Ok(result)
}

pub(crate) fn read_property(lua:&Lua,node:&Link,key:&str)->Result<Option<Value>> {
    if state(lua).is_none(){return Ok(None)}
    if let Some(value)=node.borrow().properties.get(key){return Ok(Some(value.clone()))}
    if key=="RequestQueueSize"&&node.borrow().class=="ContentProvider" {return Ok(Some(Value::Integer(required(lua)?.borrow().pending_assets as i64)))}
    if key=="Source"&&instance::is_a(&node.borrow(),"LuaSourceContainer") {return Err(unsupported(lua,"LuaSourceContainer.Source runtime capability"))}
    if key=="LocalPlayer"&&node.borrow().class=="Players" {
        let s=required(lua)?; let s=s.borrow();
        return Ok(Some(if s.side=="client" {s.nodes.get(&s.client).and_then(Weak::upgrade).map(|n|value(lua,n)).transpose()?.unwrap_or(Value::Nil)}else{Value::Nil}));
    }
    if key=="DistributedGameTime"&&node.borrow().class=="Workspace"{return Ok(Some(Value::Number(now(lua))))}
    if key=="InstanceCount"&&node.borrow().class=="Stats"{return Ok(Some(Value::Integer(required(lua)?.borrow().nodes.values().filter(|node|node.strong_count()>0).count() as i64)))}
    if event_class(key).is_some_and(|class|instance::is_a(&node.borrow(),class)){return Ok(Some(Value::UserData(signal(lua,node,key)?)))}
    default(lua,node,key)
}

pub(crate) fn write_property(lua:&Lua,node:&Link,key:&str,new:&Value)->Result<bool> {
    if state(lua).is_none(){return Ok(false)}
    guard_write(lua)?;
    let previous=||->Result<Option<Value>>{let stored=node.borrow().properties.get(key).cloned();if stored.is_some(){Ok(stored)}else{default(lua,node,key)}};
    let same=|old:&Value,new:&Value|{
        if old==new{return true}
        // Compare native value types directly; never run a Lua __eq while assigning a property.
        if let(Value::UserData(a),Value::UserData(b))=(old,new){
            macro_rules! equal_native {($ty:ty)=>{if let(Ok(a),Ok(b))=(a.borrow::<$ty>(),b.borrow::<$ty>()){return *a==*b}}}
            equal_native!(crate::types::CFrame);equal_native!(crate::types::Color3);equal_native!(crate::types::Vector2);
            equal_native!(crate::types::UDim);equal_native!(crate::types::UDim2);
        }
        false
    };
    if node.borrow().class=="Camera"{
        match key{
            "ViewportSize"|"NearPlaneZ"=>return Err(Error::lua_string(format!("Unable to assign property {key}. Property is read only"))),
            "CFrame"=>if !matches!(new,Value::UserData(value)if value.is::<crate::types::CFrame>()){return Err(Error::lua_string("Unable to assign property CFrame. CoordinateFrame expected"))},
            "FieldOfViewMode"=>{
                let encoded=crate::types::encode_value(new)?;
                let mode=if let Some(encoded)=encoded.filter(|value|value["tag"]=="EnumItem"){
                    if encoded["enum"]!="FieldOfViewMode"{return Err(Error::lua_string(format!("Unable to assign property FieldOfViewMode. EnumItem of type FieldOfViewMode expected, got an EnumItem of type {}",encoded["enum"].as_str().unwrap_or(""))))}
                    new.clone()
                }else if matches!(new,Value::String(_)|Value::Number(_)|Value::Integer(_)){
                    let text=lua.coerce_string(new.clone())?.unwrap();let text=text.to_string_lossy();
                    let name=match text.as_str(){"Vertical"=>"Vertical","Diagonal"=>"Diagonal","MaxAxis"=>"MaxAxis",_=>match lua.coerce_number(new.clone())?{
                        Some(0.0)=>"Vertical",Some(1.0)=>"Diagonal",Some(2.0)=>"MaxAxis",_=>return Err(Error::lua_string(format!("Unable to assign property FieldOfViewMode. Invalid value {text} for enum FieldOfViewMode"))),
                    }};
                    crate::types::decode_value(lua,&json!({"tag":"EnumItem","enum":"FieldOfViewMode","name":name}))?.ok_or_else(||Error::runtime("FieldOfViewMode unavailable"))?
                }else{return Err(Error::lua_string(format!("Unable to assign property FieldOfViewMode. EnumItem, number, or string expected, got {}",new.type_name())))};
                if previous()?.as_ref().is_some_and(|old|same(old,&mode)){return Ok(true)}
                node.borrow_mut().properties.insert(key.into(),mode);changed(lua,node,key)?;return Ok(true)
            },
            "FieldOfView"=>{
                let fov=lua.coerce_number(new.clone())?.ok_or_else(||Error::lua_string("Unable to assign property FieldOfView. Number expected"))?;
                let fov=Value::Number((fov as f32).clamp(1.0,120.0)as f64);
                if previous()?.as_ref().is_some_and(|old|same(old,&fov)){return Ok(true)}
                node.borrow_mut().properties.insert(key.into(),fov);changed(lua,node,key)?;return Ok(true)
            },_=>{},
        }
    }
    match key {
        "Name"=>{if let Value::String(v)=new{let name=v.to_str()?;if node.borrow().name.as_str()==&*name{return Ok(true)}node.borrow_mut().name=name.to_string()}else{return Err(Error::lua_string("Name must be a string"))}},
        "Archivable"=>{if let Value::Boolean(v)=new{if node.borrow().archivable==*v{return Ok(true)}node.borrow_mut().archivable=*v}else{return Err(Error::lua_string("Archivable must be a boolean"))}},
        "LinkedSource" if instance::is_a(&node.borrow(),"LuaSourceContainer")=>{if let Value::String(v)=new{let source=v.to_str()?;if node.borrow().linked_source.as_str()==&*source{return Ok(true)}node.borrow_mut().linked_source=source.to_string()}else{return Err(Error::lua_string("LinkedSource must be a string"))}},
        _=>{
            let Some(previous)=previous()?else{return Ok(false)};
            // Callback assignment also captures the assigning script's environment, even when the function is reused.
            if !matches!(key,"OnInvoke"|"OnServerInvoke"|"OnClientInvoke")&&same(&previous,new){return Ok(true)}
            node.borrow_mut().properties.insert(key.into(),new.clone());
            if matches!(key,"OnInvoke"|"OnServerInvoke"|"OnClientInvoke"){
                if matches!(new,Value::Function(_)){node.borrow_mut().callback_environments.insert(key.into(),lua.globals());}
                else{node.borrow_mut().callback_environments.remove(key);}
            }
            if key=="Enabled"{node.borrow_mut().properties.insert("Disabled".into(),Value::Boolean(!matches!(new,Value::Boolean(true))));}
            if key=="Disabled"{node.borrow_mut().properties.insert("Enabled".into(),Value::Boolean(!matches!(new,Value::Boolean(true))));}
        }
    }
    changed(lua,node,key)?;
    if key=="OnInvoke" { drain_invokes(lua,node)?; }
    Ok(true)
}

fn encode(lua:&Lua,v:Value,active:&mut HashSet<usize>)->Result<Json>{
    Ok(match v {
        Value::Nil=>json!({"tag":"nil"}), Value::Boolean(v)=>json!({"tag":"boolean","value":v}),
        Value::Integer(v)=>json!({"tag":"number","value":v}),Value::Number(v) if v.is_finite()=>json!({"tag":"number","value":v}),
        Value::Number(v)=>json!({"tag":"number","special":if v.is_nan(){"nan"}else if v>0.0{"+inf"}else{"-inf"}}),
        Value::String(v)=>match v.to_str(){Ok(text)=>json!({"tag":"string","value":text.as_ref()}),Err(_)=>json!({"tag":"string","bytes":v.as_bytes().as_ref()})},
        Value::Buffer(v)=>json!({"tag":"buffer","bytes":v.to_vec()}),
        Value::UserData(ref u) if u.is::<Instance>()=>json!({"tag":"instance","id":u.borrow::<Instance>()?.0.borrow().id}),
        Value::Table(t)=>{
            let id=t.to_pointer() as usize;
            if !active.insert(id){return Err(Error::lua_string("Cannot serialize cyclic table"))}
            let mut entries=Vec::new();
            for pair in t.pairs::<Value,Value>() {let(k,v)=pair?;if !matches!(k,Value::String(_)|Value::Number(_)|Value::Integer(_)|Value::Boolean(_)){return Err(Error::lua_string("Unsupported remote table key"))} entries.push(json!({"key":encode(lua,k,active)?,"value":encode(lua,v,active)?}));}
            active.remove(&id);json!({"tag":"table","entries":entries})
        }
        other=>crate::types::encode_value(&other)?.ok_or_else(||Error::lua_string(format!("Cannot serialize {} across VM boundary",other.type_name())))?,
    })
}
fn decode(lua:&Lua,v:&Json)->Result<Value>{
    Ok(match v["tag"].as_str().unwrap_or("") {
        "nil"=>Value::Nil,"boolean"=>Value::Boolean(v["value"].as_bool().unwrap_or(false)),
        "number"=>Value::Number(match v["special"].as_str(){Some("nan")=>f64::NAN,Some("+inf")=>f64::INFINITY,Some("-inf")=>f64::NEG_INFINITY,_=>v["value"].as_f64().unwrap_or(0.0)}),
        "string"=>if let Some(bytes)=v.get("bytes"){Value::String(lua.create_string(serde_json::from_value::<Vec<u8>>(bytes.clone()).map_err(Error::external)?)?)}else{str_value(lua,v["value"].as_str().unwrap_or(""))?},
        "buffer"=>Value::Buffer(lua.create_buffer(serde_json::from_value::<Vec<u8>>(v["bytes"].clone()).map_err(Error::external)?)?),
        "instance"=>lookup(lua,v["id"].as_str().unwrap_or("")).map(|n|value(lua,n)).transpose()?.unwrap_or(Value::Nil),
        "table"=>{let table=lua.create_table()?;for entry in v["entries"].as_array().ok_or_else(||Error::runtime("Invalid wire entries"))?{table.raw_set(decode(lua,&entry["key"])?,decode(lua,&entry["value"])?)?;}Value::Table(table)},
        _=>crate::types::decode_value(lua,v)?.ok_or_else(||Error::runtime("Unknown wire tag"))?,
    })
}
fn encode_args(lua:&Lua,args:MultiValue)->Result<Json>{let n=args.len();let mut values=Vec::new();for v in args {values.push(encode(lua,v,&mut HashSet::new())?)}Ok(json!({"n":n,"values":values}))}
fn decode_args(lua:&Lua,args:&Json)->Result<MultiValue>{let n=args["n"].as_u64().unwrap_or(0)as usize;if n>100000{return Err(Error::runtime("Wire argument limit"))}let mut values=MultiValue::new();for i in 0..n{values.push_back(decode(lua,&args["values"][i])?)}Ok(values)}
fn emit(lua:&Lua,kind:&str,payload:Json)->Result<()>{
    // The host wire representation must not be materialized in the Lua heap:
    // doing so changes observable gcinfo() and adds a native callback frame.
    let sink=lua.app_data_ref::<MessageSink>().map(|sink|sink.0.clone());
    if let Some(sink)=sink {sink(kind.into(),payload)}
    else {crate::internal::<Function>(lua,"__host_emit")?.call((kind,lua.to_value(&payload)?))}
}

fn env(lua:&Lua,node:&Link)->Result<Table>{
    let table:Table=lua.named_registry_value(ENVS)?;let id=node.borrow().id.clone();
    if let Value::Table(e)=table.raw_get(id.as_str())?{return Ok(e)}
    let base:Table=lua.named_registry_value(BASE_ENV)?;
    let e=lua.create_table()?;e.raw_set("script",instance::wrapper(lua,node.clone())?)?;
    for key in ["_G","shared"]{e.raw_set(key,base.raw_get::<Value>(key)?)?;}
    let mt=lua.create_table()?;mt.raw_set("__index",base)?;e.set_metatable(Some(mt))?;
    e.set_safeenv(true);
    lua.named_registry_value::<Table>(ENV_OWNERS)?.raw_set(e.clone(),instance::wrapper(lua,node.clone())?)?;
    table.raw_set(id,e.clone())?;Ok(e)
}
fn thread_environment(lua:&Lua,thread:&Thread,node:&Link)->Result<()>{crate::scheduler::set_thread_environment(lua,thread,&env(lua,node)?)}
fn callback_thread(lua:&Lua,node:&Link,key:&str,callback:Function)->Result<Thread>{
    let thread=lua.create_thread(callback)?;
    if let Some(environment)=node.borrow().callback_environments.get(key).cloned(){crate::scheduler::set_thread_environment(lua,&thread,&environment)?;}
    Ok(thread)
}
fn compile(lua:&Lua,node:&Link)->Result<Function>{let n=node.borrow();if n.source.len()>MAX_SOURCE{return Err(Error::runtime("BudgetExceeded: source size"))}lua.load(&n.source).set_name(format!("={}",instance::full_name(node))).set_environment(env(lua,node)?).into_function()}
fn clone_node(lua:&Lua,node:&Link)->Result<Option<Link>>{
    if !node.borrow().archivable{return Ok(None)}
    let n=node.borrow();let result=instance::create_node(lua,&n.class)?;created(lua,&result)?;
    {let mut r=result.borrow_mut();r.name=n.name.clone();r.source=n.source.clone();r.linked_source=n.linked_source.clone();r.attributes=n.attributes.clone();r.properties=n.properties.clone();r.properties.retain(|_,v|!matches!(v,Value::Function(_)));}
    let children=n.children.clone();drop(n);
    for child in children {if let Some(copy)=clone_node(lua,&child)?{parent(lua,&copy,Some(result.clone()))?;}}
    Ok(Some(result))
}

fn reject_cycles(value:&Value)->Result<()>{
    enum Visit{Table(Table),Leave(usize)}
    let Value::Table(table)=value else{return Ok(())};
    let mut stack=vec![Visit::Table(table.clone())];let mut active=HashSet::new();let mut inspected=0;
    while let Some(visit)=stack.pop(){match visit{
        Visit::Leave(id)=>{active.remove(&id);},
        Visit::Table(table)=>{
            let id=table.to_pointer()as usize;if !active.insert(id){return Err(Error::lua_string("tables cannot be cyclic"))}
            stack.push(Visit::Leave(id));
            for entry in table.pairs::<Value,Value>(){
                inspected+=1;if inspected>100_000{return Err(Error::runtime("BudgetExceeded: table traversal"))}
                let(key,value)=entry?;for value in [key,value]{if let Value::Table(table)=value{stack.push(Visit::Table(table));}}
            }
        },
    }}Ok(())
}
fn asset_cast(value:&Value,target:&str)->Error {
    let kind=match value { Value::Boolean(_)=>"bool",Value::Integer(_)|Value::Number(_)=>"double",Value::Table(_)=>"Array",Value::Function(_)=>"Function",Value::UserData(_)=>"Object",_=>value.type_name() };
    Error::lua_string(format!("Unable to cast {kind} to {target}"))
}
fn asset_resource(value:Value)->Result<AssetResource>{
    reject_cycles(&value)?;
    if value.is_nil(){return Err(Error::lua_string("Argument 1 missing or nil"))}
    let Value::String(value)=value else{return Err(asset_cast(&value,"ContentId"))};
    let content=value.to_str()?.to_string();
    if content.len()>2048{return Err(Error::runtime("BudgetExceeded: asset identifier length"))}
    let numeric=content.strip_prefix("rbxassetid://").unwrap_or(&content);
    let key=numeric.parse::<u64>().map(|id|id.to_string()).unwrap_or_else(|_|content.clone());
    Ok(AssetResource{key,content})
}
fn content_id(_lua:&Lua,value:Value)->Result<String>{Ok(asset_resource(value)?.key)}
fn asset_enum(lua:&Lua,name:&str)->Result<Value>{
    crate::types::decode_value(lua,&json!({"tag":"EnumItem","enum":"AssetFetchStatus","name":name}))?.ok_or_else(||Error::runtime("AssetFetchStatus unavailable"))
}
fn asset_status(lua:&Lua,key:&str)->Result<Value>{
    let name=match required(lua)?.borrow().assets.get(key){Some(AssetFetch::Loading)=>"Loading",Some(AssetFetch::Failure)=>"Failure",None if key.is_empty()=>"Failure",None=>"None"};
    asset_enum(lua,name)
}
fn offline_asset(lua:&Lua,resource:&AssetResource)->Result<bool>{
    let state=required(lua)?;let mut model=state.borrow_mut();
    if model.assets.get(&resource.key)==Some(&AssetFetch::Failure){return Ok(false)}
    if !model.assets.contains_key(&resource.key)&&model.assets.len()>=MAX_ASSETS{return Err(Error::runtime("BudgetExceeded: offline asset cache"))}
    model.assets.insert(resource.key.clone(),AssetFetch::Loading);
    // There are no configured asset bytes or network provider. Cache the actual
    // failed offline lookup; never manufacture a successful fetch.
    model.assets.insert(resource.key.clone(),AssetFetch::Failure);
    Ok(true)
}
fn asset_failed(lua:&Lua,node:&Link,resource:&AssetResource)->Result<()>{
    fire(lua,node,&format!("{}AssetFetchStatusChanged",resource.key),args([asset_enum(lua,"Failure")?]))?;
    fire(lua,node,"AssetFetchFailed",args([str_value(lua,&resource.content)?]))?;
    if let Ok(logger)=crate::internal::<Function>(lua,"__host_log"){
        logger.call::<()>(("error",format!("Failed to load {}: resource is unavailable in the offline emulator",resource.content)))?;
    }
    Ok(())
}
fn preload_resources(input:Value)->Result<Vec<AssetResource>>{
    reject_cycles(&input)?;
    if input.is_nil(){return Err(Error::lua_string("Argument 1 missing or nil"))}
    let Value::Table(table)=input else{return Err(asset_cast(&input,"Array"))};
    let length=table.raw_len();
    if length>MAX_ASSETS{return Err(Error::runtime("BudgetExceeded: preload asset count"))}
    if length==0&&table.pairs::<Value,Value>().next().transpose()?.is_some(){return Err(Error::lua_string("Unable to cast Dictionary to Array"))}
    let mut resources=Vec::new();let mut inspected=0;
    for index in 1..=length{
        let item:Value=table.raw_get(index)?;
        if let Value::UserData(value)=&item{if let Ok(instance)=value.borrow::<Instance>(){
            let mut nodes=vec![instance.0.clone()];
            while let Some(node)=nodes.pop(){
                inspected+=1;if inspected>MAX_ASSETS{return Err(Error::runtime("BudgetExceeded: preload Instance count"))}
                let node=node.borrow();
                for property in ["Image","Texture","TextureID","MeshId","SoundId","AnimationId","ColorMapContent"]{
                    if let Some(Value::String(value))=node.properties.get(property){if !value.as_bytes().is_empty(){resources.push(asset_resource(Value::String(value.clone()))?);}}
                }
                if nodes.len()+node.children.len()>MAX_ASSETS{return Err(Error::runtime("BudgetExceeded: preload Instance count"))}
                nodes.extend(node.children.iter().rev().cloned());
                if resources.len()>MAX_ASSETS{return Err(Error::runtime("BudgetExceeded: preload asset count"))}
            }
            continue
        }}
        resources.push(asset_resource(item)?);
        if resources.len()>MAX_ASSETS{return Err(Error::runtime("BudgetExceeded: preload asset count"))}
    }
    Ok(resources)
}
fn preload_async(lua:&Lua,mut values:MultiValue)->Result<NativeCall>{
    let node=object_arg(&mut values)?;instance::validate_method(&node.borrow(),"PreloadAsync")?;
    let resources=preload_resources(values.pop_front().unwrap_or(Value::Nil))?;
    let callback=match values.pop_front(){None|Some(Value::Nil)=>None,Some(Value::Function(function))=>Some(function),Some(value)=>return Err(asset_cast(&value,"Function"))};
    if resources.is_empty(){return Ok(NativeCall::Return(MultiValue::new()))}
    guard_write(lua)?;
    let count=resources.len();let state=required(lua)?;
    if state.borrow().pending_assets.saturating_add(count)>MAX_WAITERS{return Err(Error::runtime("BudgetExceeded: pending asset requests"))}
    crate::scheduler::park_current(lua)?;
    let caller=lua.current_thread();let environment=lua.globals();
    let work=lua.create_function(move|lua,()|{
        let mut completed=0;
        let result=(||->Result<Vec<AssetResource>>{
            let mut failures=Vec::new();
            for resource in &resources{
                if offline_asset(lua,resource)?{failures.push(resource.clone());}
                let state=required(lua)?;let pending=state.borrow().pending_assets;state.borrow_mut().pending_assets=pending.saturating_sub(1);completed+=1;
                if let Some(callback)=&callback{
                    crate::scheduler::spawn_with(lua,Value::Function(callback.clone()),args([str_value(lua,&resource.content)?,asset_enum(lua,"Failure")?]),Some("serial"),None)?;
                }
            }
            Ok(failures)
        })();
        let state=required(lua)?;let pending=state.borrow().pending_assets;state.borrow_mut().pending_assets=pending.saturating_sub(count-completed);
        match result{
            Ok(failures)=>{
                crate::scheduler::schedule(lua,caller.clone(),MultiValue::new(),None)?;
                for resource in failures{asset_failed(lua,&node,&resource)?;}
            },
            Err(error)=>crate::scheduler::schedule_error(lua,caller.clone(),str_value(lua,&error.to_string())?)?,
        }
        Ok(())
    })?;
    let worker=lua.create_thread(work)?;crate::scheduler::set_thread_environment(lua,&worker,&environment)?;
    crate::scheduler::schedule(lua,worker,MultiValue::new(),Some("serial"))?;
    state.borrow_mut().pending_assets+=count;
    Ok(NativeCall::Yield(MultiValue::new()))
}

const UNSUPPORTED:&[(&str,&CStr)]=&[("InspectPlayerFromHumanoidDescription",c"InspectPlayerFromHumanoidDescription"),("CloseInspectMenu",c"CloseInspectMenu"),("GetRealPhysicsFPS",c"GetRealPhysicsFPS"),("Raycast",c"Raycast"),("Blockcast",c"Blockcast"),("GetPartBoundsInBox",c"GetPartBoundsInBox"),("Kick",c"Kick"),("Clear",c"Clear"),("PostAsync",c"PostAsync"),("PostAsyncFullUrl",c"PostAsyncFullUrl"),("PromptRobloxPurchase",c"PromptRobloxPurchase"),("PromptThirdPartyPurchase",c"PromptThirdPartyPurchase")];
const METHODS:&[(&str,&CStr)]=&[("Clone",c"Clone"),("GetPropertyChangedSignal",c"GetPropertyChangedSignal"),("GetAttributeChangedSignal",c"GetAttributeChangedSignal"),("GetPlayers",c"GetPlayers"),("GetNetworkPing",c"GetNetworkPing"),("IsClient",c"IsClient"),("IsServer",c"IsServer"),("IsStudio",c"IsStudio"),("IsRunning",c"IsRunning"),("IsLoaded",c"IsLoaded"),("FireServer",c"FireServer"),("FireClient",c"FireClient"),("FireAllClients",c"FireAllClients"),("SendMessage",c"SendMessage"),("BindToMessage",c"BindToMessage"),("BindToMessageParallel",c"BindToMessageParallel"),("GetLogHistory",c"GetLogHistory"),("JSONDecode",c"JSONDecode"),("QueryDescendants",c"QueryDescendants"),("ComputeStringHash",c"ComputeStringHash")];
pub(crate) fn method_names()->Vec<(&'static str,&'static CStr)>{METHODS.iter().chain(UNSUPPORTED.iter()).copied().chain([("BindAction",c"BindAction"),("BindActionAtPriority",c"BindActionAtPriority"),("UnbindAction",c"UnbindAction"),("UnbindAllActions",c"UnbindAllActions"),("GetBoundActionInfo",c"GetBoundActionInfo"),("GetAllBoundActionInfo",c"GetAllBoundActionInfo"),("GetStyled",c"GetStyled"),("GenerateGUID",c"GenerateGUID"),("GetFocusedTextBox",c"GetFocusedTextBox"),("IsTenFootInterface",c"IsTenFootInterface"),("GetPlatform",c"GetPlatform"),("GetPlayerFromCharacter",c"GetPlayerFromCharacter"),("GetMemoryUsageMbForTag",c"GetMemoryUsageMbForTag"),("GetServerTimeNow",c"GetServerTimeNow"),("RequestLimitedAsync",c"RequestLimitedAsync"),("GetRobuxBalance",c"GetRobuxBalance"),("PerformPurchaseV2",c"PerformPurchaseV2"),("ScreenPointToRay",c"ScreenPointToRay"),("ViewportPointToRay",c"ViewportPointToRay"),("GetAssetFetchStatus",c"GetAssetFetchStatus"),("GetAssetFetchStatusChangedSignal",c"GetAssetFetchStatusChangedSignal"),("Preload",c"Preload"),("PromptPurchase",c"PromptPurchase"),("PromptRealWorldCommerceBrowser",c"PromptRealWorldCommerceBrowser")]).collect()}
pub(crate) fn method_class(name:&str)->Option<&'static str>{
    Some(match name {
        "Clone"|"GetPropertyChangedSignal"|"GetAttributeChangedSignal"|"QueryDescendants"|"WaitForChild"=>"Instance",
        "GetPlayers"|"GetPlayerFromCharacter"=>"Players","GetNetworkPing"|"Kick"=>"Player",
        "GetMemoryUsageMbForTag"=>"Stats","GetServerTimeNow"=>"Workspace",
        "IsClient"|"IsServer"|"IsStudio"|"IsRunning"=>"RunService","IsLoaded"=>"DataModel",
        "Fire"=>"BindableEvent","Invoke"=>"BindableFunction",
        "FireServer"|"FireClient"|"FireAllClients"=>"BaseRemoteEvent","InvokeServer"|"InvokeClient"=>"RemoteFunction",
        "SendMessage"|"BindToMessage"|"BindToMessageParallel"=>"Actor",
        "GetLogHistory"=>"LogService","JSONEncode"|"JSONDecode"|"GenerateGUID"=>"HttpService","ComputeStringHash"=>"EncodingService",
        "InspectPlayerFromHumanoidDescription"|"CloseInspectMenu"|"IsTenFootInterface"=>"GuiService",
        "GetFocusedTextBox"|"GetPlatform"=>"UserInputService",
        "BindAction"|"BindActionAtPriority"|"UnbindAction"|"UnbindAllActions"|"GetBoundActionInfo"|"GetAllBoundActionInfo"=>"ContextActionService",
        "GetAssetFetchStatusChangedSignal"|"GetAssetFetchStatus"|"PreloadAsync"|"Preload"=>"ContentProvider",
        "GetRealPhysicsFPS"=>"Workspace","Raycast"|"Blockcast"|"GetPartBoundsInBox"=>"WorldRoot",
        "ScreenPointToRay"|"ViewportPointToRay"=>"Camera","GetStyled"=>"Instance","Clear"=>"Terrain",
        "PostAsync"|"PostAsyncFullUrl"|"RequestLimitedAsync"=>"HttpRbxApiService",
        "PromptPurchase"|"GetRobuxBalance"|"PerformPurchaseV2"|"PromptRobloxPurchase"|"PromptThirdPartyPurchase"=>"MarketplaceService",
        "PromptRealWorldCommerceBrowser"=>"CommerceService",
        _=>return None,
    })
}
fn bound_action_info(lua:&Lua,action:Option<&BoundAction>)->Result<Table>{
    let result=lua.create_table()?;
    if let Some(action)=action{
        result.raw_set("priorityLevel",action.priority)?;result.raw_set("stackOrder",action.order)?;
        result.raw_set("inputTypes",lua.create_sequence_from(action.inputs.iter().cloned())?)?;
    }
    Ok(result)
}
pub(crate) fn register_methods<M:UserDataMethods<Instance>>(m:&mut M){
    for method in ["BindAction","BindActionAtPriority"]{instance::register_method(m,method,move|lua,_,mut values:MultiValue|{
        let name=match values.pop_front(){Some(Value::String(name))=>name.to_str()?.to_owned(),_=>return Err(Error::lua_string("Argument 1 missing or nil"))};
        let callback=match values.pop_front(){Some(Value::Function(callback))=>callback,_=>return Err(Error::lua_string("Unable to cast value to function"))};
        let _touch=match values.pop_front(){Some(Value::Boolean(value))=>value,_=>return Err(Error::lua_string("Unable to cast value to bool"))};
        let priority=if method=="BindActionAtPriority"{lua.coerce_integer(values.pop_front().unwrap_or(Value::Nil))?.ok_or_else(||Error::lua_string("Unable to cast value to int"))?}else{2000};
        if name.len()>4096||values.len()>128{return Err(Error::runtime("BudgetExceeded: input binding"))}
        for input in &values {
            let valid=crate::types::encode_value(input)?.is_some_and(|item|item["tag"]=="EnumItem"&&matches!(item["enum"].as_str(),Some("KeyCode"|"UserInputType"|"PlayerActions")));
            if !valid{return Err(Error::lua_string("Invalid input type for ContextActionService"))}
        }
        let state=required(lua)?;let mut state=state.borrow_mut();
        if state.actions.len()>=MAX_WAITERS&&!state.actions.contains_key(&name){return Err(Error::runtime("BudgetExceeded: input bindings"))}
        state.action_order+=1;let order=state.action_order;
        state.actions.insert(name,BoundAction{_callback:callback,inputs:values.into_iter().collect(),priority,order});
        Ok(())
    });}
    instance::register_method(m,"UnbindAction",|lua,_,name:String|{required(lua)?.borrow_mut().actions.remove(&name);Ok(())});
    instance::register_method(m,"UnbindAllActions",|lua,_,()|{required(lua)?.borrow_mut().actions.clear();Ok(())});
    instance::register_method(m,"GetBoundActionInfo",|lua,_,name:String|bound_action_info(lua,required(lua)?.borrow().actions.get(&name)));
    instance::register_method(m,"GetAllBoundActionInfo",|lua,_,()|{
        let result=lua.create_table()?;
        for(name,action)in &required(lua)?.borrow().actions{result.raw_set(name.as_str(),bound_action_info(lua,Some(action))?)?;}
        Ok(result)
    });
    instance::register_method(m,"GetStyled",|lua,this,key:String|{
        // There are no style rules in the headless scene, so a styled property is its stored value.
        if matches!(key.as_str(),"Name"|"ClassName"|"Parent"|"Archivable")||key=="LinkedSource"&&instance::is_a(&this.0.borrow(),"LuaSourceContainer") {
            return instance::read_property(lua,this,&key);
        }
        if event_class(&key).is_none(){if let Some(value)=read_property(lua,&this.0,&key)?{return Ok(value)}}
        Err(Error::lua_string(format!("GetStyled(): No such property named `{key}`")))
    });
    instance::register_method(m,"GenerateGUID",|_,_,braces:Option<bool>|{
        let mut bytes=[0u8;16];crate::native::random_bytes(&mut bytes).map_err(Error::external)?;
        bytes[6]=(bytes[6]&0x0f)|0x40;bytes[8]=(bytes[8]&0x3f)|0x80;
        let hex:String=bytes.iter().map(|byte|format!("{byte:02x}")).collect();
        let id=format!("{}-{}-{}-{}-{}",&hex[..8],&hex[8..12],&hex[12..16],&hex[16..20],&hex[20..]);
        Ok(if braces.unwrap_or(true){format!("{{{id}}}")}else{id})
    });
    // The headless input service has no focused text box or ten-foot shell.
    instance::register_method(m,"GetFocusedTextBox",|_,_,()|Ok(Value::Nil));
    instance::register_method(m,"IsTenFootInterface",|_,_,()|Ok(false));
    instance::register_method(m,"GetPlatform",|lua,_,()|{
        if crate::scheduler::current_origin(lua)=="game"{return Err(Error::lua_string("The current thread cannot call 'GetPlatform' (lacking capability RobloxScript)"))}
        crate::types::decode_value(lua,&json!({"tag":"EnumItem","enum":"Platform","name":if cfg!(windows){"Windows"}else if cfg!(target_os="macos"){"OSX"}else{"Linux"}}))?.ok_or_else(||Error::runtime("Platform unavailable"))
    });
    instance::register_method(m,"GetPlayerFromCharacter",|lua,this,values:MultiValue|{
        let Some(input)=values.front()else{return Err(Error::lua_string("Argument 1 missing or nil"))};
        if input.is_nil(){return Ok(None)}
        let Value::UserData(input)=input else{return Err(Error::lua_string("Unable to cast value to Object"))};
        let character=input.borrow::<Instance>()?;
        let players=this.0.borrow().children.clone();
        let found=players.into_iter().find(|player|matches!(player.borrow().properties.get("Character"),Some(Value::UserData(value))if value.borrow::<Instance>().is_ok_and(|v|Rc::ptr_eq(&v.0,&character.0))));
        found.map(|node|instance::wrapper(lua,node)).transpose()
    });
    instance::register_method(m,"GetMemoryUsageMbForTag",|lua,_,tag:Value|{
        let tag=crate::types::encode_value(&tag)?.filter(|v|v["tag"]=="EnumItem"&&v["enum"]=="DeveloperMemoryTag").ok_or_else(||Error::lua_string("Unable to cast value to DeveloperMemoryTag"))?;
        // The headless runtime allocates no GUI renderer resources; LuaHeap is measured by Luau.
        match tag["name"].as_str(){Some("LuaHeap")=>Ok(lua.used_memory() as f64/1048576.0),Some("Gui")=>Ok(0.0),_=>Err(unsupported(lua,"Stats.GetMemoryUsageMbForTag category"))}
    });
    instance::register_method(m,"GetServerTimeNow",|lua,_,()|Ok(1700000000.0+now(lua)));
    for (name,message) in [("RequestLimitedAsync","RequestLimitedAsync is disabled"),("GetRobuxBalance","The current thread cannot call 'GetRobuxBalance' (lacking capability RobloxScript)"),("PerformPurchaseV2","The current thread cannot call 'PerformPurchaseV2' (lacking capability RobloxScript)")]{
        instance::register_method(m,name,move|lua,this,_:MultiValue|{
            if crate::scheduler::current_origin(lua)=="game"{Err::<(),_>(Error::lua_string(message))}
            else{Err(unsupported(lua,&format!("{}.{name} privileged context",this.0.borrow().class)))}
        });
    }
    instance::register_method(m,"PromptPurchase",|lua,_,values:MultiValue|{
        for value in &values{reject_cycles(value)?;}
        if values.get(1).is_none_or(Value::is_nil){return Err::<(),_>(Error::lua_string("Argument 2 missing or nil"))}
        if values.front().is_none_or(Value::is_nil){return Err(Error::lua_string("Argument 1 missing or nil"))}
        Err(unsupported(lua,"MarketplaceService.PromptPurchase"))
    });
    instance::register_method(m,"PromptRealWorldCommerceBrowser",|lua,_,values:MultiValue|{
        for value in &values{reject_cycles(value)?;}
        Err::<(),_>(unsupported(lua,"CommerceService.PromptRealWorldCommerceBrowser"))
    });
    instance::register_method(m,"GetAssetFetchStatus",|lua,_,input:Value|{
        asset_status(lua,&content_id(lua,input)?)
    });
    instance::register_method(m,"GetAssetFetchStatusChangedSignal",|lua,this,input:Value|{
        let id=content_id(lua,input)?;let key=format!("{id}AssetFetchStatusChanged");
        if !this.0.borrow().signals.contains_key(&key)&&this.0.borrow().signals.len()>=MAX_WAITERS{return Err(Error::runtime("BudgetExceeded: asset status signals"))}
        signal(lua,&this.0,&key)
    });
    instance::register_method(m,"Preload",|lua,this,input:Value|{
        guard_write(lua)?;let resource=asset_resource(input)?;
        if offline_asset(lua,&resource)?{asset_failed(lua,&this.0,&resource)?;}Ok(())
    });
    for name in ["ScreenPointToRay","ViewportPointToRay"]{instance::register_method(m,name,|lua,this,values:MultiValue|{
        let number=|index:usize,default:Option<f32>|->Result<f32>{
            let value=values.get(index-1).cloned().unwrap_or(Value::Nil);
            if value.is_nil(){return default.ok_or_else(||Error::lua_string(format!("Argument {index} missing or nil")))}
            let from=match &value{Value::Table(_)=>"Array",Value::Boolean(_)=>"bool",other=>other.type_name()};
            lua.coerce_number(value.clone())?.map(|value|value as f32).ok_or_else(||Error::lua_string(format!("Unable to cast {from} to float")))
        };
        let y=number(2,None)?;let x=number(1,None)?;let depth=number(3,Some(0.0))?;
        let frame=instance::read_property(lua,this,"CFrame")?;let viewport=instance::read_property(lua,this,"ViewportSize")?;
        let fov=lua.coerce_number(instance::read_property(lua,this,"FieldOfView")?)?.ok_or_else(||Error::lua_string("Camera FieldOfView must be a number"))?as f32;
        let near=lua.coerce_number(instance::read_property(lua,this,"NearPlaneZ")?)?.ok_or_else(||Error::lua_string("Camera NearPlaneZ must be a number"))?as f32;
        crate::types::camera_ray(lua,&frame,&viewport,fov,-near,x,y,depth)
    });}
    instance::register_method(m,"Clone",|lua,this,()|clone_node(lua,&this.0)?.map(|n|instance::wrapper(lua,n)).transpose());
    instance::register_method(m,"GetPropertyChangedSignal",|lua,this,name:String|{instance::read_property(lua,this,&name)?;signal(lua,&this.0,&format!("property:{name}"))});
    instance::register_method(m,"GetAttributeChangedSignal",|lua,this,name:String|signal(lua,&this.0,&format!("attribute:{name}")));
    instance::register_method(m,"GetPlayers",|lua,this,()|instance::instance_list(lua,this.0.borrow().children.iter().filter(|n|n.borrow().class=="Player").cloned().collect()));
    instance::register_method(m,"GetNetworkPing",|_,this,()|Ok(this.0.borrow().properties.get("NetworkPing").cloned().unwrap_or(Value::Number(0.0))));
    for (name, expected) in [("IsClient","client"),("IsServer","server")] {instance::register_method(m,name,move|lua,_,()|Ok(required(lua)?.borrow().side==expected));}
    instance::register_method(m,"IsStudio",|lua,_,()|Ok(required(lua)?.borrow().studio));
    instance::register_method(m,"IsRunning",|_,_,()|Ok(true));instance::register_method(m,"IsLoaded",|_,_,()|Ok(true));
    instance::register_method(m,"FireServer",|lua,this,values:MultiValue|{if required(lua)?.borrow().side!="client"{return Err(Error::lua_string("FireServer can only be called from client"))}emit(lua,"remote_event",json!({"remoteId":this.0.borrow().id,"target":"server","args":encode_args(lua,values)?}))});
    instance::register_method(m,"FireAllClients",|lua,this,values:MultiValue|{if required(lua)?.borrow().side!="server"{return Err(Error::lua_string("FireAllClients can only be called from server"))}emit(lua,"remote_event",json!({"remoteId":this.0.borrow().id,"target":"*","args":encode_args(lua,values)?}))});
    instance::register_method(m,"FireClient",|lua,this,mut values:MultiValue|{if required(lua)?.borrow().side!="server"{return Err(Error::lua_string("FireClient can only be called from server"))}let player=object_arg(&mut values)?;if player.borrow().class!="Player"{return Err(Error::lua_string("Player expected"))}let id=player.borrow().id.clone();emit(lua,"remote_event",json!({"remoteId":this.0.borrow().id,"target":id,"args":encode_args(lua,values)?}))});
    instance::register_method(m,"SendMessage",|lua,this,mut values:MultiValue|{let topic=values.pop_front().ok_or_else(||Error::lua_string("Topic expected"))?;let Value::String(topic)=topic else{return Err(Error::lua_string("Topic expected"))};emit(lua,"actor_message",json!({"actorId":this.0.borrow().id,"topic":topic.to_str()?.as_ref(),"args":encode_args(lua,values)?}))});
    for (name,parallel) in [("BindToMessage",false),("BindToMessageParallel",true)]{instance::register_method(m,name,move|lua,this,(topic,callback):(String,Function)|{
        if required(lua)?.borrow().actor!=this.0.borrow().id{return Err(Error::lua_string("Only scripts owned by this Actor can bind messages"))}
        let event=signal(lua,&this.0,&format!("message:{topic}"))?;crate::scheduler::connect_signal(lua,&event,callback,parallel,false)
    });}
    instance::register_method(m,"GetLogHistory",|lua,_,()|log_history(lua));
    instance::register_method(m,"JSONDecode",|lua,_,v:String|lua.to_value(&serde_json::from_str::<Json>(&v).map_err(Error::external)?));
    instance::register_method(m,"ComputeStringHash",|lua,this,(input,algorithm):(mlua::LuaString,Value)|{
        if this.0.borrow().class!="EncodingService"{return Err(Error::runtime("EncodingService expected"))}
        let algorithm=crate::types::encode_value(&algorithm)?;
        if !algorithm.is_some_and(|value|value["tag"]=="EnumItem"&&value["enum"]=="HashAlgorithm"&&value["name"]=="Sha256"){
            return Err(unsupported(lua,"EncodingService.ComputeStringHash algorithm"));
        }
        lua.create_string(Sha256::digest(input.as_bytes()))
    });
    instance::register_method(m,"QueryDescendants",|lua,this,query:String|{
        let class=query.trim();
        if !class.is_empty()&&class.bytes().all(|c|c.is_ascii_alphanumeric()||c==b'_'){
            return instance::instance_list(lua,instance::descendants(&this.0).into_iter().filter(|n|instance::is_a(&n.borrow(),class)).collect());
        }
        if class.starts_with("IsA("){return Err(Error::lua_string("Unexpected trailing character: '('"))}
        Err(unsupported(lua,"Instance.QueryDescendants selector"))
    });
    for (name,_) in UNSUPPORTED {let method=*name;instance::register_method(m,*name,move|lua,this,_:MultiValue|Err::<Value,_>(unsupported(lua,&format!("{}.{method}",this.0.borrow().class))));}
}

fn log_history(lua:&Lua)->Result<Table>{let out=lua.create_table()?;for(i,(level,message,time))in required(lua)?.borrow().logs.iter().enumerate(){let row=lua.create_table()?;row.set("message",lua.create_string(message)?)?;row.set("messageType",message_type(lua,level)?)?;row.set("timestamp",*time)?;out.raw_set(i+1,row)?;}Ok(out)}
fn message_type(lua:&Lua,level:&str)->Result<Value>{let name=match level{"warn"=>"MessageWarning","error"=>"MessageError",_=>"MessageOutput"};crate::types::decode_value(lua,&json!({"tag":"EnumItem","enum":"MessageType","name":name}))?.ok_or_else(||Error::runtime("MessageType unavailable"))}
pub(crate) fn log(lua:&Lua,level:&str,message:&[u8])->Result<()>{
    let s=required(lua)?;
    {
        let mut s=s.borrow_mut();
        s.log_bytes+=message.len();
        s.logs.push_back((level.into(),message.into(),now(lua)));
        while s.logs.len()>1000||s.log_bytes>8*1024*1024 {
            if let Some((_,removed,_))=s.logs.pop_front(){s.log_bytes-=removed.len();}else{break;}
        }
    }
    if let Some(n)=lookup(lua,"LogService"){fire(lua,&n,"MessageOut",args([Value::String(lua.create_string(message)?),message_type(lua,level)?]))?;}
    Ok(())
}
pub(crate) fn script_error(lua:&Lua,message:&str,thread:&Thread)->Result<()>{
    use std::fmt::Write as _;
    let Some(context)=lookup(lua,"ScriptContext") else{return Ok(())};
    if !context.borrow().signals.contains_key("Error"){return Ok(())}
    let mut trace=String::new();
    unsafe{lua.exec_raw::<()>(thread.clone(),|state|{
        let target=ffi::lua_tothread(state,1);
        let mut debug:ffi::lua_Debug=std::mem::zeroed();
        for level in 0..10_000{
            // ScriptContext reports Lua frames only; direct C tasks have an empty trace.
            if ffi::lua_getinfo(target,level,c"sln".as_ptr(),&mut debug)==0{break}
            if debug.what.is_null()||CStr::from_ptr(debug.what).to_bytes()!=b"Lua"{continue}
            let source=CStr::from_ptr(debug.source).to_string_lossy();
            let source=source.strip_prefix('=').or_else(||source.strip_prefix('@')).unwrap_or(&source);
            write!(trace,"{source}, line {}",debug.currentline).unwrap();
            if !debug.name.is_null(){let name=CStr::from_ptr(debug.name).to_string_lossy();if !name.is_empty(){write!(trace," - function {name}").unwrap();}}
            trace.push('\n');
        }
        ffi::lua_settop(state,0);
    })?};
    let owners:Table=lua.named_registry_value(ENV_OWNERS)?;
    let owner:Value=unsafe{lua.exec_raw((owners,thread.clone()),|state|{
        let target=ffi::lua_tothread(state,2);
        // The task belongs to its initiating Script even when its function came from a ModuleScript.
        ffi::lua_getfenv(state,2);ffi::lua_rawget(state,1);
        if ffi::lua_type(state,-1)==ffi::LUA_TUSERDATA{ffi::lua_replace(state,1);ffi::lua_settop(state,1);return}
        ffi::lua_settop(state,2);
        if ffi::lua_checkstack(target,1)!=0{
            let mut debug:ffi::lua_Debug=std::mem::zeroed();
            for level in 0..10_000{
                if ffi::lua_getinfo(target,level,c"f".as_ptr(),&mut debug)==0{break}
                ffi::lua_xmove(target,state,1);ffi::lua_getfenv(state,-1);ffi::lua_rawget(state,1);
                if ffi::lua_type(state,-1)==ffi::LUA_TUSERDATA{ffi::lua_replace(state,1);ffi::lua_settop(state,1);return}
                ffi::lua_settop(state,2);
            }
        }
        ffi::lua_settop(state,0);ffi::lua_pushnil(state);
    })?};
    fire(lua,&context,"Error",args([str_value(lua,message)?,str_value(lua,&trace)?,owner]))
}

fn snapshot(lua:&Lua,node:&Link)->Result<Json>{let n=node.borrow();let props=lua.create_table()?;for(k,v)in &n.properties{if !matches!(v,Value::Function(_)){props.raw_set(k.as_str(),v.clone())?;}}props.raw_set("Archivable",n.archivable)?;let attrs=lua.create_table()?;for(k,v)in &n.attributes{attrs.raw_set(k.as_str(),instance::attribute_value(lua,v)?)?;}Ok(json!({"id":n.id,"class":n.class,"name":n.name,"parent":n.parent.upgrade().map(|p|p.borrow().id.clone()),"source":n.source,"linkedSource":n.linked_source,"destroyed":n.destroyed,"properties":encode(lua,Value::Table(props),&mut HashSet::new())?,"attributes":encode(lua,Value::Table(attrs),&mut HashSet::new())?}))}

// Keep complete host metadata while applying only the writer's changed keys.
pub(crate) fn merge_replica(previous:Option<&Json>,incoming:&Json)->Json{
    let Some(keys)=incoming.get("$changed") else{return incoming.clone()};
    let Some(previous)=previous else{let mut full=incoming.clone();full.as_object_mut().unwrap().remove("$changed");return full};
    let mut result=previous.clone();result.as_object_mut().unwrap().remove("$changed");
    for field in ["id","class","name","parent","source","linkedSource","destroyed"]{
        if result.get(field).is_none()||keys["fields"].as_array().is_some_and(|fields|fields.iter().any(|key|key==field)){result[field]=incoming[field].clone();}
    }
    for group in ["properties","attributes"]{
        // The initial host metadata comes from unencoded scenario nodes.
        if result[group]["tag"]!="table"{result[group]=incoming[group].clone();continue}
        if let Some(changed)=keys[group].as_array(){
            let entries=result[group]["entries"].as_array_mut().unwrap();
            for key in changed{
                entries.retain(|entry|entry["key"]["value"]!=*key);
                if let Some(entry)=incoming[group]["entries"].as_array().and_then(|entries|entries.iter().find(|entry|entry["key"]["value"]==*key)){entries.push(entry.clone());}
            }
        }
    }
    result
}
fn selected(node:&Json,group:&str,key:&str)->bool{node.get("$changed").is_none_or(|changed|changed[group].as_array().is_some_and(|keys|keys.iter().any(|value|value==key)))}
fn ensure(lua:&Lua,nodes:&[Json])->Result<Vec<Link>>{
    let s=required(lua)?;let mut held=Vec::new();
    for node in nodes{
        let id=node["id"].as_str().ok_or_else(||Error::runtime("Node id missing"))?;
        let n=if let Some(n)=lookup(lua,id){n}else{
            reserve_node(&mut s.borrow_mut())?;
            let n=instance::create_node(lua,node["class"].as_str().unwrap_or("Folder"))?;
            n.borrow_mut().id=id.into();s.borrow_mut().nodes.insert(id.into(),Rc::downgrade(&n));n
        };held.push(n);
    }Ok(held)
}
fn load_nodes(lua:&Lua,nodes:Vec<Json>,wire:bool)->Result<()>{
    if nodes.iter().any(|node|node["source"].as_str().is_some_and(|source|source.len()>MAX_SOURCE)){return Err(Error::runtime("BudgetExceeded: source size"))}
    let held=ensure(lua,&nodes)?;for(node,n)in nodes.iter().zip(&held){
    if selected(node,"fields","name"){if let Some(name)=node["name"].as_str(){if n.borrow().name!=name{n.borrow_mut().name=name.into();if wire{changed(lua,n,"Name")?;}}}}
    if selected(node,"fields","source"){if let Some(source)=node["source"].as_str(){n.borrow_mut().source=source.into();}}
    if selected(node,"fields","linkedSource"){if let Some(source)=node["linkedSource"].as_str(){n.borrow_mut().linked_source=source.into();}}
    if let Some(properties)=node.get("properties"){
        let props=if wire{decode(lua,properties)?}else{lua.to_value(properties)?};
        if let Value::Table(props)=props{
            let mut keys:BTreeSet<String>=props.clone().pairs::<String,Value>().map(|entry|entry.map(|(key,_)|key)).collect::<Result<_>>()?;
            if let Some(changed)=node["$changed"]["properties"].as_array(){keys.extend(changed.iter().filter_map(Json::as_str).map(str::to_owned));}
            else if wire&&node.get("$changed").is_none(){keys.extend(n.borrow().properties.iter().filter(|(_,value)|!matches!(value,Value::Function(_))).map(|(key,_)|key.clone()));}
            for key in keys{if !selected(node,"properties",&key){continue}let next:Value=props.raw_get(key.as_str())?;
                let differs=if key=="Archivable"{let next=matches!(next,Value::Boolean(true));let different=n.borrow().archivable!=next;n.borrow_mut().archivable=next;different}
                else{let previous=n.borrow().properties.get(&key).cloned().unwrap_or(Value::Nil);let different=previous!=next;if next.is_nil(){n.borrow_mut().properties.remove(&key);}else{n.borrow_mut().properties.insert(key.clone(),next);}different};
                if wire&&differs{changed(lua,n,&key)?;}
            }
        }
    }
    if let Some(attributes)=node.get("attributes"){
        let attrs=if wire{decode(lua,attributes)?}else{lua.to_value(attributes)?};
        if let Value::Table(attrs)=attrs{
            let mut incoming=HashMap::new();for pair in attrs.pairs::<String,Value>(){let(k,v)=pair?;let a=match v{Value::Boolean(v)=>Attribute::Boolean(v),Value::Integer(v)=>Attribute::Number(v as f64),Value::Number(v)=>Attribute::Number(v),Value::String(v)=>Attribute::String(v.to_str()?.to_string()),_=>return Err(Error::runtime("Unsupported attribute type"))};incoming.insert(k,a);}
            let keys:BTreeSet<_>=n.borrow().attributes.keys().chain(incoming.keys()).cloned().collect();
            for key in keys{if !selected(node,"attributes",&key){continue}let next=incoming.remove(&key);if n.borrow().attributes.get(&key)!=next.as_ref(){
                if let Some(next)=next{n.borrow_mut().attributes.insert(key.clone(),next);}else{n.borrow_mut().attributes.remove(&key);}
                if wire{attribute_changed(lua,n,&key)?;}
            }}
        }
    }
    mark(lua,n);
}for(node,n)in nodes.iter().zip(&held){if selected(node,"fields","destroyed")&&node["destroyed"].as_bool()==Some(true){destroy(lua,n)?;}else if selected(node,"fields","parent"){let p=node["parent"].as_str().and_then(|id|lookup(lua,id));parent(lua,n,p)?;}}Ok(())}

pub fn install(lua:&Lua)->Result<()>{
    lua.set_named_registry_value(BASE_ENV,lua.globals())?;
    let config:Table=crate::internal(lua,"__config")?;let s=Rc::new(RefCell::new(Model{realm:config.get("realmId")?,side:config.get("side")?,client:config.get::<Option<String>>("clientId")?.unwrap_or_default(),actor:config.get::<Option<String>>("actorId")?.unwrap_or_default(),studio:config.get::<Option<bool>>("isStudio")?.unwrap_or(false),..Default::default()}));
    lua.set_app_data(s.clone());
    let envs=lua.create_table()?;let mt=lua.create_table()?;mt.set("__mode","v")?;envs.set_metatable(Some(mt))?;lua.set_named_registry_value(ENVS,envs)?;
    let owners=lua.create_table()?;let mt=lua.create_table()?;mt.set("__mode","k")?;owners.set_metatable(Some(mt))?;lua.set_named_registry_value(ENV_OWNERS,owners)?;
    instance::install(lua)?;
    let game:AnyUserData=lua.globals().get("game")?;let game=game.borrow::<Instance>()?.0.clone();game.borrow_mut().id="game".into();s.borrow_mut().nodes.insert("game".into(),Rc::downgrade(&game));
    for child in &game.borrow().children{let id=child.borrow().class.clone();child.borrow_mut().id=id.clone();s.borrow_mut().nodes.insert(id,Rc::downgrade(child));}
    let terrain=instance::new_node("Terrain")?;terrain.borrow_mut().id="Terrain".into();
    instance::set_parent(&terrain,lookup(lua,"Workspace"))?;s.borrow_mut().nodes.insert("Terrain".into(),Rc::downgrade(&terrain));
    let model=lua.create_table()?;
    model.set("seed",lua.create_function(|lua,v:Value|seed(lua,&lua.from_value::<Vec<Json>>(v)?))?)?;
    model.set("applyChanges",lua.create_function(|lua,v:Value|apply_changes(lua,&lua.from_value::<Vec<Json>>(v)?))?)?;
    model.set("exportChanges",lua.create_function(|lua,()|lua.to_value(&export_changes(lua)?))?)?;
    model.set("get",lua.create_function(|lua,id:String|lookup(lua,&id).map(|n|instance::wrapper(lua,n)).transpose())?)?;
    model.set("getInstances",lua.create_function(|lua,()|{let nodes=required(lua)?.borrow().nodes.values().filter_map(Weak::upgrade).filter(|n|!n.borrow().destroyed).collect();instance::instance_list(lua,nodes)})?)?;
    model.set("getNilInstances",lua.create_function(|lua,()|{let nodes=required(lua)?.borrow().nodes.values().filter_map(Weak::upgrade).filter(|n|!n.borrow().destroyed&&n.borrow().parent.upgrade().is_none()).collect();instance::instance_list(lua,nodes)})?)?;
    model.set("getUnsupportedCalls",lua.create_function(|lua,()|{let table=lua.create_table()?;for(name,count)in unsupported_calls(lua)?{table.set(name,count)?;}table.set_readonly(true);Ok(table)})?)?;
    model.set("getEnvironment",lua.create_function(|lua,u:AnyUserData|env(lua,&u.borrow::<Instance>()?.0))?)?;
    model.set("runScript",lua.create_function(|lua,id:String|run_script(lua,&id))?)?;
    model.set("log",lua.create_function(|lua,(level,message):(String,mlua::LuaString)|log(lua,&level,&message.as_bytes()))?)?;
    model.set("encode",lua.create_function(|lua,v:Value|lua.to_value(&encode(lua,v,&mut HashSet::new())?))?)?;
    model.set("decode",lua.create_function(|lua,v:Value|decode(lua,&lua.from_value(v)?))?)?;
    model.set("encodeTuple",lua.create_function(|lua,v:MultiValue|lua.to_value(&encode_args(lua,v)?))?)?;
    model.set("decodeTuple",lua.create_function(|lua,v:Value|{let args=decode_args(lua,&lua.from_value(v)?)?;let table=lua.create_table()?;table.set("n",args.len())?;for(i,v)in args.into_iter().enumerate(){table.raw_set(i+1,v)?;}Ok(table)})?)?;
    model.set("frame",lua.create_function(|lua,dt:f64|frame(lua,dt))?)?;
    model.set("deliver",lua.create_function(|lua,(kind,payload):(String,Value)|deliver(lua,&kind,lua.from_value(payload)?))?)?;
    lua.globals().set("__model",model)?;
    let require=unsafe{raw_function(lua,raw_require,c"require")?};lua.globals().set("require",require)?;
    if matches!(lua.globals().get::<Value>("shared")?,Value::Nil){lua.globals().set("shared",lua.create_table()?)?;}
    Ok(())
}

pub fn seed(lua:&Lua,nodes:&[Json])->Result<()>{load_nodes(lua,nodes.to_vec(),false)}
pub fn export_changes(lua:&Lua)->Result<Vec<Json>>{let changes=std::mem::take(&mut required(lua)?.borrow_mut().changes);let mut out=Vec::new();for change in changes.values(){let mut node=snapshot(lua,&change.node)?;if let Some(keys)=&change.keys{node["$changed"]=json!(keys);}out.push(node);}Ok(out)}
pub fn apply_changes(lua:&Lua,nodes:&[Json])->Result<()>{let s=required(lua)?;s.borrow_mut().applying=true;let result=load_nodes(lua,nodes.to_vec(),true);s.borrow_mut().applying=false;result}
pub fn run_script(lua:&Lua,id:&str)->Result<Thread>{
    let n=lookup(lua,id).ok_or_else(||Error::runtime("Script unavailable"))?;let f=compile(lua,&n)?;let thread=lua.create_thread(f.clone())?;
    thread_environment(lua,&thread,&n)?;
    let name=n.borrow().name.clone();crate::scheduler::set_script_category(lua,&thread,&f,&name)?;
    crate::scheduler::spawn_with(lua,Value::Thread(thread),MultiValue::new(),Some("serial"),Some("game"))
}
pub fn frame(lua:&Lua,dt:f64)->Result<()>{poll_children(lua)?;let n=lookup(lua,"RunService").ok_or_else(||Error::runtime("RunService unavailable"))?;for event in ["PreSimulation","PostSimulation","Heartbeat"]{fire(lua,&n,event,args([Value::Number(dt)]))?;}fire(lua,&n,"Stepped",args([Value::Number(now(lua)),Value::Number(dt)]))?;if required(lua)?.borrow().side=="client"{for event in ["PreRender","RenderStepped"]{fire(lua,&n,event,args([Value::Number(dt)]))?;}}Ok(())}
pub fn deliver(lua:&Lua,kind:&str,payload:Json)->Result<()>{
    let id=payload[if kind=="actor_message"{"actorId"}else{"remoteId"}].as_str().unwrap_or("");
    if kind=="remote_return"{
        let thread=required(lua)?.borrow_mut().pending.remove(payload["requestId"].as_str().unwrap_or(""));
        if let Some(thread)=thread{let mut values=decode_args(lua,&payload["args"])?;if payload["ok"].as_bool()==Some(true){crate::scheduler::schedule(lua,thread,values,None)?;}else{crate::scheduler::schedule_error(lua,thread,values.pop_front().unwrap_or(Value::Nil))?;}}return Ok(())
    }
    // A client-created RemoteEvent has no server counterpart. An event queued before
    // its receiver was destroyed likewise has nothing to deliver to.
    let Some(n)=lookup(lua,id)else{
        if kind=="remote_event"{return Ok(())}
        return Err(Error::runtime(format!("Message target unavailable: {id}")));
    };
    let mut values=decode_args(lua,&payload["args"])?;
    match kind{
        "actor_message"=>fire(lua,&n,&format!("message:{}",payload["topic"].as_str().unwrap_or("")),values),
        "remote_event"=>{if required(lua)?.borrow().side=="server"{let player=lookup(lua,payload["sender"].as_str().or(payload["clientId"].as_str()).unwrap_or("")).ok_or_else(||Error::runtime("Unknown remote sender"))?;values.push_front(value(lua,player)?);fire(lua,&n,"OnServerEvent",values)}else{fire(lua,&n,"OnClientEvent",values)}},
        "remote_call"=>{
            let callback_name=if required(lua)?.borrow().side=="server"{"OnServerInvoke"}else{"OnClientInvoke"};
            let callback=n.borrow().properties.get(callback_name).cloned().ok_or_else(||Error::runtime("RemoteFunction has no handler"))?;
            let Value::Function(callback)=callback else{return Err(Error::lua_string("RemoteFunction callback must be a function"))};
            let callback=Value::Thread(callback_thread(lua,&n,callback_name,callback)?);
            if required(lua)?.borrow().side=="server"{let player=lookup(lua,payload["sender"].as_str().or(payload["clientId"].as_str()).unwrap_or("")).ok_or_else(||Error::runtime("Unknown remote sender"))?;values.push_front(value(lua,player)?);}
            let request=payload["requestId"].clone();let target=payload["sourceRealm"].clone();
            crate::scheduler::spawn_tracked(lua,callback,values,Some("serial"),Some("game"),Rc::new(move|lua,result|{
                let(ok,args)=match result{Ok(args)=>(true,args),Err(err)=>(false,MultiValue::from_iter([err]))};
                emit(lua,"remote_return",json!({"requestId":request,"target":target,"ok":ok,"args":encode_args(lua,args)?}))
            }))?;Ok(())
        }
        _=>Err(Error::runtime(format!("UnsupportedApi: native message {kind}"))),
    }
}

fn object_arg(values:&mut MultiValue)->Result<Link>{match values.pop_front(){Some(Value::UserData(v))if v.is::<Instance>()=>Ok(v.borrow::<Instance>()?.0.clone()),_=>Err(Error::lua_string("Instance expected"))}}
fn text_arg(values:&mut MultiValue)->Result<String>{match values.pop_front(){Some(Value::String(v))=>Ok(v.to_str()?.to_string()),_=>Err(Error::lua_string("String expected"))}}
fn finish_waiter(lua:&Lua,thread:Thread,result:std::result::Result<MultiValue,Value>)->Result<()>{match result{Ok(v)=>crate::scheduler::schedule(lua,thread,v,None),Err(e)=>crate::scheduler::schedule_error(lua,thread,e)}}
fn invoke_callback(lua:&Lua,node:&Link,callback:Function,values:MultiValue,waiter:Thread,allow_inline:bool)->Result<Option<std::result::Result<MultiValue,Value>>>{
    let active=Rc::new(Cell::new(allow_inline));let result=Rc::new(RefCell::new(None));let in_callback=active.clone();let out=result.clone();
    let callback=Value::Thread(callback_thread(lua,node,"OnInvoke",callback)?);
    crate::scheduler::spawn_tracked(lua,callback,values,Some("serial"),Some("game"),Rc::new(move|lua,result|{
        let result=match result{Ok(values)=>Ok(values),Err(error)=>Err(Value::String(lua.coerce_string(error)?.unwrap_or(lua.create_string("")?)))};
        if in_callback.get(){*out.borrow_mut()=Some(result);Ok(())}else{finish_waiter(lua,waiter.clone(),result)}
    }))?;active.set(false);let result=result.borrow_mut().take();Ok(result)
}
fn drain_invokes(lua:&Lua,node:&Link)->Result<()>{
    let callback=node.borrow().properties.get("OnInvoke").cloned();let Some(Value::Function(callback))=callback else{return Ok(())};
    let state=required(lua)?;let waiting=std::mem::take(&mut state.borrow_mut().invokes_waiting);
    for(id,thread,args)in waiting{if id==node.borrow().id{invoke_callback(lua,node,callback.clone(),args,thread,false)?;}else{state.borrow_mut().invokes_waiting.push((id,thread,args));}}
    Ok(())
}
fn poll_children(lua:&Lua)->Result<()>{
    let state=required(lua)?;let waiting=std::mem::take(&mut state.borrow_mut().children_waiting);
    for pending in waiting{
        let child=lookup(lua,&pending.parent).and_then(|n|instance::find_child(&n,&pending.name,false));
        if let Some(child)=child{crate::scheduler::schedule(lua,pending.thread,args([value(lua,child)?]),None)?;}
        else if pending.deadline.is_some_and(|deadline|now(lua)>=deadline){crate::scheduler::schedule(lua,pending.thread,args([Value::Nil]),None)?;}
        else{state.borrow_mut().children_waiting.push(pending);}
    }Ok(())
}
// Runs outside Rust callback guards: dictionary-key __tostring executes in the engine C frame.
unsafe fn variant_key(state:*mut ffi::lua_State,index:i32,json_mode:bool){unsafe{
    let index=ffi::lua_absindex(state,index);
    match ffi::lua_type(state,index){
        ffi::LUA_TSTRING=>ffi::lua_pushvalue(state,index),
        ffi::LUA_TNUMBER|ffi::LUA_TINTEGER=>{
            if json_mode{ffi::lua_pushinteger64(state,ffi::lua_tonumber(state,index)as i64);ffi::luaL_tolstring_(state,-1,std::ptr::null_mut());ffi::lua_remove(state,-2);}
            else{ffi::luaL_tolstring_(state,index,std::ptr::null_mut());}
        },
        kind if matches!(kind,ffi::LUA_TTABLE|ffi::LUA_TUSERDATA|ffi::LUA_TVECTOR)||(json_mode&&kind==ffi::LUA_TFUNCTION)=>{
            let kind=match kind{ffi::LUA_TTABLE=>c"Table".as_ptr(),ffi::LUA_TVECTOR=>c"Vector3".as_ptr(),ffi::LUA_TFUNCTION=>c"Function".as_ptr(),_=>c"null".as_ptr()};
            ffi::lua_pushstring(state,kind);
            // mlua's real native userdata uses UTAG_IDTOR. newproxy uses UTAG_PROXY,
            // so a proxy's user-controlled __type cannot impersonate a native datatype.
            if ffi::lua_type(state,index)==ffi::LUA_TUSERDATA&&ffi::lua_userdatatag(state,index)==ffi::LUA_UTAG_LIMIT&&ffi::lua_getmetatable(state,index)!=0{
                ffi::lua_rawgetfield(state,-1,c"__type".as_ptr());
                if ffi::lua_type(state,-1)==ffi::LUA_TSTRING{ffi::lua_replace(state,-3);ffi::lua_pop(state,1);}else{ffi::lua_pop(state,2);}
            }
            let kind=ffi::lua_gettop(state);
            ffi::lua_pushstring(state,c"<".as_ptr());ffi::lua_pushvalue(state,kind);ffi::lua_pushstring(state,c"> (".as_ptr());
            ffi::luaL_tolstring_(state,index,std::ptr::null_mut());ffi::lua_pushstring(state,c")".as_ptr());ffi::lua_concat(state,5);ffi::lua_remove(state,kind);
        },
        _=>{ffi::lua_pushstring(state,c"Invalid table key type used".as_ptr());ffi::lua_error(state)},
    }
}}
unsafe fn copy_variant(state:*mut ffi::lua_State,index:i32,active:i32,depth:usize,count:&mut usize,json_mode:bool){unsafe{
    ffi::luaL_checkstack(state,12,c"variant conversion stack".as_ptr());
    let index=ffi::lua_absindex(state,index);
    if !json_mode&&ffi::lua_type(state,index)==ffi::LUA_TBUFFER{
        let mut length=0;let source=ffi::lua_tobuffer(state,index,&mut length);
        let target=ffi::lua_newbuffer(state,length);std::ptr::copy_nonoverlapping(source.cast::<u8>(),target.cast::<u8>(),length);return
    }
    if ffi::lua_type(state,index)!=ffi::LUA_TTABLE{ffi::lua_pushvalue(state,index);return}
    if depth>128{ffi::luaL_error(state,c"BudgetExceeded: table conversion depth".as_ptr())}
    ffi::lua_pushvalue(state,index);ffi::lua_rawget(state,active);
    let cyclic=ffi::lua_toboolean(state,-1)!=0;ffi::lua_pop(state,1);
    if cyclic{ffi::lua_pushstring(state,c"tables cannot be cyclic".as_ptr());ffi::lua_error(state)}
    ffi::lua_pushvalue(state,index);ffi::lua_pushboolean(state,1);ffi::lua_rawset(state,active);
    let length=ffi::lua_objlen(state,index);
    if length>100_000{ffi::luaL_error(state,c"BudgetExceeded: table conversion entries".as_ptr())}
    ffi::lua_createtable(state,if json_mode{length as i32}else{0},0);let result=ffi::lua_gettop(state);
    if length>0{
        for i in 1..=length{
            *count+=1;if *count>100_000{ffi::luaL_error(state,c"BudgetExceeded: table conversion entries".as_ptr())}
            ffi::lua_rawgeti(state,index,i as i64);copy_variant(state,-1,active,depth+1,count,json_mode);
            ffi::lua_rawseti(state,result,i as i64);ffi::lua_pop(state,1);
        }
    }else{
        ffi::lua_pushnil(state);
        while ffi::lua_next(state,index)!=0{
            *count+=1;if *count>100_000{ffi::luaL_error(state,c"BudgetExceeded: table conversion entries".as_ptr())}
            variant_key(state,-2,json_mode);copy_variant(state,-2,active,depth+1,count,json_mode);ffi::lua_rawset(state,result);ffi::lua_pop(state,1);
        }
    }
    ffi::lua_pushvalue(state,index);ffi::lua_pushnil(state);ffi::lua_rawset(state,active);
}}
fn json_value(lua:&Lua,input:Value,depth:usize,count:&mut usize,bytes:&mut usize)->Result<Json>{
    *count+=1;
    if depth>128||*count>100_000{return Err(Error::runtime("BudgetExceeded: JSON traversal"))}
    Ok(match input{
        Value::Nil|Value::UserData(_)|Value::LightUserData(_)|Value::Vector(_)|Value::Function(_)|Value::Thread(_)|Value::Error(_)=>Json::Null,
        Value::Boolean(value)=>Json::Bool(value),
        Value::Integer(value)=>json!(value),
        Value::Number(value)if !value.is_finite()=>json!({"m":null,"t":"numeric","v":if value.is_nan(){"nan"}else if value.is_sign_negative(){"-inf"}else{"inf"}}),
        Value::Number(value)if value.fract()==0.0&&value>=i64::MIN as f64&&value<(i64::MAX as f64)=>json!(value as i64),
        Value::Number(value)=>json!(value),
        Value::String(value)=>{
            *bytes+=value.as_bytes().len();if *bytes>1024*1024{return Err(Error::runtime("BudgetExceeded: JSON string bytes"))}
            Json::String(value.to_str().map_err(|_|Error::lua_string("Can't convert to JSON"))?.to_string())
        },
        Value::Table(table)=>{
            let length=table.raw_len();
            if length>0{
                let mut out=Vec::with_capacity(length);for i in 1..=length{out.push(json_value(lua,table.raw_get(i)?,depth+1,count,bytes)?);}Json::Array(out)
            }else{
                let mut out=serde_json::Map::new();
                for entry in table.pairs::<mlua::LuaString,Value>(){
                    let(key,value)=entry?;*bytes+=key.as_bytes().len();if *bytes>1024*1024{return Err(Error::runtime("BudgetExceeded: JSON string bytes"))}
                    let key=key.to_str().map_err(|_|Error::lua_string("Can't convert to JSON"))?.to_string();
                    out.insert(key,json_value(lua,value,depth+1,count,bytes)?);
                }
                if out.is_empty(){Json::Array(Vec::new())}else{Json::Object(out)}
            }
        },
        Value::Buffer(_)=>return Err(unsupported(lua,"HttpService.JSONEncode buffer encoding")),
        _=>return Err(unsupported(lua,"HttpService.JSONEncode value type")),
    })
}
unsafe extern "C-unwind" fn raw_json_value(state:*mut ffi::lua_State)->std::ffi::c_int{unsafe{
    ffi::lua_settop(state,1);ffi::luaL_checkstack(state,12,c"JSON conversion".as_ptr());
    ffi::lua_newtable(state);let mut count=0;copy_variant(state,1,2,0,&mut count,true);
    ffi::lua_replace(state,1);ffi::lua_settop(state,1);
    Lua::invoke_native_callback(state,|lua,mut values|{
        let value=json_value(lua,values.pop_front().unwrap_or(Value::Nil),0,&mut 0,&mut 0)?;
        let output=serde_json::to_string(&value).map_err(Error::external)?;
        Ok(NativeCall::Return(args([str_value(lua,&output)?])))
    })
}}
unsafe extern "C-unwind" fn raw_json_method_convert(state:*mut ffi::lua_State)->std::ffi::c_int{unsafe{
    ffi::lua_settop(state,2);ffi::lua_remove(state,1);raw_json_value(state)
}}
unsafe extern "C-unwind" fn raw_json_encode(state:*mut ffi::lua_State)->std::ffi::c_int{unsafe{Lua::invoke_native_callback(state,|_,mut values|{
    let node=object_arg(&mut values)?;instance::validate_method(&node.borrow(),"JSONEncode")?;
    Ok(NativeCall::Dispatch(raw_json_method_convert))
})}}
pub(crate) fn json_encode_function(lua:&Lua)->Result<Function>{unsafe{raw_function(lua,raw_json_value,c"JSONEncode")}}
fn finish_variant(lua:&Lua,input:Value)->Result<Value>{
    Ok(match input{
        Value::Thread(_)=>Value::Nil,
        Value::UserData(ref instance)if instance.is::<Instance>()=>input,
        Value::UserData(ref data)=>{
            if let Ok(value)=data.borrow::<crate::types::Vector2>(){return (*value).into_lua(lua)}
            if let Ok(value)=data.borrow::<crate::types::CFrame>(){return (*value).into_lua(lua)}
            if let Ok(value)=data.borrow::<crate::types::Color3>(){return (*value).into_lua(lua)}
            if let Ok(value)=data.borrow::<crate::types::Ray>(){return (*value).into_lua(lua)}
            match crate::types::encode_value(&input)?{Some(value)=>crate::types::decode_value(lua,&value)?.unwrap_or(Value::Nil),None=>Value::Nil}
        },
        Value::Table(table)=>{for pair in table.clone().pairs::<Value,Value>(){let(key,value)=pair?;table.raw_set(key,finish_variant(lua,value)?)?;}Value::Table(table)},
        _=>input,
    })
}
unsafe extern "C-unwind" fn raw_fire(state:*mut ffi::lua_State)->std::ffi::c_int{unsafe{Lua::invoke_native_callback(state,|_,mut values|{
    let node=object_arg(&mut values)?;
    if node.borrow().class!="BindableEvent"{return Err(Error::lua_string("Expected ':' not '.' calling member function Fire"))}
    Ok(NativeCall::Dispatch(raw_fire_convert))
})}}
unsafe extern "C-unwind" fn raw_fire_convert(state:*mut ffi::lua_State)->std::ffi::c_int{unsafe{
    let top=ffi::lua_gettop(state);ffi::luaL_checkstack(state,12,c"Fire conversion".as_ptr());
    ffi::lua_newtable(state);let active=ffi::lua_gettop(state);let mut count=0;
    for index in 2..=top{copy_variant(state,index,active,0,&mut count,false);ffi::lua_replace(state,index);}
    ffi::lua_settop(state,top);raw_fire_impl(state)
}}
unsafe extern "C-unwind" fn raw_fire_impl(state:*mut ffi::lua_State)->std::ffi::c_int{unsafe{Lua::invoke_native_callback(state,|lua,mut values|{
    let node=object_arg(&mut values)?;let event=signal(lua,&node,"Event")?;
    let mut copied=MultiValue::with_capacity(values.len());for value in values{copied.push_back(finish_variant(lua,value)?);}
    crate::scheduler::fire_signal(lua,&event,copied)?;Ok(NativeCall::Return(MultiValue::new()))
})}}
unsafe extern "C-unwind" fn raw_wait_convert(state:*mut ffi::lua_State)->std::ffi::c_int{unsafe{
    let top=ffi::lua_gettop(state);ffi::luaL_checkstack(state,8,c"WaitForChild conversion".as_ptr());
    match ffi::lua_type(state,2){
        ffi::LUA_TTABLE=>{ffi::lua_newtable(state);let active=ffi::lua_gettop(state);let mut count=0;copy_variant(state,2,active,0,&mut count,false);ffi::lua_settop(state,top);ffi::lua_pushstring(state,c"{}".as_ptr());ffi::lua_replace(state,2);},
        ffi::LUA_TBOOLEAN=>{ffi::lua_pushstring(state,if ffi::lua_toboolean(state,2)!=0{c"true".as_ptr()}else{c"false".as_ptr()});ffi::lua_replace(state,2);},
        ffi::LUA_TNUMBER|ffi::LUA_TINTEGER=>{ffi::lua_tolstring(state,2,std::ptr::null_mut());},
        ffi::LUA_TFUNCTION=>{ffi::lua_pushstring(state,c"function".as_ptr());ffi::lua_replace(state,2);},
        ffi::LUA_TTHREAD|ffi::LUA_TUSERDATA=>{ffi::lua_pushstring(state,c"nil".as_ptr());ffi::lua_replace(state,2);},
        _=>{},
    }
    raw_wait_impl(state)
}}
unsafe extern "C-unwind" fn raw_wait_for_child(state:*mut ffi::lua_State)->std::ffi::c_int{unsafe{Lua::invoke_native_callback(state,|lua,mut values|{
    object_arg(&mut values)?;
    let value=values.front().ok_or_else(||Error::lua_string("Argument 1 missing or nil"))?;
    if value.is_nil(){return Err(Error::lua_string("Argument 1 missing or nil"))}
    if matches!(value,Value::UserData(value)if value.is::<Instance>())||crate::types::encode_value(value)?.is_some(){return Err(unsupported(lua,"Instance.WaitForChild typed value conversion"))}
    Ok(NativeCall::Dispatch(raw_wait_convert))
})}}
unsafe extern "C-unwind" fn raw_wait_impl(state:*mut ffi::lua_State)->std::ffi::c_int{unsafe{Lua::invoke_native_callback(state,|lua,mut values|{
    let node=object_arg(&mut values)?;let name=text_arg(&mut values)?;
    let timeout=match values.pop_front(){None|Some(Value::Nil)=>None,Some(value)=>Some(lua.coerce_number(value)?.ok_or_else(||Error::lua_string("Invalid timeout"))?)};
    if timeout.is_some_and(|value|!value.is_finite()||value<=0.0){return Err(Error::lua_string("illegal argument #2 (timeOut must be greater than 0)"))}
    if let Some(child)=instance::find_child(&node,&name,false){return Ok(NativeCall::Return(args([value(lua,child)?])))}
    let model=required(lua)?;reserve_waiter(&mut model.borrow_mut())?;
    crate::scheduler::park_current(lua)?;let thread=lua.current_thread();model.borrow_mut().children_waiting.push(ChildWait{parent:node.borrow().id.clone(),name,thread,deadline:timeout.map(|n|now(lua)+n)});
    Ok(NativeCall::Yield(MultiValue::new()))
})}}
unsafe extern "C-unwind" fn raw_invoke(state:*mut ffi::lua_State)->std::ffi::c_int{unsafe{Lua::invoke_native_callback(state,|lua,mut values|{
    let node=object_arg(&mut values)?;if node.borrow().class!="BindableFunction"{return Err(Error::lua_string("BindableFunction expected"))}
    let thread=lua.current_thread();
    let callback=node.borrow().properties.get("OnInvoke").cloned();
    if let Some(Value::Function(callback))=callback{if let Some(result)=invoke_callback(lua,&node,callback,values,thread,true)?{return Ok(match result{Ok(v)=>NativeCall::Return(v),Err(e)=>NativeCall::Error(e)})}crate::scheduler::park_current(lua)?;}
    else{let model=required(lua)?;reserve_waiter(&mut model.borrow_mut())?;crate::scheduler::park_current(lua)?;model.borrow_mut().invokes_waiting.push((node.borrow().id.clone(),thread,values));}
    Ok(NativeCall::Yield(MultiValue::new()))
})}}
fn invoke_remote(lua:&Lua,mut values:MultiValue,to_server:bool)->Result<NativeCall>{
    let node=object_arg(&mut values)?;if node.borrow().class!="RemoteFunction"{return Err(Error::lua_string("RemoteFunction expected"))}
    let target=if to_server{"server".to_string()}else{let player=object_arg(&mut values)?;if player.borrow().class!="Player"{return Err(Error::lua_string("Player expected"))}let id=player.borrow().id.clone();id};
    let state=required(lua)?;let request={let mut state=state.borrow_mut();if (state.side=="client")!=to_server{return Err(Error::lua_string("RemoteFunction called from wrong side"))}state.requests+=1;format!("{}:request:{}",state.realm,state.requests)};
    let args=encode_args(lua,values)?;reserve_waiter(&mut state.borrow_mut())?;
    crate::scheduler::park_current(lua)?;state.borrow_mut().pending.insert(request.clone(),lua.current_thread());
    if let Err(error)=emit(lua,"remote_call",json!({"remoteId":node.borrow().id,"target":target,"requestId":request,"args":args})){state.borrow_mut().pending.remove(&request);return Err(error)}
    Ok(NativeCall::Yield(MultiValue::new()))
}
unsafe extern "C-unwind" fn raw_invoke_server(state:*mut ffi::lua_State)->std::ffi::c_int{unsafe{Lua::invoke_native_callback(state,|lua,args|invoke_remote(lua,args,true))}}
unsafe extern "C-unwind" fn raw_invoke_client(state:*mut ffi::lua_State)->std::ffi::c_int{unsafe{Lua::invoke_native_callback(state,|lua,args|invoke_remote(lua,args,false))}}
unsafe extern "C-unwind" fn raw_require(state:*mut ffi::lua_State)->std::ffi::c_int{unsafe{Lua::invoke_native_callback(state,|lua,mut values|{
    let node=object_arg(&mut values)?;if node.borrow().class!="ModuleScript"{return Err(Error::lua_string("ModuleScript expected"))}
    if crate::scheduler::current_mode(lua)=="parallel"{return Err(Error::lua_string("require cannot be called in parallel"))}
    let id=node.borrow().id.clone();let state=required(lua)?;let caller=lua.current_thread();
    {let mut state=state.borrow_mut();if let Some(Module::Loading{owner,..})=state.modules.get(&id){if owner.to_pointer()==caller.to_pointer(){return Err(Error::lua_string("Requested module was required recursively"))}reserve_waiter(&mut state)?;}
    if let Some(module)=state.modules.get_mut(&id){match module{
        Module::Ready(value)=>return Ok(NativeCall::Return(args([value.clone()]))),
        Module::Failed(error)=>return Ok(NativeCall::Error(error.clone())),
        Module::Loading{waiters,..}=>{crate::scheduler::park_current(lua)?;waiters.push(caller);return Ok(NativeCall::Yield(MultiValue::new()))}
    }}}
    reserve_waiter(&mut state.borrow_mut())?;
    let function=compile(lua,&node)?;let child=lua.create_thread(function.clone())?;let name=node.borrow().name.clone();
    thread_environment(lua,&child,&node)?;
    crate::scheduler::set_script_category(lua,&child,&function,&name)?;
    state.borrow_mut().modules.insert(id.clone(),Module::Loading{owner:child.clone(),waiters:vec![caller.clone()]});
    let inline=Rc::new(Cell::new(true));let in_callback=inline.clone();let caller_id=caller.to_pointer()as usize;let module_id=id.clone();
    crate::scheduler::spawn_tracked(lua,Value::Thread(child),MultiValue::new(),Some("serial"),Some("game"),Rc::new(move|lua,result|{
        let state=required(lua)?;let waiters=match state.borrow_mut().modules.remove(&id){Some(Module::Loading{waiters,..})=>waiters,_=>vec![]};
        let result=match result{Ok(mut values)if values.len()==1&&!values.front().is_none_or(Value::is_nil)=>Ok(values.pop_front().unwrap()),Ok(_)=>Err(str_value(lua,"Module code did not return exactly one value")?),Err(_)=>Err(str_value(lua,"Requested module experienced an error while loading")?)};
        match &result{Ok(value)=>{state.borrow_mut().modules.insert(id.clone(),Module::Ready(value.clone()));},Err(error)=>{state.borrow_mut().modules.insert(id.clone(),Module::Failed(error.clone()));}}
        for waiter in waiters{if in_callback.get()&&waiter.to_pointer()as usize==caller_id{continue}match &result{Ok(v)=>finish_waiter(lua,waiter,Ok(args([v.clone()])))?,Err(e)=>finish_waiter(lua,waiter,Err(e.clone()))?,}}
        Ok(())
    }))?;inline.set(false);
    {let state=state.borrow();match state.modules.get(&module_id){Some(Module::Ready(v))=>return Ok(NativeCall::Return(args([v.clone()]))),Some(Module::Failed(e))=>return Ok(NativeCall::Error(e.clone())),_=>{}}}
    if let Err(error)=crate::scheduler::park_current(lua){if let Some(Module::Loading{waiters,..})=state.borrow_mut().modules.get_mut(&module_id){waiters.retain(|thread|thread.to_pointer()!=caller.to_pointer());}return Err(error)}Ok(NativeCall::Yield(MultiValue::new()))
})}}

unsafe fn raw_function(lua:&Lua,callback:ffi::lua_CFunction,name:&'static CStr)->Result<Function>{unsafe{lua.exec_raw((),|state|ffi::lua_pushcfunctiond(state,callback,name.as_ptr()))}}
unsafe extern "C-unwind" fn raw_preload_async(state:*mut ffi::lua_State)->std::ffi::c_int{unsafe{
    Lua::invoke_native_callback(state,|lua,values|{
        if !instance::has_instance_self(&values){return Err(Error::lua_string("Expected ':' not '.' calling member function PreloadAsync"))}
        preload_async(lua,values)
    })
}}
unsafe extern "C-unwind" fn raw_namecall(state:*mut ffi::lua_State)->std::ffi::c_int{unsafe{
    let name=ffi::lua_namecallatom(state,std::ptr::null_mut());
    let callback=ffi::lua_tocfunction(state,ffi::lua_upvalueindex(2)).expect("native namecall dispatcher");
    Lua::invoke_native_callback(state,|_,mut values|{
        if !instance::has_instance_self(&values){return Ok(NativeCall::Dispatch(instance::invalid_instance))}
        if name.is_null(){return Err(Error::lua_string("attempt to call an unknown method"))}
        let name=CStr::from_ptr(name).to_str().map_err(Error::external)?;
        let node=object_arg(&mut values)?;instance::validate_method(&node.borrow(),name)?;
        let callback=match name{"JSONEncode"=>raw_json_encode,"Fire"=>raw_fire,"WaitForChild"=>raw_wait_for_child,"Invoke"=>raw_invoke,"InvokeServer"=>raw_invoke_server,"InvokeClient"=>raw_invoke_client,"PreloadAsync"=>raw_preload_async,_=>callback};
        Ok(NativeCall::Dispatch(callback))
    })
}}
pub(crate) fn install_yielding_methods(lua:&Lua,game:&AnyUserData,methods:&Table)->Result<()>{unsafe{
    methods.raw_set("PreloadAsync",raw_function(lua,raw_preload_async,c"PreloadAsync")?)?;
    for(name,cname,callback)in[("JSONEncode",c"JSONEncode",raw_json_encode as ffi::lua_CFunction),("Fire",c"Fire",raw_fire),("WaitForChild",c"WaitForChild",raw_wait_for_child),("Invoke",c"Invoke",raw_invoke),("InvokeServer",c"InvokeServer",raw_invoke_server),("InvokeClient",c"InvokeClient",raw_invoke_client)]{methods.raw_set(name,raw_function(lua,callback,cname)?)?;}
    let original:Function=game.metatable()?.get("__namecall")?;
    let replacement:Function=lua.exec_raw(original,|state|{ffi::lua_getupvalue(state,1,1);ffi::lua_pushvalue(state,1);ffi::lua_pushcclosurek(state,raw_namecall,std::ptr::null(),2,None);ffi::lua_remove(state,1);})?;
    lua.exec_raw::<()>((game.clone(),replacement),|state|{ffi::lua_getmetatable(state,1);ffi::lua_pushvalue(state,2);ffi::lua_rawsetfield(state,-2,c"__namecall".as_ptr());ffi::lua_settop(state,0);})?;Ok(())
}}

// Host completion metadata mirrors implemented defaults without constructing their
// Lua values. In particular, looking up a signal or CFrame must not allocate one.
const COMPLETION_PROPERTIES: &[(&str, &str, &str)] = &[
    ("Instance", "Name ClassName", "string"), ("Instance", "Parent", "Instance | nil"),
    ("Instance", "Archivable", "boolean"), ("LuaSourceContainer", "LinkedSource", "string"),
    ("LuaSourceContainer", "Source", "string (restricted)"),
    ("BaseScript|LayerCollector", "Enabled", "boolean"), ("BaseScript", "Disabled", "boolean"),
    ("BindableFunction", "OnInvoke", "function | nil"), ("RemoteFunction", "OnServerInvoke OnClientInvoke", "function | nil"),
    ("Model", "PrimaryPart", "BasePart | nil"), ("Player", "Character", "Model | nil"),
    ("Players", "LocalPlayer", "Player | nil"), ("Player", "UserId AccountAge", "number"), ("Player", "DisplayName", "string"),
    ("Workspace", "CurrentCamera", "Camera | nil"), ("Workspace", "Gravity DistributedGameTime", "number"),
    ("Camera", "ViewportSize", "Vector2"), ("Camera", "NearPlaneZ FieldOfView", "number"),
    ("Camera", "FieldOfViewMode", "Enum.FieldOfViewMode"), ("Camera", "CFrame", "CFrame"),
    ("DataModel", "PlaceId GameId PlaceVersion CreatorId", "number"), ("DataModel", "JobId", "string"),
    ("StringValue", "Value", "string"), ("BoolValue", "Value", "boolean"),
    ("NumberValue|IntValue", "Value", "number"), ("ObjectValue", "Value", "Instance | nil"),
    ("BasePart", "Anchored Massless CanCollide CanTouch CanQuery", "boolean"), ("BasePart", "Transparency", "number"),
    ("Humanoid", "Health MaxHealth WalkSpeed JumpPower HipHeight JumpHeight", "number"), ("Humanoid", "UseJumpPower", "boolean"),
    ("GuiObject", "Visible Active", "boolean"), ("GuiObject", "Size Position", "UDim2"), ("GuiObject", "ZIndex", "number"),
    ("GuiObject", "BackgroundTransparency", "number"), ("TextLabel|TextButton", "Text", "string"),
    ("ImageLabel|ImageButton", "Image", "string"), ("ImageLabel|ImageButton", "IsLoaded", "boolean"),
    ("ImageLabel|ImageButton", "ImageTransparency", "number"), ("LayerCollector", "ResetOnSpawn", "boolean"), ("ScreenGui", "DisplayOrder", "number"),
    ("UserInputService|VRService", "TouchEnabled GamepadEnabled VREnabled", "boolean"),
    ("UserInputService", "KeyboardEnabled MouseEnabled", "boolean"),
    ("Stats", "InstanceCount", "number"), ("Stats", "MemoryTrackingEnabled", "boolean"),
    ("Lighting", "Brightness ClockTime FogStart FogEnd ExposureCompensation", "number"),
    ("Lighting", "GlobalShadows", "boolean"), ("Lighting", "Ambient OutdoorAmbient", "Color3"),
    ("GuiService", "MenuIsOpen IsWindows", "boolean"), ("TextChatService", "ChatVersion", "Enum.ChatVersion"),
    ("ContentProvider", "RequestQueueSize", "number"),
];
const COMPLETION_EVENTS: &str = "ChildAdded ChildRemoved DescendantAdded DescendantRemoving AncestryChanged Changed Destroying AttributeChanged Loaded Event OnServerEvent OnClientEvent PlayerAdded PlayerRemoving CharacterAdded CharacterRemoving Idled Heartbeat Stepped RenderStepped PreSimulation PostSimulation PreRender MessageOut AssetFetchFailed Died StateChanged Error InputBegan InputEnded InputChanged WindowFocused WindowFocusReleased";

fn completion_class(node: &instance::Node, classes: &str) -> bool { classes.split('|').any(|class| instance::is_a(node, class)) }
fn completion_label(label: &str) -> bool { !label.is_empty() && label.len() <= 256 && !label.contains('\0') }
fn completion_item(label: &str, kind: &str, detail: &str, info: &str) -> Json {
    json!({"label":label,"type":kind,"detail":detail,"info":info})
}
fn completion_target(model: &Model, node: &Link, key: &str) -> Option<Link> {
    let node = node.borrow();
    if instance::method_class(key).is_some_and(|class| instance::is_a(&node, class)) { return None; }
    if let Some(value) = node.properties.get(key) {
        return if let Value::UserData(value) = value { value.borrow::<Instance>().ok().map(|value| value.0.clone()) } else { None };
    }
    if key == "Parent" { return node.parent.upgrade(); }
    if key == "LocalPlayer" && node.class == "Players" {
        return (model.side == "client").then(|| model.nodes.get(&model.client).and_then(Weak::upgrade)).flatten();
    }
    if COMPLETION_PROPERTIES.iter().any(|(classes, names, _)| completion_class(&node, classes) && names.split_whitespace().any(|name|name == key))
        || event_class(key).is_some_and(|class| instance::is_a(&node, class)) { return None; }
    node.children.iter().take(4096).find(|child| { let child=child.borrow(); !child.destroyed && child.name == key }).cloned()
}

pub(crate) fn completions(lua: &Lua, path: &[String]) -> Result<Vec<Json>> {
    let state = required(lua)?;
    let model = state.borrow();
    if path.is_empty() {
        return Ok([("game", "game", "DataModel"), ("workspace", "Workspace", "Workspace")].into_iter()
            .filter(|(_,id,_)| model.nodes.get(*id).and_then(Weak::upgrade).is_some())
            .map(|(label,_,class)| completion_item(label,"variable",class,"DataModel root in the selected VM")).collect());
    }
    let id = match path[0].as_str() { "game" | "Game" => "game", "workspace" | "Workspace" => "Workspace", _ => return Ok(Vec::new()) };
    let Some(mut node) = model.nodes.get(id).and_then(Weak::upgrade) else { return Ok(Vec::new()); };
    for key in &path[1..] {
        let Some(next) = completion_target(&model, &node, key) else { return Ok(Vec::new()); };
        node = next;
    }
    let node = node.borrow();
    if node.destroyed { return Ok(Vec::new()); }
    let mut items = BTreeMap::new();
    for (classes, names, detail) in COMPLETION_PROPERTIES {
        if completion_class(&node, classes) {
            for name in names.split_whitespace() { items.insert(name.to_owned(), completion_item(name,"property",detail,"Implemented Instance property")); }
        }
    }
    for name in COMPLETION_EVENTS.split_whitespace() {
        if event_class(name).is_some_and(|class| instance::is_a(&node, class)) {
            items.insert(name.to_owned(), completion_item(name,"property","RBXScriptSignal","Instance event; completion does not create the signal"));
        }
    }
    for (name,value) in node.properties.iter().take(512) {
        if !completion_label(name) { continue; }
        let detail = match value { Value::UserData(value) => value.borrow::<Instance>().ok().map(|instance|instance.0.borrow().class.clone()), _ => None };
        items.insert(name.clone(), completion_item(name,"property",detail.as_deref().unwrap_or(value.type_name()),"Stored Instance property in the selected VM"));
    }
    // Raw iteration reads existing key objects and never interns the user's path.
    let methods: Table = lua.named_registry_value(instance::METHODS)?;
    for entry in methods.pairs::<Value,Value>().take(512) {
        let (Value::String(name),Value::Function(_)) = entry? else { continue; };
        let Ok(name) = name.to_str() else { continue; };
        if !completion_label(&name) || !instance::method_class(&name).is_some_and(|class|instance::is_a(&node,class)) { continue; }
        let info = if UNSUPPORTED.iter().any(|(method,_)|*method == &*name) { "Known method; this emulator reports UnsupportedApi" } else { "Native Instance method in the selected VM" };
        items.insert(name.to_string(), completion_item(&name,"method","function",info));
    }
    for child in node.children.iter().take(4096) {
        if items.len() >= 256 { break; }
        let child = child.borrow();
        if child.destroyed || !completion_label(&child.name) { continue; }
        items.entry(child.name.clone()).or_insert_with(||completion_item(&child.name,"property",&child.class,"DataModel child in the selected VM"));
    }
    Ok(items.into_values().take(256).collect())
}

#[cfg(all(test, emulator_local_tests))]
#[path = "../local_tests/model.rs"]
mod tests;
