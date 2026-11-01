//! UNRUN tests. The ignored physical probe qualifies only the native privilege
//! bridge/access refusal, never full launcher/Main integration or service drain.
use super::*;

fn budget()->FinalizationBudget {FinalizationBudget::begin(Duration::from_secs(2)).unwrap()}
fn data(original:&FinalizationBudget)->BootstrapData {BootstrapData {version:1,nonce:uuid::Uuid::new_v4().to_string(),constraint:original.constraint()}}
fn pair()->(OwnedFd,OwnedFd) {
    let mut raw=[-1;2];assert_eq!(unsafe {libc::socketpair(libc::AF_UNIX,libc::SOCK_SEQPACKET|libc::SOCK_NONBLOCK|libc::SOCK_CLOEXEC,0,raw.as_mut_ptr())},0);
    let first=unsafe {OwnedFd::from_raw_fd(raw[0])};let second=unsafe {OwnedFd::from_raw_fd(raw[1])};
    for fd in [&first,&second] {let yes=1i32;assert_eq!(unsafe {libc::setsockopt(fd.as_raw_fd(),libc::SOL_SOCKET,libc::SO_PASSCRED,
        (&yes as *const i32).cast(),std::mem::size_of::<i32>() as _)},0);} (first,second)
}

#[test]
fn original_time_data_is_sealed_readonly_and_cannot_be_extended_or_changed() {
    let original=budget();let stamp=data(&original);let fd=sealed_data(&stamp).unwrap();
    assert_eq!(unsafe {libc::fcntl(fd.as_raw_fd(),libc::F_GET_SEALS)} & SEALS,SEALS);
    let byte=[0u8];assert_eq!(unsafe {libc::write(fd.as_raw_fd(),byte.as_ptr().cast(),1)},-1);
    if unsafe {libc::getuid()}==0 && unsafe {libc::getgid()}==0 {let read=read_data(&fd).unwrap();assert_eq!(read.constraint,original.constraint());assert_eq!(read.nonce,stamp.nonce);}
    else {assert!(read_data(&fd).is_err());}
    assert!(read_data(&File::open("/dev/null").unwrap().into()).is_err());
}

fn protected_status()->String {
    "Uid:\t0\t0\t0\t0\nGid:\t0\t0\t0\t0\nGroups:\t\nCapEff:\t00000000000001c0\nCapPrm:\t00000000000001c0\nCapBnd:\t00000000000001c0\nCapInh:\t0000000000000000\nCapAmb:\t0000000000000000\nNoNewPrivs:\t1\nTracerPid:\t0\n".into()
}
#[test]
fn protected_root_profile_refuses_old_mixed_ids_empty_or_extra_caps_and_tracers() {
    let valid=protected_status();bridge_identity(&valid).unwrap();
    for invalid in [valid.replace("0\t0\t0\t0","0\t1500\t1500\t1500"),
        valid.replace("00000000000001c0","0000000000000000"),
        valid.replace("CapEff:\t00000000000001c0","CapEff:\t00000000000801c0"),
        valid.replace("NoNewPrivs:\t1","NoNewPrivs:\t0"),valid.replace("Groups:\t\n","Groups:\t1500\n"),
        valid.replace("TracerPid:\t0","TracerPid:\t123")] {assert!(bridge_identity(&invalid).is_err());}
}
#[test]
fn credential_transition_refuses_user_dumpable_root_dumpable_or_ambiguous_policy() {
    require_safe_dumpable_bytes(b"0\n").unwrap();
    for invalid in [b"1\n".as_slice(),b"2\n",b"0",b"0\n1\n",b"",b"00\n"] {assert!(require_safe_dumpable_bytes(invalid).is_err());}
}

#[test]
fn actual_kernel_gate_credentials_pidfd_and_descriptor_transfer_are_bound() {
    let original=budget();let stamp=data(&original);let (first,second)=pair();
    let own=Process::current().unwrap();let uid=unsafe {libc::getuid()};
    let mut sender=Gate::capture(first,own.fd().try_clone().unwrap(),uid,stamp.clone(),original.clone()).unwrap();
    let mut receiver=Gate::capture(second,own.fd().try_clone().unwrap(),uid,stamp,original).unwrap();
    let transferred=storage::eventfd().unwrap();sender.send_startup(Message::EngineWaiting,Some(&transferred)).unwrap();
    if unsafe {libc::getgid()}!=uid {assert!(receiver.receive_startup(Message::EngineWaiting).is_err());return;}
    let fd=receiver.receive_startup(Message::EngineWaiting).unwrap().unwrap();
    assert_eq!(fs::read_link(format!("/proc/self/fd/{}",fd.as_raw_fd())).unwrap(),Path::new("anon_inode:[eventfd]"));
    assert_ne!(unsafe {libc::fcntl(fd.as_raw_fd(),libc::F_GETFD)} & libc::FD_CLOEXEC,0);
    assert_eq!(receiver.peer.pid,std::process::id());receiver.peer.live().unwrap();
}

