//! Private metadata forks from a trusted, quiesced prepared repository.
//!
//! Preparation scans VCS metadata/objects once, never source. Forking copies
//! only private metadata; it neither runs `jj workspace add` nor enumerates the
//! prepared source/dependency view. Copying metadata is not an O(1) CoW claim.
//! The engine retains the seed artifact and controls the RO mount lifetime.

use crate::{AttemptBinding, CommitIdentity, JjOperationId, atomic};
use anyhow::{Context, Result, ensure};
use rudder_engine::protocol::{ObjectKind, ObjectRef, canonical_json};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

const SAFE_GIT_CONFIG: &[u8] =
    b"[core]\nrepositoryformatversion = 0\nbare = false\nlogallrefupdates = false\n";

/// Narrow host-command profile, not a general Git config parser. Unsupported
/// sections, includes, continuations, helpers, hooks and external commands fail
/// before a managed CLI is launched. Internal and colocated stores may differ
/// only in these inert repository-format/filesystem boolean settings.
pub fn validate_private_git_config(bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() <= 1024 * 1024,
        "private Git config exceeds bound"
    );
    let text = std::str::from_utf8(bytes)?;
    let mut section = String::new();
    let mut seen = std::collections::BTreeSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            ensure!(line.ends_with(']'), "unsupported Git config section syntax");
            section = line[1..line.len() - 1].trim().to_ascii_lowercase();
            ensure!(
                matches!(section.as_str(), "core" | "extensions"),
                "unqualified private Git config section"
            );
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .context("unsupported private Git config syntax")?;
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim().to_ascii_lowercase();
        ensure!(
            seen.insert((section.clone(), key.clone())),
            "duplicate private Git config key"
        );
        let allowed = match (section.as_str(), key.as_str()) {
            ("core", "repositoryformatversion") => value == "0",
            (
                "core",
                "bare" | "filemode" | "logallrefupdates" | "ignorecase" | "precomposeunicode"
                | "symlinks",
            ) => matches!(value.as_str(), "true" | "false"),
            ("extensions", "objectformat") => value == "sha1",
            _ => false,
        };
        ensure!(
            allowed,
            "private Git config exceeds the fixed command profile"
        );
    }
    ensure!(
        seen.contains(&("core".into(), "repositoryformatversion".into())),
        "private Git config has no qualified format"
    );
    Ok(())
}
const MAX_TEMPLATE_FILES: usize = 32768;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeedFile {
    pub sha256: String,
    pub size_bytes: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetadataTemplate {
    pub format_version: u32,
    pub jj_version: String,
    pub baseline: CommitIdentity,
    pub operation: JjOperationId,
    pub metadata: BTreeMap<String, SeedFile>,
    pub objects: BTreeMap<String, SeedFile>,
}
impl MetadataTemplate {
    pub fn identity(&self) -> Result<ObjectRef> {
        self.validate()?;
        Ok(ObjectRef::from_bytes(
            ObjectKind::Blob,
            &canonical_json(self)?,
        ))
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.format_version == 1 && self.jj_version == "0.40.0",
            "unsupported metadata seed"
        );
        self.baseline.validate()?;
        self.operation.validate()?;
        ensure!(
            self.metadata.len() + self.objects.len() <= MAX_TEMPLATE_FILES,
            "metadata seed exceeds file-count bound"
        );
        for (path, file) in self.metadata.iter().chain(&self.objects) {
            relative_path(path)?;
            ensure!(
                file.sha256.len() == 64
                    && file
                        .sha256
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "invalid seed digest"
            );
        }
        for path in self.metadata.keys() {
            ensure!(
                path.starts_with(".jj/")
                    || path == ".git/HEAD"
                    || path == ".git/index"
                    || path == ".git/config"
                    || path == ".git/packed-refs"
                    || path == ".git/shallow"
                    || path.starts_with(".git/refs/"),
                "unexpected metadata namespace"
            );
            ensure!(
                !path.ends_with("/config.toml"),
                "jj config cannot be part of a private metadata seed"
            );
        }
        ensure!(
            !self.objects.contains_key("info/alternates"),
            "shared object seed cannot inherit external alternates"
        );
        let safe_config = SeedFile {
            sha256: hex::encode(Sha256::digest(SAFE_GIT_CONFIG)),
            size_bytes: SAFE_GIT_CONFIG.len() as u64,
        };
        let safe_target = SeedFile {
            sha256: hex::encode(Sha256::digest(b"../../../.git")),
            size_bytes: 13,
        };
        ensure!(
            self.metadata.get(".git/config") == Some(&safe_config)
                && self.metadata.get(".jj/repo/store/git_target") == Some(&safe_target),
            "private template configuration or Git target is unsafe"
        );
        ensure!(
            self.metadata.contains_key(".git/HEAD")
                && self.metadata.contains_key(".jj/working_copy/type"),
            "missing private repository metadata"
        );
        Ok(())
    }
}

