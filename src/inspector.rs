use mlua::{AnyUserData, Error, Lua, Result, Table, UserData, UserDataFields, UserDataMethods, Value};
use serde_json::{Value as Json, json};
use std::{cell::{Cell, RefCell}, collections::{BTreeMap, HashMap}, rc::Rc, time::{Duration, Instant}};

const PAGE: usize = 100;
const VALUES: &str = "emulator.inspector.values";
const SNAPSHOT_LIFETIME: Duration = Duration::from_secs(600);
const SNAPSHOT_STRING_BYTES: usize = 16 * 1024 * 1024;

// Weak Lua references: browsing never roots the inspected graph. No snapshot
// table, pagination state or UI metadata is exposed to scripts through getgc.
struct Inspector {
    values: Table,
    prefix: String,
    next: Cell<u64>,
    pointers: RefCell<HashMap<usize, u64>>,
    viewers: RefCell<BTreeMap<String, Rc<RefCell<Viewer>>>>,
}

#[derive(Default)]
struct Viewer {
    id: String,
    name: String,
    enabled: bool,
    destroyed: bool,
    revision: u64,
    captured: Option<u64>,
    items: Vec<Json>,
    snapshot: u64,
    created: Option<Instant>,
    values: Vec<GcValue>,
}

enum GcValue { Weak(String), Bytes(Vec<u8>), Unavailable }

impl Viewer {
    fn clear(&mut self) { self.items.clear(); self.values.clear(); self.captured=None; self.created=None; }
    fn expire(&mut self) { if self.created.is_some_and(|at|at.elapsed()>=SNAPSHOT_LIFETIME) { self.clear(); } }
}

fn state(lua: &Lua) -> Result<Rc<Inspector>> {
    lua.app_data_ref::<Rc<Inspector>>().map(|s| s.clone()).ok_or_else(|| Error::runtime("Inspector requires an interactive session"))
}

pub(crate) fn identity(lua: &Lua) -> Result<String> { Ok(state(lua)?.prefix.clone()) }

pub(crate) fn install(lua: &Lua) -> Result<()> {
    let values = lua.create_table()?;
    let mt = lua.create_table()?;
    mt.raw_set("__mode", "v")?;
    values.set_metatable(Some(mt))?;
    lua.set_named_registry_value(VALUES, values.clone())?;
    lua.set_named_registry_value("emulator.inspector.scan", lua.create_table()?)?;
    let mut random = [0; 8];
    crate::native::random_bytes(&mut random).map_err(Error::external)?;
    lua.set_app_data(Rc::new(Inspector { values, prefix: format!("{:016x}", u64::from_le_bytes(random)), next: Cell::new(0), pointers: RefCell::new(HashMap::new()), viewers: RefCell::new(BTreeMap::new()) }));
    Ok(())
}

impl Inspector {
    fn next(&self) -> u64 { let id = self.next.get() + 1; self.next.set(id); id }
    fn key(&self, handle: &str) -> Result<u64> {
        let (prefix, id) = handle.split_once(':').ok_or_else(|| Error::runtime("Invalid inspection handle"))?;
        if prefix != self.prefix { return Err(Error::runtime("Inspection handle belongs to another VM or an expired session")); }
        id.parse().map_err(|_| Error::runtime("Invalid inspection handle"))
    }
    fn value_handle(&self, value: &Value) -> Result<String> {
        let pointer = value.to_pointer() as usize;
        let existing = self.pointers.borrow().get(&pointer).copied();
        if let Some(id) = existing {
            if self.values.raw_get::<Option<Value>>(id)?.is_some_and(|old| old.to_pointer() == value.to_pointer() && old.type_name()==value.type_name()) {
                return Ok(format!("{}:{id}", self.prefix));
            }
        }
        // Reclaim stale pointer metadata periodically; IDs are never reused.
        if self.next.get() % 1024 == 0 {
            self.pointers.borrow_mut().retain(|_, id| self.values.raw_get::<Option<Value>>(*id).ok().flatten().is_some());
        }
        let id = self.next();
        self.values.raw_set(id, value.clone())?;
        self.pointers.borrow_mut().insert(pointer, id);
        Ok(format!("{}:{id}", self.prefix))
    }
}

pub(crate) fn handle(lua: &Lua, table: &Table) -> Option<String> {
    value_handle(lua,&Value::Table(table.clone()))
}

pub(crate) fn value_handle(lua: &Lua, value: &Value) -> Option<String> {
    state(lua).ok()?.value_handle(value).ok()
}

pub(crate) fn table_page(lua: &Lua, handle: &str, offset: usize) -> Result<Json> {
    let inspector = state(lua)?;
    let key = inspector.key(handle)?;
    let Some(table) = inspector.values.raw_get::<Option<Table>>(key)? else {
        return Ok(json!({"entries":[],"nextOffset":null,"unavailable":true}));
    };
    // A raw iterator does not invoke __iter, __index or __tostring. Paging has no
    // depth/entry ceiling; mutable tables are explicitly shown as live values.
    let mut entries = Vec::new();
    let mut pairs = table.pairs::<Value, Value>().skip(offset);
    for pair in pairs.by_ref().take(PAGE) {
        let (key, value) = pair?;
        entries.push(json!({"key":crate::console::describe(lua,&key),"value":crate::console::describe(lua,&value)}));
    }
    let more = pairs.next().transpose()?.is_some();
    Ok(json!({"entries":entries,"nextOffset":if more{Some(offset+PAGE)}else{None},"live":true}))
}

