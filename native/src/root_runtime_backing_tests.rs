//! Negative/mechanism tests only. No synthetic completed registration, Root
//! ready ACK, RuntimeImageClient, SDK grant or positive backing owner exists.
use super::*;
struct TempFixture(PathBuf);
impl TempFixture {
    fn new()->Self {
        let base=if Path::new("/workspace").is_dir(){PathBuf::from("/workspace")}else{std::env::temp_dir()};
        let path=base.join(format!("rudder-backed-test-{}",uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();Self(path)
    }
    fn path(&self)->&Path{&self.0}
}
impl Drop for TempFixture {fn drop(&mut self){let _=fs::remove_dir_all(&self.0);}}

fn original()->FinalizationBudget{FinalizationBudget::begin(Duration::from_secs(30)).unwrap()}

#[test]
fn runtime_backing_original_cancel_arc_signals_held_eventfd_without_ready_owner(){
    let cancelled=Arc::new(AtomicBool::new(false));let mut state=NativeBackingState::staged(original(),cancelled.clone());
    state.cancel=Some(cancel_fd().unwrap());assert!(check_cancel(state.cancel.as_ref().unwrap()).is_ok());
    cancelled.store(true,Ordering::Release);state.signal_original_cancellation().unwrap();
    assert!(check_cancel(state.cancel.as_ref().unwrap()).is_err());assert!(state.original.remaining().is_err());
    assert!(!state.ready&&!state.issued&&state.baked.is_none()&&state.inputs.is_none());assert!(state.cancel.is_some());
}

#[test]
fn runtime_backing_original_deadline_cancellation_is_absorbing_without_sleep(){
    let budget=original();let constraint=budget.constraint();let mut state=NativeBackingState::staged(budget.clone(),Arc::new(AtomicBool::new(false)));
    state.cancel=Some(cancel_fd().unwrap());budget.cancel();state.signal_original_cancellation().unwrap();
    assert!(state.fence.remaining().is_err());assert_eq!(state.original.constraint(),constraint);
    assert!(check_cancel(state.cancel.as_ref().unwrap()).is_err());assert!(!state.ready&&!state.issued);
}

fn received_packet()->RuntimePacket{
    let (a,b)=wire::runtime_socket_pair().unwrap();let send=RuntimeEndpoint::capture(a).unwrap();let receive=RuntimeEndpoint::capture(b).unwrap();
    let one=cancel_fd().unwrap();let two=cancel_fd().unwrap();assert!(send.send_data(b"untrusted backing reply",&[&one,&two]).unwrap());
    receive.receive_data().unwrap().unwrap()
}

#[test]
fn runtime_backing_untrusted_packet_and_received_original_fds_are_staged_before_parse(){
    let mut state=NativeBackingState::staged(original(),Arc::new(AtomicBool::new(false)));
    state.packet=Some(received_packet());let packet=state.packet.as_ref().unwrap();
    assert!(serde_json::from_slice::<Ack>(&packet.bytes).is_err());assert_eq!(packet.fds.len(),2);
    for fd in &packet.fds{let flags=unsafe{libc::fcntl(fd.as_raw_fd(),libc::F_GETFD)};assert!(flags>=0&&flags&libc::FD_CLOEXEC!=0);}
    assert!(!state.ready&&!state.issued);assert!(state.baked.is_none()&&state.inputs.is_none());
    state.original.cancel();assert!(state.fence.remaining().is_err());assert!(state.packet.is_some());
    assert!(unsafe{libc::fcntl(state.packet.as_ref().unwrap().fds[0].as_raw_fd(),libc::F_GETFD)}>=0);
}

#[test]
fn runtime_backing_root_stages_actual_bad_request_packet_and_fds_without_classifying_helper(){
    let transfer=RootBackingTransfer::stage(received_packet());assert!(serde_json::from_slice::<Request>(&transfer.packet.bytes).is_err());
    assert!(transfer.request.is_none()&&transfer.original.is_none()&&!transfer.sent);assert_eq!(transfer.packet.fds.len(),2);
    assert!(unsafe{libc::fcntl(transfer.packet.fds[1].as_raw_fd(),libc::F_GETFD)}>=0);
    assert!(is_request(b"{\"kind\":\"runtime_environment_backing_v1\"}").unwrap());
    assert!(!is_request(b"{\"request_nonce\":\"helper-data\"}").unwrap());
    assert!(is_request(b"{").is_err());
}

#[test]
fn runtime_backing_descriptor_owner_refuses_changed_original_stamp_and_keeps_handles(){
    let temp=TempFixture::new();
    let path=temp.path().join("inventory");fs::write(&path,b"original").unwrap();
    let root=OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_CLOEXEC).open(temp.path()).unwrap();
    let file=OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC).open(&path).unwrap();
    let stamps=[stamp(&root).unwrap(),stamp(&file).unwrap()];
    let owner=RootBackingDescriptors{root:Some(root),inventory:Some(file),stamps:Some(stamps)};
    fs::write(&path,b"replacement bytes").unwrap();assert_ne!(stamp(owner.inventory.as_ref().unwrap()).unwrap(),owner.stamps.as_ref().unwrap()[1]);
    assert!(owner.current().is_err());assert!(owner.duplicate_for_baked().is_err());
    assert!(owner.root.is_some()&&owner.inventory.is_some());assert_eq!(owner.inventory.as_ref().unwrap().metadata().unwrap().len(),17);
}

#[test]
fn runtime_backing_actual_wrong_sender_refuses_before_parsing_and_keeps_original_fds(){
    let mut state=NativeBackingState::staged(original(),Arc::new(AtomicBool::new(false)));
    state.packet=Some(received_packet());let packet=state.packet.as_ref().unwrap();
    assert!(check_reply_sender(packet,std::process::id()+1).is_err());
    assert_eq!(packet.credentials.pid,std::process::id() as i32);assert_eq!(packet.fds.len(),2);
    assert!(unsafe{libc::fcntl(packet.fds[0].as_raw_fd(),libc::F_GETFD)}>=0);
    assert!(!state.ready&&!state.issued&&state.baked.is_none()&&state.inputs.is_none());
}

#[test]
fn runtime_backing_qualification_scope_data_can_only_restrict_never_issue_owner(){
    assert!(reject_qualification_data(&serde_json::json!({})).is_ok());
    assert!(reject_qualification_data(&serde_json::json!({"runtime_qualification":null})).is_ok());
    for scope in [serde_json::json!({"version":1}),serde_json::json!(true),serde_json::json!(false)]{
        assert!(reject_qualification_data(&serde_json::json!({"runtime_qualification":scope})).is_err());
    }
}
