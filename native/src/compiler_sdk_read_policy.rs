//! Pure agreement with DATA from the original authenticated SDK helper. This
//! validator neither constructs execution authority nor enables reusable hits.
use super::*;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ChildReadObservation {
    format_version:u32,kind:String,policy_sha256:String,architecture:String,
    isolation_sha256:String,mount_count:u32,source_declarations:String,
    memfd_errno:i32,execveat_errno:i32,io_uring_errno:i32,sysv_ipc_errno:i32,keyring_errno:i32,
    seccomp_mode:u32,unshare_errno:i32,clone3_errno:i32,proc_uuid_errno:i32,job_file_errno:i32,
    observer_proc_fd_closed:bool,proc_empty_readonly:bool,workspace_parent_readonly:bool,
    stdin_eof:bool,descriptors:String,cache_reuse:bool,
}

pub(super) fn validate_selected_confinement_data(workload:&serde_json::Value)->Result<()> {
    let rules:serde_json::Value=serde_json::from_str(include_str!("compiler_sdk_rules.json"))?;
    let policy=&rules["compiler_input_policy"];
    ensure!(rules["format_version"]==4&&rules["sandbox_profile"]=="uid1500-user-mount-net-pid-procless-seccomp-sdk-v3"
        &&policy["kind"]=="a2-procless-seccomp-read-v2"&&policy["semantics_version"]==2
        &&policy["compiler_proc"]=="empty-readonly"&&policy["workspace_parent"]=="readonly"
        &&policy["observer_proc"]=="original-postfork-held-fd-only"
        &&policy["observer_proc_fd"]=="close-before-compiler-exec"
        &&policy["foreign_audit_arch"]=="kill-process"&&policy["x32_syscall_bit"]==0x40000000u32
        &&policy["x32_action"]=="kill-process"&&policy["deny_errno"]==libc::EPERM
        &&policy["clone3_errno"]==libc::ENOSYS
        &&policy["cache_reuse"]==false&&policy["deterministic_execution"]==false,
        "SDK keyed compiler read policy differs");
    let expected=digest(&canonical_json(policy)?);
    let isolation:serde_json::Value=serde_json::from_str(include_str!("sdk_compiler_isolation/policy.json"))?;
    let isolation_sha256=digest(&canonical_json(&isolation)?);
    let checks=workload["checks"].as_array().context("SDK actual confined invocations absent")?;
    ensure!(!checks.is_empty(),"SDK actual confined invocation list is empty");
    for check in checks {
        let actual:ChildReadObservation=serde_json::from_value(check["input_confinement"].clone())?;
        ensure!(actual.format_version==3&&actual.kind=="actual-a2-procless-seccomp-child-v3"
            &&actual.isolation_sha256==isolation_sha256&&actual.mount_count==6
            &&actual.source_declarations=="job-union-exact"
            &&actual.memfd_errno==libc::EPERM&&actual.execveat_errno==libc::EPERM
            &&actual.io_uring_errno==libc::EPERM&&actual.sysv_ipc_errno==libc::EPERM&&actual.keyring_errno==libc::EPERM
            &&actual.policy_sha256==expected&&actual.architecture==std::env::consts::ARCH
            &&matches!(actual.architecture.as_str(),"x86_64"|"aarch64")
            &&actual.seccomp_mode==2&&actual.unshare_errno==libc::EPERM&&actual.clone3_errno==libc::ENOSYS
            &&actual.proc_uuid_errno==libc::ENOENT&&actual.job_file_errno==libc::ENOENT
            &&actual.observer_proc_fd_closed&&actual.proc_empty_readonly&&actual.workspace_parent_readonly
            &&actual.stdin_eof
            &&actual.descriptors=="stdio-only-at-exec"&&!actual.cache_reuse,
            "SDK actual compiler read confinement differs or was not enforced");
    }
    Ok(())
}

