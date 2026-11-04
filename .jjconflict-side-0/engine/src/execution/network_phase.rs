//! Owned namespace admission for the v4 offline execution path. Wire fields
//! are audit data. Only descriptors from the owned control socket, corroborated
//! against a still-live pidfd, construct the in-memory witness below.
use super::{
    network::{
        linux::{self, NamespaceFd, NamespaceKind, OwnedChildNetwork},
        NetworkMode,
    },
    ExecutionSpec,
};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::Read,
    os::fd::{AsFd, AsRawFd, OwnedFd, RawFd},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NetworkReadyFrame {
    pub format_version: u32,
    pub execution_id: String,
    pub child_nonce: String,
    pub execution_request_digest: String,
    pub network_profile_digest: String,
    pub network_id: String,
    pub readiness_nonce: String,
}
impl NetworkReadyFrame {
    pub(super) fn from_child(
        spec: &ExecutionSpec,
        child_nonce: &str,
        execution_request_digest: &str,
        network: &OwnedChildNetwork,
    ) -> Result<Self> {
        network.verify_current()?;
        let profile = spec
            .profile
            .as_ref()
            .context("namespace readiness requires a v4 profile")?;
        profile.validate_metadata(spec)?;
        let validated = profile.validated_network(spec)?;
        ensure!(
            network.profile_digest() == validated.profile_digest()
                && network.network_id() == validated.network_id(),
            "child network differs from sealed profile"
        );
        Ok(Self {
            format_version: 1,
            execution_id: spec.execution_id.clone(),
            child_nonce: child_nonce.into(),
            execution_request_digest: execution_request_digest.into(),
            network_profile_digest: hex::encode(validated.profile_digest()),
            network_id: profile.network.binding.network_id.clone(),
            readiness_nonce: profile.network.readiness_nonce.clone(),
        })
    }
}

/// Retains only the child's USER/NET namespace FDs. No host/baseline namespace
/// descriptor is transferred to the guest. No serde or public constructor.
pub(super) struct OwnedNetworkReady {
    user: NamespaceFd,
    network: NamespaceFd,
    frame: NetworkReadyFrame,
}
impl OwnedNetworkReady {
    pub(super) fn receive(
        spec: &ExecutionSpec,
        child_nonce: &str,
        frame: NetworkReadyFrame,
        process: &OwnedFd,
        expected_pid: u32,
        descriptors: Vec<OwnedFd>,
    ) -> Result<Self> {
        let profile = spec
            .profile
            .as_ref()
            .context("unexpected namespace readiness")?;
        profile.validate_metadata(spec)?;
        let validated = profile.validated_network(spec)?;
        ensure!(
            profile.network.mode == NetworkMode::OfflineV1
                && frame.format_version == 1
                && frame.execution_id == spec.execution_id
                && frame.child_nonce == child_nonce
                && frame.execution_request_digest
                    == hex::encode(Sha256::digest(crate::protocol::canonical_json(spec)?))
                && frame.network_profile_digest == hex::encode(validated.profile_digest())
                && frame.network_id == profile.network.binding.network_id
                && frame.readiness_nonce == profile.network.readiness_nonce,
            "namespace readiness differs from the sealed attempt"
        );
        Self::receive_checked(frame, process, expected_pid, descriptors)
    }
    /// The outer managed receiver has already matched this frame to the exact
    /// sealed profile/request digest. Descriptor inspection remains mandatory.
    pub(super) fn receive_checked(
        frame: NetworkReadyFrame,
        process: &OwnedFd,
        expected_pid: u32,
        mut descriptors: Vec<OwnedFd>,
    ) -> Result<Self> {
        ensure!(
            descriptors.len() == 2,
            "namespace readiness requires two descriptors"
        );
        ensure!(
            live_pid(process)? == expected_pid,
            "namespace readiness has another process"
        );
        let network = NamespaceFd::inspect(descriptors.pop().unwrap(), NamespaceKind::Network)?;
        let user = NamespaceFd::inspect(descriptors.pop().unwrap(), NamespaceKind::User)?;
        linux::verify_namespace_owner(&user, &network)?;
        ensure!(
            user.identity() != NamespaceFd::current(NamespaceKind::User)?.identity()
                && network.identity() != NamespaceFd::current(NamespaceKind::Network)?.identity(),
            "execution did not enter a private child USER/NET namespace"
        );
        let witness = Self {
            user,
            network,
            frame,
        };
        witness.validate_process_namespaces(process, expected_pid)?;
        Ok(witness)
    }
    pub(super) fn frame(&self) -> &NetworkReadyFrame {
        &self.frame
    }
    pub(super) fn descriptors(&self) -> [RawFd; 2] {
        [
            self.user.as_fd().as_raw_fd(),
            self.network.as_fd().as_raw_fd(),
        ]
    }
    pub(super) fn validate_retained(&self) -> Result<()> {
        linux::verify_namespace_owner(&self.user, &self.network)?;
        Ok(())
    }
    pub(super) fn validate_process_namespaces(
        &self,
        process: &OwnedFd,
        expected_pid: u32,
    ) -> Result<()> {
        ensure!(
            live_pid(process)? == expected_pid,
            "owned namespace process retired"
        );
        ensure!(
            NamespaceFd::for_process(expected_pid, NamespaceKind::User)?.identity()
                == self.user.identity()
                && NamespaceFd::for_process(expected_pid, NamespaceKind::Network)?.identity()
                    == self.network.identity(),
            "owned process does not inhabit the admitted namespaces"
        );
        self.validate_retained()?;
        ensure!(
            live_pid(process)? == expected_pid,
            "owned namespace process retired during inspection"
        );
        Ok(())
    }
    pub(super) fn validate_backend(&self, process: &OwnedFd) -> Result<u32> {
        let pid = live_pid(process)?;
        self.validate_process_namespaces(process, pid)?;
        verify_backend_capabilities(pid)?;
        ensure!(
            live_pid(process)? == pid,
            "backend retired during capability inspection"
        );
        Ok(pid)
    }
    pub(super) fn audit(&self) -> Result<serde_json::Value> {
        self.validate_retained()?;
        Ok(
            serde_json::json!({ "format_version": 1, "profile_digest": self.frame.network_profile_digest,
            "network_id": self.frame.network_id, "readiness_nonce": self.frame.readiness_nonce,
            "user": {"device": self.user.identity().device(), "inode": self.user.identity().inode()},
            "network": {"device": self.network.identity().device(), "inode": self.network.identity().inode()},
            "mode": "offline_v1" }),
        )
    }
}

