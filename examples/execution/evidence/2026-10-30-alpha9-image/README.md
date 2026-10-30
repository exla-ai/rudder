# Alpha.9 worker image boot

The complete Node22 worker image from source
`11f1f75e5a18555f6c7e257aacc6b4097f425c5e` passed its owned disposable Fly
canary. [The report](report.json) binds the actual image digest, VM instance,
installed versions, native binary pair hashes and verified deletion.

The real image bootstrap permanently enters UID/GID1500 with no supplementary
groups, cleared capability sets and no-new-privileges. The real entrypoint
validates managed access to character device10:229 and initializes its owned
cache. A Node preload then checks FUSE open, root reentry denial, package/native
capabilities, Claude, Codex, Tailscale, Git2.56 and jj0.40, including fetching a
changed file from a disposable bare Git remote. The preload holds before the
home/sail supervisor module evaluates. No vault, model, user workspace, service,
volume or app secret participates.

[The driver](qualify-image.mjs) requires an exact expected digest, rejects app
secrets and targets only its generated name/nonce. It checks the resolved digest
before accepting any proof and deletes only that owned VM in its finalizer.
The first digest-only URI was rejected by Fly with `MANIFEST_UNKNOWN`; its
[failure report](digest-uri-failure.json) confirms no VM was left. The successful
run used the versioned tag and verified the resolved digest. This qualifies boot
and installed runtime composition, without claiming real-agent execution,
default managed launch, distributed build performance or fleet adoption.
