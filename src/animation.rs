use super::*;

#[derive(Clone)]
pub(super) struct Track {
    node: Weak<RefCell<instance::Node>>, animator: Weak<RefCell<instance::Node>>,
    active: Option<Link>, key: String, playing: bool, position: f64, started: f64,
    speed: f64, weight: f64, ended_at: Option<f64>,
}
pub(super) const METHODS: &[(&str,&CStr)] = &[("LoadAnimation",c"LoadAnimation"),("GetPlayingAnimationTracks",c"GetPlayingAnimationTracks"),("Stop",c"Stop"),("AdjustSpeed",c"AdjustSpeed")];

fn below(node:&Link,ancestor:&Link)->bool {
    let mut at=Some(node.clone());
    while let Some(node)=at { if Rc::ptr_eq(&node,ancestor){return true} at=node.borrow().parent.upgrade(); }
    false
}
pub(super) fn authorized(lua:&Lua,payload:&Json,client:Option<&str>)->Result<bool> {
    let Some(animator)=lookup(lua,payload["animator"].as_str().unwrap_or("")) else{return Ok(false)};
    if animator.borrow().class!="Animator"||animator.borrow().destroyed{return Ok(false)}
    let Some(workspace)=lookup(lua,"Workspace")else{return Ok(false)};
    if !below(&animator,&workspace){return Ok(false)}
    let Some(client)=client else{return Ok(true)};
    let Some(player)=lookup(lua,client)else{return Ok(false)};
    let Some(Value::UserData(character))=player.borrow().properties.get("Character").cloned()else{return Ok(false)};
    let Ok(character)=character.borrow::<Instance>()else{return Ok(false)};
    Ok(below(&animator,&character.0))
}
fn animator(lua:&Lua,node:&Link)->Result<Link> {
    let animator=if node.borrow().class=="Animator" {node.clone()}
        else if let Some(animator)=instance::find_child(node,"Animator",false){animator}
        else {let animator=instance::new_node("Animator")?;created(lua,&animator)?;parent(lua,&animator,Some(node.clone()))?;animator};
    let Some(workspace)=lookup(lua,"Workspace")else{return Err(Error::lua_string("Animator must be in Workspace"))};
    if animator.borrow().class!="Animator"||!below(&animator,&workspace){return Err(Error::lua_string("Animator must be in Workspace"))}
    Ok(animator)
}
fn position(track:&Track,current:f64)->f64 {track.position+if track.playing{(current-track.started).max(0.)*track.speed}else{0.}}
pub(super) fn read(lua:&Lua,node:&Link,key:&str)->Result<Option<Value>> {
    let state=required(lua)?;
    let state=state.borrow();let Some(track)=state.animation_tracks.get(&node.borrow().id)else{return Ok(None)};
    Ok(match key {
        "IsPlaying"=>Some(Value::Boolean(track.playing)),"Length"=>Some(Value::Number(0.)),
        "TimePosition"=>Some(Value::Number(position(track,now(lua)))),"Speed"=>Some(Value::Number(track.speed)),
        "WeightCurrent"|"WeightTarget"=>Some(Value::Number(if track.playing{track.weight}else{0.})),_=>None,
    })
}
pub(super) fn write(lua:&Lua,node:&Link,key:&str,value:&Value)->Result<bool> {
    if matches!(key,"Animation"|"IsPlaying"|"Length"|"Speed"|"WeightCurrent"|"WeightTarget") {return Err(Error::lua_string(format!("Unable to assign property {key}. Property is read only")))}
    if key=="Looped"&&!matches!(value,Value::Boolean(_)){return Err(Error::lua_string("Looped must be a boolean"))}
    if key=="Priority"{enum_name(value,"AnimationPriority")?;}
    if key!="TimePosition"{return Ok(false)}
    let position=lua.coerce_number(value.clone())?.filter(|v|v.is_finite()&&*v>=0.).ok_or_else(||Error::lua_string("TimePosition must be a finite non-negative number"))?;
    if let Some(track)=required(lua)?.borrow_mut().animation_tracks.get_mut(&node.borrow().id){track.position=position;track.started=now(lua);}
    Ok(true)
}
fn load(lua:&Lua,animator:&Link,animation:&Link,key:Option<&str>)->Result<Link> {
    let state=required(lua)?;
    {let mut state=state.borrow_mut();state.animation_tracks.retain(|_,track|track.node.strong_count()>0);
    if state.animation_tracks.len()>=4096{return Err(Error::runtime("BudgetExceeded: AnimationTrack count"))}}
    let node=instance::new_node("AnimationTrack")?;created(lua,&node)?;
    node.borrow_mut().properties.insert("Animation".into(),value(lua,animation.clone())?);
    let id=node.borrow().id.clone();let key=key.unwrap_or(&id).to_owned();
    state.borrow_mut().changes.remove(&id);
    state.borrow_mut().animation_tracks.insert(id,Track{node:Rc::downgrade(&node),animator:Rc::downgrade(animator),active:None,key,playing:false,position:0.,started:now(lua),speed:1.,weight:1.,ended_at:None});
    Ok(node)
}
fn number(lua:&Lua,values:&MultiValue,index:usize,default:f64)->Result<f64>{
    let Some(value)=values.get(index).filter(|value|!value.is_nil())else{return Ok(default)};
    lua.coerce_number(value.clone())?.filter(|v|v.is_finite()).ok_or_else(||Error::lua_string("Animation argument must be a finite number"))
}
fn transmit(lua:&Lua,node:&Link,action:&str,fade:f64)->Result<()> {
    let state=required(lua)?;
    let track=state.borrow().animation_tracks.get(&node.borrow().id).cloned().ok_or_else(||Error::lua_string("AnimationTrack is unavailable"))?;
    let Some(animator)=track.animator.upgrade()else{return Ok(())};
    let Some(Value::UserData(animation))=node.borrow().properties.get("Animation").cloned()else{return Ok(())};
    let animation=animation.borrow::<Instance>()?;
    let content=instance::read_property(lua,&animation,"AnimationId")?;
    let Value::String(content)=content else{return Err(Error::lua_string("AnimationId must be a string"))};
    let looped=matches!(node.borrow().properties.get("Looped"),Some(Value::Boolean(true)));
    emit(lua,"animation",json!({"animator":animator.borrow().id,"track":track.key,"action":action,"content":content.to_str()?.as_ref(),"name":animation.0.borrow().name,"speed":track.speed,"weight":track.weight,"looped":looped,"fade":fade}))
}
pub(super) fn control(lua:&Lua,node:&Link,action:&str,values:MultiValue,wire:bool)->Result<()> {
    guard_write(lua)?;let state=required(lua)?;let id=node.borrow().id.clone();
    if node.borrow().destroyed{return Err(Error::lua_string("AnimationTrack is destroyed"))}
    let fade=if action=="AdjustSpeed"{0.}else{number(lua,&values,0,0.1)?.max(0.)};
    let speed=number(lua,&values,if action=="AdjustSpeed"{0}else{2},1.)?;
    let weight=number(lua,&values,1,1.)?.max(0.);
    let current=now(lua);
    let (animator,was_playing)={let mut state=state.borrow_mut();let track=state.animation_tracks.get_mut(&id).ok_or_else(||Error::lua_string("AnimationTrack is unavailable"))?;
        let was=track.playing;track.position=position(track,current);track.started=current;
        match action {
            "Play"=>{track.playing=true;track.position=0.;track.speed=speed;track.weight=weight;track.active=Some(node.clone());track.ended_at=None;},
            "Stop"=>{track.playing=false;track.ended_at=was.then_some(current+fade);if fade==0.{track.active=None;}},
            "AdjustSpeed"=>track.speed=speed,_=>return Err(unsupported(lua,action)),
        }(track.animator.upgrade(),was)
    };
    if !wire{transmit(lua,node,action,fade)?;}
    if action=="Play" {if let Some(animator)=animator{fire(lua,&animator,"AnimationPlayed",args([value(lua,node.clone())?]))?;}}
    if action=="Stop"&&was_playing {
        fire(lua,node,"Stopped",MultiValue::new())?;
        if fade==0.{if let Some(track)=state.borrow_mut().animation_tracks.get_mut(&id){track.ended_at=None;}fire(lua,node,"Ended",MultiValue::new())?;}
    }
    Ok(())
}
pub(super) fn method(lua:&Lua,node:&Link,name:&str,mut values:MultiValue)->Result<Value>{
    match name {
        "LoadAnimation"=>{guard_write(lua)?;let animation=object_arg(&mut values)?;
            if animation.borrow().class!="Animation"{return Err(Error::lua_string("Animation expected"))}
            let animator=animator(lua,node)?;let track=load(lua,&animator,&animation,None)?;value(lua,track)},
        "GetPlayingAnimationTracks"=>{
            let animator=animator(lua,node)?;let tracks=required(lua)?.borrow().animation_tracks.values().filter(|track|track.active.is_some()&&track.animator.upgrade().is_some_and(|owner|Rc::ptr_eq(&owner,&animator))).filter_map(|track|track.node.upgrade()).collect();
            Ok(Value::Table(instance::instance_list(lua,tracks)?))
        },
        _=>{control(lua,node,name,values,false)?;Ok(Value::Nil)}
    }
}
pub(super) fn deliver(lua:&Lua,payload:&Json)->Result<()> {
    let Some(animator)=lookup(lua,payload["animator"].as_str().unwrap_or(""))else{return Ok(())};
    if animator.borrow().destroyed||animator.borrow().class!="Animator"{return Ok(())}
    let key=payload["track"].as_str().ok_or_else(||Error::runtime("Invalid animation track"))?;
    let state=required(lua)?;
    let found=state.borrow().animation_tracks.values().find(|track|track.key==key).and_then(|track|track.node.upgrade());
    let action=payload["action"].as_str().unwrap_or("");
    let node=if let Some(node)=found{node}else{
        if action!="Play"{return Ok(())}
        let animation=instance::new_node("Animation")?;created(lua,&animation)?;
        state.borrow_mut().changes.remove(&animation.borrow().id);
        animation.borrow_mut().name=payload["name"].as_str().unwrap_or("Animation").into();
        animation.borrow_mut().properties.insert("AnimationId".into(),str_value(lua,payload["content"].as_str().unwrap_or(""))?);
        load(lua,&animator,&animation,Some(key))?
    };
    node.borrow_mut().properties.insert("Looped".into(),Value::Boolean(payload["looped"].as_bool().unwrap_or(false)));
    let values=if action=="AdjustSpeed"{args([Value::Number(payload["speed"].as_f64().unwrap_or(1.))])}
        else{args([Value::Number(payload["fade"].as_f64().unwrap_or(0.)),Value::Number(payload["weight"].as_f64().unwrap_or(1.)),Value::Number(payload["speed"].as_f64().unwrap_or(1.))])};
    control(lua,&node,action,values,true)
}
pub(super) fn frame(lua:&Lua)->Result<()> {
    let state=required(lua)?;let current=now(lua);let mut ended=Vec::new();
    {let mut state=state.borrow_mut();state.animation_tracks.retain(|_,track|{
        let Some(node)=track.node.upgrade()else{return false};
        if node.borrow().destroyed||track.animator.upgrade().is_none_or(|owner|owner.borrow().destroyed){track.playing=false;track.active=None;track.ended_at=None;return true}
        if track.ended_at.is_some_and(|end|end<=current){track.ended_at=None;track.active=None;ended.push(node);}
        true
    });}
    for node in ended{fire(lua,&node,"Ended",MultiValue::new())?;}Ok(())
}
