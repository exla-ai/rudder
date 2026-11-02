//! Privileged ORIGINAL bootstrap issuer, separately from received tuple DATA.
//! This trusts the genuine retained Root producer's process/image association;
//! no received tuple independently authenticates a hostile global-Root image.
use super::*;
pub(super) const CAPSULE_KIND:&str="root-compiler-sdk-producer-witness-helper-v2";
const WITNESS_KIND:&str="retained-original-root-producer-witness-v1";
#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SdkPeerImageWitnessData {
    pub version:u32,pub kind:String,pub service_pid:u32,pub service_start_ticks:String,
    pub launcher_pid:u32,pub launcher_start_ticks:String,pub service_sha256:String,pub launcher_sha256:String,
}
#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SdkHelperCapsuleData {
    pub version:u32,pub kind:String,pub service_pid:u32,pub launcher_pid:u32,pub helper_pid:u32,pub helper_start_ticks:String,
    pub constraint:FinalizationConstraint,pub mount:CompilerSdkMountData,pub job:serde_json::Value,
    pub service_sha256:String,pub launcher_sha256:String,pub peer_image_witness:SdkPeerImageWitnessData,
}
fn decimal(text:&str)->bool{!text.is_empty()&&text.len()<=20&&!text.starts_with('0')&&text.bytes().all(|b|b.is_ascii_digit())&&text.parse::<u64>().is_ok()}
fn sha(text:&str)->bool{text.len()==64&&text.bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b))}
/// Parse DIRECTLY from the sealed raw bytes. A Value intermediary would erase
/// duplicate keys before typed deserialization could reject them.
pub(super) fn decode_capsule(raw:&[u8])->Result<SdkHelperCapsuleData>{
    ensure!(!raw.is_empty()&&raw.len()<=16*1024,"SDK producer capsule bound differs");
    let data:SdkHelperCapsuleData=serde_json::from_slice(raw)?;let w=&data.peer_image_witness;
    ensure!(data.version==2&&data.kind==CAPSULE_KIND&&w.version==1&&w.kind==WITNESS_KIND,"SDK producer witness protocol differs");
    ensure!(data.service_pid>1&&data.launcher_pid>1&&data.helper_pid>1&&data.service_pid!=data.launcher_pid
        &&data.helper_pid!=data.service_pid&&data.helper_pid!=data.launcher_pid&&decimal(&data.helper_start_ticks)
        &&w.service_pid==data.service_pid&&w.launcher_pid==data.launcher_pid&&decimal(&w.service_start_ticks)&&decimal(&w.launcher_start_ticks)
        &&sha(&data.service_sha256)&&sha(&data.launcher_sha256)&&w.service_sha256==data.service_sha256&&w.launcher_sha256==data.launcher_sha256,
        "SDK producer witness identity DATA differs");Ok(data)
}
pub(super) fn readonly_executable(fd:&OwnedFd)->Result<()>{
    let descriptor=unsafe{libc::fcntl(fd.as_raw_fd(),libc::F_GETFD)};let status=unsafe{libc::fcntl(fd.as_raw_fd(),libc::F_GETFL)};
    ensure!(descriptor>=0&&descriptor&libc::FD_CLOEXEC!=0&&status>=0&&status&libc::O_ACCMODE==libc::O_RDONLY&&status&libc::O_PATH==0,
        "SDK producer executable descriptor flags differ");Ok(())
}
/// No Deserialize, packet/PID/path/boolean constructor, or generic exporter.
/// Only a genuine retained bootstrap proof can enter this private issuer.
pub(super) struct OriginalSdkPeerImages {
    proof:Arc<RuntimeBootstrapProof>,original:FinalizationBudget,service:Option<OwnedFd>,launcher:Option<OwnedFd>,
    service_start:Option<String>,launcher_start:Option<String>,ready:bool,failed:bool,
}
impl OriginalSdkPeerImages {
    pub(super) fn capture_retaining(out:&mut Option<Self>,proof:&Arc<RuntimeBootstrapProof>,original:&FinalizationBudget)->Result<()>{
        ensure!(out.is_none(),"SDK original producer witness carrier occupied");
        *out=Some(Self{proof:proof.clone(),original:original.clone(),service:None,launcher:None,service_start:None,launcher_start:None,ready:false,failed:false});
        let owner=out.as_mut().unwrap();let result=(||->Result<()>{
            owner.service=Some(OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC).open(format!("/proc/{}/exe",owner.proof.own.pid))?.into());
            owner.launcher=Some(owner.proof.raw[3].try_clone()?);
            owner.service_start=Some(original_stat(owner.proof.own.pid)?.1);owner.launcher_start=Some(original_stat(owner.proof.root.pid)?.1);
            owner.check_original()?;owner.ready=true;Ok(())
        })();if result.is_err(){owner.failed=true;}result
    }
    fn check_original(&self)->Result<()>{
        self.original.remaining()?;self.proof.current()?;
        ensure!(self.proof.own.pid==std::process::id(),"SDK witness producer is not original Root self");
        self.proof.service_image.matches_process(&self.proof.own)?;self.proof.root_image.matches_process(&self.proof.root)?;
        let service=self.service.as_ref().context("SDK original service executable absent")?;let launcher=self.launcher.as_ref().context("SDK original launcher executable absent")?;
        readonly_executable(service)?;readonly_executable(launcher)?;self.proof.service_image.matches_fd(service)?;self.proof.root_image.matches_fd(launcher)?;
        ensure!(self.service_start.as_ref()==Some(&original_stat(self.proof.own.pid)?.1)&&self.launcher_start.as_ref()==Some(&original_stat(self.proof.root.pid)?.1),"SDK original producer start changed");
        self.original.remaining()?;Ok(())
    }
    pub(super) fn current(&self)->Result<()>{ensure!(self.ready&&!self.failed,"SDK original producer witness unavailable");self.check_original()}
    pub(super) fn data(&self)->Result<SdkPeerImageWitnessData>{self.current()?;Ok(SdkPeerImageWitnessData{version:1,kind:WITNESS_KIND.into(),service_pid:self.proof.own.pid,
        service_start_ticks:self.service_start.as_ref().unwrap().clone(),launcher_pid:self.proof.root.pid,launcher_start_ticks:self.launcher_start.as_ref().unwrap().clone(),
        service_sha256:self.proof.service_image.sha256.clone(),launcher_sha256:self.proof.root_image.sha256.clone()})}
    pub(super) fn service_fd(&self)->Result<&OwnedFd>{ensure!(self.ready&&!self.failed,"SDK original service witness unavailable");self.service.as_ref().context("SDK original service executable absent")}
    pub(super) fn launcher_fd(&self)->Result<&OwnedFd>{ensure!(self.ready&&!self.failed,"SDK original launcher witness unavailable");self.launcher.as_ref().context("SDK original launcher executable absent")}
}
/// SAME original packet, sealed bytes and kernel credentials. Returns DATA
/// only, never a bootstrap proof, typed issuer, ReadyOrigin or SDK admission.
pub(super) fn receive_capsule_data(packet:&RuntimePacket,parent:&Process)->Result<SdkHelperCapsuleData>{
    ensure!(packet.credentials.pid==parent.pid as i32&&packet.credentials.uid==0&&packet.credentials.gid==0&&[7,10].contains(&packet.fds.len()),"SDK helper capsule origin/FD set differs");
    let raw=super::super::super::read_sealed_bytes(&packet.fds[0],16*1024)?;
    ensure!(raw==packet.bytes,"SDK helper sealed original bytes differ");decode_capsule(&raw)
}
pub(super) fn validate_service_tuple_data(data:&SdkHelperCapsuleData,packet:&RuntimePacket,parent:&Process,image:&Image)->Result<()>{
    ensure!(parent.pid==unsafe{libc::getppid()}as u32&&data.service_pid==parent.pid&&data.helper_pid==std::process::id(),"SDK helper original identities differ");
    parent.live()?;storage::floor(parent.pid,0)?;wire::require_initial_id_maps(parent.pid)?;
    let service=Process::capture(packet.fds[1].try_clone()?)?;ensure!(service.pid==parent.pid,"SDK helper original service pidfd differs");
    readonly_executable(&packet.fds[2])?;image.check()?;image.matches_fd(&packet.fds[2])?;
    ensure!(image.sha256==data.service_sha256&&original_stat(parent.pid)?.1==data.peer_image_witness.service_start_ticks,"SDK helper original service witness differs");Ok(())
}
pub(super) fn validate_launcher_tuple_data(data:&SdkHelperCapsuleData,packet:&RuntimePacket,parent:&Process,image:&Image)->Result<()>{
    let launcher=Process::capture(packet.fds[3].try_clone()?)?;
    ensure!(original_stat(parent.pid)?.0==launcher.pid&&data.launcher_pid==launcher.pid,"SDK helper original launcher parent differs");
    wire::require_initial_id_maps(launcher.pid)?;storage::floor(launcher.pid,0)?;
    readonly_executable(&packet.fds[4])?;image.check()?;image.matches_fd(&packet.fds[4])?;
    ensure!(image.sha256==data.launcher_sha256&&original_stat(launcher.pid)?.1==data.peer_image_witness.launcher_start_ticks,"SDK helper original launcher witness differs");Ok(())
}
/// Arm unknown BEFORE the one original send. Only known EAGAIN disarms it.
/// Successful send latches sent BEFORE its fallible original-current recheck.
pub(super) fn send_once_retaining(sent:&mut bool,unknown:&mut bool,send:impl FnOnce()->Result<bool>,postcheck:impl FnOnce()->Result<()>)->Result<bool>{
    ensure!(!*sent&&!*unknown,"SDK original capsule send is quarantined/already sent");*unknown=true;
    if !send()?{*unknown=false;return Ok(false)}*sent=true;postcheck()?;*unknown=false;Ok(true)
}
