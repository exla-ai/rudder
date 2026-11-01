//! Restrictive admission for an ALREADY owned/capability-authenticated Journal.
//! No path, callback result or serialized value grants project/run permission.
use super::Journal;
use anyhow::{ensure,Result};
use rusqlite::{hooks::{AuthAction,AuthContext,Authorization},Connection,OpenFlags};
use std::{path::Path,sync::{Arc,Mutex,atomic::{AtomicBool,AtomicU8,Ordering}},time::{Duration,Instant}};

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub enum AgentAdmissionState {Ready,Busy,Fenced}
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub enum AgentAdmissionPoint {Dispatch,SqlStep,SqlWrite,Commit}
/// A host producer may ONLY restrict existing local capability authority.
/// Implementations must be nonblocking, no SQL/RPC/filesystem/network/waits,
/// and retain their actual source/lease producer rather than copied DATA.
pub trait AgentCommitAdmission:Send+Sync {
    fn check(&self,point:AgentAdmissionPoint)->AgentAdmissionState;
}
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub enum AgentJournalUse {SharedTool,RetainedObservation}
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub enum AgentAdmissionError {Busy,Fenced,EffectUnknown}
impl std::fmt::Display for AgentAdmissionError {
    fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result {f.write_str(match self {
        Self::Busy=>"agent shared admission busy",Self::Fenced=>"agent shared admission fenced",
        Self::EffectUnknown=>"agent effect requires retained reconciliation"})}
}
impl std::error::Error for AgentAdmissionError {}
const IDLE:u8=0;const SETUP:u8=1;const SHARED:u8=2;const OBSERVE:u8=3;const ROLLBACK:u8=4;
struct Hooks {
    admission:Arc<dyn AgentCommitAdmission>,mode:AtomicU8,cancel:Mutex<Option<Arc<AtomicBool>>>,
    denied:AtomicU8,commit_began:AtomicBool,unknown:AtomicBool,panicked:AtomicBool,
    admission_panic:Mutex<Option<Box<dyn std::any::Any+Send>>>,work_panic:Mutex<Option<Box<dyn std::any::Any+Send>>>,
}
impl Hooks {
    fn check(&self,point:AgentAdmissionPoint)->AgentAdmissionState {
        if self.unknown.load(Ordering::Acquire){return AgentAdmissionState::Fenced;}
        let cancelled=match self.cancel.try_lock(){Ok(c)=>c.as_ref().is_some_and(|c|c.load(Ordering::Acquire)),Err(_)=>true};
        if cancelled{return AgentAdmissionState::Fenced;}
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(||self.admission.check(point))){Ok(state)=>state,Err(payload)=>{
            self.panicked.store(true,Ordering::Release);self.unknown.store(true,Ordering::Release);
            // Private error-only slots have no external readers/holders. The
            // single actual connection serializes callbacks; retain payload,
            // whose destructor must never run while unwinding through C.
            *self.admission_panic.lock().unwrap_or_else(|p|p.into_inner())=Some(payload);AgentAdmissionState::Fenced}}
    }
    fn allow(&self,point:AgentAdmissionPoint)->bool {
        match self.check(point){AgentAdmissionState::Ready=>true,state=>{
            self.denied.fetch_max(if state==AgentAdmissionState::Busy{1}else{2},Ordering::AcqRel);false}}
    }
    fn progress(&self)->bool {
        match self.mode.load(Ordering::Acquire){SHARED=>!self.allow(AgentAdmissionPoint::SqlStep),
            OBSERVE=>{let cancelled=self.cancel.try_lock().map_or(true,|c|c.as_ref().is_some_and(|c|c.load(Ordering::Acquire)));if cancelled{self.denied.fetch_max(2,Ordering::AcqRel);}cancelled},
            SETUP|ROLLBACK=>false,_=>true}
    }
    fn authorize(&self,ctx:AuthContext<'_>)->Authorization {
        let mode=self.mode.load(Ordering::Acquire);
        match ctx.action {
            AuthAction::Pragma{pragma_name,..} if mode==SETUP&&pragma_name.eq_ignore_ascii_case("query_only")=>Authorization::Allow,
            AuthAction::Transaction{..}|AuthAction::Savepoint{..}=>Authorization::Allow,
            AuthAction::Read{..}|AuthAction::Select|AuthAction::Function{..}|AuthAction::Recursive
                if mode==SHARED||mode==OBSERVE=>Authorization::Allow,
            AuthAction::Insert{..}|AuthAction::Delete{..}|AuthAction::Update{..} if mode==SHARED=>{
                if self.allow(AgentAdmissionPoint::SqlWrite){Authorization::Allow}else{Authorization::Deny}}
            _=>Authorization::Deny,
        }
    }
    fn commit(&self)->bool {
        if self.mode.load(Ordering::Acquire)!=SHARED||!self.allow(AgentAdmissionPoint::Commit){return true;}
        // This is admission immediately before SQLite commit, NOT a durability
        // receipt. Any later error/expiry retains an uncertain original call.
        self.commit_began.store(true,Ordering::Release);false
    }
}

