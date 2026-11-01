//! Native original terminal-HELPER-failure ownership, separate from successful
//! compiler/artifact results. There is no public DATA/Serde constructor.
use super::*;
use super::job_protocol::{HelperFailureData,TerminalFailureAck};
pub(super) struct OriginalSdkTerminalFailure {ack:Option<TerminalFailureAck>,ready:bool}
pub struct CompilerSdkBuildFailure {owner:Arc<Mutex<BuildState>>,sdk:Arc<CompilerSdkClient>,binding:JobBinding,data:HelperFailureData}
impl CompilerSdkBuildFailure {
    pub(super) fn matches_owner(&self,owner:&Arc<Mutex<BuildState>>)->bool{Arc::ptr_eq(&self.owner,owner)}
    pub fn source_kind(&self)->&str{&self.data.source_kind}
    pub fn source_root(&self)->&ObjectRef{&self.binding.source_root}
    pub fn action_key(&self)->&ObjectRef{&self.binding.action_key}
    /// Diagnostic only: the actual original outer helper's held stderr,
    /// bounded to4096 UTF8 bytes. It is not compiler-status authority.
    pub fn diagnostic_data(&self)->&str{&self.data.stderr_diagnostic}
    pub fn helper_exit_data(&self)->(Option<i32>,Option<i32>){(self.data.exit_code,self.data.signal)}
    pub fn qualified_owner(&self)->&Arc<CompilerSdkClient>{&self.sdk}
    /// A completed timely fact outlives its operation clock. The NEW caller's
    /// existing legitimate clock checks current custody/source pin only.
    pub fn current(&self,original:&FinalizationBudget)->Result<()>{original.remaining()?;self.sdk.current(original)?;
        let owner=loop{original.remaining()?;match self.owner.try_lock(){Ok(held)=>break held,Err(TryLockError::WouldBlock)=>std::thread::sleep(original.phase_limit(Duration::from_millis(1))?),Err(_)=>anyhow::bail!("SDK failed result custody poisoned")}};owner.failure_current()?;
        ensure!(owner.terminal_failure.as_ref().and_then(|f|f.ack.as_ref()).is_some_and(|a|a.binding==self.binding&&a.helper==self.data),"SDK failed result original receipt changed");original.remaining()?;Ok(())}
}
impl RetainedCompilerSdkBuild {
    pub fn terminal_failure(&self)->Result<&CompilerSdkBuildFailure>{self.failure.as_ref().context("SDK original timely terminal helper-failure ACK absent")}
}
impl BuildState {
    pub(super) fn retain_failure_owner(&self,owner:Arc<Mutex<Self>>)->Option<CompilerSdkBuildFailure>{let fact=self.terminal_failure.as_ref().filter(|f|f.ready)?;let ack=fact.ack.as_ref()?;
        Some(CompilerSdkBuildFailure{owner,sdk:self.sdk.clone(),binding:ack.binding.clone(),data:ack.helper.clone()})}
    pub(super) fn poll_terminal_ack_retaining(&mut self)->Result<bool>{
        ensure!(self.terminal_failure.is_none(),"SDK original terminal failure cannot replay");
        self.terminal_failure=Some(OriginalSdkTerminalFailure{ack:None,ready:false});
        // pending already owns the ACTUAL packet and any transferred FDs. The
        // opaque failure carrier is installed before parse/auth/postchecks.
        let packet=self.pending.as_ref().context("original failure packet absent")?;
        let fact=self.terminal_failure.as_mut().unwrap();fact.ack=Some(serde_json::from_slice(&packet.bytes)?);
        let ack=fact.ack.as_ref().unwrap();
        ensure!(packet.credentials.pid==self.sdk.client.service.pid as i32&&packet.credentials.uid==0&&packet.credentials.gid==0&&packet.fds.is_empty(),"SDK failed reply is not actual Root service");
        ack.validate(self.dispatch.as_ref().context("original SDK dispatch absent")?,self.sdk.mount.as_ref().context("actual SDK mount absent")?)?;
        ensure!(self.sent&&self.dispatch_permit.is_none()&&self.execution.is_none()&&!self.publish_attempted&&!self.published&&self.result.is_none()&&self.artifacts.is_none(),"SDK failed receipt conflicts with original success/publication effects");
        self.live()?;self.terminal_failure.as_mut().unwrap().ready=true;
        Err(ObservedTerminalHelperFailure.into())
    }
    pub(super) fn failure_current(&self)->Result<()>{
        ensure!(self.failed&&!self.panicked&&self.terminal_failure.as_ref().is_some_and(|f|f.ready)&&!self.publish_attempted&&!self.published&&!self.finished&&self.result.is_none()&&self.artifacts.is_none()&&self.execution.is_none(),"SDK original terminal-failure fact/source owner absent");
        self.sdk.assert_current()?;let ack=self.terminal_failure.as_ref().unwrap().ack.as_ref().context("SDK original failed reply absent")?;
        ack.validate(self.dispatch.as_ref().context("SDK original failed dispatch absent")?,self.sdk.mount.as_ref().context("SDK actual mount absent")?)?;
        ensure!(self.sent&&self.dispatch_permit.is_none()&&self.store.retention()?.pins.get(&self.source_pin)==Some(self.input.source_root()),"SDK actual failed source pin/dispatch differs");
        self.store.retained_closure(self.input.source_root())?;Ok(())
    }
    pub(super) fn terminal_data(&self)->Result<(&JobBinding,&HelperFailureData)>{let fact=self.terminal_failure.as_ref().context("SDK original failure absent")?;ensure!(fact.ready,"SDK failed receipt late/unknown");let a=fact.ack.as_ref().context("SDK failed receipt absent")?;Ok((&a.binding,&a.helper))}
}
#[derive(Debug)]struct ObservedTerminalHelperFailure;
impl std::fmt::Display for ObservedTerminalHelperFailure{fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result{f.write_str("SDK original helper failed; actual nonzero exit and both pipe EOF observed")}}
impl std::error::Error for ObservedTerminalHelperFailure{}
