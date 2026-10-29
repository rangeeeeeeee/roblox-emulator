use mlua::Lua;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};

const MAX_EVENTS: usize = 200;
const MAX_BYTES: usize = 1024 * 1024;
const MAX_ARGUMENT_BYTES: usize = 16 * 1024;
const MAX_ARGUMENT_VALUES: usize = 128;
const MAX_VALUE_BYTES: usize = 4 * 1024;
const MAX_BINARY_PREVIEW: usize = 1024;
const MAX_PENDING_CALLS: usize = 200;
const MAX_BLOCKED_REMOTES: usize = 128;

#[derive(Default)]
pub(crate) struct PacketSpy {
    states: HashMap<String, CaptureState>,
    next: u64,
    dropped: HashMap<String,u64>,
    bytes: usize,
    events: VecDeque<(Value, usize)>,
    calls: VecDeque<(String, String, Value)>,
}

#[derive(Default)]
struct CaptureState { active: bool, paused: bool, blocks: HashMap<String,BlockedRemote> }

struct BlockedRemote { identity: u64, incoming: bool, outgoing: bool }

impl PacketSpy {
    pub(crate) fn watching(&self, realm: &str) -> bool {
        self.states.get(realm).is_some_and(|state|state.active && !state.paused)
    }

    pub(crate) fn control(&mut self, action: &str, realm: &str) -> Result<(), &'static str> {
        match action {
            "start" => {
                let state=self.states.entry(realm.to_owned()).or_default();
                state.active=true; state.paused=false;
            }
            "stop" => { let state=self.states.entry(realm.to_owned()).or_default();state.active=false;state.paused=false;state.blocks.clear(); }
            "pause" if self.states.get(realm).is_some_and(|state|state.active) => self.states.get_mut(realm).unwrap().paused=true,
            "resume" if self.states.get(realm).is_some_and(|state|state.active) => self.states.get_mut(realm).unwrap().paused=false,
            "clear" => self.clear(realm),
            "state" => {},
            _ => return Err("Invalid action or capture is not active for this VM"),
        }
        Ok(())
    }

    pub(crate) fn set_block(&mut self,realm:&str,id:&str,direction:&str,identity:Option<u64>,enabled:bool)->Result<(),&'static str> {
        if !matches!(direction,"incoming"|"outgoing") {return Err("Select incoming or outgoing");}
        let state=self.states.get_mut(realm).filter(|state|state.active).ok_or("Capture is not active for this VM")?;
        if enabled {
            let identity=identity.ok_or("Select a live RemoteEvent in this VM")?;
            if !state.blocks.contains_key(id) && state.blocks.len()>=MAX_BLOCKED_REMOTES {return Err("Too many blocked RemoteEvents");}
            let block=state.blocks.entry(id.to_owned()).or_insert(BlockedRemote{identity,incoming:false,outgoing:false});
            if block.identity!=identity {*block=BlockedRemote{identity,incoming:false,outgoing:false};}
            if direction=="incoming" {block.incoming=true;} else {block.outgoing=true;}
        } else if let Some(block)=state.blocks.get_mut(id) {
            if direction=="incoming" {block.incoming=false;} else {block.outgoing=false;}
            if !block.incoming && !block.outgoing {state.blocks.remove(id);}
        }
        Ok(())
    }

    pub(crate) fn blocked(&self,lua:&Lua,realm:&str,direction:&str,id:&str)->bool {
        let Some(state)=self.states.get(realm).filter(|state|state.active) else{return false};
        state.blocks.get(id).is_some_and(|block|match direction {
            "incoming"=>block.incoming,"outgoing"=>block.outgoing,_=>false,
        } && crate::model::remote_spy_event_identity(lua,id)==Some(block.identity))
    }

    pub(crate) fn prune_blocks(&mut self,lua:&Lua,realm:&str) {
        if let Some(state)=self.states.get_mut(realm) {
            state.blocks.retain(|id,block|crate::model::remote_spy_event_identity(lua,id)==Some(block.identity));
        }
    }

    fn clear(&mut self,realm:&str) {
        self.events.retain(|(event,_)|event["realm"].as_str()!=Some(realm));
        self.calls.retain(|(owner,_,_)|owner!=realm);
        self.bytes=self.events.iter().map(|(_,size)|*size).sum();
        self.dropped.remove(realm);
    }

    pub(crate) fn snapshot(&self,realm:&str) -> Value {
        let state=self.states.get(realm);
        let active=state.is_some_and(|state|state.active);
        let mut blocks=Vec::new();
        if let Some(state)=state {for (id,block) in &state.blocks {
            if block.incoming {blocks.push(json!({"remoteId":id,"direction":"incoming"}));}
            if block.outgoing {blocks.push(json!({"remoteId":id,"direction":"outgoing"}));}
        }}
        blocks.sort_by(|a,b|a["remoteId"].as_str().cmp(&b["remoteId"].as_str()).then(a["direction"].as_str().cmp(&b["direction"].as_str())));
        json!({"realm":realm,"active":active,"paused":active&&state.is_some_and(|state|state.paused),"blocks":blocks,
            "dropped":self.dropped.get(realm).copied().unwrap_or(0),
            "events":self.events.iter().filter(|(event,_)|event["realm"].as_str()==Some(realm)).map(|(event,_)|event).collect::<Vec<_>>()})
    }

    pub(crate) fn observe(&mut self, lua: &Lua, realm: &str, from: &str, side: &str, kind: &str,
        target: &str, payload: &Value, time: f64, direction: &str, blocked:bool, origin: Option<&crate::diagnostics::SpyCaller>) {
        if !self.watching(realm) || !matches!(kind,"remote_event"|"remote_call"|"remote_return") { return; }
        let request = payload.get("requestId").and_then(Value::as_str).unwrap_or("");
        let (method, reference) = if kind == "remote_return" {
            self.calls.iter().find(|(owner,id,_)|owner==realm&&id==request)
                .map(|(_,_,value)|(value["method"].as_str().unwrap_or("RemoteFunction").to_owned(),value["remote"].clone()))
                .unwrap_or_else(||("RemoteFunction".into(),Value::Null))
        } else {
            let method = match (kind,side,target) {
                ("remote_event","client",_) => "FireServer",
                ("remote_event","server","*") => "FireAllClients",
                ("remote_event","server",_) => "FireClient",
                ("remote_call","client",_) => "InvokeServer",
                ("remote_call","server",_) => "InvokeClient",
                _ => "Remote",
            };
            let reference = payload.get("remoteId").and_then(Value::as_str)
                .and_then(|id|crate::model::remote_spy_reference(lua,id)).unwrap_or(Value::Null);
            (method.to_owned(),reference)
        };
        if kind == "remote_call" && !request.is_empty() {
            self.calls.retain(|(owner,id,_)|owner!=realm||id!=request);
            if self.calls.len() >= MAX_PENDING_CALLS { self.calls.pop_front(); }
            self.calls.push_back((realm.to_owned(),request.to_owned(),json!({"method":method,"remote":reference})));
        } else if kind == "remote_return" { self.calls.retain(|(owner,id,_)|owner!=realm||id!=request); }

        let args = payload.get("args").unwrap_or(&Value::Null);
        let encoded = serde_json::to_vec(args).map(|b|b.len()).unwrap_or(usize::MAX);
        // A server-only Instance is decoded as nil on a client. Never expose
        // raw sender IDs or table entries the receiving VM cannot observe.
        let (args,truncated,omitted_reason) = match capture_tuple(lua,args,direction=="incoming") {
            Some((value,false))=>(Some(value),false,None),
            Some((value,true))=>(Some(value),true,Some("some values or arguments exceed the snapshot limits")),
            None=>(None,true,Some("the argument tuple cannot be represented")),
        };
        let mut references = HashMap::new();
        if let Some(args)=args.as_ref() { collect_references(lua,args,&mut references,0); }
        let target_ref = if side == "server" && !matches!(target,""|"*"|"server") && kind != "remote_return" {
            crate::model::remote_spy_reference(lua,target)
        } else { None };
        let sender_ref = if direction=="incoming" && side=="client" && matches!(kind,"remote_event"|"remote_call") {
            payload.get("sender").and_then(Value::as_str).or_else(||payload.get("clientId").and_then(Value::as_str))
                .and_then(|id|crate::model::remote_spy_reference(lua,id))
        } else { None };
        self.next = self.next.saturating_add(1);
        let wall_time=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()
            .and_then(|duration|u64::try_from(duration.as_millis()).ok());
        let event = json!({"id":self.next,"realm":realm,"time":time,"from":from,"direction":direction,"target":target,
            "kind":kind,"method":method,"remote":reference,"requestId":request,"ok":payload.get("ok"),
            "arguments":args,"argumentBytes":encoded,
            "truncated":truncated,"omittedReason":omitted_reason,"references":references,"targetReference":target_ref,"senderReference":sender_ref,"blocked":blocked,
            "origin":origin,"wallTimeMs":wall_time});
        let size = serde_json::to_vec(&event).map(|b|b.len()).unwrap_or(MAX_BYTES+1);
        if size > MAX_BYTES { *self.dropped.entry(realm.to_owned()).or_default() += 1; return; }
        while self.events.len() >= MAX_EVENTS || self.bytes.saturating_add(size) > MAX_BYTES {
            if let Some((event,old)) = self.events.pop_front() {
                self.bytes = self.bytes.saturating_sub(old);
                if let Some(owner)=event["realm"].as_str(){*self.dropped.entry(owner.to_owned()).or_default() += 1;}
            }
            else { break; }
        }
        self.bytes += size; self.events.push_back((event,size));
    }
}