/// Verified objects remain immutable through an engine-owned read-only mount.
/// Opening verifies the seed once; forks reverify metadata while copying it,
/// without rereading every shared Git object for each agent.
pub struct VerifiedMetadataTemplate {
    root: PathBuf,
    object_mount: PathBuf,
    manifest: MetadataTemplate,
    identity: ObjectRef,
    require_readonly: bool,
    retained_seed: Option<ObjectRef>,
}
impl VerifiedMetadataTemplate {
    pub fn open(root: &Path, object_mount: &Path, expected: &ObjectRef) -> Result<Self> {
        Self::open_inner(root, object_mount, expected, true)
    }
    /// Link the manifest to an actually retained CAS seed directory. The
    /// engine keeps that seed pin through live/recoverable metadata lifetime.
    pub fn open_retained(
        store: &rudder_engine::store::LocalStore,
        seed: &ObjectRef,
        root: &Path,
        object_mount: &Path,
    ) -> Result<Self> {
        Self::open_retained_inner(store, seed, root, object_mount, true)
    }
    #[cfg(feature = "local-development")]
    pub fn open_retained_development_integrity_only(
        store: &rudder_engine::store::LocalStore,
        seed: &ObjectRef,
        root: &Path,
        object_mount: &Path,
    ) -> Result<Self> {
        Self::open_retained_inner(store, seed, root, object_mount, false)
    }
    fn open_retained_inner(
        store: &rudder_engine::store::LocalStore,
        seed: &ObjectRef,
        root: &Path,
        object_mount: &Path,
        require_readonly: bool,
    ) -> Result<Self> {
        ensure!(
            seed.kind == ObjectKind::Directory
                && store.retention()?.pins.values().any(|old| old == seed),
            "VCS seed lacks current retained directory authority"
        );
        let entry = rudder_engine::tree::lookup(store, seed, b"manifest.json")?
            .context("retained VCS seed lacks manifest")?;
        let (object, logical_size) = match entry.node {
            rudder_engine::directory::EntryNode::File {
                object,
                logical_size,
                ..
            } => (object, logical_size),
            _ => anyhow::bail!("retained seed manifest must be a file"),
        };
        ensure!(
            logical_size <= atomic::MAX_STATE_BYTES,
            "retained manifest exceeds size limit"
        );
        let bytes = rudder_engine::content::read_file_range(
            store,
            &object,
            0,
            usize::try_from(logical_size)?,
        )?;
        let identity = ObjectRef::from_bytes(ObjectKind::Blob, &bytes);
        let mut verified = Self::open_inner(root, object_mount, &identity, require_readonly)?;
        verified.retained_seed = Some(seed.clone());
        Ok(verified)
    }
    #[cfg(feature = "local-development")]
    pub fn open_development_integrity_only(
        root: &Path,
        object_mount: &Path,
        expected: &ObjectRef,
    ) -> Result<Self> {
        Self::open_inner(root, object_mount, expected, false)
    }
    fn open_inner(
        root: &Path,
        object_mount: &Path,
        expected: &ObjectRef,
        require_readonly: bool,
    ) -> Result<Self> {
        ensure!(
            expected.kind == ObjectKind::Blob,
            "seed manifest identity must be Blob"
        );
        let manifest: MetadataTemplate = atomic::read_json(&root.join("manifest.json"))?;
        manifest.validate()?;
        ensure!(
            &manifest.identity()? == expected,
            "metadata template identity mismatch"
        );
        let object_mount = object_mount.canonicalize()?;
        ensure_directory(&object_mount)?;
        if require_readonly {
            require_readonly_mount(&object_mount)?;
        }
        let mut observed = BTreeMap::new();
        collect_descriptors(&object_mount, Path::new(""), &mut observed)?;
        ensure!(
            observed == manifest.objects,
            "read-only seed object set or bytes differ from manifest"
        );
        Ok(Self {
            root: root.canonicalize()?,
            object_mount,
            manifest,
            identity: expected.clone(),
            require_readonly,
            retained_seed: None,
        })
    }
    pub fn identity(&self) -> &ObjectRef {
        &self.identity
    }
    pub fn manifest(&self) -> &MetadataTemplate {
        &self.manifest
    }
    pub fn retained_seed(&self) -> Option<&ObjectRef> {
        self.retained_seed.as_ref()
    }
}