pub(super) fn live_pid(process: &OwnedFd) -> Result<u32> {
    let mut poll = libc::pollfd {
        fd: process.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    ensure!(
        unsafe { libc::poll(&mut poll, 1, 0) } == 0,
        "owned process pidfd is not live"
    );
    let mut bytes = Vec::new();
    File::open(format!("/proc/self/fdinfo/{}", process.as_raw_fd()))?
        .take(16 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 16 * 1024,
        "owned process fdinfo exceeds bound"
    );
    let text = std::str::from_utf8(&bytes)?;
    let mut fields = text.lines().filter_map(|line| line.strip_prefix("Pid:\t"));
    let pid: u32 = fields
        .next()
        .context("descriptor is not a live pidfd")?
        .parse()?;
    ensure!(
        pid > 1 && fields.next().is_none(),
        "invalid owned process identity"
    );
    Ok(pid)
}

fn verify_backend_capabilities(pid: u32) -> Result<()> {
    let mut bytes = Vec::new();
    File::open(format!("/proc/{pid}/status"))?
        .take(64 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 64 * 1024, "backend status exceeds bound");
    verify_capability_status(std::str::from_utf8(&bytes)?)
}
fn verify_capability_status(status: &str) -> Result<()> {
    for key in [
        "CapInh:",
        "CapPrm:",
        "CapEff:",
        "CapBnd:",
        "CapAmb:",
        "NoNewPrivs:",
    ] {
        let mut values = status.lines().filter_map(|line| line.strip_prefix(key));
        let value = values
            .next()
            .context("backend privilege field missing")?
            .trim();
        ensure!(
            values.next().is_none(),
            "backend privilege field duplicated"
        );
        if key == "NoNewPrivs:" {
            ensure!(value == "1", "backend lacks no_new_privileges");
        } else {
            ensure!(
                !value.is_empty()
                    && value.len() <= 16
                    && value.bytes().all(|b| b.is_ascii_hexdigit())
                    && u64::from_str_radix(value, 16)? == 0,
                "backend retains capabilities"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn backend_status_requires_every_zero_set_and_nnp_exactly_once() {
        let good = "Name:\tfixture\nCapInh:\t0000000000000000\nCapPrm:\t0000000000000000\nCapEff:\t0000000000000000\nCapBnd:\t0000000000000000\nCapAmb:\t0000000000000000\nNoNewPrivs:\t1\n";
        verify_capability_status(good).unwrap();
        for key in ["CapInh:", "CapPrm:", "CapEff:", "CapBnd:", "CapAmb:"] {
            assert!(verify_capability_status(&good.replace(
                &format!("{key}\t0000000000000000"),
                &format!("{key}\t0000000000000001")
            ))
            .is_err());
            assert!(verify_capability_status(
                &good.replace(&format!("{key}\t0000000000000000\n"), "")
            )
            .is_err());
        }
        assert!(
            verify_capability_status(&good.replace("NoNewPrivs:\t1", "NoNewPrivs:\t0")).is_err()
        );
        assert!(verify_capability_status(&format!("{good}CapEff:\t0\n")).is_err());
    }
    #[test]
    fn ordinary_descriptor_cannot_claim_a_live_process() {
        let fd: OwnedFd = File::open("/dev/null").unwrap().into();
        assert!(live_pid(&fd).is_err());
    }
}