#[test]
fn cancellation_uses_same_parent_budget_and_retains_actual_gate_and_pidfd() {
    let original=budget();let stamp=data(&original);let (first,second)=pair();let own=Process::current().unwrap();
    let mut sender=Gate::capture(first,own.fd().try_clone().unwrap(),unsafe {libc::getuid()},stamp,original.clone()).unwrap();
    let receiver=Endpoint::capture(second).unwrap();original.cancel();
    assert!(sender.send_startup(Message::EngineWaiting,None).is_err());assert!(receiver.receive().unwrap().is_none());
    sender.peer.live().unwrap();assert_eq!(sender.peer.pid,own.pid);
}

#[test]
fn parent_authentication_acceptance_cannot_substitute_for_actual_worker_ack() {
    let original=budget();let stamp=data(&original);let (first,second)=pair();let own=Process::current().unwrap();let uid=unsafe {libc::getuid()};
    let mut sender=Gate::capture(first,own.fd().try_clone().unwrap(),uid,stamp.clone(),original.clone()).unwrap();
    let mut receiver=Gate::capture(second,own.fd().try_clone().unwrap(),uid,stamp,original).unwrap();
    sender.send_startup(Message::ParentAccepted,None).unwrap();assert!(receiver.receive_host_serving().is_err());
    assert!(receiver.host_ack.is_none());receiver.peer.live().unwrap();
}

#[test]
fn actual_non_main_wire_ack_is_after_retained_join_and_bad_descriptor_keeps_receipt_data() {
    // Transport conformance only. This never constructs a Root client or an
    // established MachineRootBootstrap, and grants no worker readiness proof.
    for extra in [false,true] {
        let original=budget();let stamp=data(&original);let (first,second)=pair();let own=Process::current().unwrap();let uid=unsafe {libc::getuid()};
        let mut sender=Gate::capture(first,own.fd().try_clone().unwrap(),uid,stamp.clone(),original.clone()).unwrap();
        let mut receiver=Gate::capture(second,own.fd().try_clone().unwrap(),uid,stamp,original).unwrap();
        let (release,wait)=std::sync::mpsc::sync_channel(0);
        let worker=std::thread::spawn(move || {let tid=unsafe {libc::syscall(libc::SYS_gettid)} as u32;wait.recv().unwrap();
            let fd=storage::eventfd().unwrap();sender.send_startup(Message::HostServing(tid),extra.then_some(&fd)).unwrap();tid});
        release.send(()).unwrap(); // actual original JoinHandle already retained
        let received=receiver.receive_host_serving();let tid=worker.join().unwrap();assert_ne!(tid,std::process::id());
        if unsafe {libc::getgid()}!=uid {assert!(received.is_err());continue;}
        assert_eq!(receiver.host_ack,Some(tid));
        if extra {assert!(received.is_err());assert!(unsafe {libc::fcntl(receiver.host_ack_fd.as_ref().unwrap().as_raw_fd(),libc::F_GETFD)}>=0);}
        else {assert_eq!(received.unwrap(),tid);assert!(receiver.host_ack_fd.is_none());}
        receiver.peer.live().unwrap();
    }
}

