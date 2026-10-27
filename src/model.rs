use crate::instance::{self, Attribute, Instance, Link};
use mlua::{AnyUserData, Error, Function, IntoLua, Lua, LuaSerdeExt, MultiValue, Result, Table, Thread, UserDataMethods, Value, ffi, state::NativeCall};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use std::{cell::{Cell, RefCell}, collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque}, ffi::CStr, rc::{Rc, Weak}};
#[path = "animation.rs"] mod animation;

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
    paths: HashMap<String, (Weak<RefCell<instance::Node>>, Vec<crate::types::Path2DControlPoint>)>,
    tweens: HashMap<String, Tween>,
    animation_tracks: HashMap<String, animation::Track>,
}
#[derive(Clone)]
struct Tween {
    owner: Weak<RefCell<instance::Node>>, active: Option<Link>, target: Link,
    info: crate::types::TweenInfo, goals: Vec<(String, Value)>, initial: Vec<Value>,
    elapsed: f64, started: f64, playback: &'static str, revision: u64,
}
struct BoundAction { _callback:Function, inputs:Vec<Value>, priority:i64, order:u64 }
#[derive(Clone, Copy, PartialEq)]
enum AssetFetch { Loading, Failure }
#[derive(Clone)]
struct AssetResource { key: String, content: String }
struct Change { node: Link, keys: Option<BTreeMap<&'static str, BTreeSet<String>>> }
struct ChildWait { parent: String, name: Vec<u8>, thread: Thread, deadline: Option<f64> }
enum Module { Loading { owner: Thread, waiters: Vec<Thread> }, Ready(Value), Failed(Value) }
type State = Rc<RefCell<Model>>;
#[derive(Clone)]
pub(crate) struct MessageSink(pub Rc<dyn Fn(String,Json)->Result<()>>);
// Host ownership contains no Lua callback: execution always stays in the registering VM.
#[derive(Clone,Default)]
pub(crate) struct BindableRegistry(Rc<RefCell<BindableRecords>>);
#[derive(Default)]
struct BindableRecords {
    owners:HashMap<(String,String,String),(String,Weak<RefCell<instance::Node>>)>,
    events:HashMap<(String,String,String),BTreeMap<String,Weak<RefCell<instance::Node>>>>,
}
fn prune_bindables(records:&mut BindableRecords)->Result<()>{
    let alive=|node:&Weak<RefCell<instance::Node>>|node.upgrade().is_some_and(|node|!node.borrow().destroyed);
    records.owners.retain(|_,(_,node)|alive(node));
    records.events.retain(|_,realms|{realms.retain(|_,node|alive(node));!realms.is_empty()});
    if records.owners.len()+records.events.len()>=MAX_NODES{return Err(Error::runtime("BudgetExceeded: shared Bindable metadata"))}Ok(())
}
fn bindable_key(lua:&Lua,id:&str)->Result<(String,String,String)>{let state=required(lua)?;let state=state.borrow();Ok((state.side.clone(),state.client.clone(),id.into()))}
pub(crate) fn bindable_owner(lua:&Lua,id:&str)->Result<Option<String>>{
    let Some(registry)=lua.app_data_ref::<BindableRegistry>().map(|registry|registry.clone())else{return Ok(None)};
    let key=bindable_key(lua,id)?;let mut records=registry.0.borrow_mut();
    if records.owners.get(&key).is_some_and(|(_,node)|node.upgrade().is_none_or(|node|node.borrow().destroyed)){records.owners.remove(&key);}
    Ok(records.owners.get(&key).map(|(realm,_)|realm.clone()))
}
pub(crate) fn bindable_request_alive(lua:&Lua,id:&str)->Result<bool>{
    let state=required(lua)?;let mut state=state.borrow_mut();
    if state.pending.get(id).is_some_and(Thread::is_resumable){return Ok(true)}
    state.pending.remove(id);Ok(false)
}
fn register_bindable(lua:&Lua,node:&Link,enabled:bool)->Result<()>{
    let Some(registry)=lua.app_data_ref::<BindableRegistry>().map(|registry|registry.clone())else{return Ok(())};
    let id=node.borrow().id.clone();let key=bindable_key(lua,&id)?;
    if enabled{let realm=required(lua)?.borrow().realm.clone();let mut records=registry.0.borrow_mut();if !records.owners.contains_key(&key)&&((records.owners.len()+records.events.len())%1024==0||records.owners.len()+records.events.len()>=MAX_NODES){prune_bindables(&mut records)?;}records.owners.insert(key,(realm,Rc::downgrade(node)));}
    else{registry.0.borrow_mut().owners.remove(&key);}
    emit(lua,"bindable_handler",json!({"remoteId":id}))
}
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
    if node.borrow().class=="AnimationTrack"{return}
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
    if name=="Event"&&node.borrow().class=="BindableEvent"{
        if let Some(registry)=lua.app_data_ref::<BindableRegistry>().map(|registry|registry.clone()){
            let key=bindable_key(lua,&node.borrow().id)?;let realm=required(lua)?.borrow().realm.clone();let mut records=registry.0.borrow_mut();
            if !records.events.contains_key(&key)&&((records.owners.len()+records.events.len())%1024==0||records.owners.len()+records.events.len()>=MAX_NODES){prune_bindables(&mut records)?;}
            records.events.entry(key).or_default().insert(realm,Rc::downgrade(node));
        }
    }
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
    if node.borrow().destroyed {return Ok(())}
    if node.borrow().class=="AnimationTrack"&&matches!(animation::read(lua,node,"IsPlaying")?,Some(Value::Boolean(true))){animation::control(lua,node,"Stop",args([Value::Number(0.)]),false)?;}
    fire(lua,node,"Destroying",MultiValue::new())?;
    let children = node.borrow().children.clone();
    for child in children { destroy(lua,&child)?; }
    parent(lua,node,None)?;
    let events: Vec<_> = node.borrow().signals.values().cloned().collect();
    for event in events { crate::scheduler::disconnect_all(lua,&event)?; }
    if let Some(registry)=lua.app_data_ref::<BindableRegistry>().map(|registry|registry.clone()){
        let key=bindable_key(lua,&node.borrow().id)?;let mut records=registry.0.borrow_mut();records.owners.remove(&key);records.events.remove(&key);
    }
    let mut n = node.borrow_mut();
    n.signals.clear();
    n.callback_environments.clear();
    n.properties.retain(|_,v| !matches!(v,Value::Function(_)));
    n.destroyed = true;
    drop(n);
    if let Some(state)=state(lua){let mut state=state.borrow_mut();let id=node.borrow().id.clone();state.paths.remove(&id);state.tweens.remove(&id);}
    mark_key(lua,node,"Destroyed",false);
    Ok(())
}