/// DATA for pure validator regressions only. This cannot issue a Root owner,
/// helper observation, execution outcome, qualified role or cache permission.
#[cfg(test)]
pub(super) fn child_read_observation_fixture_data()->serde_json::Value {
    let rules:serde_json::Value=serde_json::from_str(include_str!("compiler_sdk_rules.json")).unwrap();
    let isolation:serde_json::Value=serde_json::from_str(include_str!("sdk_compiler_isolation/policy.json")).unwrap();
    serde_json::json!({"format_version":3,"kind":"actual-a2-procless-seccomp-child-v3",
        "isolation_sha256":digest(&canonical_json(&isolation).unwrap()),"mount_count":6,
        "source_declarations":"job-union-exact",
        "memfd_errno":libc::EPERM,"execveat_errno":libc::EPERM,"io_uring_errno":libc::EPERM,
        "sysv_ipc_errno":libc::EPERM,"keyring_errno":libc::EPERM,
        "policy_sha256":digest(&canonical_json(&rules["compiler_input_policy"]).unwrap()),
        "architecture":std::env::consts::ARCH,"seccomp_mode":2,"unshare_errno":libc::EPERM,
        "clone3_errno":libc::ENOSYS,"proc_uuid_errno":libc::ENOENT,"job_file_errno":libc::ENOENT,
        "observer_proc_fd_closed":true,"proc_empty_readonly":true,"workspace_parent_readonly":true,
        "stdin_eof":true,"descriptors":"stdio-only-at-exec","cache_reuse":false})
}

#[cfg(test)]
mod tests {
    use super::*;
    fn observation_data()->serde_json::Value {
        serde_json::json!({"checks":[{"input_confinement":child_read_observation_fixture_data()}]})
    }
    #[test]fn pure_read_observation_data_requires_exact_policy_and_actual_refusal_fields(){
        let original=observation_data();validate_selected_confinement_data(&original).unwrap();
        for (field,value) in [
            ("format_version",serde_json::json!(1)),("kind",serde_json::json!("pretend")),
            ("policy_sha256",serde_json::json!("0".repeat(64))),("architecture",serde_json::json!("wrong")),
            ("isolation_sha256",serde_json::json!("0".repeat(64))),("mount_count",serde_json::json!(7)),
            ("source_declarations",serde_json::json!("user-assertion")),
            ("memfd_errno",serde_json::json!(0)),("execveat_errno",serde_json::json!(0)),
            ("io_uring_errno",serde_json::json!(0)),("sysv_ipc_errno",serde_json::json!(0)),
            ("keyring_errno",serde_json::json!(0)),
            ("seccomp_mode",serde_json::json!(0)),("unshare_errno",serde_json::json!(0)),
            ("clone3_errno",serde_json::json!(libc::EPERM)),("proc_uuid_errno",serde_json::json!(0)),
            ("job_file_errno",serde_json::json!(0)),("observer_proc_fd_closed",serde_json::json!(false)),
            ("proc_empty_readonly",serde_json::json!(false)),("workspace_parent_readonly",serde_json::json!(false)),
            ("stdin_eof",serde_json::json!(false)),
            ("descriptors",serde_json::json!("inherited-job-fd")),("cache_reuse",serde_json::json!(true))] {
            let mut changed=original.clone();changed["checks"][0]["input_confinement"][field]=value;
            assert!(validate_selected_confinement_data(&changed).is_err(),"{field}");
        }
        let mut changed=original.clone();changed["checks"][0]["input_confinement"]["unexpected"]=true.into();
        assert!(validate_selected_confinement_data(&changed).is_err());
        assert!(validate_selected_confinement_data(&serde_json::json!({"checks":[]})).is_err());
        assert!(validate_selected_confinement_data(&serde_json::json!({"checks":[{}]})).is_err());
    }
    #[test]fn semantic_rules_include_the_consumed_no_fallback_read_policy(){
        let policy:serde_json::Value=serde_json::from_str(include_str!("compiler_sdk_rules.json")).unwrap();
        let original=digest(&canonical_json(&policy).unwrap());
        for (field,value) in [("semantics_version",serde_json::json!(3)),
            ("compiler_proc",serde_json::json!("host-proc")),
            ("observer_proc_fd",serde_json::json!("inherit-to-compiler")),
            ("x32_action",serde_json::json!("allow")),
            ("namespace_clone_mask",serde_json::json!(0)),
            ("private_read_write_trees",serde_json::json!(["/workspace/out","/tmp","/proc"])),
            ("unavailable",serde_json::json!("continue"))] {
            let mut changed=policy.clone();changed["compiler_input_policy"][field]=value;
            assert_ne!(original,digest(&canonical_json(&changed).unwrap()));
        }
    }
}
