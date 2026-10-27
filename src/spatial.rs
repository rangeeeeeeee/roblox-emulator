use crate::{instance::{self, Link}, types::{CFrame, QueryParams, Vector}};
use mlua::{Error, Lua, MultiValue, Result, UserData, UserDataFields, UserDataMethods, UserDataRegistry, Value, ffi};
use std::rc::Rc;

type V = [f64; 3];
fn add(a:V,b:V)->V{std::array::from_fn(|i|a[i]+b[i])}
fn sub(a:V,b:V)->V{std::array::from_fn(|i|a[i]-b[i])}
fn mul(a:V,s:f64)->V{a.map(|v|v*s)}
fn dot(a:V,b:V)->f64{a.into_iter().zip(b).map(|(a,b)|a*b).sum()}
fn cross(a:V,b:V)->V{[a[1]*b[2]-a[2]*b[1],a[2]*b[0]-a[0]*b[2],a[0]*b[1]-a[1]*b[0]]}
fn length(v:V)->f64{dot(v,v).sqrt()}
fn unit(v:V)->V{mul(v,1./length(v))}
fn vector_value(v:V)->Value{Value::Vector(mlua::Vector::new(v[0]as f32,v[1]as f32,v[2]as f32))}
fn cast(value:&Value,target:&str)->Error{
    let kind=match value{Value::Nil=>"nil".into(),Value::Table(_)=>"Array".into(),Value::Boolean(_)=>"bool".into(),Value::Integer(_)|Value::Number(_)=>"double".into(),Value::String(_)=>"string".into(),Value::UserData(v)=>v.type_name().map(|s|s.to_string_lossy()).unwrap_or_else(|_|"userdata".into()),Value::Vector(_)=>"Vector3".into(),_=>value.type_name().into()};
    Error::lua_string(format!("Unable to cast {kind} to {target}"))
}
pub(crate) fn vector(value:&Value)->Result<V>{
    if let Value::Vector(v)=value{return Ok([v.x()as f64,v.y()as f64,v.z()as f64])}
    if let Value::UserData(v)=value{if let Ok(v)=v.borrow::<crate::types::Vector3>(){return Ok(v.0.map(f64::from))}}
    Err(cast(value,"Vector3"))
}
pub(crate) fn identity()->CFrame{CFrame{p:Vector([0.;3]),r:[[1.,0.,0.],[0.,1.,0.],[0.,0.,1.]]}}
pub(crate) fn frame(value:&Value)->Result<CFrame>{if let Value::UserData(v)=value{if let Ok(v)=v.borrow::<CFrame>(){return Ok(*v)}}Err(cast(value,"CoordinateFrame"))}
pub(crate) fn part_frame(node:&Link)->Result<CFrame>{
    let node=node.borrow();if let Some(value)=node.properties.get("CFrame"){return frame(value)}
    let mut result=identity();if let Some(value)=node.properties.get("Position"){result.p=Vector(vector(value)?.map(|v|v as f32));}Ok(result)
}
pub(crate) fn part_size(node:&Link)->Result<V>{
    let node=node.borrow();if let Some(value)=node.properties.get("Size"){return vector(value)}
    Ok(if node.class=="Part"{[4.,1.2f32 as f64,2.]}else{[1.;3]})
}
fn enum_name(value:Option<&Value>,family:&str,default:&str)->Result<String>{
    let Some(value)=value else{return Ok(default.into())};
    let encoded=crate::types::encode_value(value)?.ok_or_else(||cast(value,family))?;
    if encoded["tag"]!="EnumItem"||encoded["enum"]!=family{return Err(cast(value,family))}
    Ok(encoded["name"].as_str().unwrap_or(default).into())
}
fn shape(node:&Link)->Result<String>{let node=node.borrow();if node.class!="Part"{return Ok(node.class.clone())}enum_name(node.properties.get("Shape"),"PartType","Block")}
pub(crate) fn extents(node:&Link)->Result<V>{
    // Studio's primitive collision extents have a 0.05-stud minimum even when
    // the editable Size property is smaller. Ball/Cylinder stay circular.
    let mut size=part_size(node)?.map(|v|v.max(0.05f32 as f64));
    match shape(node)?.as_str(){"Ball"=>size=[size.into_iter().fold(f64::INFINITY,f64::min);3],"Cylinder"=>{let diameter=size[1].min(size[2]);size[1]=diameter;size[2]=diameter;},_=>{}}
    Ok(size)
}
pub(crate) fn extents_value(node:&Link)->Result<Value>{Ok(vector_value(extents(node)?))}
pub(crate) fn position_value(node:&Link)->Result<Value>{Ok(vector_value(part_frame(node)?.p.0.map(f64::from)))}