pub(crate) fn function_page(lua: &Lua, handle: &str, section: &str, offset: usize) -> Result<Json> {
    if !matches!(section,"constants"|"upvalues") {return Err(Error::runtime("Unknown function inspection section"));}
    let inspector=state(lua)?;
    let Some(function)=inspector.values.raw_get::<Option<mlua::Function>>(inspector.key(handle)?)? else {
        return Ok(json!({"entries":[],"nextOffset":null,"unavailable":true}));
    };
    if function.info().what=="C" {return Ok(json!({"entries":[],"nextOffset":null,"native":true}));}
    let mut entries=Vec::new();
    let more;
    if section=="upvalues" {
        let count=function.info().num_upvalues as usize;
        for index in offset..count.min(offset.saturating_add(PAGE)) {
            let value:Value=unsafe {lua.exec_raw(function.clone(),|state| {
                if mlua::ffi::lua_getupvalue(state,1,index as i32+1).is_null() {mlua::ffi::lua_pushnil(state);}
                mlua::ffi::lua_remove(state,1);
            })?};
            entries.push(json!({"key":{"type":"number","value":index+1},"value":crate::console::describe(lua,&value)}));
        }
        more=offset.saturating_add(entries.len())<count;
    } else {
        let constants=crate::native::inspect_constants(lua,&function)?;
        struct ClearScan(Table);
        impl Drop for ClearScan { fn drop(&mut self) { let _=self.0.clear(); } }
        let _clear=ClearScan(constants.clone());
        let mut values=constants.pairs::<i64,Value>().collect::<Result<Vec<_>>>()?;
        values.sort_by_key(|(index,_)|*index);
        for (index,value) in values.iter().skip(offset).take(PAGE) {
            entries.push(json!({"key":{"type":"number","value":index},"value":crate::console::describe(lua,value)}));
        }
        more=offset.saturating_add(entries.len())<values.len();
    }
    Ok(json!({"entries":entries,"nextOffset":if more{Some(offset+PAGE)}else{None},"live":true}))
}

#[derive(Clone)]
struct GcViewer(Rc<RefCell<Viewer>>);

impl UserData for GcViewer {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_meta_field("__type", "GCViewer");
        fields.add_field_method_get("ClassName", |_,_|Ok("GCViewer"));
        fields.add_field_method_get("Name", |_,this|Ok(this.0.borrow().name.clone()));
        fields.add_field_method_set("Name", |_,this,name:String| {
            if name.len()>256 { return Err(Error::runtime("GCViewer name exceeds 256 bytes")); }
            let mut viewer=this.0.borrow_mut();
            if viewer.destroyed { return Err(Error::runtime("GCViewer is destroyed")); }
            viewer.name=name; Ok(())
        });
        fields.add_field_method_get("Enabled", |_,this|Ok(this.0.borrow().enabled));
        fields.add_field_method_set("Enabled", |_,this,enabled:bool| {
            let mut viewer=this.0.borrow_mut();
            if viewer.destroyed { return Err(Error::runtime("GCViewer is destroyed")); }
            if viewer.enabled!=enabled { viewer.revision+=1; viewer.clear(); }
            viewer.enabled=enabled; Ok(())
        });
    }
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("Refresh", |_,this,()| {
            let mut viewer=this.0.borrow_mut();
            if viewer.destroyed { return Err(Error::runtime("GCViewer is destroyed")); }
            viewer.revision+=1; viewer.clear(); Ok(())
        });
        methods.add_method("Destroy", |_,this,()| {
            let mut viewer=this.0.borrow_mut();viewer.destroyed=true;viewer.enabled=false;viewer.clear();Ok(())
        });
    }
}

pub(crate) fn create_viewer(lua: &Lua) -> Result<AnyUserData> {
    let inspector=state(lua)?;
    inspector.viewers.borrow_mut().retain(|_,v|!v.borrow().destroyed&&(v.borrow().enabled||Rc::strong_count(v)>1));
    if inspector.viewers.borrow().values().filter(|v|v.borrow().enabled).count()>=16 { return Err(Error::runtime("Close an existing GCViewer before creating another")); }
    let id=format!("{}:{}",inspector.prefix,inspector.next());
    let viewer=Rc::new(RefCell::new(Viewer{id:id.clone(),name:"GC Viewer".into(),enabled:true,revision:1,..Viewer::default()}));
    let object=lua.create_userdata(GcViewer(viewer.clone()))?;
    inspector.viewers.borrow_mut().insert(id,viewer);
    Ok(object)
}

