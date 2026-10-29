use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};

const MAX_NODES: usize = 4096;
const MAX_SOURCE: usize = 1024 * 1024;
const MAX_PLACE: usize = 8 * 1024 * 1024;
const MAX_DEPTH: usize = 64;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Node {
    id: String,
    parent: Option<String>,
    class: String,
    name: String,
    builtin: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    properties: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    metadata: Option<NodeMetadata>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NodeMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name_bytes: Option<Vec<u8>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    linked_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    destroyed: Option<bool>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    attributes: BTreeMap<String, Value>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ScenarioBlueprint {
    settings: Value,
    original_nodes: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Place {
    schema: u8,
    name: String,
    profile: String,
    nodes: Vec<Node>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scenario: Option<ScenarioBlueprint>,
}

fn script(class: &str) -> bool { matches!(class, "Script" | "LocalScript" | "ModuleScript") }
fn runnable(class: &str) -> bool { matches!(class, "Script" | "LocalScript") }
fn creatable(class: &str) -> bool {
    matches!(class, "Script" | "LocalScript" | "ModuleScript" | "Folder" | "Actor" | "Model" | "Part" |
        "RemoteEvent" | "RemoteFunction" | "BindableEvent" | "BindableFunction" | "StringValue" | "BoolValue" | "NumberValue" | "IntValue")
}
fn valid_property(class: &str, name: &str, value: &Value) -> bool {
    match (class, name) {
        ("Part", "Anchored" | "CanCollide" | "CanTouch" | "CanQuery" | "Massless") | ("BoolValue", "Value") => value.is_boolean(),
        ("Part", "Transparency") => value.as_f64().is_some_and(|number| number.is_finite() && (0.0..=1.0).contains(&number)),
        ("NumberValue", "Value") => value.as_f64().is_some_and(f64::is_finite),
        ("IntValue", "Value") => value.as_i64().is_some(),
        ("StringValue", "Value") => value.as_str().is_some_and(|text| text.len() <= MAX_SOURCE),
        ("Part", "Position" | "Size") => value.as_object().is_some_and(|object| {
            object.len() == 4 && object.get("tag").and_then(Value::as_str) == Some("Vector3") && ["x", "y", "z"].iter().all(|key| {
                object.get(*key).and_then(Value::as_f64).is_some_and(|number| number.is_finite() && number.abs() <= 1_000_000.0 && (name != "Size" || number > 0.0))
            })
        }),
        _ => false,
    }
}
fn valid_profile(profile: &str) -> bool { matches!(profile, "generic" | "unc" | "macsploit-744.2" | "macsploit-744.4") }
fn valid_name(name: &str) -> bool {
    !name.trim().is_empty() && name.len() <= 256 && !name.chars().any(|c| c.is_control())
}
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 96 && !id.starts_with("play-")
        && !matches!(id, "character1" | "characterRoot1" | "characterHead1" | "characterHumanoid1" | "characterAnimator1")
        && id.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
}
fn valid_scenario_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 96 && id.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
}
fn argument<'a>(body: &'a Value, key: &str) -> Result<&'a str, String> {
    body.get(key).and_then(Value::as_str).ok_or_else(|| format!("{key} must be a string"))
}

fn builtin_nodes() -> Vec<Node> {
    let mut result: Vec<_> = crate::instance::SERVICES.iter().map(|service| Node {
        id: (*service).into(), parent: None, class: (*service).into(), name: (*service).into(),
        builtin: true, source: None, enabled: None, properties: BTreeMap::new(), metadata: None,
    }).collect();
    for (id, parent, class, name) in [
        ("starterPlayerScripts", "StarterPlayer", "StarterPlayerScripts", "StarterPlayerScripts"),
        ("starterCharacterScripts", "StarterPlayer", "StarterCharacterScripts", "StarterCharacterScripts"),
        ("player1", "Players", "Player", "LocalPlayer"),
        ("playerScripts1", "player1", "PlayerScripts", "PlayerScripts"),
        ("playerGui1", "player1", "PlayerGui", "PlayerGui"),
    ] {
        result.push(Node { id: id.into(), parent: Some(parent.into()), class: class.into(), name: name.into(), builtin: true, source: None, enabled: None, properties: BTreeMap::new(), metadata: None });
    }
    result
}

