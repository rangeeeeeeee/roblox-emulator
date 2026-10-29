use crate::{Scenario, session::Session};
use serde_json::{Value, json};
use std::io::{self, Read, Write};

// Both sides validate the frame length before allocating; the parent treats all
// worker output as untrusted even after the OS sandbox has been established.
pub(crate) const MAX_FRAME: usize = 64 * 1024 * 1024;

pub(crate) fn read_frame(reader: &mut impl Read) -> io::Result<Option<Value>> {
    let mut len=[0u8;4];
    match reader.read(&mut len[..1])? { 0=>return Ok(None), _=>reader.read_exact(&mut len[1..])? }
    let len=u32::from_le_bytes(len) as usize;
    if len==0 || len>MAX_FRAME {return Err(io::Error::new(io::ErrorKind::InvalidData,"Invalid worker frame size"));}
    let mut bytes=vec![0u8;len];
    reader.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes).map(Some).map_err(io::Error::other)
}

pub(crate) fn write_frame(writer: &mut impl Write, value: &Value) -> io::Result<()> {
    struct Bounded<'a>(&'a mut Vec<u8>);
    impl Write for Bounded<'_> {
        fn write(&mut self,bytes:&[u8])->io::Result<usize> {
            if bytes.len()>MAX_FRAME-self.0.len() {return Err(io::Error::new(io::ErrorKind::InvalidData,"Worker frame exceeds limit"));}
            self.0.extend_from_slice(bytes);Ok(bytes.len())
        }
        fn flush(&mut self)->io::Result<()> {Ok(())}
    }
    let mut bytes=Vec::new();
    serde_json::to_writer(&mut Bounded(&mut bytes),value).map_err(io::Error::other)?;
    if bytes.is_empty() || bytes.len()>MAX_FRAME {return Err(io::Error::new(io::ErrorKind::InvalidData,"Worker frame exceeds limit"));}
    writer.write_all(&(bytes.len() as u32).to_le_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()
}

fn string<'a>(value: &'a Value, name: &str) -> Result<&'a str,String> {
    value.get(name).and_then(Value::as_str).ok_or_else(||format!("Missing {name}"))
}