fn json_size(value:&Value)->usize {serde_json::to_vec(value).map(|bytes|bytes.len()).unwrap_or(usize::MAX)}

fn placeholder(value:&Value,reason:&str)->Value {
    json!({"tag":"truncated","originalTag":value.get("tag").and_then(Value::as_str),"reason":reason,"truncated":true})
}

// Keep a bounded, byte-exact prefix instead of losing all other arguments when
// one serialized value is large. The UI must never replay partial snapshots.
fn capture_value(value:&Value,limit:usize,depth:usize)->(Value,bool) {
    if json_size(value)<=limit {return (value.clone(),false)}
    let tag=value.get("tag").and_then(Value::as_str).unwrap_or("");
    if matches!(tag,"buffer"|"string") {
        if let Some(bytes)=value.get("bytes").and_then(Value::as_array) {
            let mut len=bytes.len().min(MAX_BINARY_PREVIEW);
            loop {
                let sample=json!({"tag":tag,"bytes":&bytes[..len],"truncated":true,"totalBytes":bytes.len()});
                if json_size(&sample)<=limit {return (sample,true)}
                if len==0 {break} len/=2;
            }
        } else if let Some(text)=value.get("value").and_then(Value::as_str) {
            let mut len=text.len().min(MAX_BINARY_PREVIEW);
            while !text.is_char_boundary(len) {len-=1;}
            loop {
                let sample=json!({"tag":tag,"value":&text[..len],"truncated":true,"totalBytes":text.len()});
                if json_size(&sample)<=limit {return (sample,true)}
                if len==0 {break} len/=2;while !text.is_char_boundary(len) {len-=1;}
            }
        }
    }
    if tag=="table" && depth<24 {
        if let Some(entries)=value.get("entries").and_then(Value::as_array) {
            let mut output=Vec::new();
            for entry in entries.iter().take(MAX_ARGUMENT_VALUES) {
                let (Some(key),Some(item))=(entry.get("key"),entry.get("value")) else {break};
                if json_size(key)>1024 {break}
                let (item,_)=capture_value(item,MAX_VALUE_BYTES/2,depth+1);
                output.push(json!({"key":key,"value":item}));
                let candidate=json!({"tag":"table","entries":output,"truncated":true,"totalEntries":entries.len()});
                if json_size(&candidate)>limit {output.pop();break;}
            }
            let sample=json!({"tag":"table","entries":output,"truncated":true,"totalEntries":entries.len()});
            if json_size(&sample)<=limit {return (sample,true)}
        }
    }
    (placeholder(value,"value exceeds the snapshot limit"),true)
}