#[derive(Clone,Copy)]
struct Box3 { center:V, axes:[V;3], half:V }
impl Box3{
    fn new(lua:&Lua,frame:CFrame,size:V)->Result<Self>{
        let axes:[V;3]=std::array::from_fn(|i|[frame.r[0][i]as f64,frame.r[1][i]as f64,frame.r[2][i]as f64]);
        if !frame.p.0.iter().all(|v|v.is_finite())||!size.iter().all(|v|v.is_finite())||axes.iter().any(|axis|!axis.iter().all(|v|v.is_finite())||(dot(*axis,*axis)-1.).abs()>1e-3)
            ||dot(axes[0],axes[1]).abs()>1e-3||dot(axes[0],axes[2]).abs()>1e-3||dot(axes[1],axes[2]).abs()>1e-3{
            return Err(crate::model::unsupported(lua,"WorldRoot spatial query with non-finite or non-orthonormal transform"))
        }
        Ok(Self{center:frame.p.0.map(f64::from),axes,half:size.map(|v|v*0.5)})
    }
    fn local(&self,v:V)->V{self.axes.map(|axis|dot(axis,v))}
    fn world(&self,v:V)->V{add(add(mul(self.axes[0],v[0]),mul(self.axes[1],v[1])),mul(self.axes[2],v[2]))}
    fn contains(&self,point:V)->bool{let p=self.local(sub(point,self.center));(0..3).all(|i|p[i].abs()<=self.half[i])}
}
fn boxes_overlap(a:Box3,b:Box3)->bool{
    let delta=sub(b.center,a.center);
    let separated=|axis:V|{let radius=|body:Box3|(0..3).map(|i|body.half[i]*dot(body.axes[i],axis).abs()).sum::<f64>();dot(delta,axis).abs()>radius(a)+radius(b)+1e-8};
    for axis in a.axes.into_iter().chain(b.axes){if separated(axis){return false}}
    for a in a.axes{for b in b.axes{let axis=cross(a,b);if dot(axis,axis)>1e-16&&separated(axis){return false}}}true
}
fn sphere_box(center:V,radius:f64,body:Box3)->bool{
    let point=body.local(sub(center,body.center));let distance=(0..3).map(|i|(point[i].abs()-body.half[i]).max(0.).powi(2)).sum::<f64>();distance<=radius*radius+1e-8
}
fn box_interval(origin:V,direction:V,half:V)->Option<(f64,V,f64)>{
    let mut near=f64::NEG_INFINITY;let mut far=f64::INFINITY;let mut normal=[0.;3];
    for i in 0..3{
        if direction[i].abs()<1e-15{if origin[i].abs()>half[i]{return None}continue}
        let(a,b)=((-half[i]-origin[i])/direction[i],(half[i]-origin[i])/direction[i]);let(lo,hi,sign)=if a<b{(a,b,-1.)}else{(b,a,1.)};
        if lo>near{near=lo;normal=[0.;3];normal[i]=sign;}far=far.min(hi);if near>far{return None}
    }Some((near,normal,far))
}
fn ray_shape(body:Box3,kind:&str,origin:V,direction:V,limit:f64)->Option<(f64,V)>{
    let o=body.local(sub(origin,body.center));let d=body.local(direction);
    let hit=match kind{
        "Block"=>{if body.contains(origin){return None}let(t,n,_)=box_interval(o,d,body.half)?;Some((t,n))},
        "Ball"=>{
            let radius=body.half[0];let c=dot(o,o)-radius*radius;if c<=0.{return None}
            let a=dot(d,d);let b=dot(o,d);let discriminant=b*b-a*c;if discriminant<0.{return None}
            let t=(-b-discriminant.sqrt())/a;Some((t,unit(add(o,mul(d,t)))))
        },
        "Cylinder"=>{
            let radius=body.half[1];let radial=o[1]*o[1]+o[2]*o[2];if o[0].abs()<=body.half[0]&&radial<=radius*radius{return None}
            let mut best:Option<(f64,V)>=None;let mut add_hit=|t:f64,n:V|{if t>0.&&t<=limit&&best.is_none_or(|(old,_)|t<old){best=Some((t,n));}};
            let a=d[1]*d[1]+d[2]*d[2];if a>1e-15{let b=o[1]*d[1]+o[2]*d[2];let disc=b*b-a*(radial-radius*radius);if disc>=0.{for t in[(-b-disc.sqrt())/a,(-b+disc.sqrt())/a]{let p=add(o,mul(d,t));if p[0].abs()<=body.half[0]+1e-8{add_hit(t,unit([0.,p[1],p[2]]));}}}}
            if d[0].abs()>1e-15{for sign in[-1.,1.]{let t=(sign*body.half[0]-o[0])/d[0];let p=add(o,mul(d,t));if p[1]*p[1]+p[2]*p[2]<=radius*radius+1e-8{add_hit(t,[sign,0.,0.]);}}}best
        },_=>None,
    }?;
    (hit.0>0.&&hit.0<=limit).then(||(hit.0,unit(body.world(hit.1))))
}
fn under(node:&Link,ancestor:&Link)->bool{let mut current=Some(node.clone());while let Some(node)=current{if Rc::ptr_eq(&node,ancestor){return true}current=node.borrow().parent.upgrade();}false}
fn active(root:&Link)->bool{
    let mut current=Some(root.clone());let mut viewport_or_workspace=false;let mut datamodel=false;
    while let Some(node)=current{let node=node.borrow();viewport_or_workspace|=matches!(node.class.as_str(),"Workspace"|"ViewportFrame");datamodel|=node.class=="DataModel";current=node.parent.upgrade();}
    viewport_or_workspace&&datamodel&&!root.borrow().destroyed
}
fn candidates(root:&Link)->Vec<Link>{
    let mut stack=root.borrow().children.clone();let mut result=Vec::new();while let Some(node)=stack.pop(){let n=node.borrow();if n.destroyed||instance::is_a(&n,"WorldRoot"){continue}
        // The offline Terrain singleton has no voxel data; voxel writes/imports
        // are unsupported. It is known empty, rather than approximated as a box.
        if n.class=="Terrain"{continue}if instance::is_a(&n,"BasePart"){result.push(node.clone())}stack.extend(n.children.iter().cloned());
    }result.sort_by(|a,b|a.borrow().id.cmp(&b.borrow().id));result
}
fn parameters<const O:bool>(value:Option<&Value>)->Result<QueryParams<O>>{
    match value{None|Some(Value::Nil)=>Ok(QueryParams{collision_group:"Default".into(),..QueryParams::default()}),Some(Value::UserData(value))=>value.borrow::<QueryParams<O>>().map(|v|v.clone()).map_err(|_|cast(&Value::UserData(value.clone()),if O{"OverlapParams"}else{"RaycastParams"})),Some(value)=>Err(cast(value,if O{"OverlapParams"}else{"RaycastParams"}))}
}
fn eligible<const O:bool>(node:&Link,params:&QueryParams<O>)->bool{
    let matches=|list:&[Link]|list.iter().any(|ancestor|under(node,ancestor));
    if params.filter_include!=matches(&params.filter){return false}
    if params.exclude.as_ref().is_some_and(|list|matches(list))||params.include.as_ref().is_some_and(|list|!matches(list)){return false}
    if params.brute_force{return true}
    let node=node.borrow();let flag=|key:&str|!matches!(node.properties.get(key),Some(Value::Boolean(false)));
    if params.respect_can_collide{flag("CanCollide")}else{flag("CanCollide")||flag("CanQuery")}
}
fn collision_groups<const O:bool>(lua:&Lua,node:&Link,params:&QueryParams<O>)->Result<()>{
    let node=node.borrow();let default=match node.properties.get("CollisionGroup"){None=>true,Some(Value::String(value))=>value.as_bytes().as_ref()==b"Default",_=>false};
    if params.collision_group!="Default"||!default{return Err(crate::model::unsupported(lua,"WorldRoot spatial collision-group masks"))}Ok(())
}
fn supported(lua:&Lua,kind:&str)->Result<()>{if matches!(kind,"Block"|"Ball"|"Cylinder"){Ok(())}else{Err(crate::model::unsupported(lua,&format!("WorldRoot spatial geometry {kind}")))}}