/// Owns the actual connection. Safe rusqlite closures own their Arc userdata
/// until this connection actually closes; hooks are never replaced/uninstalled.
/// Connection drops before the final hook/source owner. No Serde/Clone.
pub struct GuardedAgentJournal {journal:Journal,hooks:Arc<Hooks>,attempted:bool,installed:bool}
impl GuardedAgentJournal {
    pub fn stage_retaining(out:&mut Option<Self>,journal:&mut Option<Journal>,admission:Arc<dyn AgentCommitAdmission>)->Result<()> {
        ensure!(out.is_none()&&journal.is_some(),"agent connection output/source occupied");
        *out=Some(Self{journal:journal.take().unwrap(),hooks:Arc::new(Hooks{admission,mode:AtomicU8::new(IDLE),cancel:Mutex::new(None),
            denied:AtomicU8::new(0),commit_began:AtomicBool::new(false),unknown:AtomicBool::new(false),panicked:AtomicBool::new(false),admission_panic:Mutex::new(None),work_panic:Mutex::new(None)}),attempted:false,installed:false});Ok(())
    }
    pub fn install(&mut self)->Result<()> {
        ensure!(!self.attempted,"agent connection hook installation already attempted");self.attempted=true;
        // No SQLite lock wait can outrun the subsequent write/commit checks.
        self.journal.conn.busy_timeout(Duration::ZERO)?;
        let h=self.hooks.clone();self.journal.conn.authorizer(Some(move |ctx:AuthContext<'_>|h.authorize(ctx)))?;
        let h=self.hooks.clone();self.journal.conn.progress_handler(1,Some(move ||h.progress()))?;
        let h=self.hooks.clone();self.journal.conn.commit_hook(Some(move ||h.commit()))?;
        self.installed=true;Ok(())
    }
    pub fn database_path(&self)->Result<&Path>{self.journal.database_path()}
    pub fn has_unknown(&self)->bool{self.hooks.unknown.load(Ordering::Acquire)}
    /// Restriction only: retained host postchecks may close this connection.
    pub fn retain_unknown(&self){self.hooks.unknown.store(true,Ordering::Release);}
    /// Additional host postcheck only; cannot bypass the mandatory SQL hooks.
    pub fn post_admission(&self,state:AgentAdmissionState)->Result<()> {
        if self.has_unknown(){return Err(AgentAdmissionError::EffectUnknown.into());}
        if state==AgentAdmissionState::Ready{return Ok(());}
        if self.hooks.commit_began.load(Ordering::Acquire){self.retain_unknown();return Err(AgentAdmissionError::EffectUnknown.into());}
        Err(if state==AgentAdmissionState::Busy{AgentAdmissionError::Busy}else{AgentAdmissionError::Fenced}.into())
    }
    pub fn run<T>(&mut self,usage:AgentJournalUse,cancel:Arc<AtomicBool>,work:impl FnOnce(&mut Journal)->Result<T>)->Result<T> {
        ensure!(self.installed,"agent connection hooks incomplete; retain original owner");
        ensure!(self.journal.conn.is_autocommit(),"agent connection has an unsettled transaction");
        ensure!(self.hooks.mode.load(Ordering::Acquire)==IDLE,"agent connection already borrowed");
        if self.hooks.panicked.load(Ordering::Acquire){return Err(AgentAdmissionError::EffectUnknown.into());}
        if usage==AgentJournalUse::SharedTool&&self.has_unknown(){return Err(AgentAdmissionError::EffectUnknown.into());}
        self.hooks.denied.store(0,Ordering::Release);self.hooks.commit_began.store(false,Ordering::Release);
        *self.hooks.cancel.lock().map_err(|_|AgentAdmissionError::Fenced)?=Some(cancel);
        if usage==AgentJournalUse::SharedTool&&!self.hooks.allow(AgentAdmissionPoint::Dispatch){return self.finish_error();}
        self.hooks.mode.store(SETUP,Ordering::Release);
        let query_only=usage==AgentJournalUse::RetainedObservation;
        if let Err(error)=self.journal.conn.pragma_update(None,"query_only",query_only){
            self.hooks.mode.store(IDLE,Ordering::Release);
            *self.hooks.cancel.lock().map_err(|_|AgentAdmissionError::Fenced)?=None;return Err(error.into());
        }
        self.hooks.mode.store(if query_only{OBSERVE}else{SHARED},Ordering::Release);
        let outcome=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||work(&mut self.journal)));
        // Rollback is local settlement, never another shared operation. In
        // particular a progress interruption must not prevent actual rollback.
        self.hooks.mode.store(ROLLBACK,Ordering::Release);
        if !self.journal.conn.is_autocommit()&&self.journal.conn.execute_batch("ROLLBACK").is_err(){self.hooks.unknown.store(true,Ordering::Release);}
        self.hooks.mode.store(IDLE,Ordering::Release);
        let denied=self.hooks.denied.load(Ordering::Acquire);
        let commit=self.hooks.commit_began.load(Ordering::Acquire);
        let result=match outcome{Ok(result)=>result,Err(payload)=>{self.hooks.panicked.store(true,Ordering::Release);self.hooks.unknown.store(true,Ordering::Release);*self.hooks.work_panic.lock().unwrap_or_else(|p|p.into_inner())=Some(payload);Err(AgentAdmissionError::EffectUnknown.into())}};
        if commit&&(result.is_err()||self.hooks.check(AgentAdmissionPoint::Commit)!=AgentAdmissionState::Ready){self.hooks.unknown.store(true,Ordering::Release);}
        let unknown=self.has_unknown();*self.hooks.cancel.lock().map_err(|_|AgentAdmissionError::EffectUnknown)?=None;
        // Retained read-only reconciliation remains possible with an unknown
        // original write. It never clears unknown or reopens shared admission.
        if unknown&&usage==AgentJournalUse::SharedTool{return Err(AgentAdmissionError::EffectUnknown.into());}
        if denied!=0{return Err(if denied==1{AgentAdmissionError::Busy}else{AgentAdmissionError::Fenced}.into());}
        if usage==AgentJournalUse::SharedTool {
            match self.hooks.check(AgentAdmissionPoint::Dispatch){AgentAdmissionState::Ready=>{},AgentAdmissionState::Busy=>return Err(AgentAdmissionError::Busy.into()),AgentAdmissionState::Fenced=>return Err(AgentAdmissionError::Fenced.into())}
        }
        result
    }
    fn finish_error<T>(&self)->Result<T>{let denied=self.hooks.denied.load(Ordering::Acquire);*self.hooks.cancel.lock().map_err(|_|AgentAdmissionError::Fenced)?=None;
        Err(if self.has_unknown(){AgentAdmissionError::EffectUnknown}else if denied==1{AgentAdmissionError::Busy}else{AgentAdmissionError::Fenced}.into())}
}

