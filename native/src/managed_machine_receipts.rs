//! Machine ingress receipts only. A row is DATA, never an Engine/Root/source
//! owner. Only the actual Root config owner can open this store; every effect
//! requires the non-Serde first-ACK owner retained outside callback unwind.
#![cfg(target_os="linux")]
use anyhow::{ensure,Context,Result};
use fs2::FileExt;
use rudder_engine::{execution::finalization_budget::FinalizationBudget,protocol::canonical_json};
use rudder_native::host_ingress::{HostIngressConfigOwner,HostReceiptStorePolicy,HostCommand,HostOperation};
use rusqlite::{Connection,OpenFlags,OptionalExtension,TransactionBehavior,params};
use serde::{Serialize,Deserialize};
use sha2::{Digest,Sha256};
use std::{fs::{self,File,OpenOptions},os::unix::fs::{MetadataExt,OpenOptionsExt},path::PathBuf,sync::{Arc,atomic::{AtomicBool,Ordering}},time::Duration};
const MAX_COMMANDS:i64=512;
const MAX_BYTES:i64=8*1024*1024;
const RESULT_LIMIT:usize=4096;
const APP_ID:i64=0x52554d49;
const TABLES:[(&str,&str);3]=[
    ("binding","CREATE TABLE binding(singleton INTEGER PRIMARY KEY CHECK(singleton=1),body TEXT NOT NULL)"),
    ("commands","CREATE TABLE commands(command_key TEXT PRIMARY KEY,command_id TEXT NOT NULL UNIQUE,body TEXT NOT NULL,body_hash TEXT NOT NULL,session TEXT NOT NULL,config_hash TEXT NOT NULL,original_constraint TEXT NOT NULL,state TEXT NOT NULL CHECK(state IN ('accepted','started','completed')),result TEXT)"),
    ("effects","CREATE TABLE effects(command_key TEXT PRIMARY KEY REFERENCES commands(command_key),state TEXT NOT NULL CHECK(state IN ('pending','started','completed')))"),
];
#[derive(Serialize,Deserialize,PartialEq,Eq)]
struct Binding {machine:String,store:String,incarnation:String,account:String,home:String}
fn binding(config:&HostIngressConfigOwner)->Result<String> {let d=config.data();Ok(String::from_utf8(canonical_json(&Binding {
    machine:d.machine_id.clone(),store:d.store_id.clone(),incarnation:d.incarnation.clone(),account:d.account_id.clone(),home:d.home_id.clone()})?)?)}
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(rename_all="snake_case")]
pub(crate) enum MachineCommandState {Accepted,Started,Completed}
#[derive(Clone,Serialize,Deserialize)]
pub(crate) struct MachineReceipt {pub command_key:String,pub command_id:String,pub project_alias:String,pub payload_sha256:String,pub state:MachineCommandState,pub result:Option<serde_json::Value>}
/// No constructor/Clone/Serde: never recovered from a receipt or a saved fence.
pub(crate) struct AcceptedHostCommand {origin:Arc<Origin>,pub(crate) receipt:MachineReceipt,phase:Phase,admission:FinalizationBudget,work:Option<(FinalizationBudget,Arc<AtomicBool>)>,completion:Option<serde_json::Value>}
enum Phase {InsertAttempted,AcceptedAck,AcceptedReady,WorkReserved,StartAttempted,StartedAck,StartedReady,Dispatched,FinishAttempted,FinishedAck}
pub(crate) enum DuplicateReceipt {Missing,Same(MachineReceipt),Conflict}
struct Origin {nonce:uuid::Uuid}
/// Connection closes BEFORE its fresh-created raw file, directory and actual
/// config owner. Existing databases are NEVER independently raw-opened/closed.
pub(crate) struct MachineReceipts {conn:Option<Connection>,owner_lock:Option<File>,fresh_file:Option<File>,directory:Option<File>,
    config:Arc<HostIngressConfigOwner>,path:PathBuf,directory_id:Option<(u64,u64)>,database_id:Option<(u64,u64)>,
    origin:Arc<Origin>,attempted:bool,ready:bool}