pub(crate) fn viewers(lua: &Lua, realm: &str) -> Vec<Json> {
    let Ok(inspector)=state(lua) else{return Vec::new()};
    inspector.viewers.borrow().values().map(|value| {
        let mut viewer=value.borrow_mut();viewer.expire();
        json!({"realm":realm,"id":viewer.id,"name":viewer.name,"enabled":viewer.enabled&&!viewer.destroyed,"revision":viewer.revision})
    }).collect()
}

fn viewer(lua:&Lua,id:&str)->Result<Rc<RefCell<Viewer>>> {
    state(lua)?.viewers.borrow().get(id).cloned().ok_or_else(||Error::runtime("GCViewer is unavailable in the selected VM"))
}

pub(crate) fn close_viewer(lua:&Lua,id:&str)->Result<Json> {
    let viewer=viewer(lua,id)?;let mut viewer=viewer.borrow_mut();
    viewer.enabled=false;viewer.clear();
    Ok(json!({"closed":true}))
}

pub(crate) fn gc_page(lua:&Lua,id:&str,refresh:bool,offset:usize,query:&str,value_type:&str)->Result<Json> {
    let viewer=viewer(lua,id)?;
    viewer.borrow_mut().expire();
    let revision={let viewer=viewer.borrow();if viewer.destroyed||!viewer.enabled{return Err(Error::runtime("GCViewer is closed"));}viewer.revision};
    if refresh || viewer.borrow().captured!=Some(revision) {
        // Scan through the unhooked native helper. Clear its reusable scratch
        // before returning; only Rust descriptors and weak handles survive.
        viewer.borrow_mut().clear();
        let inspector=state(lua)?;
        let snapshot=inspector.next();
        let objects=crate::native::gc_snapshot(lua)?;
        struct ClearScan(Table);
        impl Drop for ClearScan { fn drop(&mut self) { let _=self.0.clear(); } }
        let _clear=ClearScan(objects.clone());
        let mut items=Vec::with_capacity(objects.raw_len());
        let mut values=Vec::with_capacity(objects.raw_len());
        let mut string_bytes=0;
        for object in objects.sequence_values::<Value>() {
            let object=object?;
            let mut item=crate::console::describe(lua,&object);
            let reference=if let Value::String(text)=&object {
                // Luau strings are never weak. Copy immutable bytes outside the VM
                // with a fixed budget instead of adding a permanent Lua GC root.
                let bytes=text.as_bytes();
                if bytes.len()<=SNAPSHOT_STRING_BYTES-string_bytes {
                    string_bytes+=bytes.len();GcValue::Bytes(bytes.to_vec())
                } else {item["insertError"]=json!("String exceeds the 16 MiB snapshot string budget");GcValue::Unavailable}
            } else {GcValue::Weak(inspector.value_handle(&object)?)};
            item["snapshotIndex"]=json!(values.len()+1);
            items.push(item);values.push(reference);
        }
        let mut viewer=viewer.borrow_mut();viewer.items=items;viewer.values=values;viewer.captured=Some(revision);viewer.snapshot=snapshot;viewer.created=Some(Instant::now());
    }
    let query=query.to_lowercase();
    let viewer=viewer.borrow();
    let matches=|value:&&Json| (value_type.is_empty() || value["type"].as_str()==Some(value_type))
        && (query.is_empty() || value.to_string().to_lowercase().contains(&query));
    let total=viewer.items.iter().filter(matches).count();
    let items:Vec<_>=viewer.items.iter().filter(matches).skip(offset).take(PAGE).cloned().collect();
    let next=offset.saturating_add(items.len());
    Ok(json!({"items":items,"total":total,"nextOffset":if next<total{Some(next)}else{None},"revision":revision,"snapshot":viewer.snapshot}))
}

pub(crate) fn gc_value(lua:&Lua,id:&str,snapshot:u64,index:usize)->Result<Value> {
    if crate::scheduler::current_origin(lua)!="executor" {return Err(Error::runtime("getgcvalue requires Executor context"));}
    let inspector=state(lua)?;
    inspector.key(id)?;
    let viewer=viewer(lua,id)?;
    let mut viewer=viewer.borrow_mut();viewer.expire();
    if !viewer.enabled||viewer.destroyed||viewer.captured!=Some(viewer.revision)||viewer.snapshot!=snapshot {
        return Err(Error::runtime("GC snapshot expired or changed; refresh the viewer and insert the value again"));
    }
    match index.checked_sub(1).and_then(|index|viewer.values.get(index)) {
        Some(GcValue::Weak(handle))=>inspector.values.raw_get::<Option<Value>>(inspector.key(handle)?)?.ok_or_else(||Error::runtime("GC value was collected; refresh the viewer and insert the value again")),
        Some(GcValue::Bytes(bytes))=>Ok(Value::String(lua.create_string(bytes)?)),
        Some(GcValue::Unavailable)=>Err(Error::runtime("String exceeds the 16 MiB snapshot string budget")),
        None=>Err(Error::runtime("Invalid GC snapshot value index")),
    }
}
