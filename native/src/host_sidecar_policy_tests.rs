//! UNRUN packaging DATA regressions; no reconstructed custody/Root ACK.
use super::*;
fn manifest()->SidecarManifest {SidecarManifest {version:1,files:FILES.iter().enumerate().map(|(i,path)|SidecarFile {
    path:PathBuf::from(*path),sha256:format!("{i:064x}"),size_bytes:(i+1)as u64}).collect()}}
fn pin(manifest:&SidecarManifest,raw:&[u8])->serde_json::Value {serde_json::json!({
    "sidecar_manifest_sha256":format!("{:x}",Sha256::digest(raw)),"sidecar_core_sha256":manifest.files[0].sha256,
    "sidecar_script_sha256":manifest.files[1].sha256,"files":manifest.files})}
#[test]
fn complete_27_file_manifest_and_four_field_pin_are_data_only() {
    let manifest=manifest();validate_manifest(&manifest).unwrap();let raw=serde_json::to_vec(&manifest).unwrap();
    validate_pin(&serde_json::to_vec(&pin(&manifest,&raw)).unwrap(),&raw,&manifest).unwrap();
}
#[test]
fn absent_core_same_count_substitution_duplicate_alias_and_wrong_order_refuse() {
    let mut missing=manifest();missing.files.remove(0);assert!(validate_manifest(&missing).is_err());
    for path in ["extra.mjs","host-control.mjs","./host-control-core.mjs","node_modules/ws/../host-control-core.mjs"] {
        let mut replaced=manifest();replaced.files[0].path=path.into();assert!(validate_manifest(&replaced).is_err());
    }
    let mut reordered=manifest();reordered.files.swap(0,1);assert!(validate_manifest(&reordered).is_err());
}
#[test]
fn raw_manifest_core_and_script_hashes_cannot_be_substituted_or_defaulted() {
    let manifest=manifest();let raw=serde_json::to_vec(&manifest).unwrap();let valid=pin(&manifest,&raw);
    for field in ["sidecar_manifest_sha256","sidecar_core_sha256","sidecar_script_sha256"] {
        let mut changed=valid.clone();changed[field]=serde_json::Value::String("f".repeat(64));
        assert!(validate_pin(&serde_json::to_vec(&changed).unwrap(),&raw,&manifest).is_err());
        let mut removed=valid.clone();removed.as_object_mut().unwrap().remove(field);
        assert!(validate_pin(&serde_json::to_vec(&removed).unwrap(),&raw,&manifest).is_err());
    }
    let mut changed=raw.clone();changed.push(b' ');assert!(validate_pin(&serde_json::to_vec(&valid).unwrap(),&changed,&manifest).is_err());
}
#[test]
fn unknown_manifest_entry_and_pin_fields_and_nonpositive_sizes_refuse() {
    let manifest=manifest();let raw=serde_json::to_vec(&manifest).unwrap();
    let mut pin=pin(&manifest,&raw);pin["ready"]=true.into();assert!(validate_pin(&serde_json::to_vec(&pin).unwrap(),&raw,&manifest).is_err());
    let mut data=serde_json::to_value(&manifest).unwrap();data["ready"]=true.into();assert!(serde_json::from_value::<SidecarManifest>(data).is_err());
    let mut data=serde_json::to_value(&manifest).unwrap();data["files"][0]["authority"]=true.into();assert!(serde_json::from_value::<SidecarManifest>(data).is_err());
    let mut invalid=manifest.clone();invalid.files[0].size_bytes=0;assert!(validate_manifest(&invalid).is_err());
}

#[test]
fn qualification_module_is_exactly_required_not_an_arbitrary_new_module_allowlist() {
    let original=manifest();assert_eq!(original.files.last().unwrap().path,PathBuf::from("runtime-qualification.mjs"));
    let mut missing=original.clone();missing.files.pop();assert!(validate_manifest(&missing).is_err());
    let mut replaced=original;replaced.files.last_mut().unwrap().path="other-qualification.mjs".into();assert!(validate_manifest(&replaced).is_err());
}