#[derive(Clone)]
struct RaycastResult { node:Link, distance:f64, position:V, normal:V, material:String }
impl UserData for RaycastResult{
    fn register(registry:&mut UserDataRegistry<Self>){registry.add_meta_field("__type","RaycastResult");Self::add_methods(registry);}
    fn add_methods<M:UserDataMethods<Self>>(methods:&mut M){
        methods.add_meta_method(mlua::MetaMethod::Index,|lua,this,key:String|match key.as_str(){
            "Instance"=>Ok(Value::UserData(instance::wrapper(lua,this.node.clone())?)),"Distance"=>Ok(Value::Number(this.distance)),"Position"=>Ok(vector_value(this.position)),"Normal"=>Ok(vector_value(this.normal)),
            "Material"=>crate::types::decode_value(lua,&serde_json::json!({"tag":"EnumItem","enum":"Material","name":this.material}))?.ok_or_else(||Error::lua_string("Unknown raycast material")),_=>Err(Error::lua_string(format!("{key} is not a valid member of RaycastResult"))),
        });
        methods.add_meta_method(mlua::MetaMethod::NewIndex,|_,_,(key,_):(String,Value)|Err::<(),_>(Error::lua_string(if matches!(key.as_str(),"Instance"|"Distance"|"Position"|"Normal"|"Material"){format!("{key} member of RaycastResult is read-only and cannot be assigned to")}else{format!("{key} is not a valid member of RaycastResult")})));
        methods.add_meta_method(mlua::MetaMethod::ToString,|_,this,()|{let text=|v:V|v.map(|v|(v as f32).to_string()).join(", ");Ok(format!("RaycastResult{{{} @ {}; normal = {}; material = {}}}",this.node.borrow().name,text(this.position),text(this.normal),this.material))});
    }
}
pub(crate) fn raycast(lua:&Lua,root:&Link,args:MultiValue)->Result<Value>{
    let origin=vector(args.front().unwrap_or(&Value::Nil))?;let delta=vector(args.get(1).unwrap_or(&Value::Nil))?;let params=parameters::<false>(args.get(2))?;
    let distance=length(delta);if !distance.is_finite()||distance==0.||!origin.iter().all(|v|v.is_finite())||!active(root){return Ok(Value::Nil)}
    let direction=mul(delta,1./distance);let limit=distance.min(15000.);let mut best:Option<RaycastResult>=None;
    for node in candidates(root){
        if !eligible(&node,&params){continue}let body=Box3::new(lua,part_frame(&node)?,extents(&node)?)?;
        let Some((near,_,far))=box_interval(body.local(sub(origin,body.center)),body.local(direction),body.half)else{continue};
        if far<=0.||near>best.as_ref().map_or(limit,|hit|hit.distance){continue}
        collision_groups(lua,&node,&params)?;let kind=shape(&node)?;supported(lua,&kind)?;
        if let Some((distance,normal))=ray_shape(body,&kind,origin,direction,best.as_ref().map_or(limit,|hit|hit.distance)){
            let material=enum_name(node.borrow().properties.get("Material"),"Material","Plastic")?;
            best=Some(RaycastResult{node,distance,position:add(origin,mul(direction,distance)),normal,material});
        }
    }
    let Some(result)=best else{return Ok(Value::Nil)};let result=lua.create_userdata(result)?;
    unsafe{lua.exec_raw::<()>(result.clone(),|state|{ffi::lua_getmetatable(state,1);ffi::lua_pushstring(state,c"The metatable is locked".as_ptr());ffi::lua_rawsetfield(state,-2,c"__metatable".as_ptr());ffi::lua_settop(state,0);})?;}
    Ok(Value::UserData(result))
}
pub(crate) fn bounds(lua:&Lua,root:&Link,args:MultiValue,radius_query:bool)->Result<mlua::Table>{
    let params=parameters::<true>(args.get(2))?;let mut point=[0.;3];let mut radius=0.;let mut query=None;
    if radius_query{point=vector(args.front().unwrap_or(&Value::Nil))?;radius=lua.coerce_number(args.get(1).cloned().unwrap_or(Value::Nil))?.ok_or_else(||cast(args.get(1).unwrap_or(&Value::Nil),"float"))?;
        if !radius.is_finite()||radius<=0.||!point.iter().all(|v|v.is_finite()){return lua.create_table()}radius+=params.tolerance as f64;
    }else{let frame=frame(args.front().unwrap_or(&Value::Nil))?;let size=vector(args.get(1).unwrap_or(&Value::Nil))?;if size.iter().any(|v|!v.is_finite()||*v<=0.){return lua.create_table()}query=Some(Box3::new(lua,frame,size.map(|v|v+2.*params.tolerance as f64))?);}
    let mut result=Vec::new();if active(root){for node in candidates(root){if !eligible(&node,&params){continue}let body=Box3::new(lua,part_frame(&node)?,extents(&node)?)?;
        if if let Some(query)=query{!boxes_overlap(query,body)}else{!sphere_box(point,radius,body)}{continue}
        collision_groups(lua,&node,&params)?;supported(lua,&shape(&node)?)?;result.push(node);if params.max_parts>0&&result.len()>=params.max_parts as usize{break}
    }}instance::instance_list(lua,result)
}
