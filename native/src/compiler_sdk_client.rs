//! Native SDK identity consumer. Only the actual authenticated runtime client
//! creates this retained alias. DATA/specs are neither preparation nor cache
//! result ACKs; SDK storage stays separate from the runtime RunnableRoot.
use super::*;
use rudder_engine::{build::{PlatformIdentity,PreparationSpec,ToolchainIdentity},execution::compiler_sdk::{CompilerSdkMountData}};

#[path="compiler_sdk_build.rs"]
mod selected;
pub use selected::{CompilerSdkBuildOutcome,RetainedCompilerSdkBuild,CompilerSdkBuildFailure,CompilerSdkJobDisposal,CompilerSdkJobSettled};
pub struct CompilerSdkClient {build_admission_closed:AtomicBool,dispatch_serial:std::sync::atomic::AtomicU64,builds:Mutex<Vec<Arc<Mutex<selected::BuildState>>>>,client:Arc<RuntimeImageClient>,mount:Option<CompilerSdkMountData>,spec:Option<PreparationSpec>,ready:bool}
impl CompilerSdkClient {
    pub(super) fn capture_retaining(out:&mut Option<Arc<Self>>,client:Arc<RuntimeImageClient>)->Result<()> {
        ensure!(out.is_none(),"original Native SDK client already captured");
        *out=Some(Arc::new(Self{build_admission_closed:AtomicBool::new(false),dispatch_serial:std::sync::atomic::AtomicU64::new(0),builds:Mutex::new(Vec::new()),client,mount:None,spec:None,ready:false}));
        let held=Arc::get_mut(out.as_mut().unwrap()).context("original SDK alias escaped before completion")?;
        held.client.current()?;let data=held.client.ready.compiler_sdk_data()?.context("original Root SDK ACK absent")?;data.mount.validate_data()?;
        ensure!(held.client.config.data().compiler_sdk.as_ref()==Some(&data.mount.catalog),"SDK original config/Root ACK differ");
        held.mount=Some(data.mount.clone());let semantic=&held.mount.as_ref().unwrap().semantic;
        held.spec=Some(semantic_preparation_spec_data(semantic,&held.mount.as_ref().unwrap().toolchains)?);
        held.spec.as_ref().unwrap().validate()?;held.client.current()?;held.ready=true;Ok(())
    }
    /// Close under the SAME registry mutex as registration. In-flight builds
    /// remain charged until their original build and cleanup workers join.
    pub(super) fn close_build_admission(&self)->Result<()> {
        let _registry=self.builds.try_lock().map_err(|_|anyhow::anyhow!("SDK build admission busy/poisoned; retain retirement"))?;
        self.build_admission_closed.store(true,Ordering::Release);Ok(())
    }
    pub(super) fn builds_retired(&self)->Result<bool> {
        let registry=self.builds.try_lock().map_err(|_|anyhow::anyhow!("SDK original build census busy/poisoned"))?;
        Ok(self.build_admission_closed.load(Ordering::Acquire)&&registry.is_empty())
    }
    pub fn current(&self,original:&FinalizationBudget)->Result<()>{original.remaining()?;self.assert_current()?;original.remaining()?;Ok(())}
    pub(super) fn poll_cancellations(&self)->Result<()>{let builds=match self.builds.try_lock(){Ok(v)=>v,Err(TryLockError::WouldBlock)=>return Ok(()),Err(TryLockError::Poisoned(_))=>anyhow::bail!("SDK retained builds custody poisoned")};for build in &*builds{if let Ok(held)=build.try_lock(){held.signal_original_cancellation()?;}}Ok(())}
    pub fn assert_current(&self)->Result<()> {
        ensure!(self.ready,"original SDK Native client ACK absent");self.client.current()?;
        let actual=self.client.ready.compiler_sdk_data()?.context("SDK actual Root ACK absent")?;
        ensure!(self.mount.as_ref()==Some(&actual.mount)&&self.client.config.data().compiler_sdk.as_ref()==Some(&actual.mount.catalog),"SDK original owner/config changed");Ok(())
    }
    /// Stable content DATA only. Actual backed preparation must retain this
    /// client plus the SDK's separate genuine backing/retention owner. Never
    /// insert this as an ordinary foreign LocalPin or runtime env dependency.
    pub fn preparation_spec_data(&self)->Result<&PreparationSpec>{self.assert_current()?;self.spec.as_ref().context("actual SDK content spec absent")}
    pub fn toolchains_data(&self)->Result<&BTreeMap<String,ToolchainIdentity>>{Ok(&self.preparation_spec_data()?.toolchains)}
    pub fn runtime_client(&self)->&Arc<RuntimeImageClient>{&self.client}
    pub fn require_ordinary_execution(&self)->Result<()> {self.assert_current()?;
        ensure!(self.client.config.data().runtime_qualification.is_none()&&self.client.config.data().compiler_qualification.is_none(),"fixed SDK qualification never permits general task/cache execution");Ok(())}
    /// Explicit semantic compiler launches only. An ambient SDK prefix/shebang
    /// call never qualifies as this SDK's cache action. The existing driver
    /// then enters the SDK prepared/ chroot for the compiler commands.
    pub fn qualified_environment_data(&self)->Result<BTreeMap<String,String>> {
        self.require_ordinary_execution()?;let root=rudder_engine::execution::compiler_sdk::SDK_GUEST;
        Ok(BTreeMap::from([("LC_ALL".into(),"C".into()),("LANG".into(),"C".into()),("TZ".into(),"UTC".into()),
            ("PYTHONHOME".into(),format!("{root}/prepared/tools/python")),("PYTHONDONTWRITEBYTECODE".into(),"1".into()),("PYTHONHASHSEED".into(),"0".into())]))
    }
    pub fn qualified_launch_data(&self,name:&str)->Result<Vec<String>> {
        self.require_ordinary_execution()?;let root=rudder_engine::execution::compiler_sdk::SDK_GUEST;
        if name=="buck2"{return Ok(vec![format!("{root}/buck2")])} // A2 is pinned static-musl, PT_INTERP absence is qualified.
        let program=match name{"python"=>format!("{root}/prepared/tools/python/bin/python3"),_=>anyhow::bail!("SDK launcher is fixed to its frontend entry points")};
        Ok(vec![format!("{root}/prepared/lib64/ld-linux-x86-64.so.2"),"--library-path".into(),format!("{root}/prepared/lib/x86_64-linux-gnu:{root}/prepared/usr/lib/x86_64-linux-gnu"),program,"-s".into(),"-P".into()])
    }
}

/// Pure content DATA factory. Never an SDK client, pin or preparation ACK.
fn semantic_preparation_spec_data(semantic:&rudder_engine::execution::compiler_sdk::CompilerSdkSemanticData,tools:&BTreeMap<String,ToolchainIdentity>)->Result<PreparationSpec>{
    semantic.validate_data()?;let spec=PreparationSpec{format_version:1,
        platform:PlatformIdentity{os:"linux".into(),architecture:"x86_64".into(),abi:"gnu-glibc-2.36".into(),properties:BTreeMap::new()},
        base_runtime:semantic.directory.clone(),installer_inputs:BTreeMap::from([("sdk/prepared".into(),semantic.prepared.clone()),("sdk/buck2".into(),semantic.buck2.clone())]),
        installer_commands:Vec::new(),installer_environment:BTreeMap::new(),toolchains:tools.clone(),policy_version:"compiler-sdk-executable-content-v2".into(),effects:Default::default()};
    spec.validate()?;Ok(spec)
}