fn event_class(name: &str) -> Option<&'static str> {
    match name {
        "Completed" => Some("TweenBase"),
        "AnimationPlayed" => Some("Animator"), "Stopped"|"Ended"|"DidLoop"|"KeyframeReached" => Some("AnimationTrack"),
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
        "Instance" if is("Tween")=>Some(Value::Nil),
        "PlaybackState" if is("TweenBase")=>crate::types::decode_value(lua,&json!({"tag":"EnumItem","enum":"PlaybackState","name":"Begin"}))?,
        "TweenInfo" if is("Tween")=>Some(crate::types::TweenInfo{time:1.,style:"Quad".into(),direction:"Out".into(),repeat_count:0,reverses:false,delay:0.}.into_lua(lua)?),
        "Character" if is("Player")=>Some(Value::Nil),
        "AnimationId" if is("Animation")=>Some(str_value(lua,"")?),
        "Looped" if is("AnimationTrack")=>Some(Value::Boolean(false)),
        "Priority" if is("AnimationTrack")=>crate::types::decode_value(lua,&json!({"tag":"EnumItem","enum":"AnimationPriority","name":"Core"}))?,
        "UserId"|"AccountAge" if is("Player")=>Some(Value::Integer(0)),
        "DisplayName" if is("Player")=>Some(Value::String(lua.create_string(instance::name_bytes(&n))?)),
        "CurrentCamera" if is("Workspace")=>Some(Value::Nil),
        "ViewportSize" if is("Camera")=>Some(crate::types::Vector([1.0,1.0]).into_lua(lua)?),
        "NearPlaneZ" if is("Camera")=>Some(Value::Number(-0.5)),
        "FieldOfView" if is("Camera")=>Some(Value::Number(70.0)),
        "FieldOfViewMode" if is("Camera")=>crate::types::decode_value(lua,&json!({"tag":"EnumItem","enum":"FieldOfViewMode","name":"Vertical"}))?,
        "CFrame" if is("Camera")=>Some(crate::types::CFrame{p:crate::types::Vector([0.0;3]),r:[[1.0,0.0,0.0],[0.0,1.0,0.0],[0.0,0.0,1.0]]}.into_lua(lua)?),
        "CFrame" if is("BasePart")&&n.class!="Terrain"=>Some(crate::spatial::identity().into_lua(lua)?),
        "Position" if is("BasePart")&&n.class!="Terrain"=>Some(Value::Vector(mlua::Vector::zero())),
        "Size" if is("BasePart")&&n.class!="Terrain"=>Some(Value::Vector(if n.class=="Part"{mlua::Vector::new(4.,1.2,2.)}else{mlua::Vector::new(1.,1.,1.)})),
        "Shape" if n.class=="Part"=>crate::types::decode_value(lua,&json!({"tag":"EnumItem","enum":"PartType","name":"Block"}))?,
        "Material" if is("BasePart")&&n.class!="Terrain"=>crate::types::decode_value(lua,&json!({"tag":"EnumItem","enum":"Material","name":"Plastic"}))?,
        "CollisionGroup" if is("BasePart")&&n.class!="Terrain"=>Some(str_value(lua,"Default")?),
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
        "Visible" if is("GuiObject")||is("Path2D")=>Some(Value::Boolean(true)),
        "Closed" if is("Path2D")=>Some(Value::Boolean(false)),
        "Thickness" if is("Path2D")=>Some(Value::Number(1.0)),
        "Color3" if is("Path2D")=>Some(crate::types::Color3([0.;3]).into_lua(lua)?),
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
        "AccelerometerEnabled"|"GyroscopeEnabled" if is("UserInputService")=>Some(Value::Boolean(false)),
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
    if node.borrow().class=="AnimationTrack"{if let Some(value)=animation::read(lua,node,key)?{return Ok(Some(value))}}
    if instance::is_a(&node.borrow(),"BasePart")&&node.borrow().class!="Terrain"{
        match key{"Position"=>return Ok(Some(crate::spatial::position_value(node)?)),"CFrame"|"ExtentsCFrame"=>return Ok(Some(crate::spatial::part_frame(node)?.into_lua(lua)?)),"ExtentsSize"=>return Ok(Some(crate::spatial::extents_value(node)?)),_=>{}}
    }
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
    if instance::is_a(&node.borrow(),"TweenBase")&&matches!(key,"Instance"|"TweenInfo"|"PlaybackState"){return Err(Error::lua_string(format!("Unable to assign property {key}. Property is read only")))}
    if node.borrow().class=="AnimationTrack"&&animation::write(lua,node,key,new)?{return Ok(true)}
    if key=="AnimationId"&&node.borrow().class=="Animation"&&!matches!(new,Value::String(_)){return Err(Error::lua_string("AnimationId must be a string"))}
    if key=="Character"&&node.borrow().class=="Player" {
        let next=match new{Value::Nil=>None,Value::UserData(value)=>Some(value.borrow::<Instance>().map_err(|_|Error::lua_string("Character must be a Model"))?.0.clone()),_=>return Err(Error::lua_string("Character must be a Model"))};
        if next.as_ref().is_some_and(|node|!instance::is_a(&node.borrow(),"Model")){return Err(Error::lua_string("Character must be a Model"))}
        let previous=node.borrow().properties.get("Character").cloned().unwrap_or(Value::Nil);
        if previous==*new{return Ok(true)}
        if !previous.is_nil(){fire(lua,node,"CharacterRemoving",args([previous]))?;}
        node.borrow_mut().properties.insert(key.into(),new.clone());changed(lua,node,key)?;
        if next.is_some(){fire(lua,node,"CharacterAdded",args([new.clone()]))?;}return Ok(true)
    }
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
    if node.borrow().class=="Path2D" {
        let valid=match key{"Visible"|"Closed"=>matches!(new,Value::Boolean(_)),"Thickness"=>matches!(new,Value::Number(_)|Value::Integer(_)),"Color3"=>matches!(new,Value::UserData(value)if value.is::<crate::types::Color3>()),_=>true};
        if !valid{return Err(Error::lua_string(format!("Unable to assign property {key}. Invalid value")))}
    }
    if instance::is_a(&node.borrow(),"BasePart"){
        if matches!(key,"ExtentsCFrame"|"ExtentsSize"){return Err(Error::lua_string(format!("Unable to assign property {key}. Property is read only")))}
        if node.borrow().class=="Terrain"&&matches!(key,"CFrame"|"Position"|"Size"|"Shape"|"Material"|"CollisionGroup"){return Err(unsupported(lua,&format!("Terrain.{key} mutation")))}
        if matches!(key,"CFrame"|"Position"){
            let before=crate::spatial::part_frame(node)?;
            let next=if key=="CFrame"{crate::spatial::frame(new)?}else{crate::types::CFrame{p:crate::types::Vector(crate::spatial::vector(new)?.map(|v|v as f32)),..before}};
            if before==next{return Ok(true)}
            let frame=next.into_lua(lua)?;let position=Value::Vector(mlua::Vector::new(next.p.0[0],next.p.0[1],next.p.0[2]));
            {let mut node=node.borrow_mut();node.properties.insert("CFrame".into(),frame);node.properties.insert("Position".into(),position);}
            changed(lua,node,"CFrame")?;if before.p!=next.p{changed(lua,node,"Position")?;}return Ok(true)
        }
        if key=="Size"{
            let size=crate::spatial::vector(new)?.map(|v|(v as f32).clamp(0.001,2048.));
            let next=Value::Vector(mlua::Vector::new(size[0],size[1],size[2]));
            if previous()?.as_ref().is_some_and(|old|same(old,&next)){return Ok(true)}
            node.borrow_mut().properties.insert(key.into(),next);changed(lua,node,key)?;return Ok(true)
        }
        if key=="Shape"&&node.borrow().class=="Part"||key=="Material"{
            let family=if key=="Shape"{"PartType"}else{"Material"};
            let encoded=crate::types::encode_value(new)?;let item=if encoded.as_ref().is_some_and(|value|value["tag"]=="EnumItem"&&value["enum"]==family){new.clone()}
                else if let Value::String(name)=new{crate::types::decode_value(lua,&json!({"tag":"EnumItem","enum":family,"name":name.to_str()?.as_ref()}))?.ok_or_else(||Error::lua_string(format!("Unable to assign property {key}")))?}
                else if let Some(number)=match new{Value::Integer(value)=>Some(*value as f64),Value::Number(value)=>Some(*value),_=>None}{
                    static ENUM_VALUES:std::sync::OnceLock<Json>=std::sync::OnceLock::new();
                    let values=ENUM_VALUES.get_or_init(||serde_json::from_str(include_str!("../native/enums.json")).expect("Studio enum metadata"));
                    let name=values[family].as_array().and_then(|items|items.iter().find(|item|item["value"].as_f64()==Some(number))).and_then(|item|item["name"].as_str()).ok_or_else(||Error::lua_string(format!("Unable to assign property {key}. Invalid enum value")))?;
                    crate::types::decode_value(lua,&json!({"tag":"EnumItem","enum":family,"name":name}))?.ok_or_else(||Error::lua_string("Unknown enum value"))?
                }else{return Err(Error::lua_string(format!("Unable to assign property {key}. {family} expected")))};
            if previous()?.as_ref().is_some_and(|old|same(old,&item)){return Ok(true)}node.borrow_mut().properties.insert(key.into(),item);changed(lua,node,key)?;return Ok(true)
        }
        if key=="CollisionGroup"{let group=lua.coerce_string(new.clone())?.ok_or_else(||Error::lua_string("Unable to assign property CollisionGroup. String expected"))?;let next=Value::String(group);if previous()?.as_ref().is_some_and(|old|same(old,&next)){return Ok(true)}node.borrow_mut().properties.insert(key.into(),next);changed(lua,node,key)?;return Ok(true)}
    }
    match key {
        "Name"=>{if let Value::String(v)=new{if !instance::set_name(&mut node.borrow_mut(),&v.as_bytes()){return Ok(true)}}else{return Err(Error::lua_string("Name must be a string"))}},
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
    if key=="OnInvoke" { register_bindable(lua,node,matches!(new,Value::Function(_)))?; }
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
        Value::Table(table)=>{
            let mut result=None;
            unsafe{lua.exec_raw::<()>(table,|state|{
                result=Some(lua.exec_raw_lua(|raw|encode_stack(lua,raw,state,1,active)));
                ffi::lua_settop(state,0);
            })?;}
            result.expect("wire traversal completed")?
        }
        other=>crate::types::encode_value(&other)?.ok_or_else(||Error::lua_string(format!("Cannot serialize {} across VM boundary",other.type_name())))?,
    })
}
// Recursive table handles otherwise expand mlua's hidden reference-thread stack
// inside FireServer. Keep ancestors on the real Lua stack and build only Rust
// wire values; primitive/userdata leaves need at most one temporary host handle.
unsafe fn encode_stack(lua:&Lua,raw:&mlua::state::RawLua,state:*mut ffi::lua_State,index:i32,active:&mut HashSet<usize>)->Result<Json>{unsafe{
    let index=ffi::lua_absindex(state,index);let top=ffi::lua_gettop(state);
    if ffi::lua_checkstack(state,3)==0{return Err(Error::StackError)}
    if ffi::lua_type(state,index)!=ffi::LUA_TTABLE{
        ffi::lua_pushvalue(state,index);
        let value=raw.pop::<Value>();ffi::lua_settop(state,top);
        return encode(lua,value?,active)
    }
    let id=ffi::lua_topointer(state,index)as usize;
    if !active.insert(id){return Err(Error::lua_string("Cannot serialize cyclic table"))}
    let result=(||{
        let mut entries=Vec::new();ffi::lua_pushnil(state);
        while ffi::lua_next(state,index)!=0{
            if !matches!(ffi::lua_type(state,-2),ffi::LUA_TSTRING|ffi::LUA_TNUMBER|ffi::LUA_TINTEGER|ffi::LUA_TBOOLEAN){return Err(Error::lua_string("Unsupported remote table key"))}
            let key=encode_stack(lua,raw,state,-2,active)?;
            let value=encode_stack(lua,raw,state,-1,active)?;
            entries.push(Json::Object([("key".into(),key),("value".into(),value)].into_iter().collect()));
            ffi::lua_pop(state,1);
        }
        Ok(Json::Object([("tag".into(),Json::String("table".into())),("entries".into(),Json::Array(entries))].into_iter().collect()))
    })();
    active.remove(&id);ffi::lua_settop(state,top);result
}}
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
    {let mut r=result.borrow_mut();r.name=n.name.clone();r.raw_name=n.raw_name.clone();r.source=n.source.clone();r.linked_source=n.linked_source.clone();r.attributes=n.attributes.clone();r.properties=n.properties.clone();r.properties.retain(|_,v|!matches!(v,Value::Function(_)));}
    let children=n.children.clone();drop(n);
    let points=required(lua)?.borrow().paths.get(&node.borrow().id).map(|(_,points)|points.clone());
    if let Some(points)=points{set_path_points(lua,&result,points)?;}
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