fn execute(session: &mut Option<Session>, request: &Value) -> Result<Value,String> {
    let op=string(request,"op")?;
    if op=="init" {
        if session.is_some() {return Err("Worker is already initialized".into());}
        let scenario:Scenario=serde_json::from_value(request.get("scenario").cloned().ok_or("Missing scenario")?).map_err(|error|error.to_string())?;
        let mut state=Session::from_scenario(scenario).map_err(|error|error.to_string())?;
        state.set_origin(request.get("origin").and_then(Value::as_str).map(str::to_owned));
        *session=Some(state);
        return Ok(Value::Null);
    }
    if op=="lint" {
        return serde_json::to_value(crate::lint::analyze(string(request,"source")?,string(request,"context")?)?).map_err(|error|error.to_string());
    }
    if op=="batch" {
        if session.is_some() {return Err("Batch execution requires a fresh worker".into());}
        let scenario:Scenario=serde_json::from_value(request.get("scenario").cloned().ok_or("Missing scenario")?).map_err(|error|error.to_string())?;
        let snapshots:Vec<Value>=serde_json::from_value(request.get("snapshots").cloned().ok_or("Missing source snapshots")?).map_err(|error|error.to_string())?;
        return crate::run_scenario(scenario,snapshots).map_err(|error|error.to_string());
    }
    if op=="probe" {
        if session.is_some() {return Err("Probe requires a fresh worker".into());}
        return crate::native_probe_data(string(request,"source")?,request.get("reference").cloned().ok_or("Missing reference")?,
            request.get("fullRuntime").and_then(Value::as_bool).ok_or("Missing fullRuntime")?).map_err(|error|error.to_string());
    }
    let session=session.as_mut().ok_or("Worker is not initialized")?;
    let done = |result: mlua::Result<()>| result.map(|_|Value::Null).map_err(|error|error.to_string());
    match op {
        "snapshot" => Ok(Value::Null),
        "trace" => done(session.set_trace_errors(request.get("enabled").and_then(Value::as_bool).ok_or("Missing enabled")?)),
        "execute" => session.execute_input(string(request,"realm")?,string(request,"context")?,string(request,"source")?,string(request,"format")?).map_err(|error|error.to_string()),
        "input" => done(session.keyboard_input(string(request,"realm")?,string(request,"key")?,request.get("down").and_then(Value::as_bool).ok_or("Missing down")?)),
        "advance" => done(session.advance(request.get("seconds").and_then(Value::as_f64).ok_or("Missing seconds")?).map(|_|())),
        "completions" => {
            let path:Vec<String>=serde_json::from_value(request.get("path").cloned().ok_or("Missing path")?).map_err(|error|error.to_string())?;
            session.completions(string(request,"realm")?,string(request,"context")?,&path).map_err(|error|error.to_string())
        }
        "workspace_list" => session.workspace_files(string(request,"realm")?).map_err(|error|error.to_string()),
        "workspace_read" => session.workspace_file(string(request,"realm")?,string(request,"name")?).map_err(|error|error.to_string()),
        "inspect" => session.inspect(request.get("body").ok_or("Missing body")?).map_err(|error|error.to_string()),
        "budget" => done(session.set_cpu_budget(request.get("cpuMs").and_then(Value::as_u64).ok_or("Missing cpuMs")?)),
        "clear" => {session.clear_output();Ok(Value::Null)},
        "network_decide" => done(session.network_decide(string(request,"id")?,request.get("allow").and_then(Value::as_bool).ok_or("Missing allow")?)),
        "network_complete" => done(session.network_complete_external(string(request,"id")?,request.get("response").cloned().ok_or("Missing response")?)),
        _ => Err("Unknown worker operation".into()),
    }
}

pub(crate) fn run() -> io::Result<()> {
    crate::network::enable_external_broker();
    let mut session=None;
    let stdin=io::stdin();let mut input=stdin.lock();
    let stdout=io::stdout();let mut output=stdout.lock();
    while let Some(request)=read_frame(&mut input)? {
        let nonce=string(&request,"nonce").map_err(io::Error::other)?;
        if nonce.len()!=32 || !nonce.bytes().all(|byte|byte.is_ascii_hexdigit()) {return Err(io::Error::other("Invalid worker nonce"));}
        let result=execute(&mut session,&request);
        let (ok,value)=match result {Ok(value)=>(true,value),Err(error)=>(false,Value::String(error.chars().take(4096).collect()))};
        let compact=matches!(request.get("op").and_then(Value::as_str),Some("advance"|"lint"));
        let (state,pending)=match session.as_ref() {
            Some(session)=>(if compact {Value::Null} else {session.snapshot().map_err(|error|io::Error::other(error.to_string()))?},session.network_pending()),
            None=>(Value::Null,None),
        };
        write_frame(&mut output,&json!({"nonce":nonce,"ok":ok,"value":value,"state":state,"pending":pending}))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frames_are_bounded_and_reject_truncation() {
        let mut bytes=Vec::new();
        write_frame(&mut bytes,&json!({"op":"snapshot","nonce":"0123456789abcdef0123456789abcdef"})).unwrap();
        assert_eq!(read_frame(&mut bytes.as_slice()).unwrap().unwrap()["op"],"snapshot");
        assert!(read_frame(&mut (MAX_FRAME as u32+1).to_le_bytes().as_slice()).is_err());
        assert!(read_frame(&mut [4,0,0,0,b'{'].as_slice()).is_err());
        assert!(read_frame(&mut [2,0,0,0,b'{',b'}'].as_slice()).unwrap().is_some());
    }
}