fn capture_tuple(lua:&Lua,args:&Value,incoming:bool)->Option<(Value,bool)> {
    let n=usize::try_from(args.get("n")?.as_u64()?).ok()?;
    let values=args.get("values")?.as_array()?;
    if n != values.len(){return None;}
    let mut captured=Vec::new();let mut used=64usize;let mut partial=n>MAX_ARGUMENT_VALUES;
    for value in values.iter().take(MAX_ARGUMENT_VALUES) {
        let normal=if incoming {normalize_value(lua,value,0)} else {Some(value.clone())};
        let normal=normal.unwrap_or_else(||placeholder(value,"value exceeds the nesting limit"));
        let (value,truncated)=capture_value(&normal,MAX_VALUE_BYTES,0);
        let bytes=json_size(&value).saturating_add(1);
        if used.saturating_add(bytes)>MAX_ARGUMENT_BYTES {partial=true;break;}
        used+=bytes;partial|=truncated || value.get("tag").and_then(Value::as_str)==Some("truncated");
        captured.push(value);
    }
    Some((json!({"n":n,"values":captured}),partial))
}

fn normalize_value(lua:&Lua,value:&Value,depth:usize)->Option<Value> {
    if depth>=64{return None;}
    match value.get("tag")?.as_str()? {
        "instance"=>{
            let id=value.get("id")?.as_str()?;
            if crate::model::remote_spy_visible(lua,id){Some(value.clone())}else{Some(json!({"tag":"nil"}))}
        },
        "table"=>{
            let mut entries=Vec::new();
            for entry in value.get("entries")?.as_array()? {
                let key=normalize_value(lua,entry.get("key")?,depth+1)?;
                let item=normalize_value(lua,entry.get("value")?,depth+1)?;
                if key["tag"]!="nil" && item["tag"]!="nil" {entries.push(json!({"key":key,"value":item}));}
            }
            Some(json!({"tag":"table","entries":entries}))
        },
        _=>Some(value.clone()),
    }
}

fn collect_references(lua:&Lua,value:&Value,out:&mut HashMap<String,Value>,depth:usize) {
    if depth >= 32 || out.len() >= 128 { return; }
    if value.get("tag").and_then(Value::as_str)==Some("instance") {
        if let Some(id)=value.get("id").and_then(Value::as_str) {
            if !out.contains_key(id) { out.insert(id.to_owned(),crate::model::remote_spy_reference(lua,id).unwrap_or(Value::Null)); }
        }
        return;
    }
    match value {
        Value::Array(items)=>for item in items { collect_references(lua,item,out,depth+1); },
        Value::Object(items)=>for item in items.values() { collect_references(lua,item,out,depth+1); },
        _=>{},
    }
}
