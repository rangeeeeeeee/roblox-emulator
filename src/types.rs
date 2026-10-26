use mlua::{AnyUserData, Error, Function, Lua, LuaSerdeExt, MetaMethod, MultiValue, ObjectLike, Result, Table, UserData, UserDataFields, UserDataMethods, UserDataRegistry, Value, ffi};
use serde_json::{Value as Json, json};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct UDim { pub scale:f32, pub offset:i32 }
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct UDim2 { pub x:UDim, pub y:UDim }
impl UserData for UDim { fn register(r:&mut UserDataRegistry<Self>){r.add_meta_field("__type","UDim");} }
impl UserData for UDim2 { fn register(r:&mut UserDataRegistry<Self>){r.add_meta_field("__type","UDim2");} }
fn dimension<const TWO:bool>(value:&Value)->Option<UDim2>{
    let Value::UserData(value)=value else{return None};
    if TWO {value.borrow::<UDim2>().ok().map(|v|*v)}else{value.borrow::<UDim>().ok().map(|v|UDim2{x:*v,y:UDim::default()})}
}
fn dimension_value<const TWO:bool>(lua:&Lua,value:UDim2)->Result<Value>{if TWO {value.into_lua(lua)}else{value.x.into_lua(lua)}}
fn dimension_name<const TWO:bool>()->&'static str{if TWO {"UDim2"}else{"UDim"}}
fn dimension_int(value:f64)->i32 {let value=value.trunc();if !value.is_finite()||value<i32::MIN as f64||value>i32::MAX as f64{i32::MIN}else{value as i32}}
fn dimension_number(lua:&Lua,args:&MultiValue,index:usize)->Result<f64>{Ok(lua.coerce_number(args.get(index).cloned().unwrap_or(Value::Nil))?.unwrap_or(0.))}
fn float9(value:f32)->String {
    if value.is_nan(){return "NAN".into()}if value.is_infinite(){return if value.is_sign_negative(){"-INF"}else{"INF"}.into()}
    let scientific=format!("{value:.8e}");let (digits,exponent)=scientific.split_once('e').unwrap();let exponent:i32=exponent.parse().unwrap();
    let trim=|text:String|if text.contains('.') {text.trim_end_matches('0').trim_end_matches('.').to_owned()}else{text};
    if !(-4..9).contains(&exponent){format!("{}e{exponent:+03}",trim(digits.into()))}else{trim(format!("{value:.precision$}",precision=(8-exponent).max(0)as usize))}
}
fn dimension_text(v:UDim)->String{format!("{}, {}",float9(v.scale),v.offset)}
unsafe extern "C-unwind" fn dimension_error<const TWO:bool,const ARG:i32>(state:*mut ffi::lua_State)->std::ffi::c_int {unsafe{ffi::luaL_typeerror(state,ARG,if TWO {c"UDim2"}else{c"UDim"}.as_ptr())}}
// All metatable dispatch happens in the original native frame, including bad self/key arguments.
unsafe extern "C-unwind" fn dimension_meta<const TWO:bool,const OP:u8>(state:*mut ffi::lua_State)->std::ffi::c_int {unsafe{
    Lua::invoke_native_callback(state,|lua,args|{
        use mlua::state::NativeCall::{Dispatch,Return};
        let Some(a)=args.front().and_then(dimension::<TWO>) else{return Ok(Dispatch(dimension_error::<TWO,1>))};
        let name=dimension_name::<TWO>();
        let value=match OP {
            0|1=>{
                let Some(key)=lua.coerce_string(args.get(1).cloned().unwrap_or(Value::Nil))? else{return Ok(Dispatch(enum_key_error))};let key=key.to_string_lossy();
                let valid=if TWO {matches!(key.as_str(),"X"|"Y"|"Width"|"Height")}else{matches!(key.as_str(),"Scale"|"Offset")};
                if OP==1 {return Err(Error::lua_string(if valid {format!("{key} member of {name} is read-only and cannot be assigned to")}else{format!("{key} is not a valid member of {name}")}))}
                match key.as_str(){
                    "X"|"Width" if TWO=>a.x.into_lua(lua)?,"Y"|"Height" if TWO=>a.y.into_lua(lua)?,
                    "Scale" if !TWO=>Value::Number(a.x.scale as f64),"Offset" if !TWO=>Value::Integer(a.x.offset as i64),
                    "Lerp"|"lerp" if TWO=>lookup(lua,"UDim2",&key)?,
                    _=>return Err(Error::lua_string(if TWO {format!("{key} is not a valid member of UDim2")}else{format!("'{key}' is not a valid member of UDim")}))
                }
            },
            2|3=>{let Some(b)=args.get(1).and_then(dimension::<TWO>)else{return Ok(Dispatch(dimension_error::<TWO,2>))};let op=|a:UDim,b:UDim|if OP==2{UDim{scale:a.scale+b.scale,offset:a.offset.wrapping_add(b.offset)}}else{UDim{scale:a.scale-b.scale,offset:a.offset.wrapping_sub(b.offset)}};dimension_value::<TWO>(lua,UDim2{x:op(a.x,b.x),y:op(a.y,b.y)})?},
            4=>{let neg=|a:UDim|UDim{scale:-a.scale,offset:a.offset.wrapping_neg()};dimension_value::<TWO>(lua,UDim2{x:neg(a.x),y:neg(a.y)})?},
            5=>Value::Boolean(args.get(1).and_then(dimension::<TWO>).is_some_and(|b|a==b)),
            _=>if TWO {format!("{{{}}}, {{{}}}",dimension_text(a.x),dimension_text(a.y))}else{dimension_text(a.x)}.into_lua(lua)?
        };Ok(Return(MultiValue::from_vec(vec![value])))
    })
}}
fn variant_name(value:&Value)->String{match value {Value::Table(_)=>"Array".into(),Value::Boolean(_)=>"bool".into(),Value::Integer(_)|Value::Number(_)=>"double".into(),Value::String(_)=>"string".into(),Value::UserData(v)=>v.metatable().ok().and_then(|mt|mt.get::<String>("__type").ok()).unwrap_or_else(||"null".into()),Value::Vector(_)=>"Vector3".into(),_=>"null".into()}}
fn dimension_lerp(lua:&Lua,args:MultiValue)->Result<UDim2>{
    let required=|index:usize|args.get(index).filter(|v|!v.is_nil()).ok_or_else(||Error::lua_string(format!("Argument {} missing or nil",index+1)));
    let cast=|v:&Value|dimension::<true>(v).ok_or_else(||Error::lua_string(format!("Unable to cast {} to UDim2",variant_name(v))));
    let a=cast(required(0)?)?;let alpha=required(2)?;let alpha=lua.coerce_number(alpha.clone())?.ok_or_else(||Error::lua_string(format!("Unable to cast {} to float",variant_name(alpha))))? as f32;let b=cast(required(1)?)?;
    let lerp=|a:UDim,b:UDim|UDim{scale:a.scale+(b.scale-a.scale)*alpha,offset:dimension_int((a.offset as f32+(b.offset as f32-a.offset as f32)*alpha).round()as f64)};
    Ok(UDim2{x:lerp(a.x,b.x),y:lerp(a.y,b.y)})
}
fn dimension_metatable<const TWO:bool>(lua:&Lua,sample:Value)->Result<()>{unsafe{lua.exec_raw::<()>(sample,|state|{
    ffi::lua_getmetatable(state,1);
    for (key,f) in [(c"__index",dimension_meta::<TWO,0> as ffi::lua_CFunction),(c"__newindex",dimension_meta::<TWO,1>),(c"__add",dimension_meta::<TWO,2>),(c"__sub",dimension_meta::<TWO,3>),(c"__unm",dimension_meta::<TWO,4>),(c"__eq",dimension_meta::<TWO,5>),(c"__tostring",dimension_meta::<TWO,6>)]{ffi::lua_pushcfunction(state,f);ffi::lua_rawsetfield(state,-2,key.as_ptr());}
    ffi::lua_pushstring(state,c"The metatable is locked".as_ptr());ffi::lua_rawsetfield(state,-2,c"__metatable".as_ptr());ffi::lua_settop(state,0);
})}}
fn install_dimensions(lua:&Lua)->Result<()>{
    dimension_metatable::<false>(lua,UDim::default().into_lua(lua)?)?;dimension_metatable::<true>(lua,UDim2::default().into_lua(lua)?)?;
    let api=lua.create_table()?;api.set("new",named(lua,c"new",|lua,args:MultiValue|Ok(UDim{scale:dimension_number(lua,&args,0)? as f32,offset:dimension_int(dimension_number(lua,&args,1)?)}))?)?;api.set_readonly(true);lua.globals().set("UDim",api)?;
    let methods=lua.create_table()?;methods.set("Lerp",named(lua,c"Lerp",dimension_lerp)?)?;methods.set("lerp",named(lua,c"lerp",dimension_lerp)?)?;lua.set_named_registry_value("types.methods.UDim2",methods)?;
    let api=lua.create_table()?;api.set("new",named(lua,c"new",|lua,args:MultiValue|{
        if let Some(x)=args.front().and_then(dimension::<false>){return Ok(UDim2{x:x.x,y:args.get(1).and_then(dimension::<false>).map(|v|v.x).unwrap_or_default()})}
        Ok(UDim2{x:UDim{scale:dimension_number(lua,&args,0)? as f32,offset:dimension_int(dimension_number(lua,&args,1)?)},y:UDim{scale:dimension_number(lua,&args,2)? as f32,offset:dimension_int(dimension_number(lua,&args,3)?)}})
    })?)?;
    api.set("fromScale",named(lua,c"fromScale",|lua,args:MultiValue|Ok(UDim2{x:UDim{scale:dimension_number(lua,&args,0)? as f32,offset:0},y:UDim{scale:dimension_number(lua,&args,1)? as f32,offset:0}}))?)?;
    api.set("fromOffset",named(lua,c"fromOffset",|lua,args:MultiValue|Ok(UDim2{x:UDim{scale:0.,offset:dimension_int(dimension_number(lua,&args,0)?)},y:UDim{scale:0.,offset:dimension_int(dimension_number(lua,&args,1)?)}}))?)?;
    api.set_readonly(true);lua.globals().set("UDim2",api)
}

