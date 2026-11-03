//! Pure closed DATA projection tests; no SDK/process/retirement owner minted.
use super::*;
fn frame()->serde_json::Value{serde_json::json!({"kind":"rudder-root-retained-failure-v1","component":"compiler_sdk_helper","stage":"handshake","class":"io","io_kind":"permission_denied","errno":libc::EACCES,"check":"SdkHelperParentImage"})}
fn raw(value:&serde_json::Value)->Vec<u8>{let mut bytes=serde_json::to_vec(value).unwrap();bytes.extend_from_slice(b"\nError: private raw main tail is retained only\n");bytes}
fn check(bytes:&[u8],startup:bool)->String{bootstrap_failure_check(&failed_original_stderr(bytes,startup)).unwrap()}
#[test]fn fixed_first_original_startup_failure_preserves_only_closed_phase_and_os_error(){
    let bytes=raw(&frame());let error=failed_original_stderr(&bytes,true);
    assert_eq!(bootstrap_failure_check(&error).as_deref(),Some("SdkHelperParentImage"));
    assert_eq!(error.chain().find_map(|cause|cause.downcast_ref::<io::Error>()).unwrap().raw_os_error(),Some(libc::EACCES));
    let diagnostic=retained_bootstrap_diagnostic(&error,BootstrapCheck::SdkQualification);
    assert_eq!(bootstrap_failure_check(&diagnostic).as_deref(),Some("SdkHelperParentImage"));
    assert!(!format!("{diagnostic:#}").contains("private raw"));
    assert_eq!(check(&bytes,false),"SdkHelperExitUnknown");
}
#[test]fn malformed_foreign_duplicate_missing_inconsistent_or_partial_frames_are_unknown(){
    for(key,value)in [("kind",serde_json::json!("foreign")),("component",serde_json::json!("root_storage")),("stage",serde_json::json!("foreign")),("check",serde_json::json!("SdkReady")),("class",serde_json::json!("panic")),("io_kind",serde_json::json!("not_found")),("errno",serde_json::json!(0)),("errno",serde_json::json!(-13)),("errno",serde_json::json!(true)),("extra",serde_json::json!("private"))]{
        let mut data=frame();data[key]=value;assert_eq!(check(&raw(&data),true),"SdkHelperExitUnknown","{key}");
    }
    for key in ["kind","component","stage","check","class","io_kind","errno"]{let mut data=frame();data.as_object_mut().unwrap().remove(key);assert_eq!(check(&raw(&data),true),"SdkHelperExitUnknown");}
    let bytes=raw(&frame());let first=bytes.split(|byte|*byte==b'\n').next().unwrap();
    let mut duplicate=first[..first.len()-1].to_vec();duplicate.extend_from_slice(b",\"errno\":13}\n");assert_eq!(check(&duplicate,true),"SdkHelperExitUnknown");
    let mut multiple=bytes.clone();multiple.extend_from_slice(&bytes);assert_eq!(check(&multiple,true),"SdkHelperExitUnknown");
    let mut foreign=bytes.clone();foreign.extend_from_slice(b"  {\"kind\":\"foreign\"}\n");assert_eq!(check(&foreign,true),"SdkHelperExitUnknown");
    for invalid in [b"not JSON\n".to_vec(),bytes[..bytes.len()-1].to_vec(),vec![b'x';65537],Vec::new()]{assert_eq!(check(&invalid,true),"SdkHelperExitUnknown");}
}
#[test]fn relayed_tool_stderr_or_main_tail_cannot_be_promoted_to_helper_failure(){
    let mut injected=b"SDK_INNER_STDERR_PREFIX_DATA (not execution evidence)\n".to_vec();injected.extend_from_slice(&raw(&frame()));
    assert_eq!(check(&injected,true),"SdkHelperExitUnknown");
    let mut after_main=b"Error: fixed helper failed\n".to_vec();after_main.extend_from_slice(&raw(&frame()));assert_eq!(check(&after_main,true),"SdkHelperExitUnknown");
    let mut validation=frame();validation["class"]=serde_json::json!("validation_or_other");validation["errno"]=serde_json::Value::Null;validation["io_kind"]=serde_json::Value::Null;
    assert_eq!(check(&raw(&validation),true),"SdkHelperParentImage");
}
#[test]fn typed_operation_survives_original_helper_and_root_projection_without_private_text(){
    let error=anyhow::Error::new(io::Error::from_raw_os_error(libc::EACCES))
        .context("private path, compiler source and token must stay private")
        .context(SdkHelperOperation::RetainedMountReopen)
        .context(BootstrapCheck::SdkHelperNamespaceWorkload);
    let helper=storage::retained_failure_data("compiler_sdk_helper","handshake",Some(&error));
    assert_eq!(helper.as_object().unwrap().len(),8);
    assert_eq!(helper["error_text"],"SDK retained mount reopen failed: io_kind=permission_denied errno=13");
    let projected=failed_original_stderr(&raw(&helper),true);
    let diagnostic=retained_bootstrap_diagnostic(&projected,BootstrapCheck::SdkQualification);
    let root=storage::retained_failure_data("root_storage","serve_runtime",Some(&diagnostic));
    assert_eq!(root["error_text"],helper["error_text"]);
    assert_eq!(root["errno"],libc::EACCES);
    assert_eq!(root["check"],"SdkHelperNamespaceWorkload");
    assert!(!serde_json::to_string(&root).unwrap().contains("private"));
    assert!(!format!("{diagnostic:#}").contains("private"));
    let arbitrary=anyhow::anyhow!("SDK retained mount reopen failed").context(BootstrapCheck::SdkHelperNamespaceWorkload);
    assert!(storage::retained_failure_data("compiler_sdk_helper","handshake",Some(&arbitrary)).get("error_text").is_none());
}
#[test]fn optional_operation_text_is_closed_exact_and_never_required_for_historical_frames(){
    for operation in SdkHelperOperation::ALL{
        let mut data=frame();data["error_text"]=operation.text(Some("permission_denied"),Some(libc::EACCES)).into();
        assert_eq!(check(&raw(&data),true),"SdkHelperParentImage");
        assert_eq!(check(&raw(&data),false),"SdkHelperExitUnknown");
    }
    for invalid in [serde_json::Value::Null,serde_json::json!(true),
        serde_json::json!("private error"),serde_json::json!("SDK retained mount reopen failed: io_kind=not_found errno=13"),
        serde_json::json!("SDK retained mount reopen failed: io_kind=permission_denied errno=2"),
        serde_json::json!("SDK retained mount reopen failed: io_kind=permission_denied errno=13\n"),
        serde_json::json!("SDK retained mount reopen failed: io_kind=permission_denied errno=13 secret")]{
        let mut data=frame();data["error_text"]=invalid;assert_eq!(check(&raw(&data),true),"SdkHelperExitUnknown");
    }
    let mut data=frame();data["error_text"]=SdkHelperOperation::NamespaceMount.text(Some("permission_denied"),Some(libc::EACCES)).into();
    data.as_object_mut().unwrap().remove("io_kind");
    assert_eq!(check(&raw(&data),true),"SdkHelperExitUnknown");
    assert_eq!(check(&raw(&frame()),true),"SdkHelperParentImage");
}