fn observation_pipe()->(OwnedFd,OwnedFd) {
    let mut raw=[-1;2];assert_eq!(unsafe {libc::pipe2(raw.as_mut_ptr(),libc::O_NONBLOCK|libc::O_CLOEXEC)},0);
    (unsafe {OwnedFd::from_raw_fd(raw[0])},unsafe {OwnedFd::from_raw_fd(raw[1])})
}
fn ignored_sigpipe()->bool {
    let mut action:libc::sigaction=unsafe {std::mem::zeroed()};assert_eq!(unsafe {libc::sigaction(libc::SIGPIPE,std::ptr::null(),&mut action)},0);
    action.sa_sigaction==libc::SIG_IGN
}
#[test]
fn observation_backpressure_or_broken_reader_never_blocks_or_loses_actual_output_owner() {
    let (read,write)=observation_pipe();let line=b"{\"data_only\":true}\n";
    let expected=if ignored_sigpipe() {StartupObservationDelivery::Delivered}else {StartupObservationDelivery::Unsupported};
    assert_eq!(try_observation_write(&write,line).unwrap(),expected);
    if expected==StartupObservationDelivery::Delivered {let mut actual=[0u8;64];let n=unsafe {libc::read(read.as_raw_fd(),actual.as_mut_ptr().cast(),actual.len())};
        assert_eq!(n,line.len() as isize);assert_eq!(&actual[..n as usize],line);}
    let bytes=[b'x';4096];let mut full=false;
    for _ in 0..=256 {let n=unsafe {libc::write(write.as_raw_fd(),bytes.as_ptr().cast(),bytes.len())};
        if n<0 {assert_eq!(io::Error::last_os_error().kind(),io::ErrorKind::WouldBlock);full=true;break;}assert_eq!(n,bytes.len() as isize);}
    assert!(full);let expected=if ignored_sigpipe() {StartupObservationDelivery::Blocked}else {StartupObservationDelivery::Unsupported};
    assert_eq!(try_observation_write(&write,b"{\"data_only\":true}\n").unwrap(),expected);
    assert!(unsafe {libc::fcntl(write.as_raw_fd(),libc::F_GETFD)}>=0);drop(read);
    let expected=if ignored_sigpipe() {StartupObservationDelivery::Failed}else {StartupObservationDelivery::Unsupported};
    assert_eq!(try_observation_write(&write,b"{\"data_only\":true}\n").unwrap(),expected);
    assert!(unsafe {libc::fcntl(write.as_raw_fd(),libc::F_GETFD)}>=0);
}
#[test]
fn observation_unsupported_output_and_oversized_data_do_not_close_the_original_fd() {
    let fd:OwnedFd=OpenOptions::new().write(true).open("/dev/null").unwrap().into();
    assert_eq!(try_observation_write(&fd,b"{\"data_only\":true}\n").unwrap(),StartupObservationDelivery::Unsupported);
    assert!(try_observation_write(&fd,&vec![0;OBSERVATION_LIMIT+1]).is_err());
    assert!(unsafe {libc::fcntl(fd.as_raw_fd(),libc::F_GETFD)}>=0);
}

#[test]
fn ordinary_returned_unix_spawn_error_creates_no_owned_child_or_replacement_pid() {
    let mut slot=None;let mut command=Command::new("/rudder-fixed-nonexistent-bootstrap-test");command.env_clear();
    assert!(matches!(spawn_retaining(&mut slot,&mut command).unwrap(),SpawnEffect::DefiniteError));assert!(slot.is_none());
}

struct Children(Vec<Option<ChildOwner>>);
impl Drop for Children {fn drop(&mut self) {for slot in &mut self.0 {if let Some(owner)=slot {
    if let Some(process)=&owner.process {let _=unsafe {libc::syscall(libc::SYS_pidfd_send_signal,process.fd().as_raw_fd(),libc::SIGKILL,std::ptr::null::<libc::siginfo_t>(),0)};}
    else {let _=owner.child.kill();}let _=owner.child.wait();}}}}
#[test]
fn actual_child_and_original_pidfd_stay_owned_after_physical_exit_refusal() {
    let mut children=Children(vec![None]);let mut command=Command::new("/bin/sleep");
    command.arg("30").env_clear().stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    assert!(matches!(spawn_retaining(&mut children.0[0],&mut command).unwrap(),SpawnEffect::ChildStored));
    let owner=children.0[0].as_mut().unwrap();let original_pid=owner.original().unwrap().pid;
    assert_eq!(original_pid,owner.child.id());owner.live().unwrap();owner.child.kill().unwrap();owner.child.wait().unwrap();
    assert!(owner.live().is_err());assert!(owner.original().unwrap().exited().unwrap());assert_eq!(owner.original().unwrap().pid,original_pid);
}

// Fixed test-only alias. There is no production image-path override and these
// roles never construct a RootStorageClient or fabricate any service ACK.
const PROBE_ELF:&str="/usr/local/bin/rudder-machine-root-bootstrap-test";
fn filter(name:&str)->String {format!("{}::{name}",module_path!().split_once("::").unwrap().1)}
fn test_fd(fd:i32)->Result<OwnedFd> {ensure!(unsafe {libc::fcntl(fd,libc::F_GETFD)}>=0,"bridge test original fd absent");
    let fd=unsafe {OwnedFd::from_raw_fd(fd)};storage::fd_cloexec(&fd)?;Ok(fd)}
