//! Non-secret audit receipts for an actual owned managed backend. Saved data
//! never authorizes scheduling, process attachment, credentials or a resume.
use anyhow::{ensure,Context,Result};
use rudder_engine::{execution::{ExecutionIdentityV2,ExecutionProfileV4,ExecutionRecord,read_execution_recovery},protocol::{OwnerEpoch,canonical_json}};
use serde_json::Value;
use sha2::{Digest,Sha256};
use std::{fs::{self,File,OpenOptions},io::{Read,Write},os::unix::fs::{MetadataExt,OpenOptionsExt},path::PathBuf};

#[derive(Clone)]
pub(crate) struct ModelSessionPlan {
    selection:Value, root:PathBuf, execution_id:String, operation_id:String,view_id:String,
    owner:OwnerEpoch, digest:String,project_id:String,request_id:String,run_id:String,run_epoch:u64,
}
impl ModelSessionPlan {
    /// OFF App, while the original prepared spec and assignment are retained.
    pub(crate) fn capture(prepared:&crate::managed_launch::PreparedManagedLaunch)->Result<Option<Self>> {
        let spec=prepared.execution_spec()?;
        let Some(selection)=spec.environment.get("RUDDER_MODEL_PROFILE_V1") else{return Ok(None)};
        let selection:Value=serde_json::from_str(selection)?;
        ExecutionProfileV4::validate_selected_model(&selection)?;
        let fence=&prepared.assignment.fence;
        prepared.assignment.validate(fence)?;
        ensure!(spec.execution_id==fence.attempt_id && spec.owner.owner==fence.owner.owner_id && spec.owner.epoch==fence.owner.epoch,
            "selected model differs from actual execution assignment");
        let broker=spec.broker_binding.as_ref().context("selected model execution broker absent")?;
        ensure!(broker.project_id==fence.owner.project_id&&broker.run_id==fence.run_id&&broker.run_epoch==fence.run_epoch,
            "selected model broker differs from actual assignment");
        Ok(Some(Self{selection,root:spec.execution_root.clone(),execution_id:spec.execution_id.clone(),operation_id:spec.operation_id.clone(),
            view_id:spec.view_id.clone(),owner:spec.owner.clone(),digest:format!("{:x}",Sha256::digest(canonical_json(spec)?)),
            project_id:broker.project_id.clone(),request_id:fence.request_id.clone(),run_id:broker.run_id.clone(),run_epoch:broker.run_epoch}))
    }
    fn identity(&self)->Result<ExecutionIdentityV2> {
        let actual=read_execution_recovery(&self.root,&self.execution_id)?.context("actual helper execution record absent")?;
        let record=actual.record();
        ensure!(record.execution_id()==self.execution_id&&record.operation_id()==self.operation_id&&record.view_id()==self.view_id
            &&record.owner()==&self.owner&&record.request_digest()==self.digest,"actual helper record differs from selected model execution");
        // Spec3's real Record1 has no project/run fields. Those come from the
        // original prepared assignment, never a UI RunRecord or a saved PID.
        let identity=ExecutionIdentityV2{project_id:self.project_id.clone(),request_id:self.request_id.clone(),run_id:self.run_id.clone(),
            run_epoch:self.run_epoch,execution_id:self.execution_id.clone(),operation_id:self.operation_id.clone(),view_id:self.view_id.clone(),
            owner:self.owner.clone(),request_digest:self.digest.clone(),supervisor_pid:record.supervisor_pid()};
        identity.validate()?;
        if let ExecutionRecord::V2(record)=record {ensure!(record.identity==identity,"actual Record2 model identity differs");}
        Ok(identity)
    }
    fn path(&self)->PathBuf{self.root.join("runs").join(&self.execution_id).join("model-session-v1.json")}
    fn existing(&self)->Result<Option<Value>> {
        let path=self.path();
        let mut file=match OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC|libc::O_NONBLOCK).open(path){
            Ok(file)=>file,Err(e) if e.kind()==std::io::ErrorKind::NotFound=>return Ok(None),Err(e)=>return Err(e.into())};
        let metadata=file.metadata()?;
        ensure!(metadata.is_file()&&metadata.uid()==unsafe{libc::geteuid()}&&metadata.mode()&0o077==0&&metadata.nlink()==1&&metadata.len()<=65536,
            "unsafe model session receipt");
        let mut bytes=Vec::new();Read::by_ref(&mut file).take(65537).read_to_end(&mut bytes)?;ensure!(bytes.len()<=65536,"model receipt exceeds bound");
        Ok(Some(serde_json::from_slice(&bytes)?))
    }
    /// Before reopening the positively identified backend thread, compare any
    /// existing receipt with the actual helper identity and explicit selection.
    pub(crate) fn validate_resume(&self,session:&str)->Result<()> {
        let identity=self.identity()?;
        if let Some(receipt)=self.existing()? {ExecutionProfileV4::validate_model_resume(&receipt,&self.selection,&identity,session)?;}
        Ok(())
    }
    pub(crate) fn validate_installed_version(&self,initialized:&Value)->Result<()> {
        ensure!(initialized.get("userAgent")==self.selection.get("backend_version"),"live backend differs from selected installed model protocol");Ok(())
    }
    pub(crate) fn observe(&self,session:&str,resumed:&Value)->Result<()> {
        ensure!(resumed.pointer("/thread/id").and_then(Value::as_str)==Some(session),"observed model session differs");
        let model=resumed.get("model").and_then(Value::as_str).context("backend did not report its actual model")?;
        let effort=match resumed.get("reasoningEffort") {None|Some(Value::Null)=>None,Some(Value::String(value))=>Some(value.as_str()),_=>anyhow::bail!("backend reported invalid effort")};
        let identity=self.identity()?;
        let receipt=ExecutionProfileV4::bind_model_session(&self.selection,&identity,session,model,effort)?;
        if let Some(existing)=self.existing()? {
            ExecutionProfileV4::validate_model_resume(&existing,&self.selection,&identity,session)?;
            ensure!(existing==receipt,"backend model observation differs from durable session receipt");
            File::open(self.path())?.sync_all()?;File::open(self.path().parent().unwrap())?.sync_all()?;return Ok(());
        }
        let path=self.path();let parent=path.parent().unwrap();
        let temp=parent.join(format!(".model-session-{}",uuid::Uuid::new_v4()));
        let result=(||->Result<()> {
            let mut file=OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC).open(&temp)?;
            file.write_all(&canonical_json(&receipt)?)?;file.sync_all()?;
            match fs::hard_link(&temp,&path){Ok(())=>{},Err(e) if e.kind()==std::io::ErrorKind::AlreadyExists=>{
                ensure!(self.existing()?.as_ref()==Some(&receipt),"concurrent model receipt differs");},Err(e)=>return Err(e.into())};
            fs::remove_file(&temp)?;File::open(parent)?.sync_all()?;Ok(())
        })();let _=fs::remove_file(temp);result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_link::{self,OwnedPeerProcess,AgentLinkLaunch};
    use serde_json::json;
    use std::{os::{fd::AsRawFd,unix::{net::{UnixStream,UnixListener},fs::{DirBuilderExt,PermissionsExt}}},sync::{atomic::AtomicBool,mpsc},time::Duration};

    struct ActualFixturePeer;
    impl OwnedPeerProcess for ActualFixturePeer {
        fn verify_peer(&self,stream:&UnixStream)->Result<()> {
            let mut cred:libc::ucred=unsafe{std::mem::zeroed()};let mut size=std::mem::size_of::<libc::ucred>() as libc::socklen_t;
            ensure!(unsafe{libc::getsockopt(stream.as_raw_fd(),libc::SOL_SOCKET,libc::SO_PEERCRED,&mut cred as *mut _ as *mut _,&mut size)}==0
                &&cred.pid==unsafe{libc::getpid()},"fixture peer is not its actual owned server");Ok(())
        }
        fn check_current(&self)->Result<()>{Ok(())}
    }
    struct Scratch(PathBuf);
    impl Drop for Scratch {fn drop(&mut self){let _=fs::remove_dir_all(&self.0);}}
    fn fixture()->(Scratch,ModelSessionPlan) {
        let base=std::path::Path::new("/workspace/rudder-scratch/n13-model-receipts");fs::create_dir_all(base).unwrap();
        let root=Scratch(base.join(format!("protocol-{}",uuid::Uuid::new_v4())));
        fs::DirBuilder::new().mode(0o700).create(&root.0).unwrap();
        let id=uuid::Uuid::new_v4().to_string();
        let plan=ModelSessionPlan {selection:json!({"format_version":1,"backend":"codex","profile_id":"selected","provider":"openai",
            "credential_kind":"api_key","requested_model":"actual-model","model":"actual-model","requested_effort":null,"effort":"high","backend_version":"codex/0.166.0"}),
            root:root.0.clone(),execution_id:id,operation_id:uuid::Uuid::new_v4().to_string(),view_id:uuid::Uuid::new_v4().to_string(),
            owner:OwnerEpoch{owner:"actual-owner".into(),epoch:7},digest:"a".repeat(64),project_id:"actual-project".into(),request_id:uuid::Uuid::new_v4().to_string(),run_id:"actual-run".into(),run_epoch:3};
        fs::DirBuilder::new().mode(0o700).create(plan.root.join("runs")).unwrap();
        fs::DirBuilder::new().mode(0o700).create(plan.root.join("runs").join(&plan.execution_id)).unwrap();
        // A fixture of the real helper envelope, with its actual process PID.
        let record=json!({"format_version":1,"execution_id":plan.execution_id,"operation_id":plan.operation_id,"view_id":plan.view_id,
            "owner":plan.owner,"request_digest":plan.digest,"phase":"running","supervisor_pid":std::process::id(),"agent_exit":null,"checkpoint":null});
        let envelope=json!({"checksum":format!("{:x}",Sha256::digest(canonical_json(&record).unwrap())),"record":record});
        let path=plan.root.join("runs").join(&plan.execution_id).join("record.json");
        fs::write(&path,canonical_json(&envelope).unwrap()).unwrap();fs::set_permissions(path,fs::Permissions::from_mode(0o600)).unwrap();
        (root,plan)
    }
    fn protocol(plan:ModelSessionPlan,model:&str)->(String,Vec<String>) {
        let path=plan.root.join("cx.sock");let _=fs::remove_file(&path);let listener=UnixListener::bind(&path).unwrap();
        let model=model.to_owned();
        let server=std::thread::spawn(move|| {
            let (stream,_)=listener.accept().unwrap();stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
            let mut socket=tungstenite::accept(stream).unwrap();let mut methods=Vec::new();
            while let Ok(frame)=socket.read() {
                let Ok(text)=frame.to_text() else {continue};let request:Value=serde_json::from_str(text).unwrap();
                let Some(method)=request["method"].as_str() else {continue};methods.push(method.to_owned());
                if request.get("id").is_none(){continue;}
                let thread=json!({"id":"backend-session","cwd":"/workspace","ephemeral":false,"source":"cli","turns":[{"id":"turn","status":"completed"}]});
                let result=match method {
                    "initialize"=>json!({"userAgent":"codex/0.166.0"}),
                    "thread/loaded/list"=>json!({"data":["backend-session"],"nextCursor":null}),
                    "thread/read"=>json!({"thread":thread}),
                    "thread/resume"=>json!({"thread":thread,"model":model,"reasoningEffort":"high"}),
                    _=>panic!("unexpected protocol method {method}"),
                };
                socket.send(tungstenite::Message::Text(json!({"id":request["id"],"result":result}).to_string().into())).unwrap();
                if method=="thread/resume"{let _=socket.close(None);break;}
            }methods
        });
        let launch=AgentLinkLaunch{format_version:1,run_id:plan.run_id.clone(),attempt_id:plan.execution_id.clone(),backend:"codex".into(),socket_path:path,log_path:plan.root.join("backend.log")};
        let (events,_)=mpsc::channel();let (_commands,commands)=mpsc::sync_channel(1);
        let result=agent_link::codex_loop(&launch,std::path::Path::new("/workspace"),Some("backend-session"),&events,&commands,&AtomicBool::new(false),Some(&ActualFixturePeer),Some(&plan));
        (result.err().map(|e|e.to_string()).unwrap_or_default(),server.join().unwrap())
    }
    #[test]
    fn managed_model_protocol_persists_actual_identity_and_rejects_resume_drift() {
        let (_root,plan)=fixture();
        let (_,methods)=protocol(plan.clone(),"actual-model");assert!(methods.iter().any(|m|m=="thread/resume"));
        let bytes=fs::read(plan.path()).unwrap();let receipt:Value=serde_json::from_slice(&bytes).unwrap();
        assert_eq!(receipt["execution"]["supervisor_pid"],std::process::id());assert_eq!(receipt["execution"]["run_epoch"],3);
        assert_eq!(receipt["selection"]["effort"],"high");assert_eq!(receipt["observed_model"],"actual-model");
        protocol(plan.clone(),"actual-model");assert_eq!(fs::read(plan.path()).unwrap(),bytes);
        let (error,_)=protocol(plan.clone(),"fallback-model");assert!(error.contains("backend model differs"),"{error}");
        let mut changed=plan.clone();changed.selection["profile_id"]=json!("another-profile");
        let (error,methods)=protocol(changed,"actual-model");assert!(error.contains("changed"),"{error}");
        assert!(!methods.iter().any(|m|m=="thread/resume"));assert_eq!(fs::read(plan.path()).unwrap(),bytes);
        let mut changed=plan.clone();changed.run_epoch+=1;assert!(changed.validate_resume("backend-session").is_err());
        assert!(plan.validate_resume("different-session").is_err());
    }
}
