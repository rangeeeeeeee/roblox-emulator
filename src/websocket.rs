//! Approved WebSocket transport in a short-lived host-side process.
//!
//! WinHTTP's synchronous receive cannot be cancelled by closing its handle
//! from another thread. A job-owned broker process lets session shutdown
//! terminate even a peer that never responds to a close frame.
use serde_json::{Value, json};
use std::{io::{self, Read, Write}, sync::mpsc};

pub(super) const MAX_MESSAGE: usize = 256 * 1024;
const MAX_FRAME: usize = 512 * 1024;
pub(super) const MAX_EVENTS: usize = 64;
const MAX_COMMANDS: usize = 32;

fn read_frame(reader:&mut impl Read)->io::Result<Option<Value>> {
    let mut size=[0u8;4];
    match reader.read(&mut size[..1])? {0=>return Ok(None),_=>reader.read_exact(&mut size[1..])?}
    let size=u32::from_le_bytes(size) as usize;
    if size==0 || size>MAX_FRAME{return Err(io::Error::other("Invalid WebSocket broker frame"));}
    let mut bytes=vec![0;size];reader.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes).map(Some).map_err(io::Error::other)
}
fn write_frame(writer:&mut impl Write,value:&Value)->io::Result<()> {
    let bytes=serde_json::to_vec(value).map_err(io::Error::other)?;
    if bytes.is_empty()||bytes.len()>MAX_FRAME{return Err(io::Error::other("WebSocket broker frame exceeds limit"));}
    writer.write_all(&(bytes.len() as u32).to_le_bytes())?;writer.write_all(&bytes)?;writer.flush()
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::{ffi::c_void,ptr,sync::{Arc,Mutex},thread};
    type Handle=*mut c_void;

    unsafe extern "C" {
        fn emu_websocket_job_verify()->u32;
    }
    #[link(name="winhttp")]
    unsafe extern "system" {
        fn WinHttpOpen(agent:*const u16,access:u32,proxy:*const u16,bypass:*const u16,flags:u32)->Handle;
        fn WinHttpConnect(session:Handle,server:*const u16,port:u16,reserved:u32)->Handle;
        fn WinHttpOpenRequest(connection:Handle,method:*const u16,object:*const u16,version:*const u16,referrer:*const u16,accept:*const *const u16,flags:u32)->Handle;
        fn WinHttpSetOption(handle:Handle,option:u32,buffer:*mut c_void,length:u32)->i32;
        fn WinHttpSetTimeouts(handle:Handle,resolve:i32,connect:i32,send:i32,receive:i32)->i32;
        fn WinHttpSendRequest(handle:Handle,headers:*const u16,header_len:u32,body:*mut c_void,body_len:u32,total_len:u32,context:usize)->i32;
        fn WinHttpReceiveResponse(handle:Handle,reserved:*mut c_void)->i32;
        fn WinHttpQueryHeaders(handle:Handle,level:u32,name:*const u16,buffer:*mut c_void,length:*mut u32,index:*mut u32)->i32;
        fn WinHttpWebSocketCompleteUpgrade(request:Handle,context:usize)->Handle;
        fn WinHttpWebSocketSend(socket:Handle,kind:u32,buffer:*mut c_void,length:u32)->u32;
        fn WinHttpWebSocketReceive(socket:Handle,buffer:*mut c_void,length:u32,read:*mut u32,kind:*mut u32)->u32;
        fn WinHttpWebSocketShutdown(socket:Handle,status:u16,reason:*mut c_void,length:u32)->u32;
        fn WinHttpCloseHandle(handle:Handle)->i32;
    }
    struct Internet(Handle);
    impl Internet {
        fn new(value:Handle,what:&str)->Result<Self,String>{if value.is_null(){Err(format!("WebSocket {what}: {}",io::Error::last_os_error()))}else{Ok(Self(value))}}
    }
    // WinHTTP permits one receive and one send/shutdown concurrently.
    unsafe impl Send for Internet {}
    unsafe impl Sync for Internet {}
    impl Drop for Internet {fn drop(&mut self){unsafe{WinHttpCloseHandle(self.0);}}}
    fn wide(s:&str)->Vec<u16>{s.encode_utf16().chain(Some(0)).collect()}
    fn check(value:i32,what:&str)->Result<(),String>{if value==0{Err(format!("WebSocket {what}: {}",io::Error::last_os_error()))}else{Ok(())}}

    pub struct Socket { _process:crate::sandbox_windows::WorkerProcess, commands:mpsc::SyncSender<Value> }
    impl Socket {
        pub fn command(&self,value:Value)->Result<(),String>{self.commands.try_send(value).map_err(|_|"WebSocket send queue is full or closed".into())}
    }
    pub fn spawn(url:&str,id:String,events:mpsc::SyncSender<(String,Value)>)->Result<Socket,String>{
        super::super::parse_websocket_url(url).map_err(|error|error.to_string())?;
        let process=crate::sandbox_windows::spawn_websocket_broker().map_err(|error|format!("WebSocket broker could not start: {error}"))?;
        let mut input=process.stdin.try_clone().map_err(|error|error.to_string())?;
        let mut output=process.stdout.try_clone().map_err(|error|error.to_string())?;
        write_frame(&mut input,&json!({"op":"connect","url":url})).map_err(|error|error.to_string())?;
        let (commands,receiver)=mpsc::sync_channel::<Value>(MAX_COMMANDS);
        let write_events=events.clone();let write_id=id.clone();
        thread::Builder::new().name("emulator-websocket-write".into()).spawn(move||{
            while let Ok(command)=receiver.recv(){if write_frame(&mut input,&command).is_err(){let _=write_events.send((write_id,json!({"event":"close"})));break;}}
        }).map_err(|error|error.to_string())?;
        let read_events=events.clone();let read_id=id.clone();
        thread::Builder::new().name("emulator-websocket-read".into()).spawn(move||{
            loop {
                match read_frame(&mut output) {
                    Ok(Some(event))=>{if read_events.send((read_id.clone(),event)).is_err(){break;}}
                    _=>{let _=read_events.send((read_id,json!({"event":"close"})));break;}
                }
            }
        }).map_err(|error|error.to_string())?;
        Ok(Socket{_process:process,commands})
    }

    fn connect(url:&str)->Result<(Internet,Internet,Internet),String>{
        let (secure,host,port,path)=super::super::parse_websocket_url(url).map_err(|error|error.to_string())?;
        let session=Internet::new(unsafe{WinHttpOpen(ptr::null(),1,ptr::null(),ptr::null(),0)},"initialize")?;
        check(unsafe{WinHttpSetTimeouts(session.0,5000,5000,10000,10000)},"set timeouts")?;
        let mut retries=1u32;check(unsafe{WinHttpSetOption(session.0,4,(&mut retries as *mut u32).cast(),4)},"set retries")?;
        let host=wide(host.trim_start_matches('[').trim_end_matches(']'));
        let connection=Internet::new(unsafe{WinHttpConnect(session.0,host.as_ptr(),port,0)},"connect")?;
        let method=wide("GET");let path=wide(&path);
        let request=Internet::new(unsafe{WinHttpOpenRequest(connection.0,method.as_ptr(),path.as_ptr(),ptr::null(),ptr::null(),ptr::null(),0x40|0x80|if secure{0x00800000}else{0})},"open request")?;
        let mut disabled=0x01u32|0x02|0x04;
        check(unsafe{WinHttpSetOption(request.0,63,(&mut disabled as *mut u32).cast(),4)},"disable redirects, cookies and authentication")?;
        check(unsafe{WinHttpSetOption(request.0,114,ptr::null_mut(),0)},"enable upgrade")?;
        let headers=wide("User-Agent: Roblox/WinInet\r\nAccept: */*\r\n");
        check(unsafe{WinHttpSendRequest(request.0,headers.as_ptr(),u32::MAX,ptr::null_mut(),0,0,0)},"send upgrade")?;
        check(unsafe{WinHttpReceiveResponse(request.0,ptr::null_mut())},"receive upgrade")?;
        let mut status=0u32;let mut size=4u32;
        check(unsafe{WinHttpQueryHeaders(request.0,19|0x20000000,ptr::null(),(&mut status as *mut u32).cast(),&mut size,ptr::null_mut())},"query status")?;
        if status!=101{return Err(format!("WebSocket upgrade returned HTTP {status}"));}
        let socket=Internet::new(unsafe{WinHttpWebSocketCompleteUpgrade(request.0,0)},"complete upgrade")?;
        Ok((session,connection,socket))
    }

    fn emit(output:&Arc<Mutex<io::Stdout>>,event:&Value)->io::Result<()>{write_frame(&mut *output.lock().map_err(|_|io::Error::other("WebSocket output lock failed"))?,event)}
    pub fn broker_main()->Result<(),String>{
        let code=unsafe{emu_websocket_job_verify()};
        if code!=0{return Err(format!("WebSocket broker job rejected: {code}"));}
        let initial={let stdin=io::stdin();let mut input=stdin.lock();
            read_frame(&mut input).map_err(|error|error.to_string())?.ok_or("Missing WebSocket connect request")?};
        if initial.get("op").and_then(Value::as_str)!=Some("connect"){return Err("Invalid WebSocket broker operation".into());}
        let url=initial.get("url").and_then(Value::as_str).ok_or("Missing WebSocket URL")?;
        let output=Arc::new(Mutex::new(io::stdout()));
        let (_session,_connection,socket)=match connect(url) {
            Ok(socket)=>socket,
            Err(error)=>{emit(&output,&json!({"event":"error","error":error})).map_err(|error|error.to_string())?;return Ok(());}
        };
        let socket=Arc::new(socket);
        emit(&output,&json!({"event":"open"})).map_err(|error|error.to_string())?;
        enum Command {Client(Value),PeerClose,ReceiveError,CloseTimeout,Stop}
        let (sender,receiver)=mpsc::sync_channel::<Command>(MAX_COMMANDS);
        let input_sender=sender.clone();
        thread::Builder::new().name("websocket-input".into()).spawn(move||{
            let stdin=io::stdin();let mut input=stdin.lock();
            loop {
                match read_frame(&mut input) {
                    Ok(Some(command))=>{if input_sender.send(Command::Client(command)).is_err(){break;}}
                    _=>{let _=input_sender.send(Command::Stop);break;}
                }
            }
        }).map_err(|error|error.to_string())?;
        let read_socket=socket.clone();let read_output=output.clone();
        let read_sender=sender.clone();
        thread::Builder::new().name("websocket-receive".into()).spawn(move||{
            let mut buf=[0u8;16384];let mut message=Vec::new();
            loop {
                let mut size=0u32;let mut kind=0u32;
                let status=unsafe{WinHttpWebSocketReceive(read_socket.0,buf.as_mut_ptr().cast(),buf.len() as u32,&mut size,&mut kind)};
                if status!=0 {let _=read_sender.send(Command::ReceiveError);break;}
                if kind==4 {let _=read_sender.send(Command::PeerClose);break;}
                if kind>4||message.len().saturating_add(size as usize)>MAX_MESSAGE {let _=read_sender.send(Command::ReceiveError);break;}
                message.extend_from_slice(&buf[..size as usize]);
                if kind==0||kind==2 {
                    if emit(&read_output,&json!({"event":"message","data":base64::encode(&message)})).is_err(){break;}
                    message.clear();
                }
            }
        }).map_err(|error|error.to_string())?;
        let mut closing=false;
        while let Ok(command)=receiver.recv() {
            let command=match command {
                Command::Client(command)=>command,
                Command::PeerClose=>{
                    if !closing {unsafe{WinHttpWebSocketShutdown(socket.0,1000,ptr::null_mut(),0);}}
                    let _=emit(&output,&json!({"event":"close"}));std::process::exit(0);
                },
                Command::ReceiveError|Command::CloseTimeout=>{let _=emit(&output,&json!({"event":"close"}));std::process::exit(0);},
                Command::Stop=>std::process::exit(0),
            };
            match command.get("op").and_then(Value::as_str) {
                Some("send")=>{
                    if closing {continue;}
                    let encoded=command.get("data").and_then(Value::as_str).filter(|value|value.len()<=((MAX_MESSAGE+2)/3)*4).ok_or("Invalid WebSocket message")?;
                    let data=base64::decode(encoded).map_err(|_|"Invalid WebSocket message")?;
                    if data.len()>MAX_MESSAGE{return Err("WebSocket message exceeds limit".into());}
                    let kind=if std::str::from_utf8(&data).is_ok(){2}else{0};
                    let status=unsafe{WinHttpWebSocketSend(socket.0,kind,data.as_ptr() as *mut c_void,data.len() as u32)};
                    if status!=0 {let _=emit(&output,&json!({"event":"close"}));std::process::exit(0);}
                }
                Some("close")=>{
                    if closing {continue;}
                    closing=true;
                    let status=unsafe{WinHttpWebSocketShutdown(socket.0,1000,ptr::null_mut(),0)};
                    if status!=0 {let _=emit(&output,&json!({"event":"close"}));std::process::exit(0);}
                    let timer=sender.clone();
                    thread::Builder::new().name("websocket-close-timeout".into()).spawn(move||{
                        thread::sleep(std::time::Duration::from_secs(5));let _=timer.send(Command::CloseTimeout);
                    }).map_err(|error|error.to_string())?;
                }
                _=>return Err("Invalid WebSocket broker command".into()),
            }
        }
        Ok(())
    }
}

#[cfg(windows)] pub(super) use windows::{Socket,broker_main,spawn};
