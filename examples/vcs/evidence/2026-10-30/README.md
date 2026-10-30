# jj 0.40 extension API qualification, 2026-10-30

The [source/evidence archive](jj040-local-prototype.tar.gz) contains a standalone
CLI extension, exact Cargo manifest/lockfile, Apache license, Python qualification
scripts, raw JSON command results, build/feature summaries and provenance.
[SHA256SUMS](SHA256SUMS) binds the unchanged archive. No binaries, provider
profiles, credentials, checkout objects or target/cache directories are included.
All fixture inputs are synthetic VCS data; local absolute paths identify only
the disposable research workspace, not provider-private profiles.

This is the **LocalWorkingCopy API prototype**, separate from the product
`vcs-bridge/` crate and its injected VFS provider. Its scanner and eager private
fixture copies are intentional. It does not qualify live VFS snapshots, CoW
launch speed, authority isolation, distributed execution or crash-safe product
integration. The [VCS bridge note](../../../../docs/architecture/vcs-bridge.md)
records what the six tests prove and the remaining gates.

Results: **6/6 passed**, 82 real commands, 9.591 seconds qualification time on
Linux. Each case forked two private metadata copies sharing only a kernel
read-only Git object seed. Real jj commands, current and legacy change IDs,
conflict export/import into stock jj, Git-authored commits and older-Git fetch
behavior were exercised. Compile used Rust 1.99.1 and exact jj-cli/jj-lib 0.40.0;
289 crate source files matched upstream 941d0290f5511db1d1cf8e4fe7365ba7d1828ea0.

To reproduce, extract into a new directory on a filesystem with sufficient space
and follow its README. The Linux harness creates a private user/mount namespace;
its only new mount is the synthetic object seed. Qualification with the portable
mode-only fallback is explicitly weaker. macOS was not run in this evidence.

The later [product checkpoint](product/README.md) is separate: 25 feature-enabled
bridge tests, 44 journal tests, a fresh explicit RO fixture and six scanner CLI
cases. It includes actual nonempty source mapping, owning-VFS observations and
retained format-2 publication evidence. Production activation remains gated.