fn ancestors<'a>(node:&'a Node,nodes:&HashMap<&'a str,&'a Node>)->Vec<&'a Node>{
    let mut out=Vec::new();let mut cursor=Some(node);
    while let Some(current)=cursor {out.push(current);cursor=current.parent.as_deref().and_then(|id|nodes.get(id).copied());}
    out
}

fn scenario_node(node:&Node)->Value {
    let mut value=json!({"id":node.id,"parent":node.parent,"class":node.class,"name":node.name});
    if let Some(source)=&node.source {value["source"]=json!(source);}
    let mut properties=node.properties.clone();
    if let Some(enabled)=node.enabled {properties.remove("Enabled");properties.insert("Disabled".into(),json!(!enabled));}
    if !properties.is_empty(){value["properties"]=json!(properties);}
    if let Some(metadata)=&node.metadata {
        if let Some(bytes)=&metadata.name_bytes {value["nameBytes"]=json!(bytes);}
        if let Some(source)=&metadata.linked_source {value["linkedSource"]=json!(source);}
        if let Some(destroyed)=metadata.destroyed {value["destroyed"]=json!(destroyed);}
        if !metadata.attributes.is_empty(){value["attributes"]=json!(metadata.attributes);}
    }
    value
}

fn remap_instances(value:&mut Value,mapping:&HashMap<String,String>){
    match value {
        Value::Object(object)=>{
            if object.get("tag").and_then(Value::as_str)==Some("instance") {
                if let Some(replacement)=object.get("id").and_then(Value::as_str).and_then(|id|mapping.get(id)) {object.insert("id".into(),json!(replacement));}
            }
            for child in object.values_mut(){remap_instances(child,mapping);}
        },
        Value::Array(values)=>for child in values {remap_instances(child,mapping);},
        _=>{},
    }
}

impl Place {
    pub(crate) fn new(profile: &str) -> Result<Self, String> {
        if !valid_profile(profile) { return Err("Unknown executor profile".into()); }
        Ok(Self { schema: 1, name: "Untitled".into(), profile: profile.into(), nodes: builtin_nodes(), scenario: None })
    }

    // `scenario` is already assembled by Session::load. Never resolve paths here:
    // an exported place contains source bytes, not sourcePath or insertion paths.
    pub(crate) fn from_scenario(scenario: &crate::Scenario) -> Result<Self, String> {
        let mut settings=serde_json::to_value(scenario).map_err(|e|e.to_string())?;
        settings["nodes"]=json!([]);
        let profile=scenario.realms.iter().find(|realm|realm.side=="client" && realm.executor)
            .map(|realm|realm.executor_profile.clone()).unwrap_or_else(||"generic".into());
        let mut nodes:Vec<Node>=builtin_nodes().into_iter().filter(|node|node.parent.is_none()).map(|mut node|{
            node.parent=Some("game".into());node
        }).collect();
        nodes.push(Node{id:"game".into(),parent:None,class:"DataModel".into(),name:"Game".into(),builtin:true,
            source:None,enabled:None,properties:BTreeMap::new(),metadata:None});
        let mut original_nodes=Vec::new();
        for raw in &scenario.nodes {
            let id=raw.get("id").and_then(Value::as_str).ok_or("Scenario node is missing an ID")?;
            let class=raw.get("class").and_then(Value::as_str).ok_or("Scenario node is missing a class")?;
            let name=raw.get("name").and_then(Value::as_str).unwrap_or(class);
            if !valid_scenario_id(id) || original_nodes.iter().any(|entry|entry==id) {return Err("Invalid or duplicate scenario node ID".into())}
            original_nodes.push(id.to_owned());
            let properties=match raw.get("properties") {
                None|Some(Value::Null)=>BTreeMap::new(),
                Some(Value::Object(values))=>values.iter().map(|(key,value)|(key.clone(),value.clone())).collect(),
                _=>return Err("Scenario properties must be an object".into()),
            };
            let attributes=match raw.get("attributes") {
                None|Some(Value::Null)=>BTreeMap::new(),
                Some(Value::Object(values))=>values.iter().map(|(key,value)|(key.clone(),value.clone())).collect(),
                _=>return Err("Scenario attributes must be an object".into()),
            };
            let enabled=if runnable(class) {Some(match properties.get("Disabled") {
                Some(Value::Bool(disabled))=>!*disabled,
                None=>properties.get("Enabled").and_then(Value::as_bool).unwrap_or(true),
                _=>return Err("Invalid script Disabled property".into()),
            })} else {None};
            if raw.get("sourcePath").is_some() && raw.get("source").and_then(Value::as_str).is_none() {
                return Err("Scenario source was not assembled before import".into());
            }
            let source=if script(class){Some(raw.get("source").and_then(Value::as_str).unwrap_or("").to_owned())}else{None};
            let name_bytes=match raw.get("nameBytes") { None|Some(Value::Null)=>None, Some(value)=>Some(serde_json::from_value(value.clone()).map_err(|_|"Invalid binary instance name")?) };
            let linked_source=match raw.get("linkedSource") { None|Some(Value::Null)=>None, Some(Value::String(value))=>Some(value.clone()), _=>return Err("Invalid LinkedSource".into()) };
            let destroyed=match raw.get("destroyed") { None|Some(Value::Null)=>None, Some(Value::Bool(value))=>Some(*value), _=>return Err("Invalid destroyed state".into()) };
            let metadata=if name_bytes.is_some()||linked_source.is_some()||destroyed.is_some()||!attributes.is_empty(){Some(NodeMetadata{name_bytes,linked_source,destroyed,attributes})}else{None};
            if let Some(base)=nodes.iter_mut().find(|node|node.id==id) {
                if base.class!=class || base.name!=name {return Err(format!("Scenario built-in {id} does not match its service"))}
                base.properties=properties;base.metadata=metadata;
            } else {
                nodes.push(Node{id:id.into(),parent:raw.get("parent").and_then(Value::as_str).map(str::to_owned),class:class.into(),name:name.into(),builtin:false,source,enabled,properties,metadata});
            }
        }
        let place=Self{schema:1,name:scenario.name.clone(),profile,nodes,scenario:Some(ScenarioBlueprint{settings,original_nodes})};
        place.validate()?;
        Ok(place)
    }

