//! Explicit, credential-free test helper; never an installed worker entrypoint.
//! Cargo integration tests bind to this exact current-source executable.
#[cfg(target_os = "linux")]
mod qualification {
    use rudder_engine::execution::network::{linux::*, *};
    use serde_json::{json, Value};
    use std::os::{fd::OwnedFd, unix::process::CommandExt};

    fn emit(value: Value) {
        println!("{value}");
    }
    fn error(error: NetworkError) -> ! {
        let unsupported = (error.kind() == NetworkErrorKind::NamespaceUnavailable
            && matches!(
                error.raw_os_error(),
                Some(libc::EPERM | libc::EINVAL | libc::ENOSYS)
            ))
            || (error.kind() == NetworkErrorKind::CapabilityDropFailed
                && error.raw_os_error() == Some(libc::EPERM))
            || error.kind() == NetworkErrorKind::WrongIdentity;
        emit(
            json!({"kind":"execution-network-qualification","status":if unsupported {"unsupported"} else {"failed"},
            "error":format!("{:?}",error.kind()),"errno":error.raw_os_error(),"hostFallback":false}),
        );
        std::process::exit(if unsupported { 77 } else { 1 });
    }
    fn snapshot(value: CapabilitySnapshot) -> Value {
        json!({"effective":format!("{:016x}",value.effective),"permitted":format!("{:016x}",value.permitted),
            "inheritable":format!("{:016x}",value.inheritable),"ambient":format!("{:016x}",value.ambient),
            "bounding":format!("{:016x}",value.bounding),"noNewPrivileges":value.no_new_privileges,
            "supportedBits":value.supported_bits})
    }
    fn profile() -> ValidatedNetworkProfile {
        let id = "00000000-0000-0000-0000-000000000001";
        let expected = ExpectedNetworkBinding {
            project_id: "qualification",
            owner: "qualification",
            owner_epoch: 1,
            run_id: "qualification",
            run_epoch: 1,
            request_id: id,
            execution_id: id,
            operation_id: id,
            view_id: id,
            network_id: id,
            readiness_nonce: id,
        };
        let bytes = serde_json::to_vec(&json!({"format_version":1,"execution_profile":"cloud_uid1500_linux_v1",
            "binding":{"project_id":"qualification","owner":"qualification","owner_epoch":"1", "run_id":"qualification",
                "run_epoch":"1","request_id":id,"execution_id":id,"operation_id":id,"view_id":id,"network_id":id},
            "mode":"offline_v1","policy":null,"policy_sha256":null,"readiness_nonce":id,
            "budgets":{"max_connections":0,"max_dns_jobs":0,"max_request_bytes":0,"max_buffer_bytes":0,
                "max_bytes_per_connection":0,"max_total_bytes":0,"connect_timeout_ms":0,"idle_timeout_ms":0,"total_timeout_ms":0}})).unwrap();
        NetworkProfileV1::from_json(&bytes, &expected, None).unwrap()
    }
    fn fresh_uid1500() {
        if unsafe { libc::getuid() } == 0 {
            if unsafe { libc::setgroups(0, std::ptr::null()) } != 0
                || unsafe { libc::setresgid(1500, 1500, 1500) } != 0
                || unsafe { libc::setresuid(1500, 1500, 1500) } != 0
            {
                emit(
                    json!({"kind":"execution-network-qualification","status":"unsupported","error":"Uid1500Unavailable",
                    "errno":std::io::Error::last_os_error().raw_os_error(),"hostFallback":false}),
                );
                std::process::exit(77);
            }
            // UID transitions reset dumpability/proc ownership. Re-exec as
            // UID1500 before USER mapping, matching the worker's fresh helper.
            let failure = std::process::Command::new(std::env::current_exe().unwrap())
                .args(std::env::args().skip(1))
                .env_clear()
                .exec();
            emit(
                json!({"kind":"execution-network-qualification","status":"failed","error":"Uid1500ExecFailed",
                "errno":failure.raw_os_error(),"hostFallback":false}),
            );
            std::process::exit(1);
        }
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1 as libc::c_ulong, 0, 0, 0) } != 0 {
            emit(
                json!({"kind":"execution-network-qualification","status":"failed","error":"NoNewPrivilegesUnavailable","hostFallback":false}),
            );
            std::process::exit(1);
        }
    }
    fn current(kind: NamespaceKind) -> NamespaceFd {
        let path = match kind {
            NamespaceKind::User => "/proc/self/ns/user",
            NamespaceKind::Network => "/proc/self/ns/net",
        };
        let fd: OwnedFd = std::fs::File::open(path).unwrap().into();
        NamespaceFd::inspect(fd, kind).unwrap()
    }
    fn identity(value: NamespaceIdentity) -> Value {
        json!({"device":value.device().to_string(),"inode":value.inode().to_string(),"kind":format!("{:?}",value.kind())})
    }
    fn namespace_references(identity: NamespaceIdentity) -> usize {
        use std::os::unix::fs::MetadataExt;
        let mut count = 0;
        for (index, entry) in std::fs::read_dir("/proc/self/fd").unwrap().enumerate() {
            if index >= 64 {
                std::process::exit(1);
            }
            if let Ok(metadata) = std::fs::metadata(entry.unwrap().path()) {
                if metadata.dev() == identity.device() && metadata.ino() == identity.inode() {
                    count += 1;
                }
            }
        }
        count
    }
    pub fn run(mode: &str) {
        // All destructive operations occur in this newly exec'd helper main,
        // never in a libtest thread or the parent test process.
        unsafe {
            libc::alarm(15);
        }
        match mode {
            "drop-and-exec" => {
                let before = read_capabilities().unwrap_or_else(|failure| error(failure));
                let proof = unsafe { drop_all_capabilities_for_exec() }
                    .unwrap_or_else(|failure| error(failure));
                proof
                    .verify_current_for_exec()
                    .unwrap_or_else(|failure| error(failure));
                emit(
                    json!({"kind":"capability-drop","status":"verified-current-thread","uid":unsafe{libc::getuid()},
                    "before":snapshot(before),"after":snapshot(proof.snapshot())}),
                );
                let failure = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--qualification-only", "inspect-zero"])
                    .env_clear()
                    .exec();
                emit(
                    json!({"kind":"execution-network-qualification","status":"failed","error":"ExecFailed","errno":failure.raw_os_error()}),
                );
                std::process::exit(1);
            }
            "inspect-zero" => {
                let after = read_capabilities().unwrap_or_else(|failure| error(failure));
                if !after.all_five_zero() || !after.no_new_privileges {
                    std::process::exit(1);
                }
                emit(
                    json!({"kind":"capability-exec-readback","status":"qualified_current_process_only","after":snapshot(after),
                    "liveAttemptAuthorized":false,"hostFallback":false}),
                );
            }
            "proof-wrong-process" => {
                let proof = unsafe { drop_all_capabilities_for_exec() }
                    .unwrap_or_else(|failure| error(failure));
                let parent = unsafe { libc::getpid() };
                let child = unsafe { libc::fork() };
                if child < 0 {
                    std::process::exit(1);
                }
                if child == 0 {
                    // The helper main is fresh and single-threaded. Its fork
                    // child only checks the bound PID and immediately exits.
                    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } != 0
                        || unsafe { libc::getppid() } != parent
                    {
                        unsafe { libc::_exit(1) }
                    }
                    let rejected = proof
                        .verify_current_for_exec()
                        .err()
                        .is_some_and(|failure| failure.kind() == NetworkErrorKind::WrongProcess);
                    unsafe { libc::_exit(if rejected { 0 } else { 1 }) }
                }
                let mut status = 0;
                if unsafe { libc::waitpid(child, &mut status, 0) } != child
                    || !libc::WIFEXITED(status)
                    || libc::WEXITSTATUS(status) != 0
                {
                    std::process::exit(1);
                }
                proof
                    .verify_current_for_exec()
                    .unwrap_or_else(|failure| error(failure));
                emit(
                    json!({"kind":"capability-proof-replay","status":"verified_foreign_process_refused",
                    "hostFallback":false}),
                );
            }
            "namespace-baseline" => {
                fresh_uid1500();
                let profile = profile();
                let before = current(NamespaceKind::Network).identity();
                let baseline = ChildNetworkBaseline::capture(&profile)
                    .unwrap_or_else(|failure| error(failure));
                let failure = match unsafe { baseline.enter_after_new_user() } {
                    Ok(_) => std::process::exit(1),
                    Err(error) => error,
                };
                if failure.kind() != NetworkErrorKind::ForeignNamespace
                    || current(NamespaceKind::Network).identity() != before
                {
                    std::process::exit(1);
                }
                emit(
                    json!({"kind":"namespace-baseline-refusal","status":"verified_no_namespace_change","error":"ForeignNamespace","hostFallback":false}),
                );
            }
            "namespace-setup" | "namespace-and-exec" | "proof-after-user-reset" => {
                fresh_uid1500();
                let profile = profile();
                let parent_user = current(NamespaceKind::User).identity();
                let parent_net = current(NamespaceKind::Network).identity();
                let baseline = ChildNetworkBaseline::capture(&profile)
                    .unwrap_or_else(|failure| error(failure));
                let network = unsafe { baseline.enter_new_user_and_net_for_qualification() }
                    .unwrap_or_else(|failure| error(failure));
                network
                    .verify_current()
                    .unwrap_or_else(|failure| error(failure));
                if network.user_namespace().identity() == parent_user
                    || network.network_namespace().identity() == parent_net
                    || network.network_id() != profile.network_id()
                    || network.profile_digest() != profile.profile_digest()
                {
                    std::process::exit(1);
                }
                if namespace_references(parent_user) != 0
                    || namespace_references(parent_net) != 0
                    || network.child_namespace_fds().len() != 2
                {
                    std::process::exit(1);
                }
                let before_drop = read_capabilities().unwrap_or_else(|failure| error(failure));
                let proof = unsafe { drop_all_capabilities_for_exec() }
                    .unwrap_or_else(|failure| error(failure));
                proof
                    .verify_current_for_exec()
                    .unwrap_or_else(|failure| error(failure));
                if mode == "proof-after-user-reset" {
                    if unsafe { libc::unshare(libc::CLONE_NEWUSER) } != 0 {
                        emit(
                            json!({"kind":"execution-network-qualification","status":"unsupported",
                            "error":"NestedUserNamespaceUnavailable","errno":std::io::Error::last_os_error().raw_os_error(),
                            "hostFallback":false}),
                        );
                        std::process::exit(77);
                    }
                    let rejected = proof
                        .verify_current_for_exec()
                        .err()
                        .is_some_and(|failure| {
                            failure.kind() == NetworkErrorKind::CapabilitiesRemain
                        });
                    if !rejected {
                        std::process::exit(1);
                    }
                    emit(
                        json!({"kind":"capability-proof-reset","status":"verified_reset_refused","hostFallback":false}),
                    );
                    return;
                }
                network
                    .verify_current()
                    .unwrap_or_else(|failure| error(failure));
                emit(
                    json!({"kind":"namespace-setup","status":"qualified_kernel_primitives_only","uid":unsafe{libc::getuid()},
                    "parentUser":identity(parent_user),"parentNet":identity(parent_net),
                    "childUser":identity(network.user_namespace().identity()),"childNet":identity(network.network_namespace().identity()),
                    "beforeDrop":snapshot(before_drop),"afterDrop":snapshot(proof.snapshot()),"loopbackVerified":true,
                    "parentWitnessesClosed":true,"allowedNamespaceWitnesses":2,
                    "egressQualified":false,"liveAttemptAuthorized":false,"hostFallback":false}),
                );
                if mode == "namespace-and-exec" {
                    let failure = std::process::Command::new(std::env::current_exe().unwrap())
                        .args(["--qualification-only", "inspect-zero"])
                        .env_clear()
                        .exec();
                    emit(
                        json!({"kind":"execution-network-qualification","status":"failed","error":"ExecFailed",
                        "errno":failure.raw_os_error(),"hostFallback":false}),
                    );
                    std::process::exit(1);
                }
            }
            _ => std::process::exit(64),
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 || args[1] != "--qualification-only" {
        std::process::exit(64);
    }
    #[cfg(target_os = "linux")]
    qualification::run(&args[2]);
    #[cfg(not(target_os = "linux"))]
    {
        println!("{{\"kind\":\"execution-network-qualification\",\"status\":\"unsupported\",\"error\":\"LinuxOnly\",\"hostFallback\":false}}");
        std::process::exit(77);
    }
}