/// Prepare only from an engine-owned, already-quiesced private repository.
/// This is not a safe arbitrary-main-checkout snapshot or untrusted import.
/// Caller holds its source lock and owns the destination (which must be new).
pub fn prepare_metadata_template(
    private_repository: &Path,
    destination: &Path,
    baseline: CommitIdentity,
    operation: JjOperationId,
) -> Result<MetadataTemplate> {
    baseline.validate()?;
    operation.validate()?;
    ensure_directory(&private_repository.join(".jj"))?;
    ensure_directory(&private_repository.join(".git"))?;
    ensure_directory(&private_repository.join(".jj/repo"))?;
    ensure_directory(&private_repository.join(".git/objects"))?;
    let git_target = fs::read(private_repository.join(".jj/repo/store/git_target"))?;
    let git_target = std::str::from_utf8(&git_target).context("Git target path is not UTF-8")?;
    ensure!(
        private_repository
            .join(".jj/repo/store")
            .join(git_target)
            .canonicalize()?
            == private_repository.join(".git").canonicalize()?,
        "metadata seed must use its own private colocated Git repository"
    );
    ensure!(
        !private_repository
            .join(".git/objects/info/alternates")
            .try_exists()?,
        "prepare a self-contained object closure before exporting a seed"
    );
    create_private_directory(destination)?;
    create_private_directory(&destination.join("metadata"))?;
    create_private_directory(&destination.join("objects"))?;
    let mut metadata = BTreeMap::new();
    let mut objects = BTreeMap::new();
    collect_copy(
        private_repository,
        Path::new(".jj"),
        &destination.join("metadata"),
        &mut metadata,
        true,
    )?;
    // Only operational Git metadata. Config, hooks, logs, worktrees, submodule
    // repositories and credential/remote settings are deliberately not copied.
    for part in ["HEAD", "index", "refs", "packed-refs", "shallow"] {
        let rel = Path::new(".git").join(part);
        if private_repository.join(&rel).try_exists()? {
            collect_copy(
                private_repository,
                &rel,
                &destination.join("metadata"),
                &mut metadata,
                false,
            )?;
        }
    }
    write_seed_file(
        &destination.join("metadata/.git/config"),
        SAFE_GIT_CONFIG,
        ".git/config",
        &mut metadata,
    )?;
    write_seed_file(
        &destination.join("metadata/.jj/repo/store/git_target"),
        b"../../../.git",
        ".jj/repo/store/git_target",
        &mut metadata,
    )?;
    collect_copy(
        &private_repository.join(".git/objects"),
        Path::new(""),
        &destination.join("objects"),
        &mut objects,
        false,
    )?;
    let manifest = MetadataTemplate {
        format_version: 1,
        jj_version: "0.40.0".into(),
        baseline,
        operation,
        metadata,
        objects,
    };
    manifest.validate()?;
    atomic::write_json(&destination.join("manifest.json"), &manifest)?;
    File::open(destination)?.sync_all()?;
    if let Some(parent) = destination.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(manifest)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivateMetadataFork {
    pub format_version: u32,
    pub seed_manifest: ObjectRef,
    pub binding: AttemptBinding,
    pub metadata_files: u64,
    pub metadata_bytes: u64,
    pub kernel_readonly_objects: bool,
}

/// Destination must be a fresh private metadata root, independently mounted at
/// the attempt's .git/.jj paths. No source view or engine repository is scanned.
/// Presence alone is not readiness: launch only after the returned receipt and
/// atomically synced `rudder-metadata-ready.json` exist and validate.
pub fn fork_private_metadata(
    template: &VerifiedMetadataTemplate,
    destination: &Path,
    binding: AttemptBinding,
    activation: Option<&crate::LiveObservation>,
) -> Result<PrivateMetadataFork> {
    binding.validate()?;
    if let Some(retained) = &template.retained_seed {
        ensure!(
            &binding.seed == retained,
            "attempt seed differs from retained immutable artifact"
        );
    } else {
        ensure!(
            cfg!(feature = "local-development") && !template.require_readonly,
            "metadata fork requires a retained VCS seed root"
        );
    }
    ensure!(
        binding.baseline == template.manifest.baseline
            && binding.baseline_operation == template.manifest.operation,
        "attempt baseline differs from metadata template"
    );
    if template.require_readonly {
        require_readonly_mount(&template.object_mount)?;
    }
    create_private_directory(destination)?;
    let mut files = 0;
    let mut bytes = 0;
    for (path, descriptor) in &template.manifest.metadata {
        let rel = relative_path(path)?;
        // Verify while copying, never hardlink mutable VCS state to the seed.
        copy_verified(
            &template.root.join("metadata").join(&rel),
            &destination.join(rel),
            descriptor,
        )?;
        files += 1;
        bytes += descriptor.size_bytes;
    }
    let object_info = destination.join(".git/objects/info");
    create_parents_private(&object_info)?;
    let mount = template
        .object_mount
        .to_str()
        .context("object mount path is not UTF-8")?;
    ensure!(
        !mount.contains(['\n', '\r']),
        "invalid Git alternates pathname"
    );
    write_new(
        &object_info.join("alternates"),
        format!("{mount}\n").as_bytes(),
    )?;
    File::open(&object_info)?.sync_all()?;
    #[cfg(not(feature = "jj"))]
    {
        let _ = activation;
        ensure!(
            fs::read_to_string(destination.join(".jj/working_copy/type"))? != "rudder-vfs-v1",
            "production metadata rebinding requires the jj feature"
        );
    }
    #[cfg(feature = "jj")]
    if fs::read_to_string(destination.join(".jj/working_copy/type"))?
        == crate::working_copy::TYPE_NAME
    {
        let observation = activation
            .context("production metadata requires a fresh engine-issued view observation")?;
        crate::working_copy::rebind_private_fork(
            &destination.join(".jj/working_copy"),
            &binding,
            observation,
        )?;
    }
    let receipt = PrivateMetadataFork {
        format_version: 1,
        seed_manifest: template.identity.clone(),
        binding,
        metadata_files: files,
        metadata_bytes: bytes,
        kernel_readonly_objects: template.require_readonly,
    };
    atomic::write_json(&destination.join("rudder-metadata-ready.json"), &receipt)?;
    File::open(destination)?.sync_all()?;
    if let Some(parent) = destination.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(receipt)
}

fn collect_copy(
    source: &Path,
    relative: &Path,
    destination: &Path,
    manifest: &mut BTreeMap<String, SeedFile>,
    sanitize_jj: bool,
) -> Result<()> {
    let full = source.join(relative);
    let metadata = fs::symlink_metadata(&full)?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "symlink in private VCS metadata"
    );
    let name = relative
        .to_str()
        .context("VCS metadata path is not UTF-8")?
        .replace('\\', "/");
    if sanitize_jj
        && (name.ends_with("/config.toml")
            || name.ends_with("/rudder-prototype.log")
            || name.ends_with("/rudder-state.lock"))
    {
        return Ok(());
    }
    if metadata.is_dir() {
        create_parents_private(&destination.join(relative))?;
        for entry in fs::read_dir(&full)? {
            let entry = entry?;
            collect_copy(
                source,
                &relative.join(entry.file_name()),
                destination,
                manifest,
                sanitize_jj,
            )?;
        }
        File::open(destination.join(relative))?.sync_all()?;
    } else {
        ensure!(metadata.is_file(), "unsupported VCS metadata node");
        ensure!(
            manifest.len() < MAX_TEMPLATE_FILES,
            "metadata seed exceeds file-count bound"
        );
        let target = destination.join(relative);
        if target.try_exists()? {
            fs::remove_file(&target)?;
        }
        manifest.insert(name, copy_digest(&full, &target)?);
    }
    Ok(())
}

