use super::*;
use std::{fs,os::unix::net::UnixListener,sync::mpsc};
struct Process;
impl OwnedPeerProcess for Process {
    fn check_current(&self)->anyhow::Result<()> {Ok(())}
    fn verify_peer(&self,stream:&UnixStream)->anyhow::Result<()> {
        let mut peer:libc::ucred=unsafe{mem::zeroed()};let mut size=mem::size_of_val(&peer)as libc::socklen_t;
        anyhow::ensure!(unsafe{libc::getsockopt(stream.as_raw_fd(),libc::SOL_SOCKET,libc::SO_PEERCRED,(&mut peer as *mut libc::ucred).cast(),&mut size)}==0
            && peer.pid==std::process::id()as i32 && peer.uid==unsafe{libc::geteuid()},"fixture peer differs");Ok(())
    }
}
fn b64(bytes:&[u8])->String {const T:&[u8]=b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";let mut out=String::new();let(mut bits,mut count)=(0u32,0u32);
    for b in bytes {bits=(bits<<8)|*b as u32;count+=8;while count>=6 {count-=6;out.push(T[((bits>>count)&63)as usize]as char);}}if count>0{out.push(T[((bits<<(6-count))&63)as usize]as char);}out}
fn snapshot(exp:u64,now:u64)->CodexOAuthAccessSnapshotV1 {
    let jwt=format!("e30.{}.c2ln",b64(json!({"exp":exp,"https://api.openai.com/auth":{"chatgpt_account_id":"fixture-account","chatgpt_plan_type":"plus"}}).to_string().as_bytes()));
    let bytes=json!({"formatVersion":1,"kind":"CodexOAuthAccessSnapshotV1","profileId":"fixture","backend":"codex","provider":"openai-codex", "accessToken":jwt,
        "chatgptAccountId":"fixture-account","chatgptPlanType":"plus","expiresAtMs":exp*1000}).to_string().into_bytes();
    let mut frame=(bytes.len()as u32).to_be_bytes().to_vec();frame.extend(bytes);CodexOAuthAccessSnapshotV1::from_frame(&frame,"fixture",now).unwrap()
}
#[test]
fn access_snapshot_retains_original_socket_and_denies_server_refresh_without_new_login() {
    let root=std::env::temp_dir().join(format!("rudder-snapshot-{}",uuid::Uuid::new_v4()));fs::create_dir(&root).unwrap();
    let path=root.join("socket");let listener=UnixListener::bind(&path).unwrap();let (tx,rx)=mpsc::channel();
    let original=std::thread::spawn(move|| {
        let(stream,_)=listener.accept().unwrap();stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();let mut socket=tungstenite::accept(stream).unwrap();
        let mut methods=Vec::new();
        loop {
            let Message::Text(text)=socket.read().unwrap() else{panic!("fixture expected JSON");};let msg:Value=serde_json::from_str(&text).unwrap();
            let method=msg["method"].as_str().unwrap();methods.push(method.to_owned());let Some(id)=msg.get("id")else{continue;};
            let result=match method {
                "initialize"=>json!({"userAgent":"rudder_agent_link/0.166.0"}),
                "config/read"=>json!({"config":{"cli_auth_credentials_store":"ephemeral"}}),
                "account/login/start"=>{assert_eq!(msg["params"]["type"],"chatgptAuthTokens");assert_eq!(msg["params"]["chatgptAccountId"],"fixture-account");
                    assert_eq!(msg["params"]["chatgptPlanType"],"plus");assert_eq!(msg["params"].as_object().unwrap().len(),4);assert!(msg["params"]["accessToken"].as_str().unwrap().starts_with("e30."));json!({"type":"chatgptAuthTokens"})},
                "account/read"=>{assert_eq!(msg["params"],json!({"refreshToken":false}));json!({"account":{"type":"chatgpt","email":null,"planType":"plus"},"requiresOpenaiAuth":true})},
                _=>panic!("non-auth method"),
            };
            socket.send(Message::Text(json!({"id":id,"result":result}).to_string())).unwrap();
            if method=="account/read" {break;}
        }
        socket.send(Message::Text(json!({"jsonrpc":"2.0","id":"original-refresh","method":"account/chatgptAuthTokens/refresh","params":{"reason":"unauthorized","previousAccountId":"fixture-account"}}).to_string())).unwrap();
        let Message::Text(reply)=socket.read().unwrap()else{panic!("fixed denial missing")};
        tx.send((methods,serde_json::from_str::<Value>(&reply).unwrap())).unwrap();
    });
    let now=now_ms().unwrap();let selected=snapshot(now/1000+3600,now);let cancel=AtomicBool::new(false);let process=Process;
    let mut socket=connect(&path,&process,&cancel,Instant::now()+Duration::from_secs(2)).unwrap();
    bootstrap_snapshot(&mut socket,&process,&selected,&cancel,Instant::now()+Duration::from_secs(2)).unwrap();
    let stop=AtomicBool::new(false);let deadline=Instant::now()+Duration::from_secs(2);
    while !stop.load(Ordering::Acquire) {assert!(Instant::now()<deadline);poll_snapshot_owned(&mut socket,&process,selected.expires_at_ms(),&stop);std::thread::sleep(Duration::from_millis(2));}
    poll_snapshot_owned(&mut socket,&process,selected.expires_at_ms(),&stop);assert!(stop.load(Ordering::Acquire),"refusal is absorbing without replay");
    let(methods,reply)=rx.recv_timeout(Duration::from_secs(2)).unwrap();original.join().unwrap();
    assert_eq!(methods,["initialize","initialized","config/read","account/login/start","account/read"]);
    assert_eq!(reply,json!({"jsonrpc":"2.0","id":"original-refresh","error":{"code":-32000,"message":"managed OAuth access snapshot renewal required"}}));
    assert!(reply.get("result").is_none());drop(socket);fs::remove_dir_all(root).unwrap();
}
#[test]
fn refresh_denial_keeps_only_original_id_and_static_error_no_success() {
    for id in [json!(0),json!(-1),json!("same-id")] {
        let message=json!({"jsonrpc":"2.0","method":"account/chatgptAuthTokens/refresh","id":id,"params":{"reason":"unauthorized","accessToken":"secret-not-relayed"}});
        let denial=renewal_denial(&message).unwrap();assert_eq!(denial["id"],id);assert_eq!(denial.as_object().unwrap().len(),3);
        assert!(!denial.to_string().contains("secret-not-relayed"));assert!(denial.get("result").is_none());
    }
    for id in [Value::Null,json!(true),json!(1.5),json!({}),json!("x".repeat(257))] {
        assert_eq!(renewal_denial(&json!({"jsonrpc":"2.0","method":"account/chatgptAuthTokens/refresh","id":id})),Err(ManagedCodexAuthError::Protocol));
    }
}
#[test]
fn expired_snapshot_is_refused_before_any_auth_request() {
    let root=std::env::temp_dir().join(format!("rudder-expired-{}",uuid::Uuid::new_v4()));fs::create_dir(&root).unwrap();let path=root.join("socket");
    let listener=UnixListener::bind(path).unwrap();let worker=std::thread::spawn(move||{let(stream,_)=listener.accept().unwrap();tungstenite::accept(stream).unwrap()});
    let cancel=AtomicBool::new(false);let process=Process;let mut socket=connect(&root.join("socket"),&process,&cancel,Instant::now()+Duration::from_secs(2)).unwrap();let server=worker.join().unwrap();
    assert_eq!(bootstrap_snapshot(&mut socket,&process,&snapshot(1,0),&cancel,Instant::now()+Duration::from_secs(2)),Err(ManagedCodexAuthError::Renewal));
    drop(socket);drop(server);fs::remove_dir_all(root).unwrap();
}

#[test]
fn access_snapshot_experimental_schema_requires_exact_leading_server_0166_version() {
    assert!(snapshot_protocol_supported("first_originator/0.166.0 (Linux) rudder_agent_link/1.0"));
    for version in ["0.165.0","0.167.0","1.166.0","0.166.1","0.166.0-beta","00.166.0"] {
        assert!(!snapshot_protocol_supported(&format!("server/{version} rudder_agent_link/0.166.0")));
    }
    assert!(!snapshot_protocol_supported(" server/0.166.0"));
    assert!(!snapshot_protocol_supported("server/0.165.0 (client/0.166.0)"));
}
