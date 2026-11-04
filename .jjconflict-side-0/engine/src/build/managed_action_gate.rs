//! A sealed CAS input and a read-only child mount do not exclude writers to the
//! host materialization. Advisory locks, modes and pre/post hashes cannot prove
//! that an agent, peer or already-open writable descriptor stayed quiescent.
//!
//! No production carrier currently owns that exclusion from materialization
//! through frontend/child settlement and publication. Refuse action execution and
//! reusable publication rather than allowing uploads before post-verification.
//! Preparation caches and explicitly noncacheable SDK artifacts are separate.
//!
//! Reopening admission requires an original enforced lifetime guard integrated
//! with the SDK compiler boundary, including every permitted writer and existing
//! writable descriptor. Serialized claims cannot supply that guard. Qualify the
//! supported managed profile with real remote misses/hits and output-equivalence
//! changes to source, dependencies, SDK, flags, platform and environment before
//! enabling either frontend cache access or result-index reads/publication.

pub(super) fn require_input_custody() -> anyhow::Result<()> {
    anyhow::bail!(
        "managed compiler action reuse and publication disabled: full-lifetime host SDK/source writer exclusion is not enforced"
    )
}