#[derive(Clone, Debug)]
struct Random { state:std::cell::Cell<u64> }
impl UserData for Random {fn register(r:&mut UserDataRegistry<Self>){r.add_meta_field("__type","Random");}}
impl Random {
    // PCG-XSH-RR and seeding from the pinned Luau 740 VM/src/lmathlib.cpp (MIT).
    fn next(&self)->u32{let old=self.state.get();self.state.set(old.wrapping_mul(6364136223846793005).wrapping_add(105));((((old>>18)^old)>>27)as u32).rotate_right((old>>59)as u32)}
    fn seeded(seed:u64)->Self{let r=Self{state:std::cell::Cell::new(0)};r.next();r.state.set(r.state.get().wrapping_add(seed));r.next();r}
    fn next64(&self)->u64{let low=self.next()as u64;low|((self.next()as u64)<<32)}
}
fn random_int(value:f64)->i64{if !value.is_finite()||value< -9223372036854775808.||value>=9223372036854775808.{i64::MIN}else{value as i64}}
unsafe extern "C-unwind" fn random_self_error(state:*mut ffi::lua_State)->std::ffi::c_int{unsafe{ffi::luaL_typeerror(state,1,c"Random".as_ptr())}}
unsafe extern "C-unwind" fn random_number_error<const ARG:i32>(state:*mut ffi::lua_State)->std::ffi::c_int{unsafe{ffi::luaL_typeerror(state,ARG,c"number".as_ptr())}}
unsafe extern "C-unwind" fn random_count_error(state:*mut ffi::lua_State)->std::ffi::c_int{unsafe{ffi::luaL_error(state,c"Random:NextNumber requires 0 or 2 arguments".as_ptr())}}
unsafe extern "C-unwind" fn random_call<const OP:u8>(state:*mut ffi::lua_State)->std::ffi::c_int{unsafe{
    Lua::invoke_native_callback(state,|lua,args|{
        use mlua::state::NativeCall::{Dispatch,Return};
        if OP==0 {
            let seed=if args.front().is_none_or(Value::is_nil){let mut seed=[0u8;8];crate::native::random_bytes(&mut seed).map_err(Error::external)?;u64::from_ne_bytes(seed)}else{
                let Some(seed)=lua.coerce_number(args[0].clone())? else{return Ok(Dispatch(random_number_error::<1>))};
                // Studio accepts exact integer seeds in the double precision range; out-of-range seeds reset to zero.
                if seed.abs()>9007199254740991. {0}else{random_int(seed)as u64}
            };return Ok(Return(MultiValue::from_vec(vec![Random::seeded(seed).into_lua(lua)?])))
        }
        let Some(Value::UserData(userdata))=args.front()else{return Ok(Dispatch(random_self_error))};
        let Ok(random)=userdata.borrow::<Random>()else{return Ok(Dispatch(random_self_error))};
        let result=if OP==3 {(*random).clone().into_lua(lua)?}else{
            if OP==2&&args.len()==2{return Ok(Dispatch(random_count_error))}
            let (a,b)=if OP==2&&args.len()==1{(0.,1.)}else{
                let Some(a)=lua.coerce_number(args.get(1).cloned().unwrap_or(Value::Nil))?else{return Ok(Dispatch(random_number_error::<2>))};
                let Some(b)=lua.coerce_number(args.get(2).cloned().unwrap_or(Value::Nil))?else{return Ok(Dispatch(random_number_error::<3>))};(a,b)
            };
            if OP==1 {
                let (a,b)=(random_int(a),random_int(b));let (low,high)=if a>b{(b,a)}else{(a,b)};let count=(high as i128-low as i128+1)as u128;
                let offset=if count<=u32::MAX as u128{(count*random.next()as u128)>>32}else{(count*random.next64()as u128)>>64};
                Value::Number((low as i128+offset as i128)as f64)
            }else{let (low,high)=if a>b{(b,a)}else{(a,b)};let unit=(random.next64()as f64)*(1./18446744073709551616.);Value::Number(low+(high-low)*unit)}
        };Ok(Return(MultiValue::from_vec(vec![result])))
    })
}}
unsafe extern "C-unwind" fn random_meta<const OP:u8>(state:*mut ffi::lua_State)->std::ffi::c_int{unsafe{
    Lua::invoke_native_callback(state,|lua,args|{
        use mlua::state::NativeCall::{Dispatch,Return};
        let Some(Value::UserData(userdata))=args.front()else{return Ok(Dispatch(random_self_error))};
        let Ok(random)=userdata.borrow::<Random>()else{return Ok(Dispatch(random_self_error))};
        let value=match OP {
            0|1=>{let Some(key)=lua.coerce_string(args.get(1).cloned().unwrap_or(Value::Nil))?else{return Ok(Dispatch(enum_key_error))};let key=key.to_string_lossy();let method=if OP==0{lookup(lua,"Random",&key)?}else{Value::Nil};if method.is_nil(){return Err(Error::lua_string(format!("{key} is not a valid member of Random")))}method},
            2=>Value::Boolean(matches!(args.get(1),Some(Value::UserData(other))if other.borrow::<Random>().is_ok_and(|other|random.state.get()==other.state.get()))),
            _=>"Random".into_lua(lua)?
        };Ok(Return(MultiValue::from_vec(vec![value])))
    })
}}
fn install_random(lua:&Lua)->Result<()>{
    let methods=lua.create_table()?;
    for (name,callback)in [(c"NextInteger",random_call::<1>as ffi::lua_CFunction),(c"NextNumber",random_call::<2>),(c"Clone",random_call::<3>)]{
        let f:Function=unsafe{lua.exec_raw((),|s|ffi::lua_pushcfunctiond(s,callback,name.as_ptr()))?};methods.set(name.to_str().unwrap(),f)?;
    }lua.set_named_registry_value("types.methods.Random",methods)?;
    unsafe{lua.exec_raw::<()>(Random::seeded(0),|state|{
        ffi::lua_getmetatable(state,1);for(key,f)in [(c"__index",random_meta::<0>as ffi::lua_CFunction),(c"__newindex",random_meta::<1>),(c"__eq",random_meta::<2>),(c"__tostring",random_meta::<3>)]{ffi::lua_pushcfunction(state,f);ffi::lua_rawsetfield(state,-2,key.as_ptr());}
        ffi::lua_pushstring(state,c"The metatable is locked".as_ptr());ffi::lua_rawsetfield(state,-2,c"__metatable".as_ptr());ffi::lua_settop(state,0);
    })?;}
    let api=lua.create_table()?;let new:Function=unsafe{lua.exec_raw((),|s|ffi::lua_pushcfunctiond(s,random_call::<0>,c"new".as_ptr()))?};api.set("new",new)?;api.set_readonly(true);lua.globals().set("Random",api)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vector<const N: usize>(pub [f32; N]);
pub type Vector3 = Vector<3>;
pub type Vector2 = Vector<2>;
impl<const N: usize> Vector<N> {
    fn name() -> &'static str { if N == 3 { "Vector3" } else { "Vector2" } }
    fn dot(self, other: Self) -> f32 { self.0.iter().zip(other.0).map(|(a,b)| a*b).sum() }
    fn length(self) -> f32 { self.dot(self).sqrt() }
    fn unit(self) -> Self { let d=self.length(); Self(self.0.map(|x| x/d)) }
    fn add(self, other: Self) -> Self { Self(std::array::from_fn(|i| self.0[i]+other.0[i])) }
    fn sub(self, other: Self) -> Self { Self(std::array::from_fn(|i| self.0[i]-other.0[i])) }
    fn scale(self, x:f32) -> Self { Self(self.0.map(|v|v*x)) }
}
impl Vector<3> {
    fn cross(self,b:Self)->Self { let a=self.0; let b=b.0; Self([a[1]*b[2]-a[2]*b[1],a[2]*b[0]-a[0]*b[2],a[0]*b[1]-a[1]*b[0]]) }
}
fn vector<const N:usize>(v:&Value)->Result<Vector<N>> {
    if let Value::Vector(v)=v { if N==3 { let a=[v.x(),v.y(),v.z()];return Ok(Vector(std::array::from_fn(|i|a[i]))); } }
    if let Value::UserData(v)=v { if let Ok(v)=v.borrow::<Vector<N>>() { return Ok(*v); } }
    Err(Error::lua_string(format!("{} expected",Vector::<N>::name())))
}
fn vector_value<const N:usize>(lua:&Lua,v:Vector<N>)->Result<Value>{if N==3 {Ok(Value::Vector(mlua::Vector::new(v.0[0],v.0[1],v.0[2])))}else{v.into_lua(lua)}}
fn number(v:&Value)->Result<f32> { match v { Value::Integer(v)=>Ok(*v as f32),Value::Number(v)=>Ok(*v as f32),_=>Err(Error::lua_string("number expected")) } }
fn components<const N:usize>(v:&Value)->Result<[f32;N]> {
    if let Ok(v)=number(v) { Ok([v;N]) } else { Ok(vector::<N>(v)?.0) }
}
fn lookup(lua:&Lua,name:&str,key:&str)->Result<Value> { lua.named_registry_value::<Table>(format!("types.methods.{name}").as_str())?.raw_get(key) }
fn vector_index<const N:usize>(lua:&Lua,v:&Vector<N>,key:&str)->Result<Value> {
    if let Some(i)=["X","Y","Z"].iter().position(|x|*x==key) { if i<N { return Ok(Value::Number(v.0[i] as f64)); } }
    match key { "Magnitude"|"magnitude"=>Ok(Value::Number(v.length() as f64)),"Unit"|"unit"=>vector_value(lua,v.unit()),
        _=>{let method=lookup(lua,Vector::<N>::name(),key)?; if !method.is_nil(){Ok(method)} else {Err(Error::lua_string(format!("{key} is not a valid member of {}",Vector::<N>::name())))}}
    }
}
use mlua::IntoLua;
impl<const N:usize> UserData for Vector<N> {
    fn register(r:&mut UserDataRegistry<Self>) { r.add_meta_field("__type",Self::name()); Self::add_methods(r); }
    fn add_methods<M:UserDataMethods<Self>>(m:&mut M) {
        m.add_meta_method(MetaMethod::Index,|lua,v,k:String|vector_index(lua,v,&k));
        m.add_meta_method(MetaMethod::NewIndex,|_,_,(_,_): (Value,Value)|Err::<(),_>(Error::lua_string(format!("{} cannot be assigned to",Self::name()))));
        m.add_meta_method(MetaMethod::ToString,|_,v,()|Ok(v.0.iter().map(|v|v.to_string()).collect::<Vec<_>>().join(", ")));
        m.add_meta_method(MetaMethod::Eq,|_,v,o:Value|Ok(vector::<N>(&o).is_ok_and(|o|*v==o)));
        m.add_meta_function(MetaMethod::Add,|_,(a,b):(Value,Value)|Ok(vector::<N>(&a)?.add(vector::<N>(&b)?)));
        m.add_meta_function(MetaMethod::Sub,|_,(a,b):(Value,Value)|Ok(vector::<N>(&a)?.sub(vector::<N>(&b)?)));
        m.add_meta_function(MetaMethod::Mul,|_,(a,b):(Value,Value)|{let a=components::<N>(&a)?;let b=components::<N>(&b)?;Ok(Self(std::array::from_fn(|i|a[i]*b[i]))) });
        m.add_meta_function(MetaMethod::Div,|_,(a,b):(Value,Value)|{let a=vector::<N>(&a)?.0;let b=components::<N>(&b)?;Ok(Self(std::array::from_fn(|i|a[i]/b[i]))) });
        m.add_meta_method(MetaMethod::Unm,|_,v,()|Ok(v.scale(-1.0)));
        m.add_method("Dot",|_,v,o:Value|Ok(v.dot(vector(&o)?)));
        m.add_method("Lerp",|_,v,(o,t):(Value,f32)|Ok(v.add(vector(&o)?.sub(*v).scale(t))));
        m.add_method("FuzzyEq",|_,v,(o,eps):(Value,Option<f32>)|{let o=vector::<N>(&o)?;let eps=eps.unwrap_or(1e-5);Ok(v.sub(o).length()<=eps*(v.length()+o.length()+1.0))});
        if N==3 { m.add_method("Cross",|_,v,o:Value|{let o=vector::<3>(&o)?;Ok(Vector::<3>([v.0[0],v.0[1],v.0[2]]).cross(o))}); }
        if N==2 { m.add_method("Cross",|_,v,o:Value|{let o=vector::<2>(&o)?;Ok(v.0[0]*o.0[1]-v.0[1]*o.0[0])}); }
    }
}

#[derive(Clone,Copy,Debug,PartialEq)]
pub struct CFrame { pub p:Vector3, pub r:[[f32;3];3] }
impl CFrame {
    fn identity()->Self { Self{p:Vector([0.;3]),r:[[1.,0.,0.],[0.,1.,0.],[0.,0.,1.]]} }
    fn rotate(self,v:Vector3)->Vector3 { Vector(self.r.map(|row|Vector(row).dot(v))) }
    fn compose(self,b:Self)->Self { Self{p:self.p.add(self.rotate(b.p)),r:std::array::from_fn(|i|std::array::from_fn(|j|(0..3).map(|k|self.r[i][k]*b.r[k][j]).sum()))} }
    fn inverse(self)->Self { let r=std::array::from_fn(|i|std::array::from_fn(|j|self.r[j][i]));let mut f=Self{p:Vector([0.;3]),r};f.p=f.rotate(self.p.scale(-1.));f }
    fn components(self)->Vec<f32> { self.p.0.into_iter().chain(self.r.into_iter().flatten()).collect() }
    fn look_at(p:Vector3,at:Vector3,up:Vector3)->Self { let z=p.sub(at).unit();let x=up.cross(z).unit();let y=z.cross(x);Self{p,r:std::array::from_fn(|i|[x.0[i],y.0[i],z.0[i]])} }
    fn angles(x:f32,y:f32,z:f32)->Self { let (sx,cx)=x.sin_cos();let(sy,cy)=y.sin_cos();let(sz,cz)=z.sin_cos();Self{p:Vector([0.;3]),r:[[cy*cz,-cy*sz,sy],[cx*sz+sx*sy*cz,cx*cz-sx*sy*sz,-sx*cy],[sx*sz-cx*sy*cz,sx*cz+cx*sy*sz,cx*cy]]} }
}
fn cf(v:&Value)->Result<CFrame> { if let Value::UserData(v)=v { if let Ok(v)=v.borrow::<CFrame>(){return Ok(*v);} } Err(Error::lua_string("CFrame expected")) }
fn cf_index(lua:&Lua,v:&CFrame,key:&str)->Result<Value> {
    match key { "Position"|"p"=>vector_value(lua,v.p),"X"=>Ok(Value::Number(v.p.0[0] as f64)),"Y"=>Ok(Value::Number(v.p.0[1] as f64)),"Z"=>Ok(Value::Number(v.p.0[2] as f64)),
        "RightVector"|"XVector"=>vector_value(lua,Vector::<3>(v.r.map(|r|r[0]))),"UpVector"|"YVector"=>vector_value(lua,Vector::<3>(v.r.map(|r|r[1]))),"LookVector"=>vector_value(lua,Vector::<3>(v.r.map(|r|-r[2]))),"ZVector"=>vector_value(lua,Vector::<3>(v.r.map(|r|r[2]))),"Rotation"=>CFrame{p:Vector([0.;3]),..*v}.into_lua(lua),
        _=>{let method=lookup(lua,"CFrame",key)?;if !method.is_nil(){Ok(method)}else{Err(Error::lua_string(format!("{key} is not a valid member of CFrame")))}}
    }
}
impl UserData for CFrame {
    fn register(r:&mut UserDataRegistry<Self>){r.add_meta_field("__type","CFrame");Self::add_methods(r);}
    fn add_methods<M:UserDataMethods<Self>>(m:&mut M){
        m.add_meta_method(MetaMethod::Index,|lua,v,k:String|cf_index(lua,v,&k));
        m.add_meta_method(MetaMethod::ToString,|_,v,()|Ok(v.components().iter().map(|v|v.to_string()).collect::<Vec<_>>().join(", ")));
        m.add_meta_method(MetaMethod::Eq,|_,v,o:Value|Ok(cf(&o).is_ok_and(|o|*v==o)));
        m.add_meta_function(MetaMethod::Mul,|lua,(a,b):(Value,Value)|{let a=cf(&a)?; if let Ok(b)=cf(&b){a.compose(b).into_lua(lua)}else{vector_value(lua,a.p.add(a.rotate(vector(&b)?)))}});
        m.add_meta_method(MetaMethod::Add,|_,v,o:Value|Ok(Self{p:v.p.add(vector(&o)?),..*v}));
        m.add_meta_method(MetaMethod::Sub,|_,v,o:Value|Ok(Self{p:v.p.sub(vector(&o)?),..*v}));
        m.add_method("Inverse",|_,v,()|Ok(v.inverse()));
        m.add_method("GetComponents",|_,v,()|Ok(MultiValue::from_vec(v.components().into_iter().map(|v|Value::Number(v as f64)).collect())));
        m.add_method("ToWorldSpace",|_,v,o:Value|Ok(v.compose(cf(&o)?)));
        m.add_method("ToObjectSpace",|_,v,o:Value|Ok(v.inverse().compose(cf(&o)?)));
        m.add_method("PointToWorldSpace",|lua,v,o:Value|vector_value(lua,v.p.add(v.rotate(vector(&o)?))));
        m.add_method("PointToObjectSpace",|lua,v,o:Value|vector_value(lua,v.inverse().rotate(vector::<3>(&o)?.sub(v.p))));
        m.add_method("VectorToWorldSpace",|lua,v,o:Value|vector_value(lua,v.rotate(vector(&o)?)));
        m.add_method("VectorToObjectSpace",|lua,v,o:Value|vector_value(lua,v.inverse().rotate(vector(&o)?)));
        m.add_method("ToOrientation",|_,v,()|{let x=(-v.r[1][2]).clamp(-1.,1.).asin();Ok((x,v.r[0][2].atan2(v.r[2][2]),v.r[1][0].atan2(v.r[1][1]))) });
    }
}

#[derive(Clone,Copy,Debug,PartialEq)]
pub struct Color3(pub [f32;3]);
#[derive(Clone,Copy,Debug,PartialEq)]
pub struct Ray { origin:Vector3, direction:Vector3 }
impl UserData for Ray {
    fn register(r:&mut UserDataRegistry<Self>){r.add_meta_field("__type","Ray");Self::add_methods(r);}
    fn add_methods<M:UserDataMethods<Self>>(m:&mut M){
        m.add_meta_method(MetaMethod::Index,|lua,v,k:String|match k.as_str(){"Origin"=>vector_value(lua,v.origin),"Direction"=>vector_value(lua,v.direction),"Unit"=>Ray{origin:v.origin,direction:v.direction.unit()}.into_lua(lua),_=>Err(Error::lua_string(format!("{k} is not a valid member of Ray")))});
        m.add_meta_method(MetaMethod::Eq,|_,v,o:AnyUserData|Ok(o.borrow::<Self>().is_ok_and(|o|*v==*o)));
        m.add_meta_method(MetaMethod::ToString,|_,v,()|Ok(v.origin.0.into_iter().chain(v.direction.0).map(|v|v.to_string()).collect::<Vec<_>>().join(", ")));
    }
}
pub fn camera_ray(lua:&Lua,frame:&Value,viewport:&Value,fov:f32,near:f32,x:f32,y:f32,depth:f32)->Result<Value>{
    let frame=cf(frame)?;let size=vector::<2>(viewport)?.0;
    if size[0]<=0.||size[1]<=0.||near<=0.{return Err(Error::lua_string("Camera projection requires positive viewport and near plane"));}
    let tangent=(fov.to_radians()*0.5).tan();
    let px=(x*2./size[0]-1.)*size[0]/size[1]*tangent;
    let py=(1.-y*2./size[1])*tangent;
    let direction=frame.rotate(Vector([px,py,-1.]).unit());
    let origin=frame.p.add(frame.rotate(Vector([px*near,py*near,-near]))).add(direction.scale(depth));
    Ray{origin,direction}.into_lua(lua)
}
fn color_index(lua:&Lua,v:&Color3,k:&str)->Result<Value>{if let Some(i)=["R","G","B"].iter().position(|s|*s==k){return Ok(Value::Number(v.0[i] as f64));}let method=lookup(lua,"Color3",k)?;if !method.is_nil(){Ok(method)}else{Err(Error::lua_string(format!("{k} is not a valid member of Color3")))}}
impl UserData for Color3 {
    fn register(r:&mut UserDataRegistry<Self>){r.add_meta_field("__type","Color3");Self::add_methods(r);}
    fn add_methods<M:UserDataMethods<Self>>(m:&mut M){
        m.add_meta_method(MetaMethod::Index,|lua,v,k:String|color_index(lua,v,&k));
        m.add_meta_method(MetaMethod::Eq,|_,v,o:AnyUserData|Ok(o.borrow::<Self>().is_ok_and(|o|*v==*o)));
        m.add_meta_method(MetaMethod::ToString,|_,v,()|Ok(v.0.iter().map(|v|v.to_string()).collect::<Vec<_>>().join(", ")));
        m.add_method("Lerp",|_,v,(o,t):(AnyUserData,f32)|{let o=o.borrow::<Self>()?;Ok(Self(std::array::from_fn(|i|v.0[i]+(o.0[i]-v.0[i])*t)))});
        m.add_method("ToHex",|_,v,()|Ok(v.0.map(|n|format!("{:02x}",(n.clamp(0.,1.)*255.).round() as u8)).join("")));
    }
}

fn seal<T:UserData+'static>(lua:&Lua,sample:T,name:&str,names:&[(&str,&'static std::ffi::CStr)],index:Function)->Result<()> {
    let sample=lua.create_userdata(sample)?; let methods=lua.create_table()?;
    for (key,debug_name) in names {let f=sample.get::<Function>(*key)?;crate::native::name_function(lua,&f,debug_name)?;methods.raw_set(*key,f)?;}
    lua.set_named_registry_value(format!("types.methods.{name}").as_str(),methods)?;
    unsafe {lua.exec_raw::<()>((sample,index),|s|{ffi::lua_getmetatable(s,1);ffi::lua_pushvalue(s,2);ffi::lua_rawsetfield(s,-2,c"__index".as_ptr());ffi::lua_pushstring(s,c"The metatable is locked".as_ptr());ffi::lua_rawsetfield(s,-2,c"__metatable".as_ptr());ffi::lua_settop(s,0);})?;}
    Ok(())
}
fn named<A:mlua::FromLuaMulti,R:mlua::IntoLuaMulti>(lua:&Lua,name:&'static std::ffi::CStr,f:impl Fn(&Lua,A)->Result<R>+'static)->Result<Function>{let f=lua.create_function(f)?;crate::native::name_function(lua,&f,name)?;Ok(f)}
fn install_vector<const N:usize>(lua:&Lua)->Result<()> {
    let sample=Vector::<N>([0.;N]);let names=vec![("Dot",c"Dot"),("Lerp",c"Lerp"),("FuzzyEq",c"FuzzyEq"),("Cross",c"Cross")];
    let index=lua.create_function(|lua,(v,k):(AnyUserData,String)|vector_index(lua,&*v.borrow::<Vector<N>>()?,&k))?;
    seal(lua,sample,Vector::<N>::name(),&names,index)?;
    let api=lua.create_table()?;
    api.set("new",named(lua,c"new",|_,a:MultiValue|{let mut v=[0.;N];for(i,n)in v.iter_mut().enumerate(){if let Some(value)=a.get(i){*n=number(value)?;}}Ok(Vector(v))})?)?;
    api.set("zero",sample)?;api.set("one",Vector::<N>([1.;N]))?;
    for(i,n)in ["xAxis","yAxis","zAxis"].iter().enumerate().take(N){let mut v=[0.;N];v[i]=1.;api.set(*n,Vector(v))?;}
    api.set_readonly(true);lua.globals().set(Vector::<N>::name(),api)
}
fn install_vector3(lua:&Lua)->Result<()> {
    let methods=lua.create_table()?;
    methods.set("Dot",named(lua,c"Dot",|_,(a,b):(Value,Value)|Ok(vector::<3>(&a)?.dot(vector(&b)?)))?)?;
    methods.set("Cross",named(lua,c"Cross",|lua,(a,b):(Value,Value)|vector_value(lua,vector::<3>(&a)?.cross(vector(&b)?)))?)?;
    methods.set("Lerp",named(lua,c"Lerp",|lua,(a,b,t):(Value,Value,f32)|{let a=vector::<3>(&a)?;vector_value(lua,a.add(vector(&b)?.sub(a).scale(t)))})?)?;
    methods.set("FuzzyEq",named(lua,c"FuzzyEq",|_,(a,b,e):(Value,Value,Option<f32>)|{let a=vector::<3>(&a)?;let b=vector::<3>(&b)?;Ok(a.sub(b).length()<=e.unwrap_or(1e-5)*(a.length()+b.length()+1.))})?)?;
    lua.set_named_registry_value("types.methods.Vector3",methods)?;
    let mt=lua.create_table()?;mt.set("__type","Vector3")?;mt.set("__metatable","The metatable is locked")?;
    mt.set("__index",lua.create_function(|lua,(a,k):(Value,String)|vector_index(lua,&vector::<3>(&a)?,&k))?)?;
    mt.set("__tostring",lua.create_function(|_,a:Value|Ok(vector::<3>(&a)?.0.iter().map(|v|v.to_string()).collect::<Vec<_>>().join(", ")))?)?;
    unsafe{lua.exec_raw::<()>((mlua::Vector::zero(),mt),|s|{ffi::lua_setmetatable(s,1);ffi::lua_settop(s,0);})?;}
    let api=lua.create_table()?;api.set("new",named(lua,c"new",|_,(x,y,z):(Option<f32>,Option<f32>,Option<f32>)|Ok(mlua::Vector::new(x.unwrap_or(0.),y.unwrap_or(0.),z.unwrap_or(0.))))?)?;
    for (name,v) in [("zero",[0.;3]),("one",[1.;3]),("xAxis",[1.,0.,0.]),("yAxis",[0.,1.,0.]),("zAxis",[0.,0.,1.])]{api.set(name,mlua::Vector::new(v[0],v[1],v[2]))?;}
    api.set_readonly(true);lua.globals().set("Vector3",api)
}
pub fn install(lua:&Lua)->Result<()> {
    install_enums(lua)?;
    install_dimensions(lua)?;install_random(lua)?;
    install_vector::<2>(lua)?;install_vector3(lua)?;
    let index=lua.create_function(|lua,(v,k):(AnyUserData,String)|cf_index(lua,&*v.borrow::<CFrame>()?,&k))?;
    seal(lua,CFrame::identity(),"CFrame",&[("Inverse",c"Inverse"),("GetComponents",c"GetComponents"),("ToWorldSpace",c"ToWorldSpace"),("ToObjectSpace",c"ToObjectSpace"),("PointToWorldSpace",c"PointToWorldSpace"),("PointToObjectSpace",c"PointToObjectSpace"),("VectorToWorldSpace",c"VectorToWorldSpace"),("VectorToObjectSpace",c"VectorToObjectSpace"),("ToOrientation",c"ToOrientation")],index)?;
    let api=lua.create_table()?;
    api.set("new",named(lua,c"new",|_,a:MultiValue|{let mut f=CFrame::identity();if a.is_empty(){return Ok(f);}if let Ok(p)=vector::<3>(&a[0]){f.p=p;if a.len()>1{f=CFrame::look_at(p,vector(&a[1])?,Vector([0.,1.,0.]));}return Ok(f);}if a.len()!=3&&a.len()!=12{return Err(Error::lua_string("CFrame.new requires 0, 1, 2, 3, or 12 arguments"));}for i in 0..3{f.p.0[i]=number(&a[i])?;}if a.len()==12{for i in 0..3{for j in 0..3{f.r[i][j]=number(&a[3+i*3+j])?;}}}Ok(f)})?)?;
    let angles=named(lua,c"Angles",|_,(x,y,z):(f32,f32,f32)|Ok(CFrame::angles(x,y,z)))?;api.set("Angles",angles.clone())?;api.set("fromEulerAnglesXYZ",angles)?;
    api.set("lookAt",named(lua,c"lookAt",|_,(p,a,u):(Value,Value,Option<Value>)|Ok(CFrame::look_at(vector(&p)?,vector(&a)?,u.as_ref().map(vector).transpose()?.unwrap_or(Vector([0.,1.,0.])))))?)?;
    api.set("fromMatrix",named(lua,c"fromMatrix",|_,(p,x,y,z):(Value,Value,Value,Option<Value>)|{let p=vector(&p)?;let x=vector::<3>(&x)?;let y=vector::<3>(&y)?;let z=z.as_ref().map(vector).transpose()?.unwrap_or_else(||x.cross(y));Ok(CFrame{p,r:std::array::from_fn(|i|[x.0[i],y.0[i],z.0[i]])})})?)?;
    api.set("identity",CFrame::identity())?;api.set_readonly(true);lua.globals().set("CFrame",api)?;
    let index=lua.create_function(|lua,(v,k):(AnyUserData,String)|color_index(lua,&*v.borrow::<Color3>()?,&k))?;
    seal(lua,Color3([0.;3]),"Color3",&[("Lerp",c"Lerp"),("ToHex",c"ToHex")],index)?;
    let api=lua.create_table()?;
    api.set("new",named(lua,c"new",|_,(r,g,b):(Option<f32>,Option<f32>,Option<f32>)|Ok(Color3([r.unwrap_or(0.),g.unwrap_or(0.),b.unwrap_or(0.)])))?)?;
    api.set("fromRGB",named(lua,c"fromRGB",|_,(r,g,b):(f32,f32,f32)|Ok(Color3([r/255.,g/255.,b/255.])))?)?;
    api.set_readonly(true);lua.globals().set("Color3",api)?;
    lock(lua,lua.create_userdata(Ray{origin:Vector([0.;3]),direction:Vector([0.;3])})?)?;
    let api=lua.create_table()?;api.set("new",named(lua,c"new",|_,(origin,direction):(Option<Value>,Option<Value>)|Ok(Ray{origin:origin.as_ref().map(vector).transpose()?.unwrap_or(Vector([0.;3])),direction:direction.as_ref().map(vector).transpose()?.unwrap_or(Vector([0.;3]))}))?)?;api.set_readonly(true);lua.globals().set("Ray",api)?;
    let api=lua.create_table()?;
    api.set("encode",lua.create_function(|lua,v:Value|{match encode_value(&v)?{Some(v)=>lua.to_value(&v),None=>Ok(Value::Nil)}})?)?;
    api.set("decode",lua.create_function(|lua,v:Value|{decode_value(lua,&lua.from_value(v)?)?.ok_or_else(||Error::lua_string("Unsupported wire datatype"))})?)?;
    lua.globals().set("__types",api)
}
pub fn encode_value(v:&Value)->Result<Option<Json>> {
    if let Value::Vector(v)=v{return Ok(Some(json!({"tag":"Vector3","x":v.x(),"y":v.y(),"z":v.z()})));}
    let Value::UserData(v)=v else{return Ok(None)};
    if let Ok(v)=v.borrow::<Vector2>(){return Ok(Some(json!({"tag":"Vector2","x":v.0[0],"y":v.0[1]})));}
    if let Ok(v)=v.borrow::<Vector3>(){return Ok(Some(json!({"tag":"Vector3","x":v.0[0],"y":v.0[1],"z":v.0[2]})));}
    if let Ok(v)=v.borrow::<CFrame>(){return Ok(Some(json!({"tag":"CFrame","components":v.components()})));}
    if let Ok(v)=v.borrow::<Color3>(){return Ok(Some(json!({"tag":"Color3","r":v.0[0],"g":v.0[1],"b":v.0[2]})));}
    if let Ok(v)=v.borrow::<Ray>(){return Ok(Some(json!({"tag":"Ray","origin":v.origin.0,"direction":v.direction.0})));}
    if let Ok(v)=v.borrow::<UDim>(){return Ok(Some(json!({"tag":"UDim","scale":v.scale,"offset":v.offset})));}
    if let Ok(v)=v.borrow::<UDim2>(){return Ok(Some(json!({"tag":"UDim2","xScale":v.x.scale,"xOffset":v.x.offset,"yScale":v.y.scale,"yOffset":v.y.offset})));}
    if let Ok(v)=v.borrow::<EnumItem>(){return Ok(Some(json!({"tag":"EnumItem","enum":v.family,"name":v.name})));}
    Ok(None)
}
pub fn decode_value(lua:&Lua,v:&Json)->Result<Option<Value>> {
    let n=|k:&str|v[k].as_f64().unwrap_or(0.) as f32;
    Ok(match v["tag"].as_str().unwrap_or(""){
        "Vector2"=>Some(Vector([n("x"),n("y")]).into_lua(lua)?),"Vector3"=>Some(vector_value(lua,Vector([n("x"),n("y"),n("z")]))?),"Color3"=>Some(Color3([n("r"),n("g"),n("b")]).into_lua(lua)?),
        "CFrame"=>{let a=v["components"].as_array().filter(|a|a.len()==12).ok_or_else(||Error::lua_string("Invalid CFrame wire value"))?;let mut f=CFrame::identity();for i in 0..3{f.p.0[i]=a[i].as_f64().unwrap_or(0.) as f32;for j in 0..3{f.r[i][j]=a[3+i*3+j].as_f64().unwrap_or(0.) as f32;}}Some(f.into_lua(lua)?)},
        "Ray"=>{let read=|key:&str|->Result<Vector3>{let a=v[key].as_array().filter(|a|a.len()==3).ok_or_else(||Error::lua_string("Invalid Ray wire value"))?;Ok(Vector(std::array::from_fn(|i|a[i].as_f64().unwrap_or(0.)as f32)))};Some(Ray{origin:read("origin")?,direction:read("direction")?}.into_lua(lua)?)},
        "UDim"=>Some(UDim{scale:n("scale"),offset:dimension_int(v["offset"].as_f64().unwrap_or(0.))}.into_lua(lua)?),
        "UDim2"=>Some(UDim2{x:UDim{scale:n("xScale"),offset:dimension_int(v["xOffset"].as_f64().unwrap_or(0.))},y:UDim{scale:n("yScale"),offset:dimension_int(v["yOffset"].as_f64().unwrap_or(0.))}}.into_lua(lua)?),
        "EnumItem"=>{let family=v["enum"].as_str().unwrap_or("");let name=v["name"].as_str().unwrap_or("");let items:Table=lua.named_registry_value(format!("types.enum.{family}").as_str())?;let item:Value=items.get(name)?;if item.is_nil(){return Err(Error::lua_string("Invalid EnumItem wire value"));}Some(item)},_=>None})
}

