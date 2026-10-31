//! Trusted helper-side adaptation of the actual managed controller witness.
//! The Engine constructs this witness only after its authenticated descriptor
//! handoff. This module adds no PID, path, raw-FD or serialized constructor.
use crate::source_authorizer::HostSourceIssuerPeer;
use anyhow::Result;
use rudder_engine::execution::handoff::ControllerProcessWitness;
use std::os::unix::net::UnixStream;

impl HostSourceIssuerPeer for ControllerProcessWitness {
    fn check_current(&self) -> Result<()> {
        ControllerProcessWitness::check_current(self)
    }

    fn check_peer(&self, stream: &UnixStream) -> Result<()> {
        ControllerProcessWitness::verify_peer(self, stream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent_broker::{self, AgentBroker, BrokerClient, BrokerConfig},
        source_authorizer::{
            HostSourceAuthority, Limits, SourceAuthorizerClient, SourceAuthorizerServer,
        },
        source_issuer::SourceIssuerFixture,
    };
    use rudder_engine::{
        agent_api::AgentIdentityFile,
        execution::{
            handoff::{receive_managed, ManagedExecutionHandoff},
            BrokerBinding, ExecutionSpec, HelperApplicationSpec, ManagedExecutionBinding,
            ManagedFrontendStart, ProtectedHostContextRef, WorkspaceObservationProfile,
        },
        journal::CapabilityGrant,
        protocol::{ObjectKind, ObjectRef},
        vfs::partial_rpc::{partial_scope, PartialBinding, REQUIRED_OPERATION},
    };
    use serde::{Deserialize, Serialize};
    use std::{
        collections::{BTreeMap, BTreeSet},
        fs,
        io::{Read, Write},
        os::unix::net::UnixListener,
        path::{Path, PathBuf},
        process::{Child, Command, Stdio},
        time::{Duration, Instant},
    };
    use uuid::Uuid;

    const INPUT: &str = "RUDDER_SOURCE_CONTROLLER_TEST_INPUT";
    const CHILD: &str = "source_controller::tests::private_source_controller_child";

    // Routing/binding expectations only. The host token and endpoint key are
    // never in this file, environment, argv or captured test output.
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Input {
        handoff: String,
        parent: u32,
        endpoint: PathBuf,
        notifier: PathBuf,
        binding: PartialBinding,
    }

    struct OwnedChild(Option<Child>);
    impl OwnedChild {
        fn output(mut self) -> std::process::Output {
            let deadline = Instant::now() + Duration::from_secs(8);
            while self.0.as_mut().unwrap().try_wait().unwrap().is_none() {
                assert!(
                    Instant::now() < deadline,
                    "source controller child did not exit"
                );
                std::thread::sleep(Duration::from_millis(2));
            }
            self.0.take().unwrap().wait_with_output().unwrap()
        }
    }
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    fn private_file(path: &Path, bytes: &[u8]) {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
    }

    fn guest_grant(binding: &PartialBinding) -> CapabilityGrant {
        CapabilityGrant {
            principal: "source-controller-guest-fixture".into(),
            run_id: binding.attempt.run_id.clone(),
            run_epoch: binding.attempt.run_epoch,
            operations: BTreeSet::from([REQUIRED_OPERATION.into()]),
            recipients: BTreeMap::new(),
            scopes: BTreeSet::from([partial_scope(binding).unwrap()]),
            topics: BTreeSet::new(),
            expires_at_ms: None,
        }
    }

    fn identity(fixture: &SourceIssuerFixture, token: String) -> AgentIdentityFile {
        AgentIdentityFile {
            format_version: 1,
            project_id: fixture.fence.project_id.clone(),
            run_id: fixture.binding.attempt.run_id.clone(),
            run_epoch: fixture.binding.attempt.run_epoch,
            owner_epoch: fixture.fence.epoch,
            token,
        }
    }

    fn handoff(fixture: &SourceIssuerFixture, broker: &Path) -> ManagedExecutionHandoff {
        let base = fixture.directory();
        let attempt = &fixture.binding.attempt;
        let context = fixture.home.join("auth-fixture-context.json");
        private_file(&context, b"{}");
        let spec = ExecutionSpec {
            format_version: 3,
            execution_id: attempt.execution_id.clone(),
            operation_id: attempt.operation_id.clone(),
            execution_root: base.join("executions"),
            manager_root: base.join("manager"),
            store_root: base.join("cas"),
            authority_roots: vec![fixture.home.clone(), base.join("host-source")],
            view_id: attempt.view_id.clone(),
            owner: attempt.owner.clone(),
            command: vec!["/bin/true".into()],
            environment: BTreeMap::new(),
            runtime: vec![],
            home_files: vec![],
            broker_directory: Some(broker.to_path_buf()),
            broker_binding: Some(BrokerBinding {
                project_id: attempt.project_id.clone(),
                run_id: attempt.run_id.clone(),
                run_epoch: attempt.run_epoch,
                owner_epoch: attempt.owner.epoch,
            }),
            workspace_observation: Some(WorkspaceObservationProfile {
                binding: fixture.binding.observation.clone(),
                endpoint_directory: base.join("workspace"),
                application: Some(HelperApplicationSpec {
                    format_version: 1,
                    kind: "source-auth-fixture-v1".into(),
                    host_context: ProtectedHostContextRef {
                        format_version: 1,
                        path: context,
                        sha256: ObjectRef::from_bytes(ObjectKind::Blob, b"{}").digest,
                        size_bytes: 2,
                    },
                    projection_directory: base.join("application"),
                }),
            }),
            profile: None,
            timeout_seconds: 15,
        };
        let binding = ManagedExecutionBinding {
            format_version: 2,
            request_id: Uuid::new_v4().to_string(),
            run_id: attempt.run_id.clone(),
            run_epoch: attempt.run_epoch,
            execution_id: attempt.execution_id.clone(),
            operation_id: attempt.operation_id.clone(),
            view_id: attempt.view_id.clone(),
            owner: attempt.owner.clone(),
            execution_profile: "claude-sdk".into(),
            frontend_start: ManagedFrontendStart::Immediate,
            server_command: None,
            guest_socket: "/tmp/cx.sock".into(),
            profile: None,
        };
        spec.validate().unwrap();
        ManagedExecutionHandoff::new(&spec, binding).unwrap()
    }

    #[test]
    fn actual_controller_handoff_keeps_host_source_independent_of_guest_rotation_and_close() {
        let mut fixture = SourceIssuerFixture::new(None);
        let fence = fixture.fence.clone();
        let grant = guest_grant(&fixture.binding);
        let first = fixture
            .journal()
            .issue_capability(&fence, &grant, 0)
            .unwrap();
        let guest_socket = fixture.directory().join("guest/broker.sock");
        let projected =
            agent_broker::project_identity(&guest_socket, &identity(&fixture, first.clone()))
                .unwrap();
        let broker = AgentBroker::spawn(BrokerConfig {
            journal_path: fixture.database.clone(),
            identity_file: projected.clone(),
            socket_path: guest_socket.clone(),
        })
        .unwrap();
        let expected = agent_broker::BrokerBinding::from(&identity(&fixture, first.clone()));
        let guest = BrokerClient::connect_partial_workspace(
            guest_socket.clone(),
            projected,
            &expected,
            &fixture.binding,
        )
        .unwrap();
        let endpoint = fixture.directory().join("host-source");
        let mut server = SourceAuthorizerServer::bind(
            &endpoint,
            fixture.binding.clone(),
            fixture.issuer().clone(),
            Limits::default(),
        )
        .unwrap();
        let handoff = handoff(&fixture, guest_socket.parent().unwrap());
        let args = handoff.helper_args();
        let notifier = fixture.directory().join("notify.sock");
        let listener = UnixListener::bind(&notifier).unwrap();
        listener.set_nonblocking(true).unwrap();
        let input = Input {
            handoff: args[4].clone(),
            parent: std::process::id(),
            endpoint,
            notifier,
            binding: fixture.binding.clone(),
        };
        let path = fixture.directory().join("controller-input.json");
        private_file(&path, &serde_json::to_vec(&input).unwrap());
        let child = OwnedChild(Some(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    CHILD,
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env_clear()
                .env(INPUT, &path)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        ));
        let (receipt, _control) = handoff.activate(child.0.as_ref().unwrap().id()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut notify = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "actual child did not authenticate source"
                    );
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("private fixture accept: {error}"),
            }
        };
        notify
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        notify
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut phase = [0];
        notify.read_exact(&mut phase).unwrap();
        assert_eq!(phase, *b"1");
        fixture
            .journal()
            .revoke_capability(&fence, &first, 1)
            .unwrap();
        let second = fixture
            .journal()
            .issue_capability(&fence, &grant, 2)
            .unwrap();
        agent_broker::project_identity(&guest_socket, &identity(&fixture, second)).unwrap();
        guest.authorize_partial_workspace(&fixture.binding).unwrap();
        broker.close();
        let deadline = Instant::now() + Duration::from_secs(1);
        while !broker.is_closed() {
            assert!(Instant::now() < deadline, "guest broker did not close");
            std::thread::sleep(Duration::from_millis(2));
        }
        notify.write_all(b"2").unwrap();
        notify.read_exact(&mut phase).unwrap();
        assert_eq!(phase, *b"2");
        let host = fixture.token.clone();
        fixture
            .journal()
            .revoke_capability(&fence, &host, 3)
            .unwrap();
        notify.write_all(b"3").unwrap();
        notify.read_exact(&mut phase).unwrap();
        assert_eq!(phase, *b"3");
        let output = child.output();
        // Probe the retired guest transport after the active source exchange;
        // its failure timeout must not stall the helper coordination channel.
        assert!(guest.authorize_partial_workspace(&fixture.binding).is_err());
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        receipt.wait(Duration::from_secs(2)).unwrap();
        let _settled = server.await_closed(Duration::from_secs(2)).unwrap();
        assert!(fixture.issuer().authorize(&fixture.binding).is_err());
        fixture.owner.as_mut().unwrap().finish(true).unwrap();
    }

    #[test]
    #[ignore = "private cleared-environment child explicitly invoked by actual parent fixture"]
    fn private_source_controller_child() {
        let path = std::env::var_os(INPUT).expect("private parent fixture required");
        let input: Input = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        let (spec, control) = receive_managed(&input.handoff, input.parent).unwrap();
        assert_eq!(spec.execution_id, input.binding.attempt.execution_id);
        assert_eq!(
            spec.workspace_observation.as_ref().unwrap().binding,
            input.binding.observation
        );
        let issuer = control.controller_process();
        drop(control);
        let client =
            SourceAuthorizerClient::connect(&input.endpoint, input.binding.clone(), issuer)
                .unwrap();
        client.authorize(&input.binding).unwrap();
        let mut notify = UnixStream::connect(&input.notifier).unwrap();
        notify
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        notify
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        notify.write_all(b"1").unwrap();
        let mut phase = [0];
        notify.read_exact(&mut phase).unwrap();
        assert_eq!(phase, *b"2");
        client.authorize(&input.binding).unwrap();
        notify.write_all(b"2").unwrap();
        notify.read_exact(&mut phase).unwrap();
        assert_eq!(phase, *b"3");
        assert!(client.authorize(&input.binding).is_err());
        assert!(client.check_current().is_err());
        assert!(client.authorize(&input.binding).is_err());
        notify.write_all(b"3").unwrap();
    }
}
