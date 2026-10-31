//! Opt-in Cloud UID1500 network primitives. Nothing here activates execution,
//! resolves DNS, opens an egress connection, or authenticates an owner/attempt.
//! A validated profile is configuration; kernel and capability witnesses are
//! physical evidence. All still require the existing current-attempt ingress.
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fmt,
    net::{IpAddr, Ipv4Addr},
};
use uuid::Uuid;

#[cfg(target_os = "linux")]
pub mod linux;

pub const NETWORK_PROFILE_VERSION: u32 = 1;
pub const MAX_NETWORK_PROFILE_BYTES: usize = 16 * 1024;
pub const MAX_QUALIFICATION_ORIGINS: usize = 8;
pub const MAX_ORIGIN_ADDRESSES: usize = 4;
pub const MAX_HOST_IPV4_ADDRESSES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkErrorKind {
    InvalidProfile,
    BindingMismatch,
    InvalidPolicy,
    PolicyDigestMismatch,
    InvalidBudgets,
    MissingHostInventory,
    DestinationDenied,
    WrongIdentity,
    NotFreshChild,
    WrongProcess,
    NamespaceUnavailable,
    InvalidNamespaceFd,
    ForeignNamespace,
    ParentWitnessRetained,
    LoopbackSetupFailed,
    CapabilityReadFailed,
    CapabilityDropFailed,
    CapabilitiesRemain,
}

/// Static errors never include profile JSON, host names, paths or credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkError {
    kind: NetworkErrorKind,
    os_error: Option<i32>,
}
impl NetworkError {
    fn new(kind: NetworkErrorKind) -> Self {
        Self {
            kind,
            os_error: None,
        }
    }
    fn os(kind: NetworkErrorKind) -> Self {
        Self {
            kind,
            os_error: std::io::Error::last_os_error().raw_os_error(),
        }
    }
    pub fn kind(self) -> NetworkErrorKind {
        self.kind
    }
    pub fn raw_os_error(self) -> Option<i32> {
        self.os_error
    }
}
impl fmt::Display for NetworkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "execution network {:?}", self.kind)
    }
}
impl std::error::Error for NetworkError {}
pub type NetworkResult<T> = Result<T, NetworkError>;

/// Decimal wire epoch, positive and bounded by SQLite/PostgreSQL signed BIGINT.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct NetworkEpoch(u64);
impl NetworkEpoch {
    pub fn new(value: u64) -> NetworkResult<Self> {
        if value == 0 || value > i64::MAX as u64 {
            return Err(NetworkError::new(NetworkErrorKind::InvalidProfile));
        }
        Ok(Self(value))
    }
    pub fn value(self) -> u64 {
        self.0
    }
}
impl fmt::Debug for NetworkEpoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl Serialize for NetworkEpoch {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_string())
    }
}
impl<'de> Deserialize<'de> for NetworkEpoch {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        if value.is_empty()
            || value.len() > 19
            || value.starts_with('0')
            || !value.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(serde::de::Error::custom("invalid network epoch"));
        }
        value
            .parse::<u64>()
            .ok()
            .and_then(|value| Self::new(value).ok())
            .ok_or_else(|| serde::de::Error::custom("invalid network epoch"))
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkBinding {
    pub project_id: String,
    pub owner: String,
    pub owner_epoch: NetworkEpoch,
    pub run_id: String,
    pub run_epoch: NetworkEpoch,
    pub request_id: String,
    pub execution_id: String,
    pub operation_id: String,
    pub view_id: String,
    pub network_id: String,
}
impl NetworkBinding {
    fn validate(&self) -> NetworkResult<()> {
        if ![&self.project_id, &self.owner, &self.run_id]
            .into_iter()
            .all(|value| valid_identity(value))
            || ![
                &self.request_id,
                &self.execution_id,
                &self.operation_id,
                &self.view_id,
                &self.network_id,
            ]
            .into_iter()
            .all(|value| canonical_uuid(value))
        {
            return Err(NetworkError::new(NetworkErrorKind::InvalidProfile));
        }
        Ok(())
    }
}

