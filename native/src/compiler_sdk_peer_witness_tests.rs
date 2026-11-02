//! Parser/one-send regressions and actual Root->UID1500 SCM DATA checks.
//! No fixture constructs the genuine bootstrap issuer or SDK admission.
use super::*;
use super::peer_witness::*;
use rudder_engine::protocol::{ObjectKind,ObjectRef};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
fn sample_data()->SdkHelperCapsuleData{
    let object=ObjectRef::from_bytes(ObjectKind::Blob,b"schema-data-only");let directory=ObjectRef::from_bytes(ObjectKind::Directory,b"schema-data-only");
    let semantic=rudder_engine::execution::compiler_sdk::CompilerSdkSemanticData::from_members_data(directory.clone(),object.clone(),16,0o555).unwrap().0;
    // Deliberately unqualified mount DATA. Parsing never confers a real SDK
    // mount; production still validates its catalogue, tools and held floor.
    let mount=CompilerSdkMountData{catalog:CompilerSdkCatalogData{format_version:1,profile_id:"parser-fixture-data-only".into(),artifact_image:"fixture".into(),cas_image:"fixture".into(),
        artifact_manifest:object.clone(),inventory:object.clone(),manifest:object,installer_input:directory,source_binding_sha256:"a".repeat(64)},semantic,
        floor:wire::RuntimeFdData{device:1,inode:2,mount_id:3,directory:true},toolchains:BTreeMap::new()};
    let constraint=FinalizationConstraint::from_json(br#"{"format_version":1,"maximum_milliseconds":30000,"boottime_deadline_nanoseconds":"123456789"}"#).unwrap();
    SdkHelperCapsuleData{version:2,kind:CAPSULE_KIND.into(),service_pid:2,launcher_pid:3,helper_pid:4,helper_start_ticks:"11".into(),constraint,mount,job:serde_json::Value::Null,
        service_sha256:"a".repeat(64),launcher_sha256:"b".repeat(64),peer_image_witness:SdkPeerImageWitnessData{version:1,kind:"retained-original-root-producer-witness-v1".into(),service_pid:2,
            service_start_ticks:"12".into(),launcher_pid:3,launcher_start_ticks:"13".into(),service_sha256:"a".repeat(64),launcher_sha256:"b".repeat(64)}}
}
fn duplicate(raw:&[u8],key:&str)->Vec<u8>{
    let value:serde_json::Value=serde_json::from_slice(raw).unwrap();let field=serde_json::to_vec(&value[key]).unwrap();
    let mut out=Vec::from(&b"{"[..]);out.extend(serde_json::to_vec(key).unwrap());out.push(b':');out.extend(field);out.push(b',');out.extend_from_slice(&raw[1..]);out
}
#[test]fn producer_capsule_direct_raw_parser_refuses_every_missing_unknown_and_duplicate_field(){
    let raw=canonical_json(&sample_data()).unwrap();decode_capsule(&raw).unwrap();let value:serde_json::Value=serde_json::from_slice(&raw).unwrap();
    for key in value.as_object().unwrap().keys(){
        let mut missing=value.clone();missing.as_object_mut().unwrap().remove(key);assert!(decode_capsule(&canonical_json(&missing).unwrap()).is_err(),"missing outer {key}");
        assert!(decode_capsule(&duplicate(&raw,key)).is_err(),"duplicate outer {key}");
    }
    let mut foreign=value.clone();foreign["foreign"]=true.into();assert!(decode_capsule(&canonical_json(&foreign).unwrap()).is_err());
    let nested=canonical_json(&value["peer_image_witness"]).unwrap();
    for key in value["peer_image_witness"].as_object().unwrap().keys(){
        let mut missing=value.clone();missing["peer_image_witness"].as_object_mut().unwrap().remove(key);assert!(decode_capsule(&canonical_json(&missing).unwrap()).is_err(),"missing witness {key}");
        let duplicated=duplicate(&nested,key);let needle=String::from_utf8(nested.clone()).unwrap();let replacement=String::from_utf8(duplicated).unwrap();
        let duplicate_raw=String::from_utf8(raw.clone()).unwrap().replacen(&needle,&replacement,1);assert_ne!(duplicate_raw.as_bytes(),raw);
        assert!(decode_capsule(duplicate_raw.as_bytes()).is_err(),"duplicate witness {key}");
    }
    let mut foreign=value;foreign["peer_image_witness"]["foreign"]=true.into();assert!(decode_capsule(&canonical_json(&foreign).unwrap()).is_err());
}
#[test]fn producer_capsule_direct_raw_parser_refuses_protocol_identity_types_and_bounds(){
    let value=serde_json::to_value(sample_data()).unwrap();
    for (key,bad) in [("version",serde_json::json!(1)),("kind",serde_json::json!("root-compiler-sdk-fixed-helper-v1")),("service_pid",serde_json::json!(0)),
        ("launcher_pid",serde_json::json!(2)),("helper_pid",serde_json::json!(3)),("helper_start_ticks",serde_json::json!("01")),("helper_start_ticks",serde_json::json!(1)),
        ("helper_start_ticks",serde_json::json!("18446744073709551616")),("service_sha256",serde_json::json!("A".repeat(64))),("launcher_sha256",serde_json::json!("b".repeat(63)))]{
        let mut changed=value.clone();changed[key]=bad;assert!(decode_capsule(&canonical_json(&changed).unwrap()).is_err(),"outer {key}");
    }
    for (key,bad) in [("version",serde_json::json!(2)),("kind",serde_json::json!("foreign")),("service_pid",serde_json::json!(3)),("launcher_pid",serde_json::json!(4)),
        ("service_start_ticks",serde_json::json!("0")),("launcher_start_ticks",serde_json::json!("-1")),("service_sha256",serde_json::json!("c".repeat(64))),("launcher_sha256",serde_json::Value::Null)]{
        let mut changed=value.clone();changed["peer_image_witness"][key]=bad;assert!(decode_capsule(&canonical_json(&changed).unwrap()).is_err(),"witness {key}");
    }
    assert!(decode_capsule(&[]).is_err());assert!(decode_capsule(&vec![b' ';16*1024+1]).is_err());
    let mut changed=value;changed["job"]=serde_json::json!({"selected_data":true});decode_capsule(&canonical_json(&changed).unwrap()).unwrap();
}
#[test]fn producer_capsule_send_unknown_and_postcheck_failure_are_absorbing(){
    let mut sent=false;let mut unknown=false;
    assert!(send_once_retaining(&mut sent,&mut unknown,||Err(anyhow::anyhow!("original unknown send")),||panic!("no postcheck on send error")).is_err());
    assert!(!sent&&unknown);assert!(send_once_retaining(&mut sent,&mut unknown,||panic!("unknown send replay"),||Ok(())).is_err());
    let mut sent=false;let mut unknown=false;
    assert!(send_once_retaining(&mut sent,&mut unknown,||Ok(true),||Err(anyhow::anyhow!("actual post-send current failed"))).is_err());
    assert!(sent&&unknown);assert!(send_once_retaining(&mut sent,&mut unknown,||panic!("successful send replay"),||Ok(())).is_err());
    let mut sent=false;let mut unknown=false;
    assert!(!send_once_retaining(&mut sent,&mut unknown,||Ok(false),||panic!("no postcheck on EAGAIN")).unwrap());assert!(!sent&&!unknown);
    assert!(send_once_retaining(&mut sent,&mut unknown,||Ok(true),||Ok(())).unwrap());assert!(sent&&!unknown);
}
#[test]fn producer_capsule_send_panic_keeps_unknown_armed(){
    let mut sent=false;let mut unknown=false;
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(||{let _=send_once_retaining(&mut sent,&mut unknown,||panic!("original send panic"),||Ok(()));})).is_err());
    assert!(!sent&&unknown);
    let mut sent=false;let mut unknown=false;
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(||{let _=send_once_retaining(&mut sent,&mut unknown,||Ok(true),||panic!("original post-send check panic"));})).is_err());
    assert!(sent&&unknown);
}