fn test_send(endpoint:&Endpoint,bytes:&[u8],original:&FinalizationBudget)->Result<()> {
    loop {original.remaining()?;if endpoint.send(bytes,None)? {return Ok(());}std::thread::sleep(Duration::from_millis(2));}
}
fn test_receive(endpoint:&Endpoint,peer:&Process,uid:u32,original:&FinalizationBudget)->Result<Vec<u8>> {
    loop {peer.live()?;original.remaining()?;if let Some(packet)=endpoint.receive()? {
        ensure!(packet.fds.is_empty() && packet.credentials.pid==peer.pid as i32 && packet.credentials.uid==uid && packet.credentials.gid==uid,
            "bridge test original kernel peer differs");return Ok(packet.bytes);}
        std::thread::sleep(Duration::from_millis(2));}
}
fn test_wait(owner:&mut ChildOwner,original:&FinalizationBudget)->Result<()> {
    loop {original.remaining()?;if let Some(status)=owner.child.try_wait()? {ensure!(status.success() && owner.original()?.exited()?,"bridge test original child failed");return Ok(());}
        std::thread::sleep(Duration::from_millis(2));}
}

const DIAGNOSTIC_PIPE_LIMIT:usize=8192;
fn credential_observation(status:&str)->String {
    status.lines().filter(|line|["Uid:","Gid:","Groups:","CapEff:","CapPrm:","CapInh:","CapAmb:","CapBnd:","NoNewPrivs:","TracerPid:"]
        .iter().any(|field|line.starts_with(field))).collect::<Vec<_>>().join("; ")
}
// Test-only, appended AFTER the production privilege bridge pre_exec hook.
// Stack arithmetic + raw syscalls/write only; FD2 is the fixed captured pipe.
unsafe fn pre_exec_identity_line() {
    let mut u=[0u32;3];let mut g=[0u32;3];
    let up=u.as_mut_ptr();let gp=g.as_mut_ptr();
    if libc::getresuid(up,up.add(1),up.add(2))!=0 || libc::getresgid(gp,gp.add(1),gp.add(2))!=0 {return;}
    let values=[u[0],u[1],u[2],libc::setfsuid(u32::MAX) as u32,g[0],g[1],g[2],libc::setfsgid(u32::MAX) as u32];
    let mut bytes=[0u8;256];let mut at=0usize;
    for byte in b"BRIDGE_PRE_EXEC Uid(real,effective,saved,fs)/Gid(real,effective,saved,fs): " {bytes[at]=*byte;at+=1;}
    for value in values {let mut digits=[0u8;10];let mut n=0usize;let mut value=value;
        loop {digits[n]=b'0'+(value%10) as u8;n+=1;value/=10;if value==0 {break;}}
        for index in (0..n).rev() {bytes[at]=digits[index];at+=1;}bytes[at]=b' ';at+=1;
    }bytes[at]=b'\n';at+=1;let _=libc::write(2,bytes.as_ptr().cast(),at);
}
// Exact pinned libtest (58cf8f9c...) falls back to its CURRENT thread only when
// Builder::spawn returns WouldBlock. A fixed child-only filter makes that path
// explicit; otherwise the worker-only bounding drop would miss libtest's main
// thread. No production command installs this filter or accepts an override.
unsafe fn single_thread_libtest_role()->io::Result<()> {
    #[cfg(target_arch="x86_64")]
    {
        let mut instructions=[
            libc::sock_filter {code:0x20,jt:0,jf:0,k:4}, // seccomp_data.arch
            libc::sock_filter {code:0x15,jt:1,jf:0,k:0xc000003e}, // AUDIT_ARCH_X86_64
            libc::sock_filter {code:0x06,jt:0,jf:0,k:0x80000000}, // KILL_PROCESS if arch differs
            libc::sock_filter {code:0x20,jt:0,jf:0,k:0}, // seccomp_data.nr
            libc::sock_filter {code:0x15,jt:1,jf:0,k:libc::SYS_clone as u32},
            libc::sock_filter {code:0x15,jt:0,jf:1,k:libc::SYS_clone3 as u32},
            libc::sock_filter {code:0x06,jt:0,jf:0,k:0x00050000|libc::EAGAIN as u32}, // ERRNO(EAGAIN)
            libc::sock_filter {code:0x06,jt:0,jf:0,k:0x7fff0000}, // ALLOW
        ];
        let program=libc::sock_fprog {len:instructions.len() as u16,filter:instructions.as_mut_ptr()};
        if libc::syscall(libc::SYS_seccomp,1,0,&program as *const libc::sock_fprog)!=0 {return Err(io::Error::last_os_error());}Ok(())
    }
    #[cfg(not(target_arch="x86_64"))]
    {Err(io::Error::from_raw_os_error(libc::ENOTSUP))}
}
struct ChildDiagnostic {label:String,stdout:Vec<u8>,stderr:Vec<u8>,stdout_eof:bool,stderr_eof:bool,truncated:bool,status:Option<ExitStatus>}
impl ChildDiagnostic {
    fn new(label:&str)->Self {Self {label:label.into(),stdout:Vec::new(),stderr:Vec::new(),stdout_eof:false,stderr_eof:false,truncated:false,status:None}}
}
fn pipe_nonblocking(fd:i32)->Result<()> {
    let flags=unsafe {libc::fcntl(fd,libc::F_GETFL)};
    ensure!(flags>=0 && unsafe {libc::fcntl(fd,libc::F_SETFL,flags|libc::O_NONBLOCK)}==0,"bridge diagnostic pipe cannot become nonblocking");Ok(())
}
fn drain_diagnostic(pipe:&mut impl std::io::Read,stored:&mut Vec<u8>,eof:&mut bool,truncated:&mut bool)->Result<()> {
    if *eof {return Ok(());}
    let mut bytes=[0u8;4096];
    for _ in 0..16 {match pipe.read(&mut bytes) {
        Ok(0)=>{*eof=true;return Ok(());},Ok(n)=>{let keep=n.min(DIAGNOSTIC_PIPE_LIMIT.saturating_sub(stored.len()));stored.extend_from_slice(&bytes[..keep]);*truncated|=keep<n;},
        Err(e) if e.kind()==io::ErrorKind::WouldBlock=>return Ok(()),Err(e) if e.kind()==io::ErrorKind::Interrupted=>continue,Err(e)=>return Err(e.into()),
    }}Ok(())
}
fn drain_child(owner:&mut ChildOwner,diag:&mut ChildDiagnostic)->Result<()> {
    if let Some(pipe)=&mut owner.child.stdout {pipe_nonblocking(pipe.as_raw_fd())?;drain_diagnostic(pipe,&mut diag.stdout,&mut diag.stdout_eof,&mut diag.truncated)?;}
    else {diag.stdout_eof=true;}
    if let Some(pipe)=&mut owner.child.stderr {pipe_nonblocking(pipe.as_raw_fd())?;drain_diagnostic(pipe,&mut diag.stderr,&mut diag.stderr_eof,&mut diag.truncated)?;}
    else {diag.stderr_eof=true;}
    if diag.status.is_none() {diag.status=owner.child.try_wait()?;}
    Ok(())
}
fn spawn_diagnostic_retaining(slot:&mut Option<ChildOwner>,command:&mut Command)->Result<()> {
    ensure!(slot.is_none(),"diagnostic original child already stored");
    // Preserve the actual pre_exec/exec errno when the pinned Unix runtime
    // returns a definite spawn Err, including unavailable seccomp/fallback.
    let child=command.spawn().context("diagnostic fixed native pre_exec/exec returned spawn error")?;
    *slot=Some(ChildOwner {child,process:None,status:None});
    let owner=slot.as_mut().unwrap();owner.process=Some(pin_original(&owner.child)?);Ok(())
}
fn receive_diagnostic(endpoint:&Endpoint,children:&mut Children,diags:&mut [ChildDiagnostic],index:usize,uid:u32,
    original:&FinalizationBudget,phase:&str)->Result<Vec<u8>> {
    loop {original.remaining().with_context(||format!("{phase}: original deadline"))?;
        let owner=children.0[index].as_mut().context("diagnostic original child absent")?;drain_child(owner,&mut diags[index]).with_context(||format!("{phase}: child pipes"))?;
        let peer=owner.original()?;peer.live().with_context(||format!("{phase}: {} original child exited",diags[index].label))?;
        if let Some(packet)=endpoint.receive().with_context(||format!("{phase}: gate receive"))? {
            ensure!(packet.fds.is_empty() && packet.credentials.pid==peer.pid as i32 && packet.credentials.uid==uid && packet.credentials.gid==uid,
                "{phase}: original kernel gate peer differs");return Ok(packet.bytes);
        }std::thread::sleep(Duration::from_millis(2));}
}
fn wait_diagnostic(children:&mut Children,diags:&mut [ChildDiagnostic],index:usize,original:&FinalizationBudget,phase:&str)->Result<()> {
    loop {original.remaining().with_context(||format!("{phase}: original deadline"))?;
        let owner=children.0[index].as_mut().context("diagnostic original child absent")?;let diag=&mut diags[index];
        drain_child(owner,diag).with_context(||format!("{phase}: child pipes/wait"))?;
        if let Some(status)=diag.status {ensure!(status.success(),"{phase}: actual child failed: {status}");
            if diag.stdout_eof && diag.stderr_eof {ensure!(owner.original()?.exited()?,"{phase}: original pidfd exit unconfirmed");return Ok(());}}
        std::thread::sleep(Duration::from_millis(2));}
}
fn recover_diagnostics(children:&mut Children,diags:&mut [ChildDiagnostic])->String {
    // Failure diagnostics use a NEW bounded recovery operation only. No startup
    // deadline is renewed and this produces no passing/refusal/retirement proof.
    for owner in children.0.iter_mut().flatten() {if let Some(process)=&owner.process {let _=unsafe {libc::syscall(libc::SYS_pidfd_send_signal,
        process.fd().as_raw_fd(),libc::SIGKILL,std::ptr::null::<libc::siginfo_t>(),0)};}else {let _=owner.child.kill();}}
    if let Ok(recovery)=FinalizationBudget::begin(Duration::from_secs(3)) {loop {
        let mut all=true;for (index,slot) in children.0.iter_mut().enumerate() {if let (Some(owner),Some(diag))=(slot,diags.get_mut(index)) {
            let _=drain_child(owner,diag);all&=diag.status.is_some() && diag.stdout_eof && diag.stderr_eof;}}
        if all || recovery.remaining().is_err() {break;}std::thread::sleep(Duration::from_millis(2));
    }}
    diags.iter().enumerate().map(|(index,diag)|format!("child[{index}] {} status={:?} stdout_eof={} stderr_eof={} truncated={}\nstdout={}\nstderr={}\n",
        diag.label,diag.status,diag.stdout_eof,diag.stderr_eof,diag.truncated,String::from_utf8_lossy(&diag.stdout),String::from_utf8_lossy(&diag.stderr))).collect()
}

