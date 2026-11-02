//! Closed diagnostic DATA tests; no genuine SDK/Root/result authority forged.
use super::*;
fn fixed_frame()->Vec<u8>{
    let error=anyhow::anyhow!("private original workload failure").context(BootstrapCheck::SdkHelperNamespaceWorkload);
    let mut bytes=serde_json::to_vec(&storage::retained_failure_data("compiler_sdk_helper","handshake",Some(&error))).unwrap();bytes.push(b'\n');bytes
}
#[test]
fn fixed_first_encoded_inner_json_keeps_original_helper_phase_without_adopting_tool_data(){
    let inner=b"{\"kind\":\"foreign\"}\n\r\x0b\x0c\x1c\xff\0";
    let relay=inner_stderr_relay_bytes(inner);
    assert!(relay.len()<=4096&&relay.ends_with(b"\n"));
    assert_eq!(relay.iter().filter(|&&byte|byte==b'\n').count(),1);
    assert!(!relay.contains(&b'{')&&!relay.contains(&b'\r'));
    let mut bytes=fixed_frame();bytes.extend_from_slice(&relay);
    let error=super::super::handshake_diagnostic::failed_original_stderr(&bytes,true);
    assert_eq!(bootstrap_failure_check(&error).as_deref(),Some("SdkHelperNamespaceWorkload"));
    assert_eq!(bootstrap_failure_check(&super::super::handshake_diagnostic::failed_original_stderr(&bytes,false)).as_deref(),Some("SdkHelperExitUnknown"));
    let mut unencoded=fixed_frame();unencoded.extend_from_slice(inner);unencoded.push(b'\n');
    assert_eq!(bootstrap_failure_check(&super::super::handshake_diagnostic::failed_original_stderr(&unencoded,true)).as_deref(),Some("SdkHelperExitUnknown"));
}
#[test]
fn relay_is_exact_bounded_hex_prefix_and_cannot_promote_a_later_fixed_frame(){
    let raw:Vec<u8>=(0..=255).cycle().take(8192).collect();let bytes=inner_stderr_relay_bytes(&raw);
    let maximum=(4096-INNER_STDERR_HEX_PREFIX.len()-1)/2;
    assert_eq!(bytes.len(),INNER_STDERR_HEX_PREFIX.len()+maximum*2+1);assert!(bytes.len()<=4096);
    for(i,byte)in raw.iter().take(maximum).enumerate(){assert_eq!(&bytes[INNER_STDERR_HEX_PREFIX.len()+2*i..INNER_STDERR_HEX_PREFIX.len()+2*i+2],format!("{byte:02x}").as_bytes());}
    let mut prefixed=bytes;prefixed.extend_from_slice(&fixed_frame());
    assert_eq!(bootstrap_failure_check(&super::super::handshake_diagnostic::failed_original_stderr(&prefixed,true)).as_deref(),Some("SdkHelperExitUnknown"));
}
#[test]
fn helper_direct_writer_refuses_invalid_or_oversized_inputs_without_flag_effect(){
    assert!(!try_helper_diagnostic_write(-1,b"fixed\n"));assert!(!try_helper_diagnostic_write(-1,&[]));assert!(!try_helper_diagnostic_write(-1,&[0u8;4097]));
}