#[derive(Clone)]
struct EnumRoot;
#[derive(Clone)]
struct EnumType(String);
#[derive(Clone)]
struct EnumItem {family:String,name:String,value:i64}
impl UserData for EnumRoot {
    fn register(r:&mut UserDataRegistry<Self>){r.add_meta_field("__type","Enums");Self::add_methods(r);}
    fn add_methods<M:UserDataMethods<Self>>(m:&mut M){
        m.add_meta_method(MetaMethod::ToString,|_,_,()|Ok("Enums"));
    }
}
impl UserData for EnumType {
    fn register(r:&mut UserDataRegistry<Self>){r.add_meta_field("__type","Enum");Self::add_methods(r);}
    fn add_methods<M:UserDataMethods<Self>>(m:&mut M){
        m.add_meta_method(MetaMethod::ToString,|_,t,()|Ok(t.0.clone()));
        m.add_meta_method(MetaMethod::Eq,|_,t,o:AnyUserData|Ok(o.borrow::<Self>().is_ok_and(|o|o.0==t.0)));
    }
}
impl UserData for EnumItem {
    fn register(r:&mut UserDataRegistry<Self>){r.add_meta_field("__type","EnumItem");Self::add_methods(r);}
    fn add_methods<M:UserDataMethods<Self>>(m:&mut M){
        m.add_meta_method(MetaMethod::ToString,|_,t,()|Ok(format!("Enum.{}.{}",t.family,t.name)));
        m.add_meta_method(MetaMethod::Eq,|_,t,o:AnyUserData|Ok(o.borrow::<Self>().is_ok_and(|o|o.family==t.family&&o.name==t.name)));
    }
}
unsafe extern "C-unwind" fn enum_key_error(state:*mut ffi::lua_State)->std::ffi::c_int { unsafe {ffi::luaL_typeerror(state,2,c"string".as_ptr())} }
unsafe extern "C-unwind" fn enum_self_error<const KIND:usize>(state:*mut ffi::lua_State)->std::ffi::c_int {
    let expected=match KIND {0=>c"Enums",1=>c"Enum",_=>c"EnumItem"};
    unsafe {ffi::luaL_typeerror(state,1,expected.as_ptr())}
}
fn enum_value<T:UserData+Clone+'static>(args:&MultiValue)->Option<T> {
    match args.front(){Some(Value::UserData(u))=>u.borrow::<T>().ok().map(|v|v.clone()),_=>None}
}
// Dispatch validation only after Rust guards drop. Luau builds the argument
// error and traceback in this same native frame, without calling __tostring.
unsafe extern "C-unwind" fn enum_index<const KIND:usize>(state:*mut ffi::lua_State)->std::ffi::c_int { unsafe {
    Lua::invoke_native_callback(state,|lua,args|{
        let family=if KIND==1 {match enum_value::<EnumType>(&args){Some(v)=>Some(v),None=>return Ok(mlua::state::NativeCall::Dispatch(enum_self_error::<1>))}}else{None};
        let item=if KIND==2 {match enum_value::<EnumItem>(&args){Some(v)=>Some(v),None=>return Ok(mlua::state::NativeCall::Dispatch(enum_self_error::<2>))}}else{None};
        if !matches!(args.get(1),Some(Value::String(_)|Value::Number(_)|Value::Integer(_))){return Ok(mlua::state::NativeCall::Dispatch(enum_key_error))}
        let key=lua.coerce_string(args[1].clone())?.unwrap().to_string_lossy();
        let value=match KIND {
            0=>{if key=="GetEnums" {lua.named_registry_value("types.GetEnums")?}else{
                let value:Value=lua.named_registry_value::<Table>("types.enums")?.raw_get(key.as_str())?;
                if value.is_nil(){return Err(Error::lua_string(format!("{key} is not a valid member of \"Enum\"")))} value
            }},
            1=>{if key=="GetEnumItems" {lua.named_registry_value("types.GetEnumItems")?}else{
                let family=family.unwrap().0;
                let value:Value=lua.named_registry_value::<Table>(format!("types.enum.{family}").as_str())?.raw_get(key.as_str())?;
                if value.is_nil(){return Err(Error::lua_string(format!("{key} is not a valid member of \"Enum.{family}\"")))} value
            }},
            _=>{let item=item.unwrap();match key.as_str(){
                "Name"=>item.name.into_lua(lua)?,"Value"=>Value::Integer(item.value),
                "EnumType"=>lua.named_registry_value::<Table>("types.enums")?.raw_get(item.family.as_str())?,
                _=>return Err(Error::lua_string(format!("{key} is not a valid member of \"Enum.{}.{}\"",item.family,item.name)))
            }}
        };
        Ok(mlua::state::NativeCall::Return(MultiValue::from_vec(vec![value])))
    })
}}
unsafe extern "C-unwind" fn enum_newindex<const KIND:usize>(state:*mut ffi::lua_State)->std::ffi::c_int { unsafe {
    Lua::invoke_native_callback(state,|lua,args|{
        if !matches!(args.get(1),Some(Value::String(_)|Value::Number(_)|Value::Integer(_))){return Ok(mlua::state::NativeCall::Dispatch(enum_key_error))}
        let key=lua.coerce_string(args[1].clone())?.unwrap().to_string_lossy();
        let name=match KIND{0=>"Enums",1=>"Enum",_=>"EnumItem"};
        Err(Error::lua_string(format!("{key} is not a valid member of {name}")))
    })
}}
unsafe extern "C-unwind" fn enum_list<const KIND:usize>(state:*mut ffi::lua_State)->std::ffi::c_int { unsafe {
    Lua::invoke_native_callback(state,|lua,args|{
        let result=lua.create_table()?;
        if KIND==0 {
            if enum_value::<EnumRoot>(&args).is_none(){return Ok(mlua::state::NativeCall::Dispatch(enum_self_error::<0>))}
            let source:Table=lua.named_registry_value("types.enums")?;
            let mut entries=source.pairs::<String,Value>().collect::<Result<Vec<_>>>()?;
            entries.sort_by(|a,b|a.0.cmp(&b.0));
            for (i,(_,value)) in entries.into_iter().enumerate(){result.raw_set(i+1,value)?;}
        } else {
            let Some(family)=enum_value::<EnumType>(&args) else{return Ok(mlua::state::NativeCall::Dispatch(enum_self_error::<1>))};
            let source:Table=lua.named_registry_value(format!("types.enumlist.{}",family.0).as_str())?;
            for (i,value) in source.sequence_values::<Value>().enumerate(){result.raw_set(i+1,value?)?;}
        }
        Ok(mlua::state::NativeCall::Return(MultiValue::from_vec(vec![Value::Table(result)])))
    })
}}
fn enum_metatable<const KIND:usize>(lua:&Lua,sample:AnyUserData)->Result<()> { unsafe {
    lua.exec_raw::<()>(sample,|state|{
        ffi::lua_getmetatable(state,1);
        ffi::lua_pushcfunction(state,enum_index::<KIND>);ffi::lua_rawsetfield(state,-2,c"__index".as_ptr());
        ffi::lua_pushcfunction(state,enum_newindex::<KIND>);ffi::lua_rawsetfield(state,-2,c"__newindex".as_ptr());
        ffi::lua_pushstring(state,c"The metatable is locked".as_ptr());ffi::lua_rawsetfield(state,-2,c"__metatable".as_ptr());
        ffi::lua_settop(state,0);
    })
}}
fn lock(lua:&Lua,u:AnyUserData)->Result<AnyUserData>{unsafe{lua.exec_raw::<()>(u.clone(),|s|{ffi::lua_getmetatable(s,1);ffi::lua_pushstring(s,c"The metatable is locked".as_ptr());ffi::lua_rawsetfield(s,-2,c"__metatable".as_ptr());ffi::lua_settop(s,0);})?;}Ok(u)}
fn install_enums(lua:&Lua)->Result<()> {
    let definitions:Json=serde_json::from_str(include_str!("../native/enums.json")).map_err(Error::external)?;
    let families=lua.create_table()?;lua.set_named_registry_value("types.enums",families.clone())?;
    for (family,items) in definitions.as_object().unwrap(){let by_name=lua.create_table()?;let list=lua.create_table()?;for (i,item) in items.as_array().unwrap().iter().enumerate(){let name=item["name"].as_str().unwrap();let value=item["value"].as_i64().unwrap();let u=lock(lua,lua.create_userdata(EnumItem{family:family.clone(),name:name.into(),value})?)?;by_name.set(name,u.clone())?;list.set(i+1,u)?;}lua.set_named_registry_value(format!("types.enum.{family}").as_str(),by_name)?;lua.set_named_registry_value(format!("types.enumlist.{family}").as_str(),list)?;families.set(family.as_str(),lock(lua,lua.create_userdata(EnumType(family.clone()))?)?)?;}
    let get_items:Function=unsafe{lua.exec_raw((),|state|ffi::lua_pushcfunction(state,enum_list::<1>))?};
    crate::native::name_function(lua,&get_items,c"GetEnumItems")?;lua.set_named_registry_value("types.GetEnumItems",get_items)?;
    let get_enums:Function=unsafe{lua.exec_raw((),|state|ffi::lua_pushcfunction(state,enum_list::<0>))?};
    crate::native::name_function(lua,&get_enums,c"GetEnums")?;lua.set_named_registry_value("types.GetEnums",get_enums)?;
    let root=lua.create_userdata(EnumRoot)?;
    enum_metatable::<0>(lua,root.clone())?;
    enum_metatable::<1>(lua,families.raw_get("HumanoidStateType")?)?;
    enum_metatable::<2>(lua,lua.named_registry_value::<Table>("types.enum.HumanoidStateType")?.raw_get("Running")?)?;
    lua.globals().set("Enum",root)
}

#[cfg(all(test, emulator_local_tests))]
#[path = "../local_tests/types.rs"]
mod tests;