fn write_seed_file(
    path: &Path,
    bytes: &[u8],
    name: &str,
    manifest: &mut BTreeMap<String, SeedFile>,
) -> Result<()> {
    if path.try_exists()? {
        fs::remove_file(path)?;
    }
    write_new(path, bytes)?;
    manifest.insert(
        name.into(),
        SeedFile {
            sha256: hex::encode(Sha256::digest(bytes)),
            size_bytes: bytes.len() as u64,
        },
    );
    Ok(())
}
fn collect_descriptors(
    root: &Path,
    relative: &Path,
    result: &mut BTreeMap<String, SeedFile>,
) -> Result<()> {
    let path = root.join(relative);
    let metadata = fs::symlink_metadata(&path)?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "symlink in immutable Git object seed"
    );
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            collect_descriptors(root, &relative.join(entry?.file_name()), result)?;
        }
    } else {
        ensure!(
            metadata.is_file() && result.len() < MAX_TEMPLATE_FILES,
            "invalid/oversized immutable Git object seed"
        );
        let mut file = atomic::open_read(&path)?;
        let mut hash = Sha256::new();
        let mut count = 0;
        let mut buffer = [0; 64 * 1024];
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
            count += n as u64;
        }
        ensure!(
            count == metadata.len(),
            "immutable seed changed while reading"
        );
        let name = relative
            .to_str()
            .context("Git object filename is not UTF-8")?
            .replace('\\', "/");
        result.insert(
            name,
            SeedFile {
                sha256: hex::encode(hash.finalize()),
                size_bytes: count,
            },
        );
    }
    Ok(())
}
fn copy_verified(source: &Path, target: &Path, expected: &SeedFile) -> Result<()> {
    let mut input = atomic::open_read(source)?;
    ensure!(
        input.metadata()?.len() == expected.size_bytes,
        "metadata size mismatch"
    );
    create_parents_private(target.parent().context("missing metadata parent")?)?;
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut output = options.open(target)?;
    let mut hash = Sha256::new();
    let mut copied = 0;
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        output.write_all(&buffer[..count])?;
        hash.update(&buffer[..count]);
        copied += count as u64;
    }
    ensure!(
        copied == expected.size_bytes && hex::encode(hash.finalize()) == expected.sha256,
        "metadata changed or failed checksum"
    );
    output.sync_all()?;
    File::open(target.parent().unwrap())?.sync_all()?;
    Ok(())
}
fn copy_digest(source: &Path, target: &Path) -> Result<SeedFile> {
    let mut input = atomic::open_read(source)?;
    let expected_size = input.metadata()?.len();
    create_parents_private(target.parent().context("missing metadata parent")?)?;
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut output = options.open(target)?;
    let mut hash = Sha256::new();
    let mut copied = 0;
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        output.write_all(&buffer[..count])?;
        hash.update(&buffer[..count]);
        copied += count as u64;
    }
    ensure!(
        copied == expected_size,
        "prepared repository changed during copy"
    );
    output.sync_all()?;
    File::open(target.parent().unwrap())?.sync_all()?;
    Ok(SeedFile {
        sha256: hex::encode(hash.finalize()),
        size_bytes: copied,
    })
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    create_parents_private(path.parent().context("missing metadata parent")?)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}
fn relative_path(path: &str) -> Result<PathBuf> {
    ensure!(
        !path.contains(['\\', '\0']),
        "metadata paths require portable slash-separated names"
    );
    let path = Path::new(path);
    ensure!(
        !path.as_os_str().is_empty()
            && path.components().all(|c| matches!(c, Component::Normal(_))),
        "invalid relative metadata path"
    );
    Ok(path.to_path_buf())
}
fn ensure_directory(path: &Path) -> Result<()> {
    ensure!(
        fs::symlink_metadata(path)?.is_dir(),
        "expected a real directory"
    );
    Ok(())
}
fn create_private_directory(path: &Path) -> Result<()> {
    fs::create_dir(path)?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
fn create_parents_private(path: &Path) -> Result<()> {
    if path.try_exists()? {
        return ensure_directory(path);
    }
    create_parents_private(path.parent().context("missing metadata parent")?)?;
    create_private_directory(path)
}
#[cfg(unix)]
fn require_readonly_mount(path: &Path) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let path = CString::new(path.as_os_str().as_bytes())?;
    let mut state = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    ensure!(
        unsafe { libc::statvfs(path.as_ptr(), state.as_mut_ptr()) } == 0,
        "cannot inspect object mount: {}",
        std::io::Error::last_os_error()
    );
    let state = unsafe { state.assume_init() };
    ensure!(
        state.f_flag & libc::ST_RDONLY as libc::c_ulong != 0,
        "shared Git objects require an engine-owned read-only mount"
    );
    Ok(())
}
#[cfg(not(unix))]
fn require_readonly_mount(_: &Path) -> Result<()> {
    anyhow::bail!("read-only seed mount qualification requires Unix");
}
