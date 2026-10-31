use super::*;
use crate::{
    engine_host::{EngineOwnership, NativeAuthority},
    source_authorizer::{
        HostSourceIssuerPeer, Limits, SourceAuthorizerClient, SourceAuthorizerServer,
    },
};
use rudder_engine::{
    journal::{
        host_source_scope, CapabilityGrant, Journal, OwnerFence, HOST_SOURCE_REQUIRED_OPERATION,
    },
    protocol::{ObjectKind, ObjectRef, OwnerEpoch},
    remote_cache::CacheScope,
    remote_root::{AttemptBinding, RootBinding},
    store::ImportPolicy,
    vfs::{
        exclusion_policy_digest,
        partial_rpc::{partial_scope, PROTOCOL, REQUIRED_OPERATION},
        rpc::{AttemptContext, ObservationBinding},
    },
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::{
        fd::AsRawFd,
        unix::{
            fs::{symlink, DirBuilderExt, PermissionsExt},
            net::UnixStream,
        },
    },
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
};
use uuid::Uuid;

struct Temporary(PathBuf);
impl Temporary {
    fn new() -> Self {
        let path = Path::new("/tmp").canonicalize().unwrap().join(format!(
            "rsi-{}",
            &Uuid::new_v4().simple().to_string()[..12]
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub(crate) struct Fixture {
    pub(crate) owner: Option<EngineOwnership>,
    pub(crate) issuer: Option<Arc<HostSourceIssuer>>,
    pub(crate) fence: OwnerFence,
    pub(crate) binding: PartialBinding,
    pub(crate) token: String,
    pub(crate) repo: PathBuf,
    pub(crate) home: PathBuf,
    pub(crate) database: PathBuf,
    temporary: Temporary,
}
impl Fixture {
    pub(crate) fn new(expires_at_ms: Option<i64>) -> Self {
        let temporary = Temporary::new();
        let repo = temporary.0.join("r");
        let home = temporary.0.join("h");
        fs::DirBuilder::new().mode(0o700).create(&repo).unwrap();
        fs::DirBuilder::new().mode(0o700).create(&home).unwrap();
        let mut owner =
            EngineOwnership::acquire_in_home(&repo, NativeAuthority::Fabric, &home).unwrap();
        let (journal, fence) = owner.journal_parts();
        let fence = fence.clone();
        let epoch = journal.register_run(&fence, "run", 0).unwrap();
        let execution_id = Uuid::new_v4().to_string();
        journal
            .observe_run_attempt(&fence, "run", &execution_id, 0)
            .unwrap();
        let attempt = AttemptBinding {
            project_id: fence.project_id.clone(),
            run_id: "run".into(),
            run_epoch: epoch,
            owner: OwnerEpoch {
                owner: fence.owner_id.clone(),
                epoch: fence.epoch,
            },
            execution_id,
            operation_id: Uuid::new_v4().to_string(),
            view_id: Uuid::new_v4().to_string(),
        };
        let binding = PartialBinding {
            protocol: PROTOCOL.into(),
            format_version: 1,
            launch_request_digest: "a".repeat(64),
            root: RootBinding {
                scope: CacheScope {
                    provider_id: "fixture".into(),
                    tenant_id: "tenant".into(),
                },
                root_id: "root".into(),
                root: ObjectRef::from_bytes(ObjectKind::RunnableRoot, b"root"),
            },
            observation: ObservationBinding {
                format_version: 1,
                attempt: AttemptContext {
                    execution_id: attempt.execution_id.clone(),
                    operation_id: attempt.operation_id.clone(),
                },
                view_id: attempt.view_id.clone(),
                owner: attempt.owner.clone(),
                namespace: Uuid::new_v4().to_string(),
                profile: ObjectRef::from_bytes(ObjectKind::Blob, b"profile"),
                exclusion_policy_digest: exclusion_policy_digest(&ImportPolicy::default()).unwrap(),
            },
            attempt,
        };
        let grant = grant(
            &binding,
            HOST_SOURCE_REQUIRED_OPERATION,
            host_source_scope(&binding).unwrap(),
            expires_at_ms,
        );
        let token = journal.issue_capability(&fence, &grant, 0).unwrap();
        let database = journal.database_path().unwrap().to_path_buf();
        let lease = owner.source_lease(&fence).unwrap();
        let issuer = HostSourceIssuer::new(lease, binding.clone(), token.clone()).unwrap();
        Self {
            owner: Some(owner),
            issuer: Some(issuer),
            fence,
            binding,
            token,
            repo,
            home,
            database,
            temporary,
        }
    }
    pub(crate) fn issuer(&self) -> &Arc<HostSourceIssuer> {
        self.issuer.as_ref().unwrap()
    }
    pub(crate) fn journal(&mut self) -> &mut Journal {
        self.owner.as_mut().unwrap().journal_parts().0
    }
    pub(crate) fn directory(&self) -> &Path {
        &self.temporary.0
    }
}

fn grant(
    binding: &PartialBinding,
    operation: &str,
    scope: String,
    expiry: Option<i64>,
) -> CapabilityGrant {
    CapabilityGrant {
        principal: "host-source:fixture".into(),
        run_id: binding.attempt.run_id.clone(),
        run_epoch: binding.attempt.run_epoch,
        operations: BTreeSet::from([operation.into()]),
        recipients: BTreeMap::new(),
        scopes: BTreeSet::from([scope]),
        topics: BTreeSet::new(),
        expires_at_ms: expiry,
    }
}

#[test]
fn actual_issuer_queries_current_host_grant_and_survives_only_guest_rotation() {
    let mut f = Fixture::new(None);
    let fence = f.fence.clone();
    let guest_grant = grant(
        &f.binding,
        REQUIRED_OPERATION,
        partial_scope(&f.binding).unwrap(),
        None,
    );
    let guest = f
        .journal()
        .issue_capability(&fence, &guest_grant, 0)
        .unwrap();
    let result = f.issuer().authorize(&f.binding).unwrap();
    assert_eq!(result.identity.project_id, f.fence.project_id);
    assert_eq!(result.identity.run_epoch, f.binding.attempt.run_epoch);
    assert_eq!(result.expires_at_ms, None);
    f.journal().revoke_capability(&fence, &guest, 1).unwrap();
    f.journal()
        .issue_capability(&fence, &guest_grant, 2)
        .unwrap();
    f.issuer().authorize(&f.binding).unwrap();
    f.owner.as_ref().unwrap().assert_current().unwrap();
}

#[test]
fn actual_issuer_rejects_guest_token_without_retiring_the_actual_engine_owner() {
    let mut f = Fixture::new(None);
    let fence = f.fence.clone();
    let guest_grant = grant(
        &f.binding,
        REQUIRED_OPERATION,
        partial_scope(&f.binding).unwrap(),
        None,
    );
    let token = f
        .journal()
        .issue_capability(&fence, &guest_grant, 0)
        .unwrap();
    let lease = f.owner.as_ref().unwrap().source_lease(&fence).unwrap();
    let wrong = HostSourceIssuer::new(lease, f.binding.clone(), token).unwrap();
    assert!(wrong.authorize(&f.binding).is_err());
    f.issuer().authorize(&f.binding).unwrap();
    f.owner.as_ref().unwrap().assert_current().unwrap();
}

#[test]
fn current_host_revocation_owner_run_and_actual_attempt_losses_refuse() {
    for loss in 0..5 {
        let mut f = Fixture::new(None);
        let fence = f.fence.clone();
        let token = f.token.clone();
        f.issuer().authorize(&f.binding).unwrap();
        match loss {
            0 => f.journal().revoke_capability(&fence, &token, 1).unwrap(),
            1 => {
                f.journal().advance_run_epoch(&fence, "run", 1, 1).unwrap();
            }
            2 => {
                f.journal()
                    .observe_run_attempt(&fence, "run", &Uuid::new_v4().to_string(), 1)
                    .unwrap();
            }
            3 => f.journal().release_owner(&fence, 1).unwrap(),
            _ => {
                f.journal()
                    .transfer_owner(&fence, "replacement", 1)
                    .unwrap();
            }
        }
        assert!(f.issuer().authorize(&f.binding).is_err(), "loss{loss}");
        assert!(
            f.issuer().authorize(&f.binding).is_err(),
            "denial cannot resurrect"
        );
    }
}

#[test]
fn exact_binding_denial_is_absorbing_and_constructor_refuses_a_foreign_owner() {
    let f = Fixture::new(None);
    let mut wrong = f.binding.clone();
    wrong.root.scope.tenant_id.push_str("-foreign");
    assert!(f.issuer().authorize(&wrong).is_err());
    assert!(f.issuer().authorize(&f.binding).is_err());
    let lease = f.owner.as_ref().unwrap().source_lease(&f.fence).unwrap();
    wrong = f.binding.clone();
    wrong.attempt.project_id.push_str("-foreign");
    assert!(HostSourceIssuer::new(lease, wrong, f.token.clone()).is_err());
    f.owner.as_ref().unwrap().assert_current().unwrap();
}

#[test]
fn short_expiry_snapshot_has_no_rounding_up_or_post_expiry_reentry() {
    let f = Fixture::new(Some(10));
    let result = f.issuer().authorize_using(&f.binding, || Ok(9)).unwrap();
    assert_eq!(result.validated_at_ms, 9);
    assert_eq!(result.expires_at_ms, Some(10));
    assert!(f.issuer().authorize_using(&f.binding, || Ok(10)).is_err());
    assert!(
        f.issuer().authorize_using(&f.binding, || Ok(9)).is_err(),
        "a stale clock/result cannot revive the denied callback"
    );
}

#[test]
fn trusted_clock_failure_is_absorbing_without_a_fabricated_timestamp() {
    let f = Fixture::new(None);
    assert!(f
        .issuer()
        .authorize_using(&f.binding, || anyhow::bail!("fixture clock unavailable"))
        .is_err());
    assert!(f.issuer().authorize(&f.binding).is_err());
}

#[test]
fn revoked_capability_errors_do_not_disclose_the_private_token() {
    let mut f = Fixture::new(None);
    let fence = f.fence.clone();
    let token = f.token.clone();
    f.journal().revoke_capability(&fence, &token, 1).unwrap();
    let error = f.issuer().authorize(&f.binding).unwrap_err();
    assert!(!format!("{error:#}").contains(&token));
}

#[test]
fn actual_database_replacement_missing_mode_and_hardlink_refuse_after_restoration() {
    for change in 0..5 {
        let f = Fixture::new(None);
        let retained = f.temporary.0.join("retained.sqlite");
        match change {
            0 => {
                fs::rename(&f.database, &retained).unwrap();
                fs::copy(&retained, &f.database).unwrap();
            }
            1 => fs::rename(&f.database, &retained).unwrap(),
            2 => fs::set_permissions(&f.database, fs::Permissions::from_mode(0o644)).unwrap(),
            3 => fs::hard_link(&f.database, &retained).unwrap(),
            _ => {
                fs::rename(&f.database, &retained).unwrap();
                symlink(&retained, &f.database).unwrap();
            }
        }
        assert!(f.issuer().authorize(&f.binding).is_err(), "change{change}");
        match change {
            0 | 4 => {
                fs::remove_file(&f.database).unwrap();
                fs::rename(&retained, &f.database).unwrap();
            }
            1 => fs::rename(&retained, &f.database).unwrap(),
            2 => fs::set_permissions(&f.database, fs::Permissions::from_mode(0o600)).unwrap(),
            _ => fs::remove_file(&retained).unwrap(),
        }
        assert!(
            f.issuer().authorize(&f.binding).is_err(),
            "restoring paths cannot renew"
        );
        assert!(f.owner.as_ref().unwrap().assert_current().is_err());
    }
}

#[test]
fn retired_callback_keeps_actual_checkout_lock_until_real_query_frame_settles() {
    struct CallbackGuard {
        release: Option<mpsc::SyncSender<()>>,
        worker: Option<thread::JoinHandle<Result<HostSourceAuthorization>>>,
    }
    impl Drop for CallbackGuard {
        fn drop(&mut self) {
            if let Some(tx) = self.release.take() {
                let _ = tx.send(());
            }
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }
    for clean in [true, false] {
        let mut f = Fixture::new(None);
        let issuer = f.issuer.take().unwrap();
        let binding = f.binding.clone();
        let (entered, ready) = mpsc::sync_channel(1);
        let (release, go) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            issuer.authorize_using(&binding, || {
                entered.send(()).unwrap();
                go.recv_timeout(Duration::from_secs(10)).unwrap();
                trusted_query_time()
            })
        });
        let mut callback = CallbackGuard {
            release: Some(release),
            worker: Some(worker),
        };
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        f.owner.as_mut().unwrap().finish(clean).unwrap();
        drop(f.owner.take());
        assert!(
            EngineOwnership::acquire_in_home(&f.repo, NativeAuthority::Fabric, &f.home).is_err()
        );
        callback.release.take().unwrap().send(()).unwrap();
        assert!(callback.worker.take().unwrap().join().unwrap().is_err());
        let replacement =
            EngineOwnership::acquire_in_home(&f.repo, NativeAuthority::Fabric, &f.home).unwrap();
        replacement.assert_current().unwrap();
    }
}

/// Composed-fixture witness only: actual issuer lease plus actual kernel peer
/// PID of this same process. No saved/caller-selected PID constructor or helper
/// bootstrap is provided or qualified by this test implementation.
struct LocalIssuerPeer(Arc<HostSourceIssuer>);
impl HostSourceIssuerPeer for LocalIssuerPeer {
    fn check_current(&self) -> Result<()> {
        self.0.check_current()
    }
    fn check_peer(&self, stream: &UnixStream) -> Result<()> {
        self.check_current()?;
        #[cfg(target_os = "linux")]
        {
            let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
            let mut size = std::mem::size_of_val(&peer) as libc::socklen_t;
            let rc = unsafe {
                libc::getsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    (&mut peer as *mut libc::ucred).cast(),
                    &mut size,
                )
            };
            ensure!(
                rc == 0
                    && size as usize == std::mem::size_of_val(&peer)
                    && peer.pid == std::process::id() as libc::pid_t
                    && peer.uid == unsafe { libc::geteuid() },
                "fixture peer is not the actual held issuer process"
            );
        }
        #[cfg(target_os = "macos")]
        {
            let mut pid: libc::pid_t = 0;
            let mut size = std::mem::size_of_val(&pid) as libc::socklen_t;
            let rc = unsafe {
                libc::getsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_LOCAL,
                    libc::LOCAL_PEERPID,
                    (&mut pid as *mut libc::pid_t).cast(),
                    &mut size,
                )
            };
            ensure!(
                rc == 0
                    && size as usize == std::mem::size_of_val(&pid)
                    && pid == std::process::id() as libc::pid_t,
                "fixture peer is not the actual held issuer process"
            );
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        anyhow::bail!("fixture issuer peer custody is unsupported");
        self.check_current()
    }
}

#[test]
fn actual_journal_issuer_composes_with_authenticated_transport_and_revocation() {
    let mut f = Fixture::new(None);
    let issuer = Arc::clone(f.issuer());
    let directory = f.temporary.0.join("s");
    let mut server = SourceAuthorizerServer::bind(
        &directory,
        f.binding.clone(),
        issuer.clone(),
        Limits::default(),
    )
    .unwrap();
    let peer = Arc::new(LocalIssuerPeer(issuer));
    let client = SourceAuthorizerClient::connect(&directory, f.binding.clone(), peer).unwrap();
    let context = fs::read_to_string(directory.join("source-context.json")).unwrap();
    assert!(
        !context.contains(&f.token),
        "Journal capability must never enter endpoint context"
    );
    client.check_current().unwrap();
    // Real authenticated finalization-sized burst, without pacing sleeps.
    for _ in 0..27 {
        client.authorize(&f.binding).unwrap();
    }
    let fence = f.fence.clone();
    let token = f.token.clone();
    f.journal()
        .revoke_capability(&fence, &token, trusted_query_time().unwrap())
        .unwrap();
    assert!(client.authorize(&f.binding).is_err());
    assert!(client.check_current().is_err());
    let settled = server.await_closed(Duration::from_secs(2)).unwrap();
    drop(client);
    drop(server);
    drop(settled);
    f.owner.as_mut().unwrap().finish(true).unwrap();
    drop(f.owner.take());
    assert!(
        EngineOwnership::acquire_in_home(&f.repo, NativeAuthority::Fabric, &f.home).is_err(),
        "the separately retained callback owner must still retain its actual lease"
    );
    drop(f.issuer.take());
    // Endpoint settlement does not claim join-coordinator teardown. Its last
    // retained callback owner may still be dropping; this DATA receipt is not
    // used to infer synchronous checkout-lock release. The direct actual frame
    // settlement test above independently proves release after worker join.
}