#[test]
#[ignore="Root-only fixed-ELF privilege bridge probe on an exact-owned disposable VM"]
fn machine_root_privilege_bridge_kernel_qualification()->Result<()> {
    ensure!(cfg!(target_arch="x86_64"),"fixed libtest single-thread probe requires x86_64");
    ensure!(unsafe {libc::getuid()}==0 && unsafe {libc::geteuid()}==0 && unsafe {libc::getgid()}==0 && unsafe {libc::getegid()}==0,"bridge probe requires root UID/GID");
    let policy=PrivilegeDropPolicy::capture().context("parent: retain actual safe credential-drop policy")?;
    for (held,name,kind) in [(&policy.mnt,"mnt",libc::CLONE_NEWNS),(&policy.user,"user",libc::CLONE_NEWUSER)] {
        ensure!(storage::id(held)?==storage::id(&storage::namespace(Some(1),name,kind)?)?,"parent: probe is not in original global namespaces");
    }
    let original=FinalizationBudget::begin(Duration::from_secs(30))?;let stamp=data(&original);let sealed=sealed_data(&stamp).context("parent: seal original time DATA")?;
    let image=Image::capture(Path::new(PROBE_ELF)).context("parent: capture fixed pinned ELF")?;let own=Process::current()?;image.matches_process(&own).context("parent: actual pinned ELF identity")?;
    let (parent,target)=pair();let parent=Endpoint::capture(parent)?;let mut children=Children(vec![None]);
    let mut diagnostics=vec![ChildDiagnostic::new("target.protected-root-phase")];
    let result=(||->Result<()> {
        let args=["--exact".to_string(),filter("privilege_bridge_target_role"),"--ignored".into(),"--nocapture".into(),"--test-threads=1".into()];
        let argv=args.iter().map(String::as_str).collect::<Vec<_>>();
        let (mut command,held)=mapped_command(PROBE_ELF,&argv,true,&[(&target,3),(own.fd(),4),(&sealed,5)]).context("target.spawn: fixed descriptor map")?;
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        unsafe {command.pre_exec(||{pre_exec_identity_line();single_thread_libtest_role()});}
        spawn_diagnostic_retaining(&mut children.0[0],&mut command).context("target.spawn: store original Child/pidfd")?;drop(held);drop(target);
        image.matches_process(children.0[0].as_ref().unwrap().original()?).context("target.exec: actual pinned ELF")?;
        ensure!(receive_diagnostic(&parent,&mut children,&mut diagnostics,0,0,&original,"target.bridge-held")?==b"bridge_held","target.bridge-held: stage differs");
        bridge_identity(&String::from_utf8(storage::proc_bytes(&format!("/proc/{}/status",children.0[0].as_ref().unwrap().original()?.pid),16384)?)?)
            .context("parent: actual protected root kernel profile")?;policy.check()?;
        let mut proofs=Vec::new();
        for stage in ["protected_root_limited_caps","final_uid1500_nd0"] {
            if stage=="final_uid1500_nd0" {test_send(&parent,b"finish_floor",&original).context("target.final-floor: release original bridge barrier")?;
                ensure!(receive_diagnostic(&parent,&mut children,&mut diagnostics,0,1500,&original,"target.final-floor-held")?==b"floor_held","target.final-floor-held: stage differs");
                storage::floor(children.0[0].as_ref().unwrap().original()?.pid,1500).context("parent: actual complete UID1500 kernel floor")?;policy.check()?;}
            let (probe_parent,probe_child)=pair();let probe_parent=Endpoint::capture(probe_parent)?;children.0.push(None);let index=children.0.len()-1;
            diagnostics.push(ChildDiagnostic::new(&format!("same-uid-probe.{stage}")));
            let args=["--exact".to_string(),filter("same_uid_access_probe_role"),"--ignored".into(),"--nocapture".into(),"--test-threads=1".into()];
            let argv=args.iter().map(String::as_str).collect::<Vec<_>>();
            let (mut command,held)=mapped_command(PROBE_ELF,&argv,true,&[(&probe_child,3),(children.0[0].as_ref().unwrap().original()?.fd(),4),(&sealed,5),(own.fd(),6)])
                .with_context(||format!("{stage}.probe.spawn: fixed descriptor map"))?;
            command.stdout(Stdio::piped()).stderr(Stdio::piped());
            unsafe {command.pre_exec(||{pre_exec_identity_line();single_thread_libtest_role()});}
            spawn_diagnostic_retaining(&mut children.0[index],&mut command).with_context(||format!("{stage}.probe.spawn: store original Child/pidfd"))?;drop(held);drop(probe_child);
            let proof=receive_diagnostic(&probe_parent,&mut children,&mut diagnostics,index,1500,&original,&format!("{stage}.probe.refusal-result"))?;
            let proof:serde_json::Value=serde_json::from_slice(&proof).with_context(||format!("{stage}.probe.result: JSON"))?;
            ensure!(proof["uid"]==1500 && proof["ptrace_refused"]==true && proof["proc_fd_readlink_refused"]==true,"{stage}.probe.result: actual access was not refused");
            test_send(&probe_parent,b"exit",&original).with_context(||format!("{stage}.probe.exit: authenticated barrier"))?;
            wait_diagnostic(&mut children,&mut diagnostics,index,&original,&format!("{stage}.probe.reap-and-pipe-eof"))?;
            children.0[0].as_mut().unwrap().live().with_context(||format!("{stage}: original target still live"))?;
            proofs.push(serde_json::json!({"stage":stage,"probe":proof}));
        }
        test_send(&parent,b"exit",&original).context("target.exit: authenticated barrier")?;
        wait_diagnostic(&mut children,&mut diagnostics,0,&original,"target.reap-and-pipe-eof")?;
        println!("{}",serde_json::json!({"scope":"machine-root-native-privilege-bridge-only","pinned_elf_sha256":image.sha256,
            "actual_same_uid_refusal_both_stages":true,"probes":proofs,"actual_children_reaped":3,
            "protected_root_caps_hex":"00000000000001c0","host_suid_dumpable":0,"test_roles_original_main_single_thread":true,
            "production_launcher":false,"main_route":false,"normal_retirement":false,"models":false}));Ok(())
    })();
    match result {Ok(())=>Ok(()),Err(error)=>{
        let captured=recover_diagnostics(&mut children,&mut diagnostics);
        // A stalled kill/reap/EOF never blocks diagnostics forever. Keep actual
        // child/pidfd/pipe owners through this failed command's process lifetime;
        // the existing exact-owned VM teardown remains the final recovery gate.
        let _retained=Box::leak(Box::new(children));
        Err(error.context(format!("bridge physical child diagnostics (bounded)\n{captured}")))
    }}
}