impl MachineReceipts {
    pub(crate) fn open_retaining(out:&mut Option<Self>,config:Arc<HostIngressConfigOwner>,original:&FinalizationBudget)->Result<()> {
        original.remaining()?;config.assert_current()?;ensure!(out.is_none(),"original receipt store already captured");
        let path=config.data().state_directory.join("receipts.sqlite");
        *out=Some(Self {conn:None,owner_lock:None,fresh_file:None,directory:None,config,path,directory_id:None,database_id:None,
            origin:Arc::new(Origin {nonce:uuid::Uuid::new_v4()}),attempted:false,ready:false});
        out.as_mut().unwrap().open_inner(original)
    }
    fn open_inner(&mut self,original:&FinalizationBudget)->Result<()> {
        ensure!(!self.attempted,"receipt store open already attempted; retain original state");self.attempted=true;
        self.config.assert_current()?;original.remaining()?;
        let directory=OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_CLOEXEC|libc::O_NOFOLLOW).open(&self.config.data().state_directory)?;
        let m=directory.metadata()?;ensure!(m.is_dir()&&m.uid()==1500&&m.gid()==1500&&m.mode()&0o7777==0o700,"receipt state directory must be original UID1500 private directory");
        self.directory_id=Some((m.dev(),m.ino()));self.directory=Some(directory);self.check_directory()?;
        if matches!(self.config.data().store_policy,HostReceiptStorePolicy::InitializeEmptyV1) {
            let shared=self.config.data().state_directory.join("cas");
            let expected_shared=self.config.data().projects.iter().all(|p|p.cas==shared);
            for entry in fs::read_dir(&self.config.data().state_directory)? {let entry=entry?;
                ensure!(expected_shared&&entry.file_name()==std::ffi::OsStr::new("cas"),"initial receipt directory has unknown prior data; explicit recovery required");
                let m=fs::symlink_metadata(entry.path())?;ensure!(m.is_dir()&&!m.file_type().is_symlink()&&m.uid()==1500&&m.gid()==1500&&m.mode()&0o7777==0o700,"declared new shared CAS directory changed");
            }
        }
        self.owner_lock=Some(OpenOptions::new().read(true).write(true).create(true).mode(0o600).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW)
            .open(self.config.data().state_directory.join("receipts-owner.lock"))?);
        private_file(&self.owner_lock.as_ref().unwrap().metadata()?)?;self.owner_lock.as_ref().unwrap().try_lock_exclusive()?;
        match &self.config.data().store_policy {
            HostReceiptStorePolicy::InitializeEmptyV1=>{
                // Fresh inode only, held until AFTER SQLite closes. Never open
                // another raw descriptor to an existing live SQLite inode.
                self.fresh_file=Some(OpenOptions::new().read(true).write(true).create_new(true).mode(0o600)
                    .custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW).open(&self.path)?);
            }
            HostReceiptStorePolicy::OpenExistingV1=>{},
        }
        check_sidecars(&self.path)?;
        let m=fs::symlink_metadata(&self.path)?;private_file(&m)?;self.database_id=Some((m.dev(),m.ino()));
        ensure!(rusqlite::version_number()>=3_051_003,"receipt SQLite WAL version unsupported");
        let conn=Connection::open_with_flags(&self.path,OpenFlags::SQLITE_OPEN_READ_WRITE|OpenFlags::SQLITE_OPEN_NOFOLLOW
            |OpenFlags::SQLITE_OPEN_NO_MUTEX|OpenFlags::SQLITE_OPEN_PRIVATE_CACHE)?;
        self.conn=Some(conn);let conn=self.conn.as_mut().unwrap();conn.busy_timeout(Duration::from_millis(100))?;
        conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;")?;
        if matches!(self.config.data().store_policy,HostReceiptStorePolicy::InitializeEmptyV1) {
            conn.execute_batch("PRAGMA journal_mode=WAL;")?;
            let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            for (_,sql) in TABLES {tx.execute_batch(sql)?;}
            tx.pragma_update(None,"application_id",APP_ID)?;tx.pragma_update(None,"user_version",1)?;
            tx.execute("INSERT INTO binding(singleton,body) VALUES(1,?1)",[binding(&self.config)?])?;tx.commit()?;
            self.fresh_file.as_ref().unwrap().sync_all()?;self.directory.as_ref().unwrap().sync_all()?;
        }
        self.validate()?;original.remaining()?;self.ready=true;Ok(())
    }
    fn check_directory(&self)->Result<()> {
        self.config.assert_current()?;ensure!(self.config.data().state_directory.canonicalize()?==self.config.data().state_directory,"receipt state path redirects");
        if let Some(lock)=&self.owner_lock {let held=lock.metadata()?;let named=fs::symlink_metadata(self.config.data().state_directory.join("receipts-owner.lock"))?;
            private_file(&held)?;private_file(&named)?;ensure!((held.dev(),held.ino())==(named.dev(),named.ino()),"original receipt owner lock replaced");}
        for m in [self.directory.as_ref().context("actual receipt directory absent")?.metadata()?,fs::symlink_metadata(&self.config.data().state_directory)?] {
            ensure!(m.is_dir()&&!m.file_type().is_symlink()&&m.uid()==1500&&m.gid()==1500&&m.mode()&0o7777==0o700
                &&Some((m.dev(),m.ino()))==self.directory_id,"original receipt directory replaced");
        }Ok(())
    }
    fn current(&self)->Result<()> {
        self.check_directory()?;check_sidecars(&self.path)?;let m=fs::symlink_metadata(&self.path)?;private_file(&m)?;
        ensure!(Some((m.dev(),m.ino()))==self.database_id,"original receipt database replaced");
        let mut moved:libc::c_int=-1;
        let code=unsafe {rusqlite::ffi::sqlite3_file_control(self.conn.as_ref().context("actual receipt connection absent")?.handle(),
            c"main".as_ptr(),rusqlite::ffi::SQLITE_FCNTL_HAS_MOVED,(&mut moved as *mut libc::c_int).cast())};
        ensure!(code==rusqlite::ffi::SQLITE_OK&&moved==0,"actual receipt SQLite file moved/unknown");self.config.assert_current()
    }
    fn validate(&self)->Result<()> {
        self.current()?;let conn=self.conn.as_ref().unwrap();
        let id:i64=conn.pragma_query_value(None,"application_id",|r|r.get(0))?;let version:i64=conn.pragma_query_value(None,"user_version",|r|r.get(0))?;
        let mode:String=conn.pragma_query_value(None,"journal_mode",|r|r.get(0))?;let sync:i64=conn.pragma_query_value(None,"synchronous",|r|r.get(0))?;
        let foreign:i64=conn.pragma_query_value(None,"foreign_keys",|r|r.get(0))?;
        ensure!(id==APP_ID&&version==1&&mode=="wal"&&sync==2&&foreign==1,"receipt existing format/WAL/FULL/foreign-keys mismatch");
        let count:i64=conn.query_row("SELECT count(*) FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%'",[],|r|r.get(0))?;ensure!(count==3,"receipt schema has unknown tables/triggers/views/indexes");
        for (name,expected) in TABLES {let actual:String=conn.query_row("SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",[name],|r|r.get(0))?;ensure!(actual==expected,"receipt schema differs");}
        let actual:String=conn.query_row("SELECT body FROM binding WHERE singleton=1",[],|r|r.get(0))?;ensure!(actual==binding(&self.config)?,"receipt machine/account/home/incarnation binding differs; explicit recovery required");Ok(())
    }
    pub(crate) fn duplicate_receipt(&self,command:&HostCommand,original:&FinalizationBudget)->Result<DuplicateReceipt> {
        self.current()?;command.validate()?;original.remaining()?;
        let body=String::from_utf8(canonical_json(&command.operation)?)?;let hash=format!("{:x}",Sha256::digest(body.as_bytes()));
        let row=lookup(self.conn.as_ref().unwrap(),&command.command_key)?;
        let value=if let Some((receipt,old,digest))=row {if old==body&&digest==hash {DuplicateReceipt::Same(receipt)}else{DuplicateReceipt::Conflict}}else{DuplicateReceipt::Missing};
        self.current()?;original.remaining()?;Ok(value)
    }
    /// First ACK owner is staged BEFORE insert. Duplicate returns original DATA
    /// only, including across session changes. Neither readback nor a retry can
    /// manufacture the non-Serde accepted/started permit.
    pub(crate) fn accept_retaining(&mut self,out:&mut Option<AcceptedHostCommand>,command:&HostCommand,original:&FinalizationBudget)->Result<MachineReceipt> {
        ensure!(self.ready&&out.is_none(),"receipt store/owner not fresh");self.current()?;original.remaining()?;command.validate()?;
        ensure!(!matches!(command.operation,HostOperation::ReceiptV1 {..}),"receipt queries cannot start effects");
        let body=String::from_utf8(canonical_json(&command.operation)?)?;let digest=format!("{:x}",Sha256::digest(body.as_bytes()));
        if let Some((receipt,old_body,old_digest))=lookup(self.conn.as_ref().unwrap(),&command.command_key)? {
            ensure!(old_body==body&&old_digest==digest,"stable command key conflicts with original payload");return Ok(receipt);
        }
        let receipt=MachineReceipt {command_key:command.command_key.clone(),command_id:uuid::Uuid::new_v4().to_string(),project_alias:operation_alias(&command.operation)?.to_owned(),payload_sha256:digest.clone(),state:MachineCommandState::Accepted,result:None};
        *out=Some(AcceptedHostCommand {origin:self.origin.clone(),receipt:receipt.clone(),phase:Phase::InsertAttempted,admission:original.clone(),work:None,completion:None});
        insert_accepted(self.conn.as_mut().unwrap(),command,&receipt,&body,&digest,self.config.config_sha256())?;
        out.as_mut().unwrap().phase=Phase::AcceptedAck; // BEFORE postchecks
        self.current()?;original.remaining()?;out.as_mut().unwrap().phase=Phase::AcceptedReady;Ok(receipt)
    }
    /// One actual preparation lifetime, captured BEFORE queue/work, separate
    /// from the transport admission deadline. No duplicate can mint another.
    pub(crate) fn reserve_work_retaining(&self,owner:&mut AcceptedHostCommand)->Result<(FinalizationBudget,Arc<AtomicBool>)> {
        ensure!(Arc::ptr_eq(&self.origin,&owner.origin)&&matches!(owner.phase,Phase::AcceptedReady),"original qualified acceptance absent/already reserved");
        self.current()?;owner.admission.remaining()?;let original=FinalizationBudget::begin(Duration::from_secs(30))?;
        let cancelled=Arc::new(AtomicBool::new(false));owner.work=Some((original.clone(),cancelled.clone()));owner.phase=Phase::WorkReserved;owner.admission.remaining()?;Ok((original,cancelled))
    }
    pub(crate) fn start_retaining(&mut self,owner:&mut AcceptedHostCommand)->Result<()> {
        ensure!(Arc::ptr_eq(&self.origin,&owner.origin)&&matches!(owner.phase,Phase::WorkReserved),"original acceptance ACK absent/foreign/already attempted");
        self.current()?;work_current(owner)?;owner.phase=Phase::StartAttempted;
        start_original(self.conn.as_mut().unwrap(),&owner.receipt)?;
        owner.phase=Phase::StartedAck;owner.receipt.state=MachineCommandState::Started;self.current()?;work_current(owner)?;owner.phase=Phase::StartedReady;Ok(())
    }
    pub(crate) fn claim_dispatch(&self,owner:&mut AcceptedHostCommand)->Result<()> {
        self.current()?;work_current(owner)?;ensure!(Arc::ptr_eq(&self.origin,&owner.origin)&&matches!(owner.phase,Phase::StartedReady),"actual original started ACK unavailable");
        owner.phase=Phase::Dispatched;Ok(())
    }
    pub(crate) fn finish_retaining(&mut self,owner:&mut AcceptedHostCommand,result:&serde_json::Value)->Result<()> {
        ensure!(Arc::ptr_eq(&self.origin,&owner.origin)&&matches!(owner.phase,Phase::Dispatched),"actual dispatched original owner unavailable");
        let bytes=canonical_json(result)?;ensure!(bytes.len()<=RESULT_LIMIT,"receipt result exceeds bound");self.current()?;work_current(owner)?;owner.completion=Some(result.clone());owner.phase=Phase::FinishAttempted;
        finish_original(self.conn.as_mut().unwrap(),&owner.receipt,String::from_utf8(bytes)?)?;
        owner.receipt.result=Some(result.clone());owner.receipt.state=MachineCommandState::Completed;owner.phase=Phase::FinishedAck;
        self.current()?;work_current(owner)?;Ok(())
    }
    /// Explicit same-process completion reconciliation ONLY. This borrows the
    /// original staged result and original work lifetime. It cannot acquire,
    /// dispatch, create another budget, or promote a saved row into a permit.
    pub(crate) fn reconcile_completion_retaining(&mut self,owner:&mut AcceptedHostCommand)->Result<()> {
        ensure!(Arc::ptr_eq(&self.origin,&owner.origin)&&matches!(owner.phase,Phase::FinishAttempted|Phase::FinishedAck),"original completion intent absent");
        self.current()?;work_current(owner)?;
        let result=owner.completion.as_ref().context("original completion result absent")?;let encoded=String::from_utf8(canonical_json(result)?)?;
        let tx=self.conn.as_mut().unwrap().transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row=lookup(&tx,&owner.receipt.command_key)?.context("original started receipt missing")?;
        ensure!(row.0.command_id==owner.receipt.command_id&&row.0.payload_sha256==owner.receipt.payload_sha256,"original completion identity changed");
        match row.0.state {
            MachineCommandState::Started=>{
                ensure!(tx.execute("UPDATE effects SET state='completed' WHERE command_key=?1 AND state='started'",[&owner.receipt.command_key])?==1,"original effect changed");
                ensure!(tx.execute("UPDATE commands SET state='completed',result=?1 WHERE command_key=?2 AND command_id=?3 AND state='started'",params![encoded,owner.receipt.command_key,owner.receipt.command_id])?==1,"original command changed");
            },MachineCommandState::Completed=>ensure!(row.0.result.as_ref()==Some(result),"original completion differs"),
            _=>anyhow::bail!("original started intent absent"),
        }
        tx.commit()?;owner.receipt.result=owner.completion.clone();owner.receipt.state=MachineCommandState::Completed;owner.phase=Phase::FinishedAck;
        self.current()?;work_current(owner)?;Ok(())
    }
    pub(crate) fn receipt(&self,key:&str,original:&FinalizationBudget)->Result<Option<MachineReceipt>> {
        self.current()?;original.remaining()?;let row=lookup(self.conn.as_ref().unwrap(),key)?.map(|(row,_,_)|row);self.current()?;original.remaining()?;Ok(row)
    }
}
fn work_current(owner:&AcceptedHostCommand)->Result<()> {
    let (original,cancelled)=owner.work.as_ref().context("original work lifetime absent")?;
    if cancelled.load(Ordering::Acquire){original.cancel();anyhow::bail!("original preparation cancelled");}original.remaining()?;Ok(())
}
fn insert_accepted(conn:&mut Connection,command:&HostCommand,receipt:&MachineReceipt,body:&str,digest:&str,config_hash:&str)->Result<()> {
    let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (count,bytes):(i64,i64)=tx.query_row("SELECT count(*),coalesce(sum(length(cast(body AS BLOB))),0) FROM commands",[],|r|Ok((r.get(0)?,r.get(1)?)))?;
    ensure!(count<MAX_COMMANDS&&bytes+body.len() as i64<=MAX_BYTES,"machine receipt capacity full; originals retained");
    tx.execute("INSERT INTO commands(command_key,command_id,body,body_hash,session,config_hash,original_constraint,state) VALUES(?1,?2,?3,?4,?5,?6,?7,'accepted')",
        params![receipt.command_key,receipt.command_id,body,digest,command.session_generation,config_hash,String::from_utf8(canonical_json(&command.constraint)?)?])?;
    tx.execute("INSERT INTO effects(command_key,state) VALUES(?1,'pending')",[&receipt.command_key])?;tx.commit()?;Ok(())
}
fn start_original(conn:&mut Connection,receipt:&MachineReceipt)->Result<()> {
    let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    ensure!(tx.execute("UPDATE effects SET state='started' WHERE command_key=?1 AND state='pending'",[&receipt.command_key])?==1,"original pending effect changed");
    ensure!(tx.execute("UPDATE commands SET state='started' WHERE command_key=?1 AND command_id=?2 AND state='accepted'",params![receipt.command_key,receipt.command_id])?==1,"original accepted command changed");tx.commit()?;Ok(())
}
fn finish_original(conn:&mut Connection,receipt:&MachineReceipt,encoded:String)->Result<()> {
    let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    ensure!(tx.execute("UPDATE effects SET state='completed' WHERE command_key=?1 AND state='started'",[&receipt.command_key])?==1,"original started effect differs");
    ensure!(tx.execute("UPDATE commands SET state='completed',result=?1 WHERE command_key=?2 AND command_id=?3 AND state='started'",params![encoded,receipt.command_key,receipt.command_id])?==1,"original started receipt differs");tx.commit()?;Ok(())
}
fn private_file(m:&fs::Metadata)->Result<()> {ensure!(m.is_file()&&!m.file_type().is_symlink()&&m.uid()==1500&&m.gid()==1500&&m.nlink()==1&&m.mode()&0o7777==0o600&&m.len()<=64*1024*1024,"receipt database must be original private UID1500 file");Ok(())}
fn lookup(conn:&Connection,key:&str)->Result<Option<(MachineReceipt,String,String)>> {
    let row:Option<(String,String,String,String,Option<String>)>=conn.query_row("SELECT command_id,body,body_hash,state,result FROM commands WHERE command_key=?1",[key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
    row.map(|(id,body,hash,state,result)|->Result<_> {
        ensure!(body.len()<=32*1024&&result.as_ref().is_none_or(|v|v.len()<=RESULT_LIMIT),"stored receipt exceeds bound");
        ensure!(uuid::Uuid::parse_str(&id).is_ok_and(|v|v.get_version_num()==4&&v.to_string()==id),"stored receipt ID invalid");
        let operation:HostOperation=serde_json::from_str(&body)?;
        ensure!(canonical_json(&operation)?==body.as_bytes()&&format!("{:x}",Sha256::digest(body.as_bytes()))==hash,"stored receipt body/hash corrupt");
        let state=match state.as_str(){"accepted"=>MachineCommandState::Accepted,"started"=>MachineCommandState::Started,"completed"=>MachineCommandState::Completed,_=>anyhow::bail!("receipt state corrupt")};
        ensure!((state==MachineCommandState::Completed)==result.is_some(),"stored receipt result/state corrupt");
        Ok((MachineReceipt {command_key:key.into(),command_id:id,project_alias:operation_alias(&operation)?.to_owned(),payload_sha256:hash.clone(),state,result:result.map(|v|serde_json::from_str(&v)).transpose()?},body,hash))
    }).transpose()
}

fn operation_alias(operation:&HostOperation)->Result<&str> {match operation {HostOperation::OpenPreparedProjectV1 {project_alias,..}|HostOperation::StartTaskV1 {project_alias,..}|HostOperation::StopTaskV1 {project_alias,..}|HostOperation::BuildCurrentSourceSdkV1 {project_alias,..}=>Ok(project_alias),_=>anyhow::bail!("query has no effect project")}}

fn check_sidecars(path:&std::path::Path)->Result<()> {for suffix in ["-wal","-shm"] {let mut name=path.as_os_str().to_os_string();name.push(suffix);
    match fs::symlink_metadata(std::path::Path::new(&name)) {Ok(m)=>private_file(&m)?,Err(e) if e.kind()==std::io::ErrorKind::NotFound=>{},Err(e)=>return Err(e.into())}}Ok(())}

#[cfg(test)]
#[path="managed_machine_receipt_rows_tests.rs"]
mod tests;
