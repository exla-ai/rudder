//! Caller custody only. Actual Root bootstrap issues the client; this module
//! cannot construct it, a helper grant, an image floor, or an environment pin.
#![cfg(target_os = "linux")]
use anyhow::{ensure, Context, Result};
use rudder_engine::execution::{finalization_budget::FinalizationBudget,
    handoff::{ManagedExecutionStartBarrier, helper_process::HelperProcessWitness}};
use rudder_native::{host_ingress::{HostIngressConfigOwner, HostBakedRuntimePolicy},
    root_storage_bootstrap::{ManagedMachineBootstrap,
        runtime_image::{RuntimeImageClient, RuntimeHelperGrant, RuntimeReadyObservation, RuntimeReadyObservationData,
            RuntimeHelperUseIdentity,RuntimeNativeUseObservation}}};
use std::{sync::{Arc, Mutex, MutexGuard, TryLockError,
    atomic::{AtomicBool, Ordering}}, thread::{self, ThreadId}, time::Duration};

fn time(original: &FinalizationBudget, cancelled: &AtomicBool) -> Result<()> {
    if cancelled.load(Ordering::Acquire) { original.cancel(); }
    original.remaining()?; Ok(())
}

/// A refusal only, never an authority producer. The current host importer and
/// fresh EnvironmentV2 producers do not retain a baked/composed closure. They
/// must not run as an implicit fallback for the configured image-mount profile.
fn require_legacy_environment_profile(policy: Option<&HostBakedRuntimePolicy>) -> Result<()> {
    ensure!(policy.is_none(),
        "configured baked runtime requires genuine composed environment/preparation backing; legacy host import refused");
    Ok(())
}
pub(crate) fn require_legacy_environment_backing(config: &HostIngressConfigOwner) -> Result<()> {
    config.assert_current()?;
    require_legacy_environment_profile(config.data().image_preseed.as_ref().and_then(|p| p.runtime_mount.as_ref()))
}

/// Machine worker owns the actual protected config and client. Unknown policy
/// is pending; it cannot authorize either runtime enrollment or legacy fallback.
pub(crate) struct MachineRuntimeImage {
    config: Option<Arc<HostIngressConfigOwner>>,
    client: Option<Arc<RuntimeImageClient>>,
    ready: Option<RuntimeReadyObservation>,
    ready_data: Option<RuntimeReadyObservationData>,
    app_thread: ThreadId,
}
impl MachineRuntimeImage {
    pub(crate) fn new(app_thread: ThreadId) -> Self {
        Self { config: None, client: None, ready: None, ready_data: None, app_thread }
    }
    fn off_app(&self) -> Result<()> {
        ensure!(thread::current().id() != self.app_thread, "runtime Root IO requires off-App worker"); Ok(())
    }
    /// Called only with the config from the SAME actual HostIngress retained
    /// by the machine before its offer to the App/ingress worker.
    pub(crate) fn observe_config_retaining(&mut self, config: Arc<HostIngressConfigOwner>) -> Result<()> {
        self.off_app()?;
        if let Some(prior) = &self.config {
            ensure!(Arc::ptr_eq(prior, &config), "original runtime config owner replaced");
        } else { self.config = Some(config); }
        self.config.as_ref().unwrap().assert_current()
    }
    fn selected(&self) -> Result<bool> {
        let config = self.config.as_ref().context("actual runtime config pending; no legacy fallback")?;
        config.assert_current()?;
        Ok(config.data().image_preseed.as_ref().and_then(|p| p.runtime_mount.as_ref()).is_some())
    }
    /// The bootstrap keeps its original endpoint/registration/service owners.
    /// This caller retains only another alias obtained from that actual owner.
    pub(crate) fn poll_retaining(&mut self, bootstrap: &mut ManagedMachineBootstrap) -> Result<()> {
        self.off_app()?;
        if self.config.is_none() { return Ok(()); }
        if !self.selected()? {
            ensure!(self.client.is_none(), "unselected runtime cannot retain an image client"); return Ok(());
        }
        if let Some(client) = bootstrap.runtime_image_client_retaining()? {
            if let Some(prior) = &self.client {
                ensure!(Arc::ptr_eq(prior, &client), "actual runtime client replaced");
            } else { self.client = Some(client); }
        }
        if let Some(client)=&self.client {
            // The actual client alias is retained above BEFORE the one-time
            // observation transfer/current check. Never rebuild from DATA.
            if self.ready.is_none(){bootstrap.take_runtime_ready_observation_retaining(&mut self.ready)?;}
            if let Some(owner)=&self.ready {
                ensure!(owner.matches_original_client(client),"runtime ready observation replaced original client");
                self.ready_data=Some(owner.checked_data_snapshot_off_app()?);
                bootstrap.export_runtime_qualification_retaining(owner)?;
            }
        }
        Ok(()) // None remains configured/pending, never an absent profile.
    }
    /// Pure last checked DATA, never a readiness or original owner constructor.
    pub(crate) fn ready_observation_data(&self)->Option<RuntimeReadyObservationData> {self.ready_data.clone()}
    /// OFF App, pure alias retention. None is Pending, never permission for a
    /// legacy fallback. No bootstrap/currentness IO or renewed request clock.
    pub(crate) fn retain_original_use_observation(&self,out:&mut Option<RuntimeNativeUseObservation>)->Result<()> {
        self.off_app()?;
        if let Some(client)=&self.client {client.capture_native_use_observation_retaining(out)?;}
        else {ensure!(out.is_none(),"retained original runtime observation lost machine client association");}
        Ok(())
    }
    pub(crate) fn enroll_helper_retaining(&mut self, bootstrap: &mut ManagedMachineBootstrap,
        start: &RuntimeTaskStart, original: &FinalizationBudget, cancelled: &Arc<AtomicBool>) -> Result<()> {
        self.off_app()?; time(original, cancelled)?;
        if !self.selected()? { return start.check_legacy_off_app(original, cancelled); }
        start.require_image_off_app(original, cancelled)?;
        loop {
            time(original, cancelled)?; self.poll_retaining(bootstrap)?;
            if let Some(client) = &self.client {
                return start.enroll_image_off_app(client, original, cancelled);
            }
            thread::sleep(original.phase_limit(Duration::from_millis(2))?);
        }
    }
}