#[test]
#[ignore="Only paired root privilege bridge probe may invoke this target role"]
fn privilege_bridge_target_role()->Result<()> {
    protect_early_entry().context("target.entry: FIRST native ND0")?;
    single_original_main_thread().context("target.entry: exact libtest fallback must be original main/one task")?;
    ensure!(std::env::vars_os().next().is_none(),"target.env: bridge test env not cleared");
    let status=current_status().context("target.entry: current kernel status")?;
    let dumpable=unsafe {libc::prctl(libc::PR_GET_DUMPABLE,0,0,0,0)};
    bridge_identity(&status).with_context(||format!("target.entry: protected bridge identity; actual {}; PR_GET_DUMPABLE={dumpable}",credential_observation(&status)))?;
    let endpoint=Endpoint::capture(test_fd(3)?).context("target.entry: inherited gate")?;let parent=Process::capture(test_fd(4)?).context("target.entry: original controller pidfd")?;let sealed=test_fd(5)?;
    let stamp=read_data(&sealed).context("target.entry: actual sealed original DATA")?;let original=FinalizationBudget::from_constraint(&stamp.constraint)?;
    test_send(&endpoint,b"bridge_held",&original).context("target.bridge-held: send")?;
    ensure!(test_receive(&endpoint,&parent,0,&original).context("target.bridge-held: wait release")?==b"finish_floor","target.bridge-held: parent stage differs");
    let mut policy=None;finish_engine_floor(&mut policy).context("target.final-floor: native ND0/all-ID and cap drop")?;
    test_send(&endpoint,b"floor_held",&original).context("target.final-floor: send")?;
    ensure!(test_receive(&endpoint,&parent,0,&original).context("target.final-floor: wait exit")?==b"exit","target.final-floor: parent exit differs");Ok(())
}