    pub(crate) fn from_json(value: Value) -> Result<Self, String> {
        if serde_json::to_vec(&value).map_err(|e| e.to_string())?.len() > MAX_PLACE { return Err("Place exceeds 8 MiB".into()); }
        let place: Self = serde_json::from_value(value).map_err(|e| format!("Invalid place: {e}"))?;
        place.validate()?;
        Ok(place)
    }

    pub(crate) fn snapshot(&self) -> Value { serde_json::to_value(self).expect("place serializes") }

    pub(crate) fn set_name(&mut self, name: &str) -> Result<(), String> {
        if !valid_name(name) { return Err("Place name must contain 1..256 bytes without control characters".into()); }
        let previous = std::mem::replace(&mut self.name, name.into());
        if let Err(error) = self.validate() { self.name = previous; return Err(error); }
        Ok(())
    }

    fn validate(&self) -> Result<(), String> {
        if self.schema != 1 || !valid_name(&self.name) || !valid_profile(&self.profile) { return Err("Invalid place metadata".into()); }
        if self.nodes.len() > MAX_NODES { return Err("Place contains too many instances".into()); }
        let imported=self.scenario.is_some();
        if let Some(blueprint)=&self.scenario {
            if blueprint.settings.get("nodes").and_then(Value::as_array).is_none_or(|nodes|!nodes.is_empty()) {
                return Err("Scenario metadata cannot contain nodes or source paths".into());
            }
            let scenario:crate::Scenario=serde_json::from_value(blueprint.settings.clone()).map_err(|e|format!("Invalid scenario metadata: {e}"))?;
            if scenario.engine!="native" || !matches!(scenario.signal_behavior.as_str(),"immediate"|"deferred")
                || !scenario.step.is_finite() || !(0.001..=1.0).contains(&scenario.step)
                || !scenario.latency.is_finite() || !(0.0..=60.0).contains(&scenario.latency)
                || !(8..=512).contains(&scenario.memory_mb) || !(10..=10000).contains(&scenario.cpu_ms)
                || scenario.realms.is_empty() || scenario.realms.len()>16 || blueprint.original_nodes.len()>MAX_NODES {
                return Err("Invalid scenario limits".into());
            }
            let mut realm_ids=HashSet::new();
            for realm in &scenario.realms {
                if !valid_scenario_id(&realm.id) || !realm_ids.insert(&realm.id) || !matches!(realm.side.as_str(),"server"|"client")
                    || (realm.side=="client" && realm.client_id.as_ref().is_none_or(|id|!valid_scenario_id(id)))
                    || realm.actor_id.as_ref().is_some_and(|id|!valid_scenario_id(id)) || !valid_profile(&realm.executor_profile)
                    || realm.entry.len()+realm.executor_entry.len()>MAX_NODES
                    || realm.entry.iter().chain(&realm.executor_entry).any(|id|!valid_scenario_id(id)) {
                    return Err("Invalid scenario realm".into());
                }
            }
            let mut originals=HashSet::new();
            if blueprint.original_nodes.iter().any(|id|!valid_scenario_id(id)||!originals.insert(id)) {return Err("Invalid scenario node IDs".into())}
        }
        let builtins = if imported {
            let mut services:Vec<Node>=builtin_nodes().into_iter().filter(|node|node.parent.is_none()).map(|mut node|{node.parent=Some("game".into());node}).collect();
            services.push(Node{id:"game".into(),parent:None,class:"DataModel".into(),name:"Game".into(),builtin:true,source:None,enabled:None,properties:BTreeMap::new(),metadata:None});
            services
        } else {builtin_nodes()};
        let expected: HashMap<_, _> = builtins.iter().map(|node| (node.id.as_str(), node)).collect();
        let mut nodes = HashMap::new();
        for node in &self.nodes {
            if !(if imported {valid_scenario_id(&node.id)} else {valid_id(&node.id)}) || !valid_name(&node.name) || nodes.insert(node.id.as_str(), node).is_some() {
                return Err("Invalid or duplicate instance ID or name".into());
            }
            if let Some(base) = expected.get(node.id.as_str()) {
                if !node.builtin || node.name != base.name || node.parent != base.parent || node.class != base.class || node.source.is_some() || node.enabled.is_some() {
                    return Err("Built-in instances cannot be changed".into());
                }
            } else if node.builtin || !(if imported {crate::instance::implemented(&node.class)} else {creatable(&node.class)}) || node.parent.is_none() {
                return Err("Invalid instance class or parent".into());
            }
            if script(&node.class) {
                if node.source.as_ref().is_none_or(|value| value.len() > MAX_SOURCE) || node.enabled.is_some() != runnable(&node.class) {
                    return Err("Invalid script source or enabled state".into());
                }
            } else if node.source.is_some() || node.enabled.is_some() {
                return Err("Only scripts may contain source or an enabled state".into());
            }
            if node.properties.iter().any(|(name,value)| if imported {!valid_name(name)} else {!valid_property(&node.class,name,value)}) {
                return Err("Invalid or unsupported instance property".into());
            }
            if !imported && node.metadata.is_some() {return Err("Unexpected instance metadata".into())}
        }
        if builtins.iter().any(|base| !nodes.contains_key(base.id.as_str())) { return Err("A built-in instance is missing".into()); }
        for node in &self.nodes {
            let mut cursor = node;
            let mut seen = HashSet::new();
            for depth in 0..=MAX_DEPTH {
                if !seen.insert(cursor.id.as_str()) { return Err("Instance parent cycle".into()); }
                match cursor.parent.as_deref() {
                    None => break,
                    Some(parent) if depth == MAX_DEPTH => return Err(format!("Instance tree exceeds {MAX_DEPTH} levels at {parent}")),
                    Some(parent) => cursor = nodes.get(parent).copied().ok_or_else(|| "Instance parent is missing".to_owned())?,
                }
            }
        }
        if serde_json::to_vec(self).map_err(|e| e.to_string())?.len() > MAX_PLACE { return Err("Place exceeds 8 MiB".into()); }
        Ok(())
    }