/// Refusal evidence only. No raw readback can produce guarded/fresh authority.
/// Trusted fresh namespaces are canonical UUIDs; existing markers also prevent
/// legacy startup. Old incompatible binaries still require physical exclusion.
pub fn reject_unguarded_fresh_agent(path:&Path,project:&str)->Result<()> {
    startup_refusal_guard(path, project, None).map(drop)
}

impl Journal {
    /// Read-only refusal evidence, never host/lease authority. The caller keeps
    /// this actual connection through the separately authenticated writer open.
    pub(crate) fn retain_agent_startup_guard_before(path:&Path,project:&str,deadline:Instant)->Result<Self> {
        startup_refusal_guard(path, project, Some(deadline))
    }

    pub(crate) fn open_agent_startup_before(path:&Path,deadline:Instant)->Result<Self> {
        startup_before(deadline)?;
        let journal=Self::open_with_connection_config(path, |conn| {
            startup_before(deadline)?;
            // SQLite may call this across several statements. Every wait uses
            // the SAME deadline; no per-statement timeout can renew startup.
            conn.busy_handler(Some(move |_| startup_wait(deadline)))?;
            Ok(())
        })?;
        startup_before(deadline)?;
        Ok(journal)
    }

    pub(crate) fn finish_agent_startup_before(&self,deadline:Instant,ordinary_wait:Duration)->Result<()> {
        // Never leave an expired startup callback installed on a returned
        // connection. Ordinary API loads and retained hook owners keep their
        // existing policies; their admission hooks are not changed here.
        self.conn.busy_timeout(ordinary_wait)?;
        startup_before(deadline)
    }
}