#[test]
#[ignore="Only paired root privilege bridge probe may invoke this actual UID1500 adversary"]
fn same_uid_access_probe_role()->Result<()> {
    protect_early_entry().context("probe.entry: FIRST native ND0")?;
    single_original_main_thread().context("probe.entry: exact libtest fallback must be original main/one task")?;
    let status=current_status().context("probe.entry: current kernel status before floor")?;
    let dumpable=unsafe {libc::prctl(libc::PR_GET_DUMPABLE,0,0,0,0)};
    let mut policy=None;finish_engine_floor(&mut policy).with_context(||format!("probe.entry: native ND0/all-ID and cap drop; pre-floor actual {}; PR_GET_DUMPABLE={dumpable}",credential_observation(&status)))?;
    ensure!(std::env::vars_os().next().is_none(),"probe.entry: env not cleared");
    let endpoint=Endpoint::capture(test_fd(3)?).context("probe.entry: inherited gate")?;
    let target=Process::capture(test_fd(4)?).context("probe.entry: original target pidfd")?;let sealed=test_fd(5)?;
    let controller=Process::capture(test_fd(6)?).context("probe.entry: original controller pidfd")?;
    let stamp=read_data(&sealed).context("probe.entry: actual sealed original DATA")?;let original=FinalizationBudget::from_constraint(&stamp.constraint)?;
    // Controller authentication uses the connected pair's fixed root peer;
    // the target pidfd is ONLY identity DATA for these actual refusal probes.
    let result=unsafe {libc::ptrace(libc::PTRACE_SEIZE,target.pid as libc::pid_t,std::ptr::null_mut::<libc::c_void>(),std::ptr::null_mut::<libc::c_void>())};
    let ptrace_error=io::Error::last_os_error().raw_os_error();
    let link=fs::read_link(format!("/proc/{}/fd/3",target.pid));let proc_error=link.as_ref().err().and_then(|e|e.raw_os_error());
    ensure!(result==-1 && matches!(ptrace_error,Some(libc::EPERM)|Some(libc::EACCES))
        && matches!(proc_error,Some(libc::EPERM)|Some(libc::EACCES)),"probe.refusal: target access allowed/uncertain, ptrace={ptrace_error:?}, proc-fd={proc_error:?}");target.live().context("probe.refusal: original target still live")?;
    test_send(&endpoint,&canonical_json(&serde_json::json!({"uid":unsafe {libc::getuid()},"ptrace_refused":true,
        "ptrace_errno":ptrace_error,"proc_fd_readlink_refused":true,"proc_fd_errno":proc_error}))?,&original)?;
    ensure!(test_receive(&endpoint,&controller,0,&original).context("probe.refusal: wait controller exit")?==b"exit","probe.refusal: controller exit differs");Ok(())
}