struct StartState {
    barrier: ManagedExecutionStartBarrier,
    environment:Option<Arc<rudder_native::root_storage_bootstrap::runtime_image::environment::RuntimeEnvironmentBacking>>,
    client: Option<Arc<RuntimeImageClient>>,
    grant: Option<RuntimeHelperGrant>,
    required: bool,
    attached: bool,
    failed: bool,
}
pub(crate) enum OriginalTaskRuntimeUse {Image(RuntimeHelperUseIdentity),Legacy}
/// SAME original barrier shared with its existing task, Pending helper and
/// TaskDrain. No replacement barrier, receipt deserialization or IO in Drop.
/// Its mutex protects only this retained capsule; a worker never borrows App.
#[must_use = "retain the original task barrier and runtime grant through recovery/drain"]
pub(crate) struct RuntimeTaskStart {
    state: Mutex<StartState>,
    helper: Arc<HelperProcessWitness>,
    app_thread: ThreadId,
}
impl RuntimeTaskStart {
    /// Pure ownership transfer, immediately after successful actual PTY stage.
    pub(crate) fn capture(barrier: ManagedExecutionStartBarrier) -> Arc<Self> {
        Self::capture_with_environment(barrier,None)
    }
    pub(crate) fn capture_with_environment(barrier:ManagedExecutionStartBarrier,environment:Option<Arc<rudder_native::root_storage_bootstrap::runtime_image::environment::RuntimeEnvironmentBacking>>)->Arc<Self>{
        let helper = barrier.pending_helper_process();
        let required=environment.is_some();if required{barrier.require_runtime_image_before_release();}
        Arc::new(Self { helper, app_thread: thread::current().id(), state: Mutex::new(StartState {
            barrier,environment, client: None, grant: None, required, attached: false, failed: false }) })
    }
    fn on_app(&self) -> Result<MutexGuard<'_, StartState>> {
        ensure!(thread::current().id() == self.app_thread, "runtime start adoption requires original App thread");
        self.state.try_lock().map_err(|_| anyhow::anyhow!("original task barrier busy/poisoned; retain and wait"))
    }
    fn off_app(&self, original: &FinalizationBudget, cancelled: &Arc<AtomicBool>) -> Result<MutexGuard<'_, StartState>> {
        ensure!(thread::current().id() != self.app_thread, "runtime Root enrollment cannot run on App");
        loop {
            time(original, cancelled)?;
            match self.state.try_lock() {
                Ok(state) => {
                    let (actual, flag) = state.barrier.original_request_retaining()?;
                    ensure!(actual.constraint() == original.constraint() && Arc::ptr_eq(&flag, cancelled),
                        "runtime enrollment replaced actual original task clock/cancellation");
                    ensure!(!state.failed && Arc::ptr_eq(&state.barrier.pending_helper_process(), &self.helper),
                        "original runtime task barrier failed/replaced; custody retained");
                    return Ok(state);
                }
                Err(TryLockError::WouldBlock) => thread::sleep(original.phase_limit(Duration::from_millis(2))?),
                Err(TryLockError::Poisoned(_)) => anyhow::bail!("original runtime task barrier poisoned; custody retained"),
            }
        }
    }
    /// Called only while the existing whole completion/enrollment carrier is
    /// retained. Matching is a prerequisite, never a runtime drain proof.
    pub(crate) fn check_joined_original_off_app(&self)->Result<()> {
        ensure!(thread::current().id()!=self.app_thread,"runtime completion matching requires off-App worker");
        let state=self.state.try_lock().map_err(|_|anyhow::anyhow!("original runtime start busy/poisoned"))?;
        ensure!(!state.failed&&!state.barrier.is_pending()&&Arc::ptr_eq(&state.barrier.pending_helper_process(),&self.helper),
            "original runtime start failed/pending/replaced");
        if state.required {
            ensure!(state.attached,"original runtime helper attachment absent");
            let client=state.client.as_ref().context("original runtime client absent")?;
            if let Some(environment)=&state.environment{ensure!(environment.matches_runtime_client(client),"joined task environment changed original runtime client");}
            state.grant.as_ref().context("original runtime grant absent")?.check_original_start(client,&state.barrier)?;
        } else {ensure!(!state.attached&&state.environment.is_none()&&state.client.is_none()&&state.grant.is_none(),"legacy completion changed runtime profile");}
        Ok(())
    }
    /// Called ONLY through the fixed whole completion/enrollment observer.
    /// New borrowing is by try-lock; no task clock, pidfd/currentness or IO.
    pub(crate) fn observe_joined_original_off_app(&self)->Result<OriginalTaskRuntimeUse> {
        ensure!(thread::current().id()!=self.app_thread,"runtime use observation requires off App");
        let state=self.state.try_lock().map_err(|_|anyhow::anyhow!("observed original runtime start busy/poisoned"))?;
        ensure!(!state.failed&&!state.barrier.is_pending()&&Arc::ptr_eq(&state.barrier.pending_helper_process(),&self.helper),
            "observed original runtime start failed/pending/replaced");
        if state.required {
            ensure!(state.attached,"observed original runtime attachment absent");
            let client=state.client.as_ref().context("observed original runtime client absent")?;
            if let Some(environment)=&state.environment {ensure!(environment.matches_runtime_client(client),"observed environment changed runtime client");}
            Ok(OriginalTaskRuntimeUse::Image(state.grant.as_ref().context("observed original runtime grant absent")?.observe_original_start(client,&state.barrier)?))
        } else {
            ensure!(!state.attached&&state.environment.is_none()&&state.client.is_none()&&state.grant.is_none(),"observed legacy completion changed runtime profile");
            Ok(OriginalTaskRuntimeUse::Legacy)
        }
    }
    pub(crate) fn pending_on_app(&self) -> Result<Arc<HelperProcessWitness>> {
        let state = self.on_app()?;
        ensure!(state.barrier.is_pending() && !state.failed, "original pending task barrier unavailable");
        Ok(Arc::clone(&self.helper))
    }
    pub(crate) fn release_after_seed_ack_on_app(&self, helper: &Arc<HelperProcessWitness>) -> Result<()> {
        let mut state = self.on_app()?;
        ensure!(!state.failed && Arc::ptr_eq(helper, &self.helper), "original runtime helper changed/requires recovery");
        ensure!(!state.required || state.attached, "original runtime enrollment/attachment pending");
        // Only existing ManagedSeedEnrollment's verified-ACK adoption calls
        // this method. Its actual App row/pane/epochs checks remain separate.
        state.barrier.release_after_enrollment()
    }
    fn check_legacy_off_app(&self, original: &FinalizationBudget, cancelled: &Arc<AtomicBool>) -> Result<()> {
        let state = self.off_app(original, cancelled)?;
        ensure!(!state.required&&state.environment.is_none() && state.client.is_none() && state.grant.is_none(), "runtime task cannot fall back to legacy");
        Ok(())
    }
    fn require_image_off_app(&self, original: &FinalizationBudget, cancelled: &Arc<AtomicBool>) -> Result<()> {
        let mut state = self.off_app(original, cancelled)?;
        state.required = true; state.barrier.require_runtime_image_before_release(); Ok(())
    }
    fn enroll_image_off_app(&self, client: &Arc<RuntimeImageClient>, original: &FinalizationBudget,
        cancelled: &Arc<AtomicBool>) -> Result<()> {
        let mut state = self.off_app(original, cancelled)?;
        ensure!(state.required && !state.attached, "original runtime enrollment already attached/not required");
        if let Some(environment)=&state.environment{ensure!(environment.matches_runtime_client(client),"task environment changed actual original Root runtime client");
            loop{time(original,cancelled)?;match environment.assert_original_custody(){Ok(())=>break,
                Err(e)if e.downcast_ref::<rudder_native::root_storage_bootstrap::runtime_image::environment::RuntimeBackingBusy>().is_some()=>thread::sleep(original.phase_limit(Duration::from_millis(1))?),Err(e)=>return Err(e)}}}
        if let Some(prior) = &state.client {
            ensure!(Arc::ptr_eq(prior, client), "runtime helper cannot change its original client");
        } else { state.client = Some(Arc::clone(client)); }
        // Capsule, actual client and out-grant are retained OUTSIDE every
        // fallible begin/poll/attach. Never replay a sent or unknown request.
        let result = (|| -> Result<()> {
            loop {
                time(original, cancelled)?;
                if state.grant.is_none() {
                    let StartState { barrier, grant, .. } = &mut *state;
                    if !client.try_begin_helper_retaining(grant, barrier, original)? {
                        thread::sleep(original.phase_limit(Duration::from_millis(2))?); continue;
                    }
                }
                if state.grant.as_mut().unwrap().poll_retaining()? { break; }
                thread::sleep(original.phase_limit(Duration::from_millis(2))?);
            }
            time(original, cancelled)?;
            let StartState { barrier, grant, .. } = &mut *state;
            grant.as_mut().unwrap().attach_to_original_barrier_retaining(barrier)?;
            time(original, cancelled)?; state.attached = true; Ok(())
        })();
        if result.is_err() { state.failed = true; }
        result
    }
}

#[cfg(test)]
#[path = "managed_runtime_task_tests.rs"]
mod tests;

#[cfg(test)]
#[path="managed_runtime_ready_tests.rs"]
mod ready_observation_tests;

impl MachineRuntimeImage {
    /// OFF App original owner only. A pending client/observation stays pending;
    /// no snapshot DATA or configured flag can create a usable client alias.
    pub(crate) fn environment_client_retaining(&self,registered:&Arc<rudder_native::engine_registration::RegisteredEngineOwner>)->Result<Option<Arc<RuntimeImageClient>>>{
        self.off_app()?;if !self.selected()?{return Ok(None)}let (Some(client),Some(ready))=(&self.client,&self.ready)else{return Ok(None)};
        ensure!(ready.matches_original_client(client),"runtime environment observation/client origin changed");ready.assert_current()?;
        client.assert_environment_scope(self.config.as_ref().unwrap(),registered)?;Ok(Some(client.clone()))
    }
}