    pub(crate) fn update(&mut self, body: &Value) -> Result<Value, String> {
        let action = argument(body, "action")?;
        if action == "profile" {
            let profile = argument(body, "profile")?;
            if !valid_profile(profile) { return Err("Unknown executor profile".into()); }
            let previous_scenario=self.scenario.clone();
            if profile!=self.profile {
                if let Some(blueprint)=&mut self.scenario {
                    if let Some(realms)=blueprint.settings.get_mut("realms").and_then(Value::as_array_mut) {
                        for realm in realms {
                            if realm["side"]=="client" && realm["executor"]==true {realm["executorProfile"]=json!(profile);}
                        }
                    }
                }
            }
            let previous = std::mem::replace(&mut self.profile, profile.into());
            if let Err(error) = self.validate() { self.profile = previous; self.scenario=previous_scenario; return Err(error); }
            return Ok(json!({}));
        }
        if action == "create" {
            if self.nodes.len() >= MAX_NODES { return Err("Place contains too many instances".into()); }
            let parent = argument(body, "parent")?;
            let class = argument(body, "class")?;
            let name = body.get("name").map(|_| argument(body, "name")).transpose()?.unwrap_or(class);
            if !self.nodes.iter().any(|node| node.id == parent) { return Err("Parent instance is unavailable".into()); }
            if !creatable(class) || !valid_name(name) { return Err("Invalid class or name".into()); }
            let ids: HashSet<_> = self.nodes.iter().map(|node| node.id.as_str())
                .chain(self.scenario.iter().flat_map(|scenario|scenario.original_nodes.iter().map(String::as_str))).collect();
            let id = (1..=MAX_NODES).map(|i| format!("node-{i}")).find(|id| !ids.contains(id.as_str())).ok_or("No instance ID available")?;
            self.nodes.push(Node { id: id.clone(), parent: Some(parent.into()), class: class.into(), name: name.into(), builtin: false,
                source: script(class).then(String::new), enabled: runnable(class).then_some(true), properties:BTreeMap::new(), metadata:None });
            if let Err(error) = self.validate() { self.nodes.pop(); return Err(error); }
            return Ok(json!({"id":id}));
        }
        let id = argument(body, "id")?;
        let index = self.nodes.iter().position(|node| node.id == id).ok_or("Instance is unavailable")?;
        if self.nodes[index].builtin { return Err("Built-in instances cannot be changed".into()); }
        match action {
            "rename" => {
                let name = argument(body, "name")?;
                if !valid_name(name) { return Err("Invalid instance name".into()); }
                let previous = std::mem::replace(&mut self.nodes[index].name, name.into());
                let previous_metadata = self.nodes[index].metadata.clone();
                if let Some(metadata) = &mut self.nodes[index].metadata { metadata.name_bytes = None; }
                if let Err(error) = self.validate() { self.nodes[index].name = previous; self.nodes[index].metadata = previous_metadata; return Err(error); }
            },
            "source" => {
                let source = argument(body, "source")?;
                if !script(&self.nodes[index].class) || source.len() > MAX_SOURCE { return Err("Invalid script source".into()); }
                let previous = self.nodes[index].source.replace(source.into());
                if let Err(error) = self.validate() { self.nodes[index].source = previous; return Err(error); }
            },
            "enabled" => {
                if !runnable(&self.nodes[index].class) { return Err("Only Script and LocalScript can be enabled".into()); }
                let enabled = body.get("enabled").and_then(Value::as_bool).ok_or("enabled must be boolean")?;
                let previous = self.nodes[index].enabled.replace(enabled);
                if let Err(error) = self.validate() { self.nodes[index].enabled = previous; return Err(error); }
            },
            "property" => {
                let name=argument(body,"name")?;
                let value=body.get("value").ok_or("Property value is required")?;
                if !valid_property(&self.nodes[index].class,name,value) {return Err("Invalid or unsupported instance property".into())}
                let previous=self.nodes[index].properties.insert(name.into(),value.clone());
                if let Err(error)=self.validate() {
                    if let Some(previous)=previous {self.nodes[index].properties.insert(name.into(),previous);} else {self.nodes[index].properties.remove(name);}
                    return Err(error)
                }
            },
            "reparent" => {
                let parent = argument(body, "parent")?;
                if !self.nodes.iter().any(|node| node.id == parent) { return Err("Parent instance is unavailable".into()); }
                let previous = self.nodes[index].parent.replace(parent.into());
                if let Err(error) = self.validate() { self.nodes[index].parent = previous; return Err(error); }
            },
            "delete" => {
                let mut removed = HashSet::from([id.to_owned()]);
                loop {
                    let before = removed.len();
                    for node in &self.nodes {
                        if node.parent.as_ref().is_some_and(|parent| removed.contains(parent)) { removed.insert(node.id.clone()); }
                    }
                    if removed.len() == before { break; }
                }
                if self.nodes.iter().any(|node| node.builtin && removed.contains(&node.id)) { return Err("Built-in instances cannot be deleted".into()); }
                self.nodes.retain(|node| !removed.contains(&node.id));
            },
            "duplicate" => {
                let mut selected=HashSet::from([id.to_owned()]);
                loop {
                    let before=selected.len();
                    for node in &self.nodes {if node.parent.as_ref().is_some_and(|parent|selected.contains(parent)) {selected.insert(node.id.clone());}}
                    if selected.len()==before {break}
                }
                if self.nodes.iter().any(|node|node.builtin && selected.contains(&node.id)) {return Err("Built-in instances cannot be duplicated".into())}
                if self.nodes.len().saturating_add(selected.len())>MAX_NODES {return Err("Place contains too many instances".into())}
                let mut used:HashSet<String>=self.nodes.iter().map(|node|node.id.clone())
                    .chain(self.scenario.iter().flat_map(|scenario|scenario.original_nodes.iter().cloned())).collect();
                let mut mapping=HashMap::new();
                for node in self.nodes.iter().filter(|node|selected.contains(&node.id)) {
                    let next=(1..=MAX_NODES).map(|i|format!("node-{i}")).find(|candidate|!used.contains(candidate)).ok_or("No instance ID available")?;
                    used.insert(next.clone());mapping.insert(node.id.clone(),next);
                }
                let before=self.nodes.len();
                for mut copy in self.nodes[..before].iter().filter(|node|selected.contains(&node.id)).cloned().collect::<Vec<_>>() {
                    copy.id=mapping[&copy.id].clone();
                    if let Some(parent)=copy.parent.as_mut(){if let Some(replacement)=mapping.get(parent){*parent=replacement.clone();}}
                    for value in copy.properties.values_mut(){remap_instances(value,&mapping);}
                    self.nodes.push(copy);
                }
                if let Err(error)=self.validate(){self.nodes.truncate(before);return Err(error)}
                return Ok(json!({"id":mapping[id]}));
            },
            _ => return Err("Unknown place action".into()),
        }
        Ok(json!({"id":id}))
    }