fn startup_before(deadline:Instant)->Result<()> {
    ensure!(Instant::now()<deadline,"agent startup lock window expired");
    Ok(())
}

fn startup_wait(deadline:Instant)->bool {
    let entered=Instant::now();
    let Some(remaining)=deadline.checked_duration_since(entered) else{
        #[cfg(test)]
        agent_startup_busy_tests::record_startup_wait(deadline,entered,Duration::ZERO,entered,false);
        return false;
    };
    // Off-App startup only. No SQL, filesystem, network or authority callback
    // runs from SQLite's busy handler; a tiny bounded sleep avoids hot spin.
    let requested=remaining.min(Duration::from_millis(1));
    std::thread::sleep(requested);
    let returned=Instant::now();let retry=returned<deadline;
    #[cfg(test)]
    agent_startup_busy_tests::record_startup_wait(deadline,entered,requested,returned,retry);
    retry
}

fn startup_sqlite_busy(error:&anyhow::Error)->bool {
    error.chain().any(|cause|cause.downcast_ref::<rusqlite::Error>().is_some_and(|error|
        matches!(error,rusqlite::Error::SqliteFailure(code,_) if matches!(code.code,
            rusqlite::ErrorCode::DatabaseBusy|rusqlite::ErrorCode::DatabaseLocked))))
}

fn retry_startup_read<T>(deadline:Option<Instant>,mut read:impl FnMut()->Result<T>)->Result<T> {
    loop {
        if let Some(deadline)=deadline {startup_before(deadline)?;}
        match read() {
            Ok(value)=>{
                if let Some(deadline)=deadline {startup_before(deadline)?;}
                return Ok(value);
            }
            Err(error)=>{
                // This helper is used only for the read-only refusal query.
                // Never retry Journal initialization, a mutation or a marker
                // filesystem error, and never treat Busy as an absent receipt.
                if !startup_sqlite_busy(&error)||!deadline.is_some_and(startup_wait) {
                    return Err(error);
                }
            }
        }
    }
}

fn startup_refusal_guard(path:&Path,project:&str,deadline:Option<Instant>)->Result<Journal> {
    ensure!(!uuid::Uuid::parse_str(project).is_ok_and(|id|id.to_string()==project)&&!project.starts_with("fresh-v4:"),"fresh agent requires its retained host broker");
    if let Some(deadline)=deadline {startup_before(deadline)?;}
    let conn=Connection::open_with_flags(path,OpenFlags::SQLITE_OPEN_READ_ONLY|OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    conn.busy_timeout(Duration::ZERO)?;let journal=Journal{conn};
    if let Some(receipt)=retry_startup_read(deadline, ||journal.managed_project(project))? {
        let marker=receipt.request.namespace.repository.join(".rudder/engine/fresh-project.json");
        match std::fs::symlink_metadata(marker){Ok(_)=>anyhow::bail!("fresh marker refuses unguarded agent access"),Err(e) if e.kind()==std::io::ErrorKind::NotFound=>{},Err(e)=>return Err(e.into())}
    }
    if let Some(deadline)=deadline {startup_before(deadline)?;}
    Ok(journal)
}

#[cfg(test)]
#[path="agent_admission_tests.rs"]
mod agent_admission_tests;

#[cfg(test)]
#[path="agent_startup_busy_tests.rs"]
mod agent_startup_busy_tests;