struct OriginalChild {
    child:Child,stdout:Option<std::process::ChildStdout>,stderr:Option<std::process::ChildStderr>,
    pidfd:Option<OwnedFd>,process:Option<Process>,start:Option<String>,out:Vec<u8>,err:Vec<u8>,out_eof:bool,err_eof:bool,
    exit:Option<ExitStatus>,failure:Option<anyhow::Error>,kill_sent:bool,may_kill_original:bool,
}
impl OriginalChild {
    fn drain(fd:i32,raw:&mut Vec<u8>,eof:&mut bool)->Result<()>{
        if *eof{return Ok(())}let mut poll=libc::pollfd{fd,events:libc::POLLIN,revents:0};
        let ready=unsafe{libc::poll(&mut poll,1,0)};
        if ready<0{let error=io::Error::last_os_error();if error.kind()==io::ErrorKind::Interrupted{return Ok(())}return Err(error.into())}
        ensure!(poll.revents&(libc::POLLERR|libc::POLLNVAL)==0,"strict original pipe poll failed");
        if ready==0{return Ok(())}let mut bytes=[0u8;4096];let n=unsafe{libc::read(fd,bytes.as_mut_ptr().cast(),bytes.len())};
        if n<0{let error=io::Error::last_os_error();if error.kind()==io::ErrorKind::Interrupted{return Ok(())}return Err(error.into())}
        if n==0{*eof=true;return Ok(())}
        let room=8192usize.saturating_sub(raw.len());raw.extend_from_slice(&bytes[..(n as usize).min(room)]);
        ensure!(n as usize<=room,"strict original pipe bound exceeded");Ok(())
    }
    fn fail(&mut self,error:anyhow::Error){if self.failure.is_none(){self.failure=Some(error);}}
    fn signal_failed_original(&mut self){
        if self.failure.is_none()||self.kill_sent||!self.may_kill_original{return}self.kill_sent=true;
        let result=if let Some(process)=&self.process{if unsafe{libc::syscall(libc::SYS_pidfd_send_signal,process.fd().as_raw_fd(),libc::SIGKILL,std::ptr::null::<libc::siginfo_t>(),0)}==0{Ok(())}else{Err(io::Error::last_os_error())}}else{self.child.kill()};
        if let Err(error)=result{if error.raw_os_error()!=Some(libc::ESRCH){self.fail(error.into())}}
    }
    fn run(mut command:Command,may_kill_original:bool,original:&FinalizationBudget,mut step:impl FnMut(&mut Self)->Result<()>)->Result<Self>{
        // Store the SAME actual Child before every fallible pipe/pidfd/start
        // operation. On error/timeout keep it until actual wait AND both EOFs.
        original.remaining()?;let child=command.spawn()?;
        let mut owner=Self{child,stdout:None,stderr:None,pidfd:None,process:None,start:None,out:Vec::new(),err:Vec::new(),out_eof:false,err_eof:false,exit:None,failure:None,kill_sent:false,may_kill_original};
        owner.stdout=owner.child.stdout.take();owner.stderr=owner.child.stderr.take();
        let captured=(||->Result<()>{
            ensure!(owner.stdout.is_some()&&owner.stderr.is_some(),"strict original pipe custody absent");
            let raw=unsafe{libc::syscall(libc::SYS_pidfd_open,owner.child.id(),0)}as i32;
            if raw<0{return Err(io::Error::last_os_error().into())}
            owner.pidfd=Some(unsafe{OwnedFd::from_raw_fd(raw)});owner.process=Some(Process::capture(owner.pidfd.as_ref().unwrap().try_clone()?)?);owner.start=Some(original_stat(owner.child.id())?.1);Ok(())
        })();if let Err(error)=captured{owner.fail(error)}
        loop{
            if original.remaining().is_err(){owner.fail(anyhow::anyhow!("strict original fixture wait/EOF deadline expired"));}
            if let Some(fd)=&owner.stdout{if let Err(error)=Self::drain(fd.as_raw_fd(),&mut owner.out,&mut owner.out_eof){owner.fail(error)}}
            if let Some(fd)=&owner.stderr{if let Err(error)=Self::drain(fd.as_raw_fd(),&mut owner.err,&mut owner.err_eof){owner.fail(error)}}
            if owner.exit.is_none(){match owner.child.try_wait(){Ok(exit)=>owner.exit=exit,Err(error)=>owner.fail(error.into())}}
            if owner.failure.is_none(){match std::panic::catch_unwind(std::panic::AssertUnwindSafe(||step(&mut owner))){
                Ok(Ok(()))=>{},Ok(Err(error))=>owner.fail(error),Err(_)=>owner.fail(anyhow::anyhow!("strict original step panicked; child retained")),
            }}
            owner.signal_failed_original();
            if owner.exit.is_some()&&owner.out_eof&&owner.err_eof{break}
            // Expiration never creates another work/acceptance clock. Remain
            // with the SAME originals if actual settlement cannot be observed.
            std::thread::sleep(Duration::from_millis(2));
        }
        let waited=loop{match owner.child.wait(){Ok(exit)=>break exit,Err(error)=>{owner.fail(error.into());std::thread::sleep(Duration::from_millis(2));}}};ensure!(owner.exit==Some(waited),"strict original actual wait changed");
        if let Some(error)=owner.failure.take(){return Err(error)}
        let process=owner.process.as_ref().context("strict original pidfd absent")?;
        ensure!(process.pid==owner.child.id()&&process.exited()?&&owner.start.as_ref().is_some_and(|start|!start.is_empty()),"strict original birth/pidfd/Child differs");
        ensure!(waited.success()&&owner.out_eof&&owner.err_eof,"strict original Child failed or EOF absent");Ok(owner)
    }
}
const ROLE:&str="RUDDER_SDK_WITNESS_FIXTURE_ROLE";
const CLOCK:&str="RUDDER_SDK_WITNESS_FIXTURE_ORIGINAL_CLOCK";
const CASE:&str="RUDDER_SDK_WITNESS_FIXTURE_CASE";
const SOCKET:&str="RUDDER_SDK_WITNESS_FIXTURE_SOCKET";
const CHILD_DONE:&str="RUDDER_SDK_WITNESS_FIXTURE_CHILD_V1=passed";
const PUBLISHER_DONE:&str="RUDDER_SDK_WITNESS_FIXTURE_PUBLISHER_V1=passed";
const LAUNCHER_DONE:&str="RUDDER_SDK_WITNESS_FIXTURE_LAUNCHER_V1=passed";
const TEST:&str="sdk_helper_root_producer_witness_real_scm_data_checks";
fn strict_root()->Result<()>{
    storage::floor(std::process::id(),0)?;wire::require_initial_id_maps(std::process::id())?;
    let text=String::from_utf8(storage::proc_bytes("/proc/thread-self/status",16384)?)?;
    let effective=u64::from_str_radix(text.lines().find_map(|line|line.strip_prefix("CapEff:")).context("strict Root capabilities absent")?.trim(),16)?;
    let bits=(1u64<<6)|(1u64<<7)|(1u64<<8);ensure!(effective&bits==bits,"strict Root requires existing SETGID/SETUID/SETPCAP");
    ensure!(text.lines().any(|line|line.strip_prefix("NoNewPrivs:").is_some_and(|s|s.trim()=="0")),"strict Root must begin NNP0");Ok(())
}
fn clock()->Result<FinalizationBudget>{let raw=std::env::var(CLOCK)?;let constraint=FinalizationConstraint::from_json(raw.as_bytes())?;FinalizationBudget::from_constraint(&constraint)}
fn exact_command(path:&Path,role:&str,original:&FinalizationBudget)->Result<Command>{
    let full=format!("{}::{TEST}",module_path!());let exact=full.split_once("::").unwrap().1;
    let mut command=Command::new(path);command.args(["--exact",exact,"--ignored","--nocapture","--test-threads=1"]).env_clear().env(ROLE,role)
        .env(CLOCK,String::from_utf8(canonical_json(&original.constraint())?)?).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());Ok(command)
}
fn lines(raw:&[u8],fixed:&str)->usize{raw.split(|b|*b==b'\n').filter(|line|*line==fixed.as_bytes()).count()}
fn expected(case:&str)->Option<&'static str>{match case{
    "valid"=>None,"writable"|"opath"|"cloexec"=>Some("SDK producer executable descriptor flags differ"),
    "image"=>Some("storage inherited service image differs"),"service_pidfd"=>Some("SDK helper original service pidfd differs"),"launcher_pidfd"=>Some("SDK helper original launcher parent differs"),
    "service_start"=>Some("SDK helper original service witness differs"),"launcher_start"=>Some("SDK helper original launcher witness differs"),
    "unsealed"=>Some("machine original Root certificate unavailable"),"mismatch"=>Some("SDK helper sealed original bytes differ"),
    "count"|"credentials_local"=>Some("SDK helper capsule origin/FD set differs"),"duplicate"|"nested_duplicate"=>Some("duplicate field"),_=>panic!("unknown closed fixture case"),
}}
fn probe()->Result<()>{
    let original=clock()?;original.remaining()?;storage::floor(std::process::id(),1500)?;wire::require_initial_id_maps(std::process::id())?;
    ensure!(unsafe{libc::prctl(libc::PR_SET_DUMPABLE,0,0,0,0)}==0,"strict witness ND0 unavailable");require_protected_descriptor_directory(std::process::id())?;
    let image=Image::capture(&std::env::current_exe()?)?;image.check()?;
    // This is a FRESH socket after actual exec+floor+ND0, never an inherited
    // capability. Actual SCM credentials and received rights come from Root.
    let raw=unsafe{libc::socket(libc::AF_UNIX,libc::SOCK_SEQPACKET|libc::SOCK_NONBLOCK|libc::SOCK_CLOEXEC,0)};
    ensure!(raw>=0,"strict witness socket absent");let socket=unsafe{OwnedFd::from_raw_fd(raw)};
    let passcred:libc::c_int=1;ensure!(unsafe{libc::setsockopt(socket.as_raw_fd(),libc::SOL_SOCKET,libc::SO_PASSCRED,(&passcred as *const libc::c_int).cast(),std::mem::size_of_val(&passcred)as _)}==0,"strict witness pre-connect credentials unavailable");
    let name=std::env::var(SOCKET)?;let(address,len)=abstract_address(&name)?;
    ensure!(unsafe{libc::connect(socket.as_raw_fd(),(&address as *const libc::sockaddr_un).cast(),len)}==0,"strict witness fresh connection failed");
    let parent=Process::capture(peer_pidfd(socket.as_raw_fd())?)?;ensure!(parent.pid==unsafe{libc::getppid()}as u32,"strict witness peer is not actual Root parent");
    let endpoint=RuntimeEndpoint::capture(socket)?;let mut packet=loop{original.remaining()?;parent.live()?;if let Some(packet)=endpoint.receive_data()?{break packet}std::thread::sleep(Duration::from_millis(1));};
    let case=std::env::var(CASE)?;
    // These two are explicitly local negative mutations AFTER actual receive:
    // the kernel normally supplies CLOEXEC and authentic SCM credentials.
    if case=="cloexec"{ensure!(unsafe{libc::fcntl(packet.fds[2].as_raw_fd(),libc::F_SETFD,0)}==0,"strict local CLOEXEC fault unavailable");}
    if case=="credentials_local"{packet.credentials.pid+=1;}
    let checked=(||->Result<()>{let data=receive_capsule_data(&packet,&parent)?;validate_service_tuple_data(&data,&packet,&parent,&image)?;validate_launcher_tuple_data(&data,&packet,&parent,&image)?;
        ensure!(data.helper_start_ticks==original_stat(std::process::id())?.1&&data.constraint==original.constraint(),"strict actual helper/fixture clock changed");Ok(())})();
    if let Some(fixed)=expected(&case){let error=checked.expect_err("strict negative receiver DATA case accepted");ensure!(format!("{error:#}").contains(fixed),"strict receiver failed at a different boundary");}
    else{checked?;}
    original.remaining()?;storage::floor(std::process::id(),1500)?;ensure!(unsafe{libc::prctl(libc::PR_GET_DUMPABLE,0,0,0,0)}==0,"strict witness ND0 changed");
    println!("\n{CHILD_DONE}");std::io::stdout().flush()?;Ok(())
}
struct FixturePacket {bytes:Vec<u8>,fds:Vec<OwnedFd>,sent:bool,unknown:bool}
fn fixture_packet(case:&str,owner:&OriginalChild,image:&Image,original:&FinalizationBudget)->Result<FixturePacket>{
    let service=Process::current()?;let launcher_pid=original_stat(service.pid)?.0;
    let raw=unsafe{libc::syscall(libc::SYS_pidfd_open,launcher_pid,0)}as i32;ensure!(raw>=0,"strict original launcher pidfd absent");let launcher_fd=unsafe{OwnedFd::from_raw_fd(raw)};
    let launcher=Process::capture(launcher_fd.try_clone()?)?;image.matches_process(&service)?;image.matches_process(&launcher)?;
    let mut data=sample_data();data.service_pid=service.pid;data.launcher_pid=launcher.pid;data.helper_pid=owner.child.id();data.helper_start_ticks=owner.start.as_ref().unwrap().clone();data.constraint=original.constraint();
    data.service_sha256=image.sha256.clone();data.launcher_sha256=image.sha256.clone();let w=&mut data.peer_image_witness;w.service_pid=service.pid;w.launcher_pid=launcher.pid;
    w.service_start_ticks=original_stat(service.pid)?.1;w.launcher_start_ticks=original_stat(launcher.pid)?.1;w.service_sha256=image.sha256.clone();w.launcher_sha256=image.sha256.clone();
    if case=="service_start"{w.service_start_ticks.push('1');}if case=="launcher_start"{w.launcher_start_ticks.push('1');}
    let mut bytes=canonical_json(&data)?;
    if case=="duplicate"{bytes=duplicate(&bytes,"service_pid");}
    if case=="nested_duplicate"{let nested=canonical_json(&data.peer_image_witness)?;let duplicate=duplicate(&nested,"service_pid");bytes=String::from_utf8(bytes)?.replacen(&String::from_utf8(nested)?,&String::from_utf8(duplicate)?,1).into_bytes();}
    let certificate=if case=="unsealed"{let raw=unsafe{libc::memfd_create(c"sdk-witness-unsealed-fixture".as_ptr(),libc::MFD_CLOEXEC|libc::MFD_ALLOW_SEALING)};ensure!(raw>=0,"strict unsealed fixture absent");unsafe{OwnedFd::from_raw_fd(raw)}}else{super::super::super::super::sealed_bytes(&bytes)?};
    let exe=std::env::current_exe()?;
    let service_image:OwnedFd=if case=="writable"{OpenOptions::new().read(true).write(true).custom_flags(libc::O_CLOEXEC).open("/dev/null")?.into()}
        else if case=="opath"{OpenOptions::new().read(true).custom_flags(libc::O_PATH|libc::O_CLOEXEC).open(&exe)?.into()}
        else if case=="image"{File::open("/dev/null")?.into()}else{File::open(&exe)?.into()};
    let service_fd=if case=="service_pidfd"{launcher.fd().try_clone()?}else{service.fd().try_clone()?};
    let launcher_fd=if case=="launcher_pidfd"{service.fd().try_clone()?}else{launcher.fd().try_clone()?};
    let mut fds=vec![certificate,service_fd,service_image,launcher_fd,File::open(&exe)?.into(),File::open("/dev/null")?.into(),File::open("/dev/null")?.into()];
    if case=="count"{fds.pop();}if case=="mismatch"{bytes.push(b' ');}
    Ok(FixturePacket{bytes,fds,sent:false,unknown:false})
}
fn publisher()->Result<()>{
    strict_root()?;let original=clock()?;let path=std::env::current_exe()?;let image=Image::capture(&path)?;
    for case in ["valid","writable","opath","cloexec","image","service_pidfd","launcher_pidfd","service_start","launcher_start","unsealed","mismatch","count","credentials_local","duplicate","nested_duplicate"]{
        original.remaining()?;let name=format!("rudder-sdk-witness-fixture-{}",uuid::Uuid::new_v4());
        let raw=unsafe{libc::socket(libc::AF_UNIX,libc::SOCK_SEQPACKET|libc::SOCK_NONBLOCK|libc::SOCK_CLOEXEC,0)};ensure!(raw>=0,"strict publisher listener absent");let listener=unsafe{OwnedFd::from_raw_fd(raw)};
        let(address,len)=abstract_address(&name)?;ensure!(unsafe{libc::bind(raw,(&address as *const libc::sockaddr_un).cast(),len)}==0&&unsafe{libc::listen(raw,1)}==0,"strict publisher listen failed");
        let mut command=exact_command(&path,"probe",&original)?;command.env(CASE,case).env(SOCKET,&name);
        unsafe{command.pre_exec(||sdk_helper_exec_floor());}
        let mut accepted=None;let mut peer=None;let mut endpoint=None;let mut packet=None;
        let owner=OriginalChild::run(command,true,&original,|owner|{
            if endpoint.is_none(){let raw=unsafe{libc::accept4(listener.as_raw_fd(),std::ptr::null_mut(),std::ptr::null_mut(),libc::SOCK_CLOEXEC|libc::SOCK_NONBLOCK)};
                if raw<0{ensure!(io::Error::last_os_error().kind()==io::ErrorKind::WouldBlock,"strict publisher accept failed");return Ok(())}
                accepted=Some(unsafe{OwnedFd::from_raw_fd(raw)});peer=Some(Process::capture(peer_pidfd(raw)?)?);ensure!(peer.as_ref().unwrap().pid==owner.child.id(),"strict foreign original probe");
                storage::floor(owner.child.id(),1500)?;require_protected_descriptor_directory(owner.child.id())?;endpoint=Some(RuntimeEndpoint::capture(accepted.as_ref().unwrap().try_clone()?)?);
            }
            if packet.is_none(){packet=Some(fixture_packet(case,owner,&image,&original)?);}
            let packet=packet.as_mut().unwrap();if !packet.sent{send_once_retaining(&mut packet.sent,&mut packet.unknown,||endpoint.as_ref().unwrap().send_data(&packet.bytes,&packet.fds.iter().collect::<Vec<_>>()),||{original.remaining()?;Ok(())})?;}Ok(())
        })?;
        ensure!(packet.as_ref().is_some_and(|p|p.sent&&!p.unknown)&&lines(&owner.out,CHILD_DONE)==1,"strict original probe did not finish the exact case");
        // Keep original listener/peer/packet owners until actual wait+both EOFs.
        drop((accepted,peer,endpoint,packet,owner,listener));
    }
    original.remaining()?;println!("\n{PUBLISHER_DONE}");Ok(())
}
fn launcher()->Result<()>{
    strict_root()?;let original=clock()?;let owner=OriginalChild::run(exact_command(&std::env::current_exe()?,"publisher",&original)?,false,&original,|_|Ok(()))?;
    ensure!(lines(&owner.out,PUBLISHER_DONE)==1,"strict original publisher completion absent");println!("\n{LAUNCHER_DONE}");Ok(())
}
#[test]
#[ignore="requires genuine initial-namespace UID0 and existing SETGID/SETUID/SETPCAP; no skips, grants or SDK authority"]
fn sdk_helper_root_producer_witness_real_scm_data_checks(){
    match std::env::var(ROLE).as_deref(){Ok("probe")=>{probe().unwrap();return},Ok("publisher")=>{publisher().unwrap();return},Ok("launcher")=>{launcher().unwrap();return},Err(_)=>{},_=>panic!("strict witness role differs")}
    strict_root().unwrap();let original=FinalizationBudget::begin(Duration::from_secs(30)).unwrap();
    let directory=PathBuf::from(format!("/run/rudder-sdk-witness-fixture-{}",uuid::Uuid::new_v4()));fs::DirBuilder::new().mode(0o755).create(&directory).unwrap();fs::set_permissions(&directory,fs::Permissions::from_mode(0o755)).unwrap();
    let path=directory.join("probe");let mut source=File::open(std::env::current_exe().unwrap()).unwrap();let mut target=OpenOptions::new().write(true).create_new(true).mode(0o555).open(&path).unwrap();
    io::copy(&mut source,&mut target).unwrap();target.sync_all().unwrap();drop(target);fs::set_permissions(&path,fs::Permissions::from_mode(0o555)).unwrap();Image::capture(&path).unwrap();
    // Never signal a carrier owning a nested original child. Inner publisher
    // owns probe teardown; every carrier retains its SAME Child until EOF+wait.
    let owner=OriginalChild::run(exact_command(&path,"launcher",&original).unwrap(),false,&original,|_|Ok(())).unwrap();assert_eq!(lines(&owner.out,LAUNCHER_DONE),1);
    println!("\nRUDDER_SDK_ROOT_PRODUCER_WITNESS_SCM_DATA_V1=passed");
}