    fn imported_scenario(&self,blueprint:&ScenarioBlueprint)->Result<crate::Scenario,String>{
        let mut scenario:crate::Scenario=serde_json::from_value(blueprint.settings.clone()).map_err(|e|format!("Invalid scenario metadata: {e}"))?;
        scenario.name=self.name.clone();
        let nodes:HashMap<&str,&Node>=self.nodes.iter().map(|node|(node.id.as_str(),node)).collect();
        let originals:HashSet<&str>=blueprint.original_nodes.iter().map(String::as_str).collect();
        scenario.nodes=self.nodes.iter().filter(|node|!node.builtin || originals.contains(node.id.as_str())).map(scenario_node).collect();

        // Deleting an Actor deletes its VM only when the Actor itself is gone.
        // Existing actor identities and explicitly configured entry order survive Play.
        scenario.realms.retain(|realm|realm.actor_id.as_ref().is_none_or(|id|nodes.contains_key(id.as_str())));
        let configured=scenario.realms.clone();
        let mut relocated=Vec::<(usize,String)>::new();
        for realm in &mut scenario.realms {
            if let Some(id)=&realm.client_id {
                if nodes.get(id.as_str()).is_none_or(|node|node.class!="Player") {return Err(format!("Client {} has no matching Player instance",realm.id))}
            }
            if let Some(id)=&realm.actor_id {
                if nodes.get(id.as_str()).is_none_or(|node|node.class!="Actor") {return Err(format!("Realm {} has no matching Actor instance",realm.id))}
            }
            let mut entry=Vec::new();
            for id in &realm.entry {
                let Some(node)=nodes.get(id.as_str()).copied() else{continue};
                if !runnable(&node.class) {return Err(format!("Realm {} entry {} is not a Script or LocalScript",realm.id,id))}
                if node.enabled!=Some(true){continue}
                let path=ancestors(node,&nodes);
                let actor=path.iter().skip(1).find(|parent|parent.class=="Actor").map(|parent|parent.id.as_str());
                let owner=(realm.side=="client").then(||path.iter().find(|parent|parent.class=="Player").map(|parent|parent.id.as_str())).flatten();
                if actor!=realm.actor_id.as_deref() || owner.is_some_and(|owner|realm.client_id.as_deref()!=Some(owner)) {
                    let candidates:Vec<usize>=configured.iter().enumerate().filter(|(_,target)|target.side==realm.side
                        && target.actor_id.as_deref()==actor && owner.is_none_or(|owner|target.client_id.as_deref()==Some(owner)))
                        .map(|(index,_)|index).collect();
                    let preferred:Vec<usize>=candidates.iter().copied().filter(|index|configured[*index].client_id==realm.client_id).collect();
                    let target=if candidates.len()==1 {candidates[0]} else if owner.is_none() && preferred.len()==1 {preferred[0]}
                        else{return Err(format!("Script {} was moved; its target VM is ambiguous",node.name))};
                    relocated.push((target,id.clone()));
                } else {entry.push(id.clone());}
            }
            realm.entry=entry;
            realm.executor_entry.retain(|id|nodes.get(id.as_str()).is_some_and(|node|node.source.is_some() && node.enabled!=Some(false)));
        }
        for (index,id) in relocated {if !scenario.realms[index].entry.contains(&id){scenario.realms[index].entry.push(id);}}
        for node in self.nodes.iter().filter(|node|!originals.contains(node.id.as_str()) && runnable(&node.class) && node.enabled==Some(true)) {
            let path=ancestors(node,&nodes);
            let actor=path.iter().skip(1).find(|parent|parent.class=="Actor").map(|parent|parent.id.clone());
            let is_under=|id:&str|path.iter().any(|ancestor|ancestor.id==id);
            let (side,client)=if node.class=="Script" {
                if !is_under("Workspace") && !is_under("ServerScriptService"){continue}
                ("server",None)
            } else {
                let player=path.iter().find(|parent|parent.class=="Player").map(|parent|parent.id.clone());
                if player.is_none() && !is_under("ReplicatedFirst") && !(is_under("Workspace") && actor.is_some()){continue}
                let client=if let Some(player)=player {player} else {
                    let clients:HashSet<_>=scenario.realms.iter().filter_map(|realm|realm.client_id.as_deref()).collect();
                    if clients.len()!=1 {return Err(format!("Cannot choose a client VM for new script {}",node.name))}
                    clients.into_iter().next().unwrap().to_owned()
                };
                ("client",Some(client))
            };
            let candidates:Vec<usize>=scenario.realms.iter().enumerate().filter(|(_,realm)|
                realm.side==side && realm.actor_id==actor && (side=="server" || realm.client_id==client)).map(|(index,_)|index).collect();
            let index=if candidates.len()==1 {candidates[0]} else if candidates.is_empty() && actor.is_some() {
                if scenario.realms.len()>=16{return Err("A place can use at most 16 VMs".into())}
                let id=format!("actor-{side}-{}",actor.as_deref().unwrap());
                if !valid_scenario_id(&id) || scenario.realms.iter().any(|realm|realm.id==id){return Err("Cannot assign a unique Actor VM".into())}
                let realm:crate::RealmConfig=serde_json::from_value(json!({"id":id,"side":side,"actorId":actor,"clientId":client,"entry":[]})).map_err(|e|e.to_string())?;
                scenario.realms.push(realm);scenario.realms.len()-1
            } else {return Err(format!("Cannot uniquely assign a VM for new script {}",node.name))};
            scenario.realms[index].entry.push(node.id.clone());
        }
        Ok(scenario)
    }