const UNSUPPORTED:&[(&str,&CStr)]=&[("InspectPlayerFromHumanoidDescription",c"InspectPlayerFromHumanoidDescription"),("CloseInspectMenu",c"CloseInspectMenu"),("GetRealPhysicsFPS",c"GetRealPhysicsFPS"),("Blockcast",c"Blockcast"),("Kick",c"Kick"),("Clear",c"Clear"),("PostAsync",c"PostAsync"),("PostAsyncFullUrl",c"PostAsyncFullUrl"),("PromptRobloxPurchase",c"PromptRobloxPurchase"),("PromptThirdPartyPurchase",c"PromptThirdPartyPurchase")];
const METHODS:&[(&str,&CStr)]=&[("Clone",c"Clone"),("GetPropertyChangedSignal",c"GetPropertyChangedSignal"),("GetAttributeChangedSignal",c"GetAttributeChangedSignal"),("GetPlayers",c"GetPlayers"),("GetNetworkPing",c"GetNetworkPing"),("IsClient",c"IsClient"),("IsServer",c"IsServer"),("IsStudio",c"IsStudio"),("IsRunning",c"IsRunning"),("IsLoaded",c"IsLoaded"),("FireServer",c"FireServer"),("FireClient",c"FireClient"),("FireAllClients",c"FireAllClients"),("SendMessage",c"SendMessage"),("BindToMessage",c"BindToMessage"),("BindToMessageParallel",c"BindToMessageParallel"),("GetLogHistory",c"GetLogHistory"),("JSONDecode",c"JSONDecode"),("QueryDescendants",c"QueryDescendants"),("ComputeStringHash",c"ComputeStringHash")];
pub(crate) fn method_names()->Vec<(&'static str,&'static CStr)>{METHODS.iter().chain(UNSUPPORTED.iter()).chain(PATH_METHODS.iter()).chain(TWEEN_METHODS.iter()).chain(INPUT_METHODS.iter()).chain(SPATIAL_METHODS.iter()).chain(animation::METHODS.iter()).copied().chain([("BindAction",c"BindAction"),("BindActionAtPriority",c"BindActionAtPriority"),("UnbindAction",c"UnbindAction"),("UnbindAllActions",c"UnbindAllActions"),("GetBoundActionInfo",c"GetBoundActionInfo"),("GetAllBoundActionInfo",c"GetAllBoundActionInfo"),("GetStyled",c"GetStyled"),("GenerateGUID",c"GenerateGUID"),("GetFocusedTextBox",c"GetFocusedTextBox"),("IsTenFootInterface",c"IsTenFootInterface"),("GetPlatform",c"GetPlatform"),("GetPlayerFromCharacter",c"GetPlayerFromCharacter"),("GetMemoryUsageMbForTag",c"GetMemoryUsageMbForTag"),("GetServerTimeNow",c"GetServerTimeNow"),("RequestLimitedAsync",c"RequestLimitedAsync"),("GetRobuxBalance",c"GetRobuxBalance"),("PerformPurchaseV2",c"PerformPurchaseV2"),("ScreenPointToRay",c"ScreenPointToRay"),("ViewportPointToRay",c"ViewportPointToRay"),("GetAssetFetchStatus",c"GetAssetFetchStatus"),("GetAssetFetchStatusChangedSignal",c"GetAssetFetchStatusChangedSignal"),("Preload",c"Preload"),("PromptPurchase",c"PromptPurchase"),("PromptRealWorldCommerceBrowser",c"PromptRealWorldCommerceBrowser")]).collect()}
const SPATIAL_METHODS:&[(&str,&CStr)]=&[("Raycast",c"Raycast"),("GetPartBoundsInBox",c"GetPartBoundsInBox"),("GetPartBoundsInRadius",c"GetPartBoundsInRadius")];
const PATH_METHODS:&[(&str,&CStr)]=&[("SetControlPoints",c"SetControlPoints"),("GetControlPoints",c"GetControlPoints"),("GetControlPoint",c"GetControlPoint"),("InsertControlPoint",c"InsertControlPoint"),("UpdateControlPoint",c"UpdateControlPoint"),("RemoveControlPoint",c"RemoveControlPoint"),("GetLength",c"GetLength"),("GetPositionOnCurve",c"GetPositionOnCurve"),("GetPositionOnCurveArcLength",c"GetPositionOnCurveArcLength"),("GetTangentOnCurve",c"GetTangentOnCurve"),("GetTangentOnCurveArcLength",c"GetTangentOnCurveArcLength")];
const TWEEN_METHODS:&[(&str,&CStr)]=&[("Create",c"Create"),("GetValue",c"GetValue"),("Play",c"Play"),("Pause",c"Pause"),("Cancel",c"Cancel")];
const INPUT_METHODS:&[(&str,&CStr)]=&[("IsKeyDown",c"IsKeyDown"),("IsMouseButtonPressed",c"IsMouseButtonPressed"),("GetKeysPressed",c"GetKeysPressed")];
pub(crate) fn method_class(name:&str)->Option<&'static str>{
    if PATH_METHODS.iter().any(|(method,_)|*method==name){return Some("Path2D")}
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
        "GetFocusedTextBox"|"GetPlatform"|"IsKeyDown"|"IsMouseButtonPressed"|"GetKeysPressed"=>"UserInputService",
        "Create"|"GetValue"=>"TweenService", "Play"=>"TweenBase|AnimationTrack", "Pause"|"Cancel"=>"TweenBase",
        "LoadAnimation"|"GetPlayingAnimationTracks"=>"Animator|Humanoid|AnimationController", "Stop"|"AdjustSpeed"=>"AnimationTrack",
        "BindAction"|"BindActionAtPriority"|"UnbindAction"|"UnbindAllActions"|"GetBoundActionInfo"|"GetAllBoundActionInfo"=>"ContextActionService",
        "GetAssetFetchStatusChangedSignal"|"GetAssetFetchStatus"|"PreloadAsync"|"Preload"=>"ContentProvider",
        "GetRealPhysicsFPS"=>"Workspace","Raycast"|"Blockcast"|"GetPartBoundsInBox"|"GetPartBoundsInRadius"=>"WorldRoot",
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
fn path_point(value:&Value)->Result<crate::types::Path2DControlPoint>{
    if let Value::UserData(value)=value{if let Ok(value)=value.borrow::<crate::types::Path2DControlPoint>(){return Ok(*value)}}
    Err(Error::lua_string("Variant cast failed"))
}
fn path_points(lua:&Lua,node:&Link)->Result<Vec<crate::types::Path2DControlPoint>>{
    Ok(required(lua)?.borrow().paths.get(&node.borrow().id).map(|(_,points)|points.clone()).unwrap_or_default())
}
fn set_path_points(lua:&Lua,node:&Link,points:Vec<crate::types::Path2DControlPoint>)->Result<()>{
    guard_write(lua)?;
    if points.len()>1024{return Err(Error::runtime("BudgetExceeded: Path2D control points"))}
    let state=required(lua)?;let mut model=state.borrow_mut();let id=node.borrow().id.clone();
    model.paths.retain(|_,(owner,_)|owner.strong_count()>0);
    let total:usize=model.paths.iter().filter(|(key,_)|**key!=id).map(|(_,(_,points))|points.len()).sum();
    if total.saturating_add(points.len())>65536{return Err(Error::runtime("BudgetExceeded: Path2D point storage"))}
    model.paths.insert(id,(Rc::downgrade(node),points));Ok(())
}