/// Trusted expected data supplied by host ingress, not a live owner grant.
/// Matching these strings neither authenticates the ingress nor keeps it live.
pub struct ExpectedNetworkBinding<'a> {
    pub project_id: &'a str,
    pub owner: &'a str,
    pub owner_epoch: u64,
    pub run_id: &'a str,
    pub run_epoch: u64,
    pub request_id: &'a str,
    pub execution_id: &'a str,
    pub operation_id: &'a str,
    pub view_id: &'a str,
    pub network_id: &'a str,
    pub readiness_nonce: &'a str,
}
impl ExpectedNetworkBinding<'_> {
    fn matches(&self, binding: &NetworkBinding, nonce: &str) -> bool {
        binding.project_id == self.project_id
            && binding.owner == self.owner
            && binding.owner_epoch.value() == self.owner_epoch
            && binding.run_id == self.run_id
            && binding.run_epoch.value() == self.run_epoch
            && binding.request_id == self.request_id
            && binding.execution_id == self.execution_id
            && binding.operation_id == self.operation_id
            && binding.view_id == self.view_id
            && binding.network_id == self.network_id
            && nonce == self.readiness_nonce
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkExecutionProfile {
    /// Only this first Cloud Linux qualification identity is supported.
    CloudUid1500LinuxV1,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkMode {
    OfflineV1,
    /// Proxy-aware HTTPS qualification only, never a general packet profile.
    QualificationHttpsConnectV1,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkBudgets {
    pub max_connections: u16,
    pub max_dns_jobs: u16,
    pub max_request_bytes: u32,
    pub max_buffer_bytes: u32,
    pub max_bytes_per_connection: u64,
    pub max_total_bytes: u64,
    pub connect_timeout_ms: u32,
    pub idle_timeout_ms: u32,
    pub total_timeout_ms: u32,
}
impl NetworkBudgets {
    fn validate(&self, mode: NetworkMode) -> NetworkResult<()> {
        if mode == NetworkMode::OfflineV1 {
            if self.max_connections != 0
                || self.max_dns_jobs != 0
                || self.max_request_bytes != 0
                || self.max_buffer_bytes != 0
                || self.max_bytes_per_connection != 0
                || self.max_total_bytes != 0
                || self.connect_timeout_ms != 0
                || self.idle_timeout_ms != 0
                || self.total_timeout_ms != 0
            {
                return Err(NetworkError::new(NetworkErrorKind::InvalidBudgets));
            }
            return Ok(());
        }
        let bytes = self
            .max_bytes_per_connection
            .checked_mul(u64::from(self.max_connections));
        let buffers = u64::from(self.max_buffer_bytes).checked_mul(u64::from(self.max_connections));
        if !(1..=8).contains(&self.max_connections)
            || !(1..=4).contains(&self.max_dns_jobs)
            || !(256..=16 * 1024).contains(&self.max_request_bytes)
            || !(4096..=256 * 1024).contains(&self.max_buffer_bytes)
            || self.max_request_bytes > self.max_buffer_bytes
            || !(1..=64 * 1024 * 1024).contains(&self.max_bytes_per_connection)
            || self.max_total_bytes < self.max_bytes_per_connection
            || self.max_total_bytes > 256 * 1024 * 1024
            || bytes.is_none_or(|maximum| self.max_total_bytes > maximum)
            || buffers.is_none_or(|maximum| maximum > 2 * 1024 * 1024)
            || !(1..=10_000).contains(&self.connect_timeout_ms)
            || !(1..=30_000).contains(&self.idle_timeout_ms)
            || !(1..=120_000).contains(&self.total_timeout_ms)
            || self.connect_timeout_ms > self.total_timeout_ms
            || self.idle_timeout_ms > self.total_timeout_ms
        {
            return Err(NetworkError::new(NetworkErrorKind::InvalidBudgets));
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationOrigin {
    pub host: String,
    pub ipv4_addresses: Vec<Ipv4Addr>,
    pub port: u16,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicIpv4HttpsPolicy {
    pub format_version: u32,
    pub origins: Vec<QualificationOrigin>,
}
impl PublicIpv4HttpsPolicy {
    fn validate(&self) -> NetworkResult<()> {
        if self.format_version != 1
            || self.origins.is_empty()
            || self.origins.len() > MAX_QUALIFICATION_ORIGINS
        {
            return Err(NetworkError::new(NetworkErrorKind::InvalidPolicy));
        }
        let mut previous: Option<&str> = None;
        for origin in &self.origins {
            if !canonical_public_hostname(&origin.host)
                || origin.port != 443
                || previous.is_some_and(|host| host >= origin.host.as_str())
                || origin.ipv4_addresses.is_empty()
                || origin.ipv4_addresses.len() > MAX_ORIGIN_ADDRESSES
                || !origin
                    .ipv4_addresses
                    .iter()
                    .all(|address| is_qualification_public_ipv4(*address))
                || !origin
                    .ipv4_addresses
                    .windows(2)
                    .all(|pair| pair[0] < pair[1])
            {
                return Err(NetworkError::new(NetworkErrorKind::InvalidPolicy));
            }
            previous = Some(&origin.host);
        }
        Ok(())
    }
    /// Schema1 canonical bytes: declaration-order keys, sorted origins/IPs, no
    /// arbitrary JSON objects, optional fields, floats or unordered maps.
    pub fn canonical_bytes(&self) -> NetworkResult<Vec<u8>> {
        self.validate()?;
        serde_json::to_vec(self).map_err(|_| NetworkError::new(NetworkErrorKind::InvalidPolicy))
    }
    pub fn sha256(&self) -> NetworkResult<String> {
        Ok(hex::encode(Sha256::digest(self.canonical_bytes()?)))
    }
}

/// Trusted host inventory, not an authority grant. Callers must refresh it or
/// freeze admission when actual host addresses change. No implicit empty set.
pub struct HostIpv4DenySet(BTreeSet<Ipv4Addr>);
impl HostIpv4DenySet {
    pub fn from_trusted_host_inventory(
        addresses: impl IntoIterator<Item = Ipv4Addr>,
    ) -> NetworkResult<Self> {
        let mut set = BTreeSet::new();
        let mut count = 0usize;
        for address in addresses {
            count = count
                .checked_add(1)
                .ok_or_else(|| NetworkError::new(NetworkErrorKind::MissingHostInventory))?;
            if count > MAX_HOST_IPV4_ADDRESSES {
                return Err(NetworkError::new(NetworkErrorKind::MissingHostInventory));
            }
            set.insert(address);
        }
        if set.is_empty() {
            return Err(NetworkError::new(NetworkErrorKind::MissingHostInventory));
        }
        Ok(Self(set))
    }
    pub fn contains(&self, address: Ipv4Addr) -> bool {
        self.0.contains(&address)
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkProfileV1 {
    pub format_version: u32,
    pub execution_profile: NetworkExecutionProfile,
    pub binding: NetworkBinding,
    pub mode: NetworkMode,
    pub policy: Option<PublicIpv4HttpsPolicy>,
    pub policy_sha256: Option<String>,
    pub budgets: NetworkBudgets,
    pub readiness_nonce: String,
}
impl NetworkProfileV1 {
    /// Prefer this bounded entry to deserializing unbounded caller JSON. This
    /// still validates configuration only; ingress authorization is external.
    pub fn from_json(
        bytes: &[u8],
        expected: &ExpectedNetworkBinding<'_>,
        host: Option<&HostIpv4DenySet>,
    ) -> NetworkResult<ValidatedNetworkProfile> {
        if bytes.is_empty() || bytes.len() > MAX_NETWORK_PROFILE_BYTES {
            return Err(NetworkError::new(NetworkErrorKind::InvalidProfile));
        }
        let profile: Self = serde_json::from_slice(bytes)
            .map_err(|_| NetworkError::new(NetworkErrorKind::InvalidProfile))?;
        profile.validate(expected, host)
    }
    pub fn validate(
        self,
        expected: &ExpectedNetworkBinding<'_>,
        host: Option<&HostIpv4DenySet>,
    ) -> NetworkResult<ValidatedNetworkProfile> {
        if self.format_version != NETWORK_PROFILE_VERSION || !canonical_uuid(&self.readiness_nonce)
        {
            return Err(NetworkError::new(NetworkErrorKind::InvalidProfile));
        }
        self.binding.validate()?;
        if !expected.matches(&self.binding, &self.readiness_nonce) {
            return Err(NetworkError::new(NetworkErrorKind::BindingMismatch));
        }
        self.budgets.validate(self.mode)?;
        match self.mode {
            NetworkMode::OfflineV1 => {
                if self.policy.is_some() || self.policy_sha256.is_some() {
                    return Err(NetworkError::new(NetworkErrorKind::InvalidPolicy));
                }
            }
            NetworkMode::QualificationHttpsConnectV1 => {
                let policy = self
                    .policy
                    .as_ref()
                    .ok_or_else(|| NetworkError::new(NetworkErrorKind::InvalidPolicy))?;
                let host =
                    host.ok_or_else(|| NetworkError::new(NetworkErrorKind::MissingHostInventory))?;
                let digest = policy.sha256()?;
                if self.policy_sha256.as_deref() != Some(digest.as_str()) {
                    return Err(NetworkError::new(NetworkErrorKind::PolicyDigestMismatch));
                }
                if policy
                    .origins
                    .iter()
                    .flat_map(|origin| &origin.ipv4_addresses)
                    .any(|address| host.contains(*address))
                {
                    return Err(NetworkError::new(NetworkErrorKind::DestinationDenied));
                }
            }
        }
        let network_id = Uuid::parse_str(&self.binding.network_id)
            .map_err(|_| NetworkError::new(NetworkErrorKind::InvalidProfile))?;
        let canonical = serde_json::to_vec(&self)
            .map_err(|_| NetworkError::new(NetworkErrorKind::InvalidProfile))?;
        if canonical.len() > MAX_NETWORK_PROFILE_BYTES {
            return Err(NetworkError::new(NetworkErrorKind::InvalidProfile));
        }
        let profile_digest = Sha256::digest(canonical).into();
        Ok(ValidatedNetworkProfile {
            profile: self,
            network_id,
            profile_digest,
        })
    }
}

/// Immutable validated configuration, never a live network/current-owner grant.
pub struct ValidatedNetworkProfile {
    profile: NetworkProfileV1,
    network_id: Uuid,
    profile_digest: [u8; 32],
}
impl fmt::Debug for ValidatedNetworkProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ValidatedNetworkProfile")
            .field("mode", &self.profile.mode)
            .finish_non_exhaustive()
    }
}
impl ValidatedNetworkProfile {
    pub fn binding(&self) -> &NetworkBinding {
        &self.profile.binding
    }
    pub fn mode(&self) -> NetworkMode {
        self.profile.mode
    }
    pub fn budgets(&self) -> &NetworkBudgets {
        &self.profile.budgets
    }
    pub fn execution_profile(&self) -> NetworkExecutionProfile {
        self.profile.execution_profile
    }
    pub fn network_id(&self) -> Uuid {
        self.network_id
    }
    /// Bind physical setup to the full validated configuration, including the
    /// policy, budgets and readiness challenge, not only a reusable UUID label.
    pub fn profile_digest(&self) -> [u8; 32] {
        self.profile_digest
    }
    /// Check an already resolved numeric target against pinned configuration
    /// and a freshly supplied host deny set. Does not resolve, dial or grant
    /// live attempt authority; no unchecked DNS/IPv6 fallback is available.
    pub fn check_destination(
        &self,
        hostname: &str,
        port: u16,
        address: IpAddr,
        host: &HostIpv4DenySet,
    ) -> NetworkResult<CheckedQualificationDestination> {
        let IpAddr::V4(address) = address else {
            return Err(NetworkError::new(NetworkErrorKind::DestinationDenied));
        };
        if self.mode() != NetworkMode::QualificationHttpsConnectV1
            || port != 443
            || !canonical_public_hostname(hostname)
            || !is_qualification_public_ipv4(address)
            || host.contains(address)
        {
            return Err(NetworkError::new(NetworkErrorKind::DestinationDenied));
        }
        let allowed = self.profile.policy.as_ref().is_some_and(|policy| {
            policy.origins.iter().any(|origin| {
                origin.host == hostname
                    && origin.port == port
                    && origin.ipv4_addresses.contains(&address)
            })
        });
        if !allowed {
            return Err(NetworkError::new(NetworkErrorKind::DestinationDenied));
        }
        Ok(CheckedQualificationDestination { address, port })
    }
}

/// Checked numeric configuration only; this is not a socket or admission slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckedQualificationDestination {
    address: Ipv4Addr,
    port: u16,
}
impl CheckedQualificationDestination {
    pub fn address(self) -> Ipv4Addr {
        self.address
    }
    pub fn port(self) -> u16 {
        self.port
    }
}

fn canonical_uuid(value: &str) -> bool {
    value.len() == 36
        && Uuid::parse_str(value).is_ok_and(|uuid| !uuid.is_nil() && uuid.to_string() == value)
}
fn valid_identity(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.trim() == value
        && !value.chars().any(char::is_control)
}
fn canonical_public_hostname(host: &str) -> bool {
    if host.len() > 253
        || !host.contains('.')
        || !host.bytes().any(|byte| byte.is_ascii_lowercase())
        || !host.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'.'
        })
    {
        return false;
    }
    if [
        "localhost",
        "local",
        "localdomain",
        "internal",
        "invalid",
        "test",
    ]
    .into_iter()
    .any(|suffix| {
        host == suffix
            || host
                .strip_suffix(suffix)
                .is_some_and(|prefix| prefix.ends_with('.'))
    }) {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty() && label.len() <= 63 && !label.starts_with('-') && !label.ends_with('-')
    })
}

/// Conservative first qualification policy: reject the entire special-purpose
/// prefixes, including globally reachable exceptions. This does not establish
/// routability or account for actual host IPs (the separate deny set does).
/// IANA IPv4 special-purpose registry, checked 2026-10-31; multicast/reserved
/// 224/3 additionally denied. No IPv6, mapped or NAT64 address is accepted.
pub fn is_qualification_public_ipv4(address: Ipv4Addr) -> bool {
    let value = u32::from(address);
    const DENIED: &[(u32, u32)] = &[
        (0x00000000, 8),
        (0x0a000000, 8),
        (0x64400000, 10),
        (0x7f000000, 8),
        (0xa9fe0000, 16),
        (0xac100000, 12),
        (0xc0000000, 24),
        (0xc0000200, 24),
        (0xc01fc400, 24),
        (0xc034c100, 24),
        (0xc0586300, 24),
        (0xc0a80000, 16),
        (0xc0af3000, 24),
        (0xc6120000, 15),
        (0xc6336400, 24),
        (0xcb007100, 24),
        (0xe0000000, 3),
    ];
    !DENIED
        .iter()
        .any(|(prefix, bits)| value & (u32::MAX << (32 - bits)) == *prefix)
}
