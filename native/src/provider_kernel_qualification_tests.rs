//! Shape tests grant no authority. Only the ignored actual-CLI test runs kernel
//! effects, with explicit Root-supplied fixed-image DATA and actual owned cleanup.
use super::*;
#[test]
fn provider_kernel_probe_frame_requires_complete_unique_bounded_line(){
    assert!(one_guest_frame(b"RUDDER_PROVIDER_KERNEL_GUEST {\"partial\":").unwrap().is_none());
    assert_eq!(one_guest_frame(b"unrelated\nRUDDER_PROVIDER_KERNEL_GUEST {}\n").unwrap(),Some(b"{}".as_slice()));
    assert!(one_guest_frame(b"RUDDER_PROVIDER_KERNEL_GUEST {}\nRUDDER_PROVIDER_KERNEL_GUEST {}\n").is_err());
    let raw=[b"RUDDER_PROVIDER_KERNEL_GUEST ".as_slice(),vec![b'x';16*1024].as_slice(),b"\n".as_slice()].concat();assert!(one_guest_frame(&raw).is_err());
}
#[test]
fn provider_kernel_probe_guest_data_refuses_wrong_nonce_capabilities_and_namespace(){
    let host:BTreeMap<String,NamespaceId>=["user","mnt","pid","net","uts"].into_iter().map(|n|(n.into(),NamespaceId{device:"1".into(),inode:"1".into()})).collect();
    let guest:BTreeMap<String,NamespaceId>=["user","mnt","pid","net","uts"].into_iter().map(|n|(n.into(),NamespaceId{device:"2".into(),inode:"2".into()})).collect();
    let value=json!({"format_version":1,"nonce":"shape-only","profile":PROFILE,"uid":1500,"gid":1500,"all_capabilities_zero":true,"no_new_privileges":true,"mask_empty_readonly":true,"merged_alias_masked":true,"provider_tools_masked":true,"private_and_old_root_denied":true,"inherited_fd_closed":true,"namespaces":guest,"interfaces":["lo"],"seccomp_mode":0,"seccomp_filter_installed_by_fixture":false,"denial_errno":{"mount":1,"unmount_mask":1,"setns":1,"ptrace_init":1},"nested_namespace":{"unshare_result":0,"unshare_errno":0,"unmount_errno":1,"payload_visible":false},"nested_child_reaped":true,"nested_pipe_eof":true});
    let parsed:Guest=serde_json::from_value(value.clone()).unwrap();validate_guest(&parsed,"shape-only",&host).unwrap();assert!(validate_guest(&parsed,"another",&host).is_err());
    for field in ["all_capabilities_zero","provider_tools_masked","mask_empty_readonly","inherited_fd_closed","nested_child_reaped"]{let mut bad=value.clone();bad[field]=json!(false);assert!(validate_guest(&serde_json::from_value(bad).unwrap(),"shape-only",&host).is_err());}
    let mut bad=value.clone();bad["namespaces"]["mnt"]=json!({"device":"1","inode":"1"});assert!(validate_guest(&serde_json::from_value(bad).unwrap(),"shape-only",&host).is_err());
    let mut filter=value;filter["seccomp_mode"]=json!(2);validate_guest(&serde_json::from_value(filter).unwrap(),"shape-only",&host).unwrap(); // no invented new seccomp policy
}
#[test]
fn provider_kernel_startup_owner_can_be_retained_across_actual_workers(){fn send_sync<T:Send+Sync+'static>(){}send_sync::<KernelStartupOwner>();}

#[test]
#[ignore="Root-only actual immutable-image Native CLI/namespace/FUSE/module-mask probe; no model calls"]
fn provider_kernel_actual_native_guest_probe()->Result<()> {
    let original=FinalizationBudget::begin(Duration::from_secs(30))?;
    let expected=std::env::var("RUDDER_PROVIDER_KERNEL_NATIVE_SHA256").context("explicit actual native SHA DATA required")?;
    let image=std::env::var("RUDDER_PROVIDER_KERNEL_IMAGE").context("explicit resolved immutable image DATA required")?;
    let base=PathBuf::from(std::env::var_os("RUDDER_PROVIDER_KERNEL_BASE").context("explicit private UID1500 parent required")?);
    let mut staged=None;KernelStartupProbe::begin_component_retaining(&mut staged,base,expected,image,original)?;let probe=staged.as_mut().unwrap();
    if probe.run_retaining().is_err(){let cleanup=FinalizationBudget::begin(Duration::from_secs(10))?;let cleanup_result=probe.recover_retaining(&cleanup);let _=probe.save_private_diagnostics();println!("RUDDER_PROVIDER_KERNEL_RECOVERY {}",probe.recovery_data());ensure!(cleanup_result.is_ok(),"actual kernel probe failed; unresolved original owners retained until VM teardown");return Err(fault());}
    let report=probe.component_report()?;let mut forbidden=None;ensure!(probe.take_startup_owner_retaining(&mut forbidden).is_err()&&forbidden.is_none(),"component fixture attempted to issue registered startup authority");
    let held=probe.state.as_ref().unwrap();let original_root=held.root.as_ref().unwrap().clone();let root_metadata=held.root_fd.as_ref().unwrap().metadata()?;
    let expected_root=(root_metadata.dev(),root_metadata.ino());let expected_view=held.spec.as_ref().unwrap().view_id.clone();let expected_nonce=held.nonce.clone();
    let cleanup=FinalizationBudget::begin(Duration::from_secs(30))?;let cancel=Arc::new(AtomicBool::new(false));
    let receipt=match probe.retire_own_fixture_retaining(&cleanup,&cancel) {
        Ok(receipt)=>receipt,
        Err(_)=>{println!("RUDDER_PROVIDER_KERNEL_RECOVERY {}",probe.recovery_data());return Err(fault());}
    };
    ensure!(receipt.probe_nonce()==expected_nonce&&receipt.view_id()==expected_view&&receipt.original_root_identity()==expected_root,"original fixture retirement ACK differs");
    ensure!(matches!(fs::symlink_metadata(&original_root),Err(ref e)if e.kind()==std::io::ErrorKind::NotFound),"own fixture namespace remains/replaced after actual retirement");
    let mut report=report;report["own_fixture_retired"]=json!(true);report["own_fixture_retirement_scope"]=json!("original-probe-only");
    ensure!(probe.recovery_data()["own_fixture_retired"]==json!(true),"actual retirement ACK not retained");
    println!("RUDDER_PROVIDER_KERNEL_COMPONENT {}",report);Ok(())
}