    pub(crate) fn scenario(&self) -> Result<crate::Scenario, String> {
        self.validate()?;
        if let Some(blueprint)=&self.scenario{return self.imported_scenario(blueprint)}
        let nodes: HashMap<_, _> = self.nodes.iter().map(|node| (node.id.as_str(), node)).collect();
        let is_below = |node: &Node, root: &str| {
            let mut cursor = Some(node);
            while let Some(current) = cursor {
                if current.id == root { return true; }
                cursor = current.parent.as_deref().and_then(|id| nodes.get(id).copied());
            }
            false
        };
        let mut scenario_nodes = Vec::new();
        let mut run_nodes = Vec::<Node>::new();
        let mut clones = Vec::<Node>::new();
        for node in &self.nodes {
            if node.parent.is_none() { continue; } // Built-in services exist in every VM.
            let mut value = json!({"id":node.id,"parent":node.parent,"class":node.class,"name":node.name});
            if let Some(source) = &node.source { value["source"] = json!(source); }
            if !node.properties.is_empty() {value["properties"] = json!(node.properties);}
            if let Some(enabled) = node.enabled { value["properties"]["Disabled"] = json!(!enabled); }
            if node.id == "player1" { value["properties"] = json!({"UserId":1,"Character":{"tag":"instance","id":"character1"}}); }
            scenario_nodes.push(value);
            if runnable(&node.class) && !is_below(node, "starterPlayerScripts") && !is_below(node, "starterCharacterScripts") && !is_below(node, "StarterGui") {
                run_nodes.push(node.clone());
            }
        }
        // This is a headless character: no body rig or physics are simulated.
        scenario_nodes.extend([
            json!({"id":"character1","parent":"Workspace","class":"Model","name":"LocalPlayer","properties":{"PrimaryPart":{"tag":"instance","id":"characterRoot1"}}}),
            json!({"id":"characterRoot1","parent":"character1","class":"Part","name":"HumanoidRootPart"}),
            json!({"id":"characterHead1","parent":"character1","class":"Part","name":"Head"}),
            json!({"id":"characterHumanoid1","parent":"character1","class":"Humanoid","name":"Humanoid"}),
            json!({"id":"characterAnimator1","parent":"characterHumanoid1","class":"Animator","name":"Animator"}),
        ]);
        clones.push(Node { id:"character1".into(),parent:Some("Workspace".into()),class:"Model".into(),name:"LocalPlayer".into(),builtin:true,source:None,enabled:None,properties:BTreeMap::new(),metadata:None });
        // Clone starter containers into the player and character on Play.
        for (root, target) in [("starterPlayerScripts", "playerScripts1"), ("starterCharacterScripts", "character1"), ("StarterGui", "playerGui1")] {
            for node in self.nodes.iter().filter(|node| node.id != root && is_below(node, root)) {
                let mut copy = node.clone();
                copy.id = format!("play-{}", node.id);
                copy.parent = node.parent.as_ref().map(|parent| if parent == root { target.into() } else { format!("play-{parent}") });
                copy.builtin = false;
                let mut value = json!({"id":copy.id,"parent":copy.parent,"class":copy.class,"name":copy.name});
                if let Some(source) = &copy.source { value["source"] = json!(source); }
                if !copy.properties.is_empty() {value["properties"] = json!(copy.properties);}
                if let Some(enabled) = copy.enabled { value["properties"]["Disabled"] = json!(!enabled); }
                scenario_nodes.push(value);
                if runnable(&copy.class) { run_nodes.push(copy.clone()); }
                clones.push(copy);
            }
        }
        let run_map: HashMap<_, _> = self.nodes.iter().cloned().chain(clones)
            .map(|node| (node.id.clone(), node)).collect();
        let ancestor = |node: &Node, roots: &[&str]| {
            let mut cursor = Some(node);
            while let Some(current) = cursor {
                if roots.contains(&current.id.as_str()) { return true; }
                cursor = current.parent.as_deref().and_then(|id| run_map.get(id));
            }
            false
        };
        let mut server_entry = Vec::<String>::new();
        let mut client_entry = Vec::<String>::new();
        let mut actor_entries: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
        for node in &run_nodes {
            if node.enabled != Some(true) { continue; }
            let side = match node.class.as_str() {
                "Script" if ancestor(node, &["Workspace", "ServerScriptService"]) => "server",
                "LocalScript" if ancestor(node, &["ReplicatedFirst", "playerScripts1", "playerGui1", "character1"]) => "client",
                _ => continue,
            };
            let mut cursor = node.parent.as_deref().and_then(|id| run_map.get(id));
            let mut actor = None;
            while let Some(current) = cursor {
                if current.class == "Actor" { actor = Some(current.id.clone()); break; }
                cursor = current.parent.as_deref().and_then(|id| run_map.get(id));
            }
            if let Some(actor) = actor { actor_entries.entry((side.into(), actor)).or_default().push(node.id.clone()); }
            else if side == "server" { server_entry.push(node.id.clone()); }
            else { client_entry.push(node.id.clone()); }
        }
        if actor_entries.len() > 14 { return Err("A place can use at most 14 active Actor VMs".into()); }
        let mut realms = vec![json!({"id":"server","side":"server","entry":server_entry}),
            json!({"id":"client1","side":"client","clientId":"player1","executor":true,"executorProfile":self.profile,"entry":client_entry})];
        for ((side, actor), entry) in actor_entries {
            let mut realm = json!({"id":format!("actor-{side}-{actor}"),"side":side,"actorId":actor,"entry":entry});
            if side == "client" { realm["clientId"] = json!("player1"); }
            realms.push(realm);
        }
        serde_json::from_value(json!({"name":self.name,"engine":"native","duration":0.0,"nodes":scenario_nodes,"realms":realms}))
            .map_err(|e| format!("Invalid generated scenario: {e}"))
    }
}

#[cfg(all(test, emulator_local_tests))]
#[path = "../local_tests/place.rs"]
mod tests;