#[derive(Clone)]
struct PathSegment { controls:[[f64;2];4], count:usize }
impl PathSegment {
    fn sample(&self,t:f64,tangent:bool)->[f64;2]{
        let mut points=self.controls;let mut count=self.count;
        if tangent{count-=1;for i in 0..count{for axis in 0..2{points[i][axis]=(self.controls[i+1][axis]-self.controls[i][axis])*count as f64;}}}
        for width in (1..count).rev(){for i in 0..width{for axis in 0..2{points[i][axis]=points[i][axis]*(1.-t)+points[i+1][axis]*t;}}}points[0]
    }
    fn speed(&self,t:f64)->f64{let v=self.sample(t,true);v[0].hypot(v[1])}
    fn length(&self,end:f64)->f64{
        if self.count==2{return self.speed(0.)*end}
        fn integrate(curve:&PathSegment,a:f64,b:f64,fa:f64,fm:f64,fb:f64,whole:f64,tolerance:f64,depth:u8)->f64{
            let mid=(a+b)*0.5;let fl=curve.speed((a+mid)*0.5);let fr=curve.speed((mid+b)*0.5);
            let left=(mid-a)*(fa+4.*fl+fm)/6.;let right=(b-mid)*(fm+4.*fr+fb)/6.;let delta=left+right-whole;
            if depth==0||delta.abs()<=15.*tolerance{return left+right+delta/15.}
            integrate(curve,a,mid,fa,fl,fm,left,tolerance*0.5,depth-1)+integrate(curve,mid,b,fm,fr,fb,right,tolerance*0.5,depth-1)
        }
        let fa=self.speed(0.);let fm=self.speed(end*0.5);let fb=self.speed(end);let whole=end*(fa+4.*fm+fb)/6.;
        integrate(self,0.,end,fa,fm,fb,whole,1e-8*(whole.abs()+1.),12)
    }
}
fn gui_size(node:&Link)->Result<[f64;2]>{
    let mut chain=Vec::new();let mut cursor=Some(node.clone());
    while let Some(node)=cursor{
        if chain.len()>=256{return Err(Error::runtime("BudgetExceeded: GUI ancestry"))}
        let value=node.borrow();if !instance::is_a(&value,"GuiObject"){break}
        let size=match value.properties.get("Size"){Some(Value::UserData(value))=>*value.borrow::<crate::types::UDim2>()?,_=>crate::types::UDim2::default()};
        chain.push(size);cursor=value.parent.upgrade();
    }
    let mut size=[0.;2];for value in chain.into_iter().rev(){size=[value.x.scale as f64*size[0]+value.x.offset as f64,value.y.scale as f64*size[1]+value.y.offset as f64];}Ok(size)
}
fn path_geometry(lua:&Lua,node:&Link)->Result<([f64;2],Vec<PathSegment>)>{
    let parent=node.borrow().parent.upgrade().filter(|parent|instance::is_a(&parent.borrow(),"GuiObject")).ok_or_else(||Error::lua_string("Attempting to use Path2D with invalid parent"))?;
    let size=gui_size(&parent)?;
    let pixel=|value:crate::types::UDim2|[value.x.scale as f64*size[0]+value.x.offset as f64,value.y.scale as f64*size[1]+value.y.offset as f64];
    let points=path_points(lua,node)?;let closed=matches!(node.borrow().properties.get("Closed"),Some(Value::Boolean(true)));
    let count=if points.len()<2{0}else{points.len()-1+usize::from(closed)};
    let mut segments=Vec::with_capacity(count);
    for i in 0..count{
        let a=points[i];let b=points[(i+1)%points.len()];let start=pixel(a.position);let end=pixel(b.position);
        let mut controls=[[0.;2];4];controls[0]=start;let mut count=1;
        for (handle,anchor) in [(a.right,start),(b.left,end)]{if handle!=crate::types::UDim2::default(){let offset=pixel(handle);controls[count]=[anchor[0]+offset[0],anchor[1]+offset[1]];count+=1;}}
        controls[count]=end;count+=1;
        if controls[..count].iter().flatten().any(|value|!value.is_finite()){return Err(Error::lua_string("Path2D contains non-finite coordinates"))}
        segments.push(PathSegment{controls,count});
    }Ok((size,segments))
}
fn path_method(lua:&Lua,node:&Link,name:&str,values:MultiValue)->Result<Value>{
    let required_arg=|index:usize|values.get(index).filter(|value|!value.is_nil()).ok_or_else(||Error::lua_string(format!("Argument {} missing or nil",index+1)));
    if name=="SetControlPoints"{
        let input=required_arg(0)?;let Value::Table(input)=input else{return Err(asset_cast(input,"Array"))};
        if input.raw_len()>1024{return Err(Error::runtime("BudgetExceeded: Path2D control points"))}
        if input.raw_len()==0&&input.pairs::<Value,Value>().next().transpose()?.is_some(){return Err(Error::lua_string("Unable to cast Dictionary to Array"))}
        let points=input.clone().sequence_values::<Value>().map(|value|path_point(&value?)).collect::<Result<Vec<_>>>()?;
        set_path_points(lua,node,points)?;return Ok(Value::Nil)
    }
    let mut points=path_points(lua,node)?;
    if name=="GetControlPoints"{return Ok(Value::Table(lua.create_sequence_from(points)?))}
    if matches!(name,"GetControlPoint"|"InsertControlPoint"|"UpdateControlPoint"|"RemoveControlPoint"){
        let input=required_arg(0)?;let index=lua.coerce_number(input.clone())?.ok_or_else(||asset_cast(input,"int"))? as i64;
        let maximum=points.len()+usize::from(name=="InsertControlPoint");
        if index<1||index as usize>maximum{return Err(Error::lua_string(format!("Path2D:{name}() at index {index} access out of bounds")))}
        let index=index as usize-1;if name=="GetControlPoint"{return points[index].into_lua(lua)}
        match name{"InsertControlPoint"=>points.insert(index,path_point(required_arg(1)?)?),"UpdateControlPoint"=>points[index]=path_point(required_arg(1)?)?,_=>{points.remove(index);}}
        set_path_points(lua,node,points)?;return Ok(Value::Nil)
    }
    let (size,segments)=path_geometry(lua,node)?;
    if name=="GetLength"{return Ok(Value::Number(segments.iter().map(|segment|segment.length(1.)).sum::<f64>()as f32 as f64))}
    let input=required_arg(0)?;let alpha=lua.coerce_number(input.clone())?.ok_or_else(||asset_cast(input,"float"))?;
    if !alpha.is_finite(){return Err(Error::lua_string("Path2D curve parameter must be finite"))}
    if segments.is_empty(){return Err(Error::lua_string(format!("Path2D:{name}() attempting to query with zero segments")))}
    let alpha=alpha.clamp(0.,1.);let (index,t)=if name.ends_with("ArcLength"){
        let lengths:Vec<_>=segments.iter().map(|segment|segment.length(1.)).collect();let total=lengths.iter().sum::<f64>();let mut distance=alpha*total;let mut index=0;
        while index+1<segments.len()&&distance>lengths[index]{distance-=lengths[index];index+=1;}
        let mut low=0.;let mut high=1.;for _ in 0..28{let mid=(low+high)*0.5;if segments[index].length(mid)<distance{low=mid}else{high=mid}}
        (index,if total==0.{0.}else{(low+high)*0.5})
    }else{let position=alpha*segments.len()as f64;let index=(position.floor()as usize).min(segments.len()-1);(index,position-index as f64)};
    let tangent=name.starts_with("GetTangent");let point=segments[index].sample(t,tangent);
    if tangent{return crate::types::Vector([point[0]as f32,point[1]as f32]).into_lua(lua)}
    let dim=|axis:usize|crate::types::UDim{scale:if size[axis]==0.{0.}else{(point[axis]/size[axis])as f32},offset:0};
    crate::types::UDim2{x:dim(0),y:dim(1)}.into_lua(lua)
}
fn enum_name(value:&Value,family:&str)->Result<String>{
    let Some(encoded)=crate::types::encode_value(value)?.filter(|value|value["tag"]=="EnumItem")else{return Err(asset_cast(value,family))};
    if encoded["enum"]!=family{return Err(Error::lua_string(format!("Unable to cast {} to {family}",encoded["enum"].as_str().unwrap_or("EnumItem"))))}
    Ok(encoded["name"].as_str().unwrap_or("").into())
}
fn tween_enum(lua:&Lua,name:&str)->Result<Value>{crate::types::decode_value(lua,&json!({"tag":"EnumItem","enum":"PlaybackState","name":name}))?.ok_or_else(||Error::runtime("PlaybackState unavailable"))}
fn tween_status(lua:&Lua,node:&Link,name:&str)->Result<()>{node.borrow_mut().properties.insert("PlaybackState".into(),tween_enum(lua,name)?);changed(lua,node,"PlaybackState")}
fn tween_kind(value:&Value)->Option<&'static str>{match value{
    Value::Integer(_)|Value::Number(_)=>Some("double"),Value::Boolean(_)=>Some("bool"),Value::Vector(_)=>Some("Vector3"),
    Value::UserData(value)=>{macro_rules! typed{($type:ty,$name:literal)=>{if value.is::<$type>(){return Some($name)}}}typed!(crate::types::Vector2,"Vector2");typed!(crate::types::Color3,"Color3");typed!(crate::types::UDim,"UDim");typed!(crate::types::UDim2,"UDim2");typed!(crate::types::CFrame,"CFrame");None},_=>None,
}}
fn tween_ease(alpha:f64,style:&str,direction:&str)->f64{
    fn bounce(t:f64)->f64{if t<1./2.75{7.5625*t*t}else if t<2./2.75{let t=t-1.5/2.75;7.5625*t*t+0.75}else if t<2.5/2.75{let t=t-2.25/2.75;7.5625*t*t+0.9375}else{let t=t-2.625/2.75;7.5625*t*t+0.984375}}
    let ease=|t:f64|match style{
        "Sine"=>1.-(t*std::f64::consts::FRAC_PI_2).cos(),"Quad"=>t*t,"Cubic"=>t*t*t,"Quart"=>t.powi(4),"Quint"=>t.powi(5),
        "Back"=>t*t*(2.70158*t-1.70158),"Bounce"=>1.-bounce(1.-t),"Circular"=>1.-(1.-t*t).max(0.).sqrt(),
        "Exponential"=>if t==0.{0.}else{2f64.powf(10.*t-10.)},
        "Elastic"=>if t==0.||t==1.{t}else{-2f64.powf(10.*t-10.)*((t*10.-10.75)*(2.*std::f64::consts::PI/3.)).sin()},_=>t,
    };
    let t=alpha.clamp(0.,1.);match direction{"In"=>ease(t),"InOut"=>if t<0.5{ease(t*2.)*0.5}else{1.-ease((1.-t)*2.)*0.5},_=>1.-ease(1.-t)}
}
fn tween_cframe(a:crate::types::CFrame,b:crate::types::CFrame,t:f64)->crate::types::CFrame{
    fn quaternion(r:[[f32;3];3])->[f64;4]{
        let m=r.map(|row|row.map(f64::from));let trace=m[0][0]+m[1][1]+m[2][2];
        if trace>0.{let s=(trace+1.).sqrt()*2.;return[(m[2][1]-m[1][2])/s,(m[0][2]-m[2][0])/s,(m[1][0]-m[0][1])/s,s*0.25]}
        let i=if m[0][0]>m[1][1]&&m[0][0]>m[2][2]{0}else if m[1][1]>m[2][2]{1}else{2};let j=(i+1)%3;let k=(i+2)%3;
        let s=(1.+m[i][i]-m[j][j]-m[k][k]).max(0.).sqrt()*2.;let mut q=[0.;4];q[i]=s*0.25;q[j]=(m[j][i]+m[i][j])/s;q[k]=(m[k][i]+m[i][k])/s;q[3]=(m[k][j]-m[j][k])/s;q
    }
    let qa=quaternion(a.r);let mut qb=quaternion(b.r);let mut dot=qa.iter().zip(qb).map(|(a,b)|a*b).sum::<f64>();if dot<0.{dot=-dot;qb=qb.map(|v|-v);}
    let q=if dot>0.9995{let mut q=std::array::from_fn::<_,4,_>(|i|qa[i]+(qb[i]-qa[i])*t);let length=q.iter().map(|v|v*v).sum::<f64>().sqrt();for v in &mut q{*v/=length;}q}else{let angle=dot.clamp(-1.,1.).acos();let sine=angle.sin();std::array::from_fn(|i|(qa[i]*((1.-t)*angle).sin()+qb[i]*(t*angle).sin())/sine)};
    let[x,y,z,w]=q;let r=[[1.-2.*(y*y+z*z),2.*(x*y-z*w),2.*(x*z+y*w)],[2.*(x*y+z*w),1.-2.*(x*x+z*z),2.*(y*z-x*w)],[2.*(x*z-y*w),2.*(y*z+x*w),1.-2.*(x*x+y*y)]];
    crate::types::CFrame{p:crate::types::Vector(std::array::from_fn(|i|(a.p.0[i]as f64+(b.p.0[i]as f64-a.p.0[i]as f64)*t)as f32)),r:r.map(|row|row.map(|v|v as f32))}
}
fn tween_interpolate(lua:&Lua,a:&Value,b:&Value,t:f64)->Result<Value>{
    if t==0.{return Ok(a.clone())}if t==1.{return Ok(b.clone())}
    let blend=|a:f64,b:f64|a+(b-a)*t;
    let dimension=|a:crate::types::UDim,b:crate::types::UDim|crate::types::UDim{scale:blend(a.scale as f64,b.scale as f64)as f32,offset:blend(a.offset as f64,b.offset as f64).round()as i32};
    match(a,b){
        (Value::Boolean(_),Value::Boolean(_))=>Ok(b.clone()),
        (Value::Integer(_)|Value::Number(_),Value::Integer(_)|Value::Number(_))=>{let number=|value:&Value|match value{Value::Integer(value)=>*value as f64,Value::Number(value)=>*value,_=>unreachable!()};Ok(Value::Number(blend(number(a),number(b))))},
        (Value::Vector(a),Value::Vector(b))=>Ok(Value::Vector(mlua::Vector::new(blend(a.x()as f64,b.x()as f64)as f32,blend(a.y()as f64,b.y()as f64)as f32,blend(a.z()as f64,b.z()as f64)as f32))),
        (Value::UserData(a),Value::UserData(b))=>{
            if let(Ok(a),Ok(b))=(a.borrow::<crate::types::Vector2>(),b.borrow::<crate::types::Vector2>()){return crate::types::Vector(std::array::from_fn::<_,2,_>(|i|blend(a.0[i]as f64,b.0[i]as f64)as f32)).into_lua(lua)}
            if let(Ok(a),Ok(b))=(a.borrow::<crate::types::Color3>(),b.borrow::<crate::types::Color3>()){return crate::types::Color3(std::array::from_fn(|i|blend(a.0[i]as f64,b.0[i]as f64)as f32)).into_lua(lua)}
            if let(Ok(a),Ok(b))=(a.borrow::<crate::types::UDim>(),b.borrow::<crate::types::UDim>()){return dimension(*a,*b).into_lua(lua)}
            if let(Ok(a),Ok(b))=(a.borrow::<crate::types::UDim2>(),b.borrow::<crate::types::UDim2>()){return crate::types::UDim2{x:dimension(a.x,b.x),y:dimension(a.y,b.y)}.into_lua(lua)}
            if let(Ok(a),Ok(b))=(a.borrow::<crate::types::CFrame>(),b.borrow::<crate::types::CFrame>()){return tween_cframe(*a,*b,t).into_lua(lua)}
            Err(Error::lua_string("Tween interpolation type mismatch"))
        },_=>Err(Error::lua_string("Tween interpolation type mismatch")),
    }
}
fn tween_control(lua:&Lua,node:&Link,operation:&str)->Result<()>{
    guard_write(lua)?;let state=required(lua)?;let id=node.borrow().id.clone();
    let Some(mut tween)=state.borrow().tweens.get(&id).cloned()else{return Ok(())};
    let current=now(lua);let playing=matches!(tween.playback,"Playing"|"Delayed");
    if operation=="Play"{
        if playing{return Ok(())}
        if tween.playback!="Paused"{tween.elapsed=0.;tween.initial=tween.goals.iter().map(|(key,_)|instance::read_property(lua,&Instance(tween.target.clone()),key)).collect::<Result<_>>()?;}
        let conflicts:Vec<_>=state.borrow().tweens.values().filter(|other|other.active.is_some()&&Rc::ptr_eq(&other.target,&tween.target)&&other.goals.iter().any(|(key,_)|tween.goals.iter().any(|(goal,_)|goal==key))).filter_map(|other|other.owner.upgrade()).collect();
        for conflict in conflicts{tween_control(lua,&conflict,"Cancel")?;}
        tween.started=current;tween.active=Some(node.clone());tween.playback=if tween.info.delay>tween.elapsed{"Delayed"}else{"Playing"};
    }else{
        if !playing&&tween.playback!="Paused"{return Ok(())}
        if playing{tween.elapsed+=(current-tween.started).max(0.);}
        tween.active=None;tween.playback=if operation=="Pause"{"Paused"}else{"Cancelled"};
    }
    tween.revision+=1;let playback=tween.playback;state.borrow_mut().tweens.insert(id,tween);tween_status(lua,node,playback)?;
    if operation=="Cancel"{fire(lua,node,"Completed",args([tween_enum(lua,"Cancelled")?]))?;}Ok(())
}
fn advance_tweens(lua:&Lua)->Result<()>{
    let state=required(lua)?;state.borrow_mut().tweens.retain(|_,tween|tween.active.is_some()||tween.owner.strong_count()>0);
    let jobs:Vec<_>=state.borrow().tweens.iter().filter(|(_,tween)|tween.active.is_some()).map(|(id,tween)|(id.clone(),tween.clone())).collect();let current=now(lua);
    for(id,mut tween)in jobs{
        let Some(owner)=tween.owner.upgrade()else{continue};
        if owner.borrow().destroyed||tween.target.borrow().destroyed{state.borrow_mut().tweens.remove(&id);continue}
        let duration=tween.info.time.max(0.);let delay=tween.info.delay.max(0.);let motion=duration*if tween.info.reverses{2.}else{1.};let cycle=delay+motion;
        let elapsed=tween.elapsed+(current-tween.started).max(0.);let complete=tween.info.repeat_count>=0&&(cycle==0.||elapsed>=cycle*(tween.info.repeat_count as f64+1.));
        let in_cycle=if cycle==0.{0.}else{elapsed%cycle};let progress=(in_cycle-delay).max(0.);
        let playback=if complete{"Completed"}else if in_cycle<delay{"Delayed"}else{"Playing"};
        let previous_cycle=playback=="Delayed"&&elapsed>=cycle;
        let alpha=if complete||previous_cycle{if tween.info.reverses{0.}else{1.}}else if duration==0.{1.}else if tween.info.reverses&&progress>duration{2.-progress/duration}else{progress/duration};
        let alpha=tween_ease(alpha,&tween.info.style,&tween.info.direction);
        let mut valid=true;
        if playback!="Delayed"||previous_cycle{for((key,goal),initial)in tween.goals.iter().zip(&tween.initial){
            if state.borrow().tweens.get(&id).is_none_or(|value|value.revision!=tween.revision){valid=false;break}
            let value=tween_interpolate(lua,initial,goal,alpha)?;write_property(lua,&tween.target,key,&value)?;
        }}
        if !valid||state.borrow().tweens.get(&id).is_none_or(|value|value.revision!=tween.revision){continue}
        let changed=tween.playback!=playback;tween.playback=playback;if complete{tween.active=None;}
        state.borrow_mut().tweens.insert(id,tween);
        if changed{tween_status(lua,&owner,playback)?;}
        if complete{fire(lua,&owner,"Completed",args([tween_enum(lua,"Completed")?]))?;}
    }Ok(())
}
pub(crate) fn register_methods<M:UserDataMethods<Instance>>(m:&mut M){
    instance::register_method(m,"Raycast",|lua,this,args:MultiValue|crate::spatial::raycast(lua,&this.0,args));
    instance::register_method(m,"GetPartBoundsInBox",|lua,this,args:MultiValue|crate::spatial::bounds(lua,&this.0,args,false));
    instance::register_method(m,"GetPartBoundsInRadius",|lua,this,args:MultiValue|crate::spatial::bounds(lua,&this.0,args,true));
    for name in ["IsKeyDown","IsMouseButtonPressed"]{instance::register_method(m,name,move|_,_,value:Value|{enum_name(&value,if name=="IsKeyDown"{"KeyCode"}else{"UserInputType"})?;Ok(false)});}
    instance::register_method(m,"GetKeysPressed",|lua,_,()|lua.create_table());
    instance::register_method(m,"GetValue",|lua,_,values:MultiValue|{
        let a=values.front().ok_or_else(||Error::lua_string("Argument 1 missing or nil"))?;let alpha=lua.coerce_number(a.clone())?.ok_or_else(||asset_cast(a,"float"))?;
        let style=enum_name(values.get(1).unwrap_or(&Value::Nil),"EasingStyle")?;let direction=enum_name(values.get(2).unwrap_or(&Value::Nil),"EasingDirection")?;Ok(tween_ease(alpha,&style,&direction))
    });
    for name in ["Play","Pause","Cancel"]{instance::register_method(m,name,move|lua,this,values:MultiValue|if this.0.borrow().class=="AnimationTrack"{animation::control(lua,&this.0,name,values,false)}else{tween_control(lua,&this.0,name)});}
    for &(name,_) in animation::METHODS{instance::register_method(m,name,move|lua,this,values:MultiValue|animation::method(lua,&this.0,name,values));}
    instance::register_method(m,"Create",|lua,_,mut values:MultiValue|{
        guard_write(lua)?;let target=object_arg(&mut values)?;let info_value=values.pop_front().unwrap_or(Value::Nil);
        let info=if let Value::UserData(value)=&info_value{value.borrow::<crate::types::TweenInfo>().ok().map(|value|value.clone())}else{None}.ok_or_else(||asset_cast(&info_value,"TweenInfo"))?;
        let goals_value=values.pop_front().unwrap_or(Value::Nil);let Value::Table(goals)=goals_value else{return Err(asset_cast(&goals_value,"Dictionary"))};
        let mut fields=BTreeMap::new();for pair in goals.pairs::<Value,Value>(){
            if fields.len()>=64{return Err(Error::runtime("BudgetExceeded: Tween goal count"))}
            let(key,goal)=pair?;let Value::String(key)=key else{return Err(Error::lua_string("TweenService:Create property keys must be strings"))};let key=key.to_str()?.to_owned();
            let initial=instance::read_property(lua,&Instance(target.clone()),&key).map_err(|_|Error::lua_string(format!("TweenService:Create no property named '{key}' for object '{}'",target.borrow().name)))?;
            let Some(kind)=tween_kind(&initial)else{return Err(Error::lua_string(format!("TweenService:Create property named '{key}' on object '{}' is not a data type that can be tweened",target.borrow().name)))};
            if tween_kind(&goal)!=Some(kind){return Err(Error::lua_string(format!("TweenService:Create property named '{key}' cannot be tweened due to type mismatch (property is a '{kind}', but given type is '{}')",goal.type_name())))}
            fields.insert(key,goal);
        }
        let state=required(lua)?;state.borrow_mut().tweens.retain(|_,value|value.active.is_some()||value.owner.strong_count()>0);
        if state.borrow().tweens.len()>=4096{return Err(Error::runtime("BudgetExceeded: Tween count"))}
        let node=instance::new_node("Tween")?;created(lua,&node)?;let id=node.borrow().id.clone();
        node.borrow_mut().properties.insert("Instance".into(),value(lua,target.clone())?);node.borrow_mut().properties.insert("TweenInfo".into(),info_value);node.borrow_mut().properties.insert("PlaybackState".into(),tween_enum(lua,"Begin")?);
        state.borrow_mut().tweens.insert(id,Tween{owner:Rc::downgrade(&node),active:None,target,info,goals:fields.into_iter().collect(),initial:Vec::new(),elapsed:0.,started:0.,playback:"Begin",revision:0});
        value(lua,node)
    });
    for &(name,_) in PATH_METHODS{instance::register_method(m,name,move|lua,this,values:MultiValue|{
        let value=path_method(lua,&this.0,name,values)?;
        Ok(if matches!(name,"SetControlPoints"|"InsertControlPoint"|"UpdateControlPoint"|"RemoveControlPoint"){MultiValue::new()}else{args([value])})
    });}
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

fn snapshot(lua:&Lua,node:&Link)->Result<Json>{let n=node.borrow();let props=lua.create_table()?;for(k,v)in &n.properties{if !matches!(v,Value::Function(_)){props.raw_set(k.as_str(),v.clone())?;}}props.raw_set("Archivable",n.archivable)?;let attrs=lua.create_table()?;for(k,v)in &n.attributes{attrs.raw_set(k.as_str(),instance::attribute_value(lua,v)?)?;}Ok(json!({"id":n.id,"class":n.class,"name":n.name,"nameBytes":n.raw_name,"parent":n.parent.upgrade().map(|p|p.borrow().id.clone()),"source":n.source,"linkedSource":n.linked_source,"destroyed":n.destroyed,"properties":encode(lua,Value::Table(props),&mut HashSet::new())?,"attributes":encode(lua,Value::Table(attrs),&mut HashSet::new())?}))}

// Keep complete host metadata while applying only the writer's changed keys.
pub(crate) fn merge_replica(previous:Option<&Json>,incoming:&Json)->Json{
    let Some(keys)=incoming.get("$changed") else{return incoming.clone()};
    let Some(previous)=previous else{let mut full=incoming.clone();full.as_object_mut().unwrap().remove("$changed");return full};
    let mut result=previous.clone();result.as_object_mut().unwrap().remove("$changed");
    for field in ["id","class","name","parent","source","linkedSource","destroyed"]{
        if result.get(field).is_none()||keys["fields"].as_array().is_some_and(|fields|fields.iter().any(|key|key==field)){result[field]=incoming[field].clone();}
    }
    if previous.get("name").is_none()||selected(incoming,"fields","name"){result["nameBytes"]=incoming["nameBytes"].clone();}
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
    if selected(node,"fields","name"){
        let raw=if node["nameBytes"].is_null(){None}else{Some(serde_json::from_value::<Vec<u8>>(node["nameBytes"].clone()).map_err(|_|Error::runtime("Invalid binary Instance name"))?)};
        if let Some(name)=raw.as_deref().or_else(||node["name"].as_str().map(str::as_bytes)){
            let renamed=instance::set_name(&mut n.borrow_mut(),name);if renamed&&wire{changed(lua,n,"Name")?;}
        }
    }
    if selected(node,"fields","source"){if let Some(source)=node["source"].as_str(){n.borrow_mut().source=source.into();}}
    if selected(node,"fields","linkedSource"){if let Some(source)=node["linkedSource"].as_str(){n.borrow_mut().linked_source=source.into();}}
    if let Some(properties)=node.get("properties"){
        let props=if wire{decode(lua,properties)?}else{lua.to_value(properties)?};
        if let Value::Table(props)=props{
            let mut keys:BTreeSet<String>=props.clone().pairs::<String,Value>().map(|entry|entry.map(|(key,_)|key)).collect::<Result<_>>()?;
            if let Some(changed)=node["$changed"]["properties"].as_array(){keys.extend(changed.iter().filter_map(Json::as_str).map(str::to_owned));}
            else if wire&&node.get("$changed").is_none(){keys.extend(n.borrow().properties.iter().filter(|(_,value)|!matches!(value,Value::Function(_))).map(|(key,_)|key.clone()));}
            for key in keys{if !selected(node,"properties",&key){continue}let mut next:Value=props.raw_get(key.as_str())?;
                if matches!(key.as_str(),"Character"|"PrimaryPart")&&!wire{if let Some(encoded)=properties.get(&key).filter(|v|v["tag"]=="instance"){next=decode(lua,encoded)?;}}
                if key=="Character"&&n.borrow().class=="Player"{write_property(lua,n,&key,&next)?;continue}
                if instance::is_a(&n.borrow(),"BasePart")&&n.borrow().class!="Terrain"&&matches!(key.as_str(),"CFrame"|"Position"|"Size"|"Shape"|"Material"|"CollisionGroup")&&!next.is_nil(){
                    if !wire{if let Some(encoded)=properties.get(&key){if let Some(native)=crate::types::decode_value(lua,encoded)?{next=native;}}}
                    // Host seeds and replicas use the same transform normalization
                    // as script writes, including Position-only updates.
                    write_property(lua,n,&key,&next)?;continue
                }
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
pub fn frame(lua:&Lua,dt:f64)->Result<()>{poll_children(lua)?;advance_tweens(lua)?;animation::frame(lua)?;let n=lookup(lua,"RunService").ok_or_else(||Error::runtime("RunService unavailable"))?;for event in ["PreSimulation","PostSimulation","Heartbeat"]{fire(lua,&n,event,args([Value::Number(dt)]))?;}fire(lua,&n,"Stepped",args([Value::Number(now(lua)),Value::Number(dt)]))?;if required(lua)?.borrow().side=="client"{for event in ["PreRender","RenderStepped"]{fire(lua,&n,event,args([Value::Number(dt)]))?;}}Ok(())}
pub(crate) fn animation_authorized(lua:&Lua,payload:&Json,client:Option<&str>)->Result<bool>{animation::authorized(lua,payload,client)}
pub fn deliver(lua:&Lua,kind:&str,payload:Json)->Result<()>{
    if kind=="animation"{return animation::deliver(lua,&payload)}
    let id=payload[if kind=="actor_message"{"actorId"}else{"remoteId"}].as_str().unwrap_or("");
    if kind=="remote_return"||kind=="bindable_return"{
        let thread=required(lua)?.borrow_mut().pending.remove(payload["requestId"].as_str().unwrap_or(""));
        if let Some(thread)=thread{if kind=="bindable_return"&&!thread.is_resumable(){return Ok(())}let mut values=decode_args(lua,&payload["args"])?;if payload["ok"].as_bool()==Some(true){crate::scheduler::schedule(lua,thread,values,None)?;}else{crate::scheduler::schedule_error(lua,thread,values.pop_front().unwrap_or(Value::Nil))?;}}return Ok(())
    }
    // A client-created RemoteEvent has no server counterpart. An event queued before
    // its receiver was destroyed likewise has nothing to deliver to.
    let Some(n)=lookup(lua,id)else{
        if matches!(kind,"remote_event"|"bindable_event"|"bindable_handler"){return Ok(())}
        return Err(Error::runtime(format!("Message target unavailable: {id}")));
    };
    if kind=="bindable_handler"{return drain_invokes(lua,&n)}
    let mut values=decode_args(lua,&payload["args"])?;
    match kind{
        "bindable_event"=>fire(lua,&n,"Event",values),
        "bindable_call"=>{
            let Some(callback)=local_bindable_callback(lua,&n)?else{return Err(Error::runtime("Bindable callback owner changed during delivery"))};
            let callback=Value::Thread(callback_thread(lua,&n,"OnInvoke",callback)?);
            let request=payload["requestId"].clone();let target=payload["sourceRealm"].clone();
            crate::scheduler::spawn_tracked(lua,callback,values,Some("serial"),Some("game"),Rc::new(move|lua,result|{
                let(ok,values)=match result{Ok(values)=>(true,values),Err(error)=>(false,args([bindable_error(lua,error)?]))};
                let(ok,encoded)=match encode_args(lua,values){Ok(values)=>(ok,values),Err(error)=>(false,encode_args(lua,args([str_value(lua,&error.to_string())?]))?)};
                emit(lua,"bindable_return",json!({"requestId":request,"target":target,"ok":ok,"args":encoded}))
            }))?;Ok(())
        },
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
fn finish_waiter(lua:&Lua,thread:Thread,result:std::result::Result<MultiValue,Value>)->Result<()>{match result{Ok(v)=>crate::scheduler::schedule(lua,thread,v,None),Err(e)=>crate::scheduler::schedule_error(lua,thread,e)}}
fn bindable_error(lua:&Lua,error:Value)->Result<Value>{Ok(Value::String(match lua.coerce_string(error)?{Some(value)=>value,None=>lua.create_string("")?}))}
fn local_bindable_callback(lua:&Lua,node:&Link)->Result<Option<Function>>{
    if lua.app_data_ref::<BindableRegistry>().is_some(){
        let realm=required(lua)?.borrow().realm.clone();
        if bindable_owner(lua,&node.borrow().id)?.as_deref()!=Some(realm.as_str()){return Ok(None)}
    }
    Ok(match node.borrow().properties.get("OnInvoke"){Some(Value::Function(callback))=>Some(callback.clone()),_=>None})
}
fn queue_bindable(lua:&Lua,node:&Link,thread:Thread,values:MultiValue)->Result<()>{
    let state=required(lua)?;let request={let mut state=state.borrow_mut();reserve_waiter(&mut state)?;state.requests+=1;format!("{}:bindable:{}",state.realm,state.requests)};
    let encoded=encode_args(lua,values)?;state.borrow_mut().pending.insert(request.clone(),thread);
    if let Err(error)=emit(lua,"bindable_call",json!({"remoteId":node.borrow().id,"requestId":request,"args":encoded})){state.borrow_mut().pending.remove(&request);return Err(error)}
    Ok(())
}
fn dispatch_bindable(lua:&Lua,node:&Link,thread:Thread,values:MultiValue,inline:bool)->Result<Option<std::result::Result<MultiValue,Value>>>{
    if let Some(callback)=local_bindable_callback(lua,node)?{return invoke_callback(lua,node,callback,values,thread,inline)}
    if bindable_owner(lua,&node.borrow().id)?.is_some(){queue_bindable(lua,node,thread,values)?;}
    else{let state=required(lua)?;reserve_waiter(&mut state.borrow_mut())?;state.borrow_mut().invokes_waiting.push((node.borrow().id.clone(),thread,values));}
    Ok(None)
}
fn invoke_callback(lua:&Lua,node:&Link,callback:Function,values:MultiValue,waiter:Thread,allow_inline:bool)->Result<Option<std::result::Result<MultiValue,Value>>>{
    let active=Rc::new(Cell::new(allow_inline));let result=Rc::new(RefCell::new(None));let in_callback=active.clone();let out=result.clone();
    let callback=Value::Thread(callback_thread(lua,node,"OnInvoke",callback)?);
    crate::scheduler::spawn_tracked(lua,callback,values,Some("serial"),Some("game"),Rc::new(move|lua,result|{
        let result=match result{Ok(values)=>Ok(values),Err(error)=>Err(bindable_error(lua,error)?)};
        if in_callback.get(){*out.borrow_mut()=Some(result);Ok(())}else{finish_waiter(lua,waiter.clone(),result)}
    }))?;active.set(false);let result=result.borrow_mut().take();Ok(result)
}
fn drain_invokes(lua:&Lua,node:&Link)->Result<()>{
    if local_bindable_callback(lua,node)?.is_none()&&bindable_owner(lua,&node.borrow().id)?.is_none(){return Ok(())}
    let state=required(lua)?;let waiting=std::mem::take(&mut state.borrow_mut().invokes_waiting);
    for(id,thread,args)in waiting{if !thread.is_resumable(){continue}if id==node.borrow().id{dispatch_bindable(lua,node,thread,args,false)?;}else{state.borrow_mut().invokes_waiting.push((id,thread,args));}}
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
    if let Some(registry)=lua.app_data_ref::<BindableRegistry>().map(|registry|registry.clone()){
        let key=bindable_key(lua,&node.borrow().id)?;let realm=required(lua)?.borrow().realm.clone();let mut targets=Vec::new();
        if let Some(peers)=registry.0.borrow().events.get(&key){for(peer,target)in peers{
            if peer==&realm{continue}let Some(target)=target.upgrade()else{continue};let target=target.borrow();if target.destroyed{continue}
            if let Some(signal)=target.signals.get("Event"){if crate::scheduler::signal_has_listeners(signal)?{targets.push(peer.clone());}}
        }}
        if !targets.is_empty(){emit(lua,"bindable_event",json!({"remoteId":node.borrow().id,"targets":targets,"args":encode_args(lua,copied.clone())?}))?;}
    }
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
    let node=object_arg(&mut values)?;let name=match values.pop_front(){Some(Value::String(name))=>name.as_bytes().to_vec(),_=>return Err(Error::lua_string("String expected"))};
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
    if local_bindable_callback(lua,&node)?.is_none()&&ffi::lua_isyieldable(state)==0{return Err(Error::lua_string("attempt to yield across metamethod/C-call boundary"))}
    if let Some(result)=dispatch_bindable(lua,&node,thread,values,true)?{return Ok(match result{Ok(v)=>NativeCall::Return(v),Err(e)=>NativeCall::Error(e)})}
    crate::scheduler::park_current(lua)?;
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
    ("Animation", "AnimationId", "string"), ("AnimationTrack", "Animation", "Animation"),
    ("AnimationTrack", "IsPlaying Looped", "boolean"), ("AnimationTrack", "Length TimePosition Speed WeightCurrent WeightTarget", "number"),
    ("AnimationTrack", "Priority", "Enum.AnimationPriority"),
    ("Players", "LocalPlayer", "Player | nil"), ("Player", "UserId AccountAge", "number"), ("Player", "DisplayName", "string"),
    ("Workspace", "CurrentCamera", "Camera | nil"), ("Workspace", "Gravity DistributedGameTime", "number"),
    ("Camera", "ViewportSize", "Vector2"), ("Camera", "NearPlaneZ FieldOfView", "number"),
    ("Camera", "FieldOfViewMode", "Enum.FieldOfViewMode"), ("Camera", "CFrame", "CFrame"),
    ("DataModel", "PlaceId GameId PlaceVersion CreatorId", "number"), ("DataModel", "JobId", "string"),
    ("StringValue", "Value", "string"), ("BoolValue", "Value", "boolean"),
    ("NumberValue|IntValue", "Value", "number"), ("ObjectValue", "Value", "Instance | nil"),
    ("BasePart", "Anchored Massless CanCollide CanTouch CanQuery", "boolean"), ("BasePart", "Transparency", "number"),
    ("BasePart", "CFrame ExtentsCFrame", "CFrame"), ("BasePart", "Position Size ExtentsSize", "Vector3"),
    ("BasePart", "Material", "Enum.Material"), ("BasePart", "CollisionGroup", "string"), ("Part", "Shape", "Enum.PartType"),
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
    ("Path2D", "Closed Visible", "boolean"), ("Path2D", "Thickness", "number"), ("Path2D", "Color3", "Color3"),
];
const COMPLETION_EVENTS: &str = "ChildAdded ChildRemoved DescendantAdded DescendantRemoving AncestryChanged Changed Destroying AttributeChanged Loaded Completed Event OnServerEvent OnClientEvent PlayerAdded PlayerRemoving CharacterAdded CharacterRemoving Idled Heartbeat Stepped RenderStepped PreSimulation PostSimulation PreRender MessageOut AssetFetchFailed Died StateChanged Error InputBegan InputEnded InputChanged WindowFocused WindowFocusReleased AnimationPlayed Stopped Ended DidLoop KeyframeReached";

fn completion_class(node: &instance::Node, classes: &str) -> bool { classes.split('|').any(|class| instance::is_a(node, class)) }
fn completion_label(label: &str) -> bool { !label.is_empty() && label.len() <= 256 && !label.contains('\0') }
fn completion_item(label: &str, kind: &str, detail: &str, info: &str) -> Json {
    json!({"label":label,"type":kind,"detail":detail,"info":info})
}
fn completion_target(model: &Model, node: &Link, key: &str) -> Option<Link> {
    let node = node.borrow();
    if instance::method_class(key).is_some_and(|class| class.split('|').any(|class|instance::is_a(&node, class))) { return None; }
    if let Some(value) = node.properties.get(key) {
        return if let Value::UserData(value) = value { value.borrow::<Instance>().ok().map(|value| value.0.clone()) } else { None };
    }
    if key == "Parent" { return node.parent.upgrade(); }
    if key == "LocalPlayer" && node.class == "Players" {
        return (model.side == "client").then(|| model.nodes.get(&model.client).and_then(Weak::upgrade)).flatten();
    }
    if COMPLETION_PROPERTIES.iter().any(|(classes, names, _)| completion_class(&node, classes) && names.split_whitespace().any(|name|name == key))
        || event_class(key).is_some_and(|class| class.split('|').any(|class|instance::is_a(&node, class))) { return None; }
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
        if event_class(name).is_some_and(|class| class.split('|').any(|class|instance::is_a(&node, class))) {
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
        if !completion_label(&name) || !instance::method_class(&name).is_some_and(|class|class.split('|').any(|class|instance::is_a(&node,class))) { continue; }
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

const INSPECT_PAGE_SIZE: usize = 100;

fn inspect_text(text: &str) -> Json {
    let (value, truncated) = crate::console::render_bytes(text.as_bytes(), 512);
    let escaped = text.chars().any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'));
    json!({"type":"string","value":value,"bytes":text.len(),"encoding":if escaped {"escaped-bytes"} else {"utf8"},"truncated":truncated})
}
fn inspect_reference(node: &instance::Node) -> Json {
    json!({"type":"Instance","id":node.id,"name":crate::console::display_bytes(node.name.as_bytes(),512),"class":node.class})
}
fn inspect_row(node: &Link) -> Json {
    let node = node.borrow();
    json!({"id":node.id,"name":crate::console::display_bytes(node.name.as_bytes(),512),"class":node.class,
        "hasChildren":node.children.iter().any(|child| !child.borrow().destroyed)})
}

pub(crate) fn inspect_tree(lua: &Lua, parent: Option<&str>, offset: usize) -> Result<Json> {
    if parent.is_some_and(|id| id.len() > 1024) { return Err(Error::runtime("Instance identifier is too long")); }
    let state = required(lua)?;
    let model = state.borrow();
    let mut nodes: Vec<Link> = match parent {
        Some(id) => model.nodes.get(id).and_then(Weak::upgrade).filter(|node| !node.borrow().destroyed)
            .map(|node| node.borrow().children.iter().filter(|child| !child.borrow().destroyed).cloned().collect()).unwrap_or_default(),
        None => model.nodes.values().filter_map(Weak::upgrade).filter(|node| {
            let node = node.borrow(); !node.destroyed && node.parent.upgrade().is_none()
        }).collect(),
    };
    if parent.is_none() {
        nodes.sort_by(|a,b| { let a=a.borrow();let b=b.borrow();
            (a.id != "game", &a.name, &a.class, &a.id).cmp(&(b.id != "game", &b.name, &b.class, &b.id))
        });
    }
    let total = nodes.len();
    let items: Vec<Json> = nodes.iter().skip(offset).take(INSPECT_PAGE_SIZE).map(inspect_row).collect();
    let end = offset.saturating_add(items.len());
    Ok(json!({"items":items,"total":total,"nextOffset":(end < total).then_some(end)}))
}

pub(crate) fn inspect_node(lua: &Lua, id: &str) -> Result<Json> {
    if id.len() > 1024 { return Err(Error::runtime("Instance identifier is too long")); }
    let node = lookup(lua,id).filter(|node| !node.borrow().destroyed)
        .ok_or_else(|| Error::runtime("Instance unavailable in selected VM"))?;
    let mut result = inspect_row(&node);
    let node = node.borrow();
    let mut properties = BTreeMap::new();
    // These fields live in Rust; making temporary Lua strings or Instance wrappers
    // to display them would change the heap observed by executor getgc.
    properties.insert("Name".to_owned(), inspect_text(&node.name));
    properties.insert("ClassName".to_owned(), inspect_text(&node.class));
    properties.insert("Parent".to_owned(), node.parent.upgrade().map(|parent| inspect_reference(&parent.borrow())).unwrap_or_else(||json!({"type":"nil"})));
    properties.insert("Archivable".to_owned(), json!({"type":"boolean","value":node.archivable}));
    if instance::is_a(&node,"LuaSourceContainer") {
        properties.insert("Source".to_owned(), inspect_text(&node.source));
        properties.insert("LinkedSource".to_owned(), inspect_text(&node.linked_source));
    }
    for (name,value) in &node.properties {
        let preview = crate::console::snapshot(lua,std::iter::once(value)).into_iter().next().unwrap_or_else(||json!({"type":value.type_name(),"truncated":true}));
        properties.insert(name.clone(),preview);
    }
    let mut attributes: Vec<Json> = node.attributes.iter().map(|(name,value)| {
        let value = match value {
            Attribute::Boolean(value) => json!({"type":"boolean","value":value}),
            Attribute::Number(value) => crate::console::snapshot(lua,std::iter::once(&Value::Number(*value))).remove(0),
            Attribute::String(value) => inspect_text(value),
        };
        json!({"name":name,"value":value})
    }).collect();
    attributes.sort_by(|a,b| a["name"].as_str().cmp(&b["name"].as_str()));
    result["properties"] = json!(properties.into_iter().map(|(name,value)|json!({"name":name,"value":value})).collect::<Vec<_>>());
    result["attributes"] = json!(attributes);
    Ok(result)
}

#[cfg(all(test, emulator_local_tests))]
#[path = "../local_tests/model.rs"]
mod tests;
