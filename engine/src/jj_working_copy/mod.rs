//! jj working copy backed by a copy-on-write overlay fork.
//!
//! The directory an agent works in is an OverlayFS mount (mounted by the
//! workspace namespace helper, see `workspace/linux.rs`):
//!
//! - lower: a read-only directory holding one jj tree, materialized once into a
//!   shared cache (`lowers/<tree-key>`) and reused by every fork of that tree.
//!   A stacked warm snapshot's top layer lists its whole stack in
//!   `<dir>.lowerdir` (see [`lower_layers`]); the mount helper passes that list.
//! - upper: this working copy's private changes (`<state>/upper`);
//! - work: OverlayFS scratch (`<state>/work`).
//!
//! Creating a fork of a tree whose lower already exists copies no file bytes:
//! it is two empty directories plus a small state file.
//!
//! Snapshot reads only the upper layer: regular files and symlinks there are
//! changed or added paths, character devices 0/0 are whiteouts (deletions) and
//! directories with `user.overlay.opaque=y` replace the lower directory. It
//! never scans the full tree. Check-out points the lower layer at the new
//! tree's cached directory and resets the upper layer; no per-fork file copies.
//!
//! Limits (callers fall back to jj's `LocalWorkingCopy` there):
//! - Linux only (rootless OverlayFS with `userxattr`). macOS uses jj's local
//!   working copy.
//! - Sparse patterns are not supported (always the whole tree).
//! - Conflicted paths are left out of the lower directory; they stay conflicted
//!   in snapshots until the agent writes the path. Conflict markers are not
//!   materialized or parsed.
//! - `check_out` swaps the lower directory, so the overlay must be remounted
//!   by the caller afterwards (a live mount keeps its old lower).
//! - The `.jj` and `.git` entries at the root of the upper layer are ignored.

use std::fs;
use std::fs::File;
use std::io::Write as _;
use std::os::unix::fs::FileTypeExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use async_trait::async_trait;
use futures::StreamExt as _;
use jj_lib::backend::CopyId;
use jj_lib::backend::TreeId;
use jj_lib::backend::TreeValue;
use jj_lib::commit::Commit;
use jj_lib::conflict_labels::ConflictLabels;
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::matchers::EverythingMatcher;
use jj_lib::matchers::PrefixMatcher;
use jj_lib::merge::Merge;
use jj_lib::merge::MergedTreeValue;
use jj_lib::merged_tree::MergedTree;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::OperationId;
use jj_lib::ref_name::WorkspaceName;
use jj_lib::ref_name::WorkspaceNameBuf;
use jj_lib::repo_path::RepoPath;
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::settings::UserSettings;
use jj_lib::store::Store;
use jj_lib::working_copy::CheckoutError;
use jj_lib::working_copy::CheckoutStats;
use jj_lib::working_copy::LockedWorkingCopy;
use jj_lib::working_copy::ResetError;
use jj_lib::working_copy::SnapshotError;
use jj_lib::working_copy::SnapshotOptions;
use jj_lib::working_copy::SnapshotStats;
use jj_lib::working_copy::UntrackedReason;
use jj_lib::working_copy::WorkingCopy;
use jj_lib::working_copy::WorkingCopyFactory;
use jj_lib::working_copy::WorkingCopyStateError;
use serde::Deserialize;
use serde::Serialize;

#[cfg(test)]
mod tests;

const STATE_FILE: &str = "overlay_state.json";
const LOCK_FILE: &str = "overlay.lock";

/// Paths the namespace helper mounts:
/// `mount -t overlay overlay -o <options()> <merged>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlayMount {
    pub lower: PathBuf,
    pub upper: PathBuf,
    pub work: PathBuf,
    pub merged: PathBuf,
}

impl OverlayMount {
    pub fn options(&self) -> String {
        let lowers: Vec<String> = lower_layers(&self.lower).iter().map(|l| l.display().to_string()).collect();
        format!(
            "lowerdir={},upperdir={},workdir={},userxattr",
            lowers.join(":"),
            self.upper.display(),
            self.work.display()
        )
    }
}

/// The read-only layers under a working copy, top first: the lower directory
/// itself, or, when it is the top layer of a stacked warm snapshot, the layers
/// listed in `<real path>.lowerdir` (colon-separated, as OverlayFS takes them).
pub fn lower_layers(lower: &Path) -> Vec<PathBuf> {
    use std::os::unix::ffi::OsStrExt as _;
    let real = fs::canonicalize(lower).unwrap_or_else(|_| lower.to_owned());
    let mut list = real.clone().into_os_string();
    list.push(".lowerdir");
    let layers: Vec<PathBuf> = fs::read(&list)
        .map(|bytes| {
            bytes
                .split(|b| *b == b':' || *b == b'\n')
                .filter(|part| !part.is_empty())
                .map(|part| PathBuf::from(std::ffi::OsStr::from_bytes(part)))
                .collect()
        })
        .unwrap_or_default();
    if layers.is_empty() { vec![real] } else { layers }
}

/// Where OverlayFS finds directory `name` below the given layers: each layer's
/// `name` directory, top first, until an opaque directory or a non-directory
/// entry (a file, symlink or whiteout) hides the layers beneath.
fn child_layers(layers: &[PathBuf], name: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for layer in layers {
        let path = layer.join(name);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => {
                let opaque = is_opaque(&path);
                out.push(path);
                if opaque {
                    break;
                }
            }
            Ok(_) => break,
            Err(_) => {}
        }
    }
    out
}

/// The `.gitignore` the overlay shows in a directory: the topmost entry in the
/// upper directory or the layers. A whiteout is returned as well: it is not a
/// file, so it adds no patterns and hides the ones below.
fn ignore_file(upper: &Path, layers: &[PathBuf]) -> PathBuf {
    std::iter::once(upper)
        .chain(layers.iter().map(PathBuf::as_path))
        .map(|dir| dir.join(".gitignore"))
        .find(|path| fs::symlink_metadata(path).is_ok())
        .unwrap_or_else(|| upper.join(".gitignore"))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct State {
    operation_id: String,
    workspace_name: String,
    working_copy_path: PathBuf,
    lowers_root: PathBuf,
    tree_ids: Vec<String>,
    conflict_labels: Vec<String>,
    /// The tree the lower directory holds. Differs from `tree_ids` only after
    /// `reset`, which records a new tree without touching files.
    lower_tree_ids: Vec<String>,
    lower_conflict_labels: Vec<String>,
}

fn tree_to_ids(tree: &MergedTree) -> (Vec<String>, Vec<String>) {
    (
        tree.tree_ids().iter().map(|id| id.hex()).collect(),
        tree.labels().as_slice().to_vec(),
    )
}

fn tree_from_ids(store: &Arc<Store>, ids: &[String], labels: &[String]) -> anyhow::Result<MergedTree> {
    let ids = ids
        .iter()
        .map(|hex_id| Ok(TreeId::new(hex::decode(hex_id)?)))
        .collect::<anyhow::Result<Vec<_>>>()?;
    anyhow::ensure!(ids.len() % 2 == 1, "invalid tree id count");
    Ok(MergedTree::new(
        store.clone(),
        Merge::from_vec(ids),
        ConflictLabels::from_vec(labels.to_vec()),
    ))
}

fn tree_key(tree: &MergedTree) -> String {
    tree.tree_ids()
        .iter()
        .map(|id| id.hex())
        .collect::<Vec<_>>()
        .join("-")
}

fn state_err(message: &str, err: impl Into<anyhow::Error>) -> WorkingCopyStateError {
    let err: anyhow::Error = err.into();
    WorkingCopyStateError {
        message: message.to_owned(),
        err: err.into(),
    }
}

fn write_state(state_path: &Path, state: &State) -> anyhow::Result<()> {
    let tmp = state_path.join(format!("{STATE_FILE}.tmp"));
    let mut file = File::create(&tmp)?;
    file.write_all(&serde_json::to_vec_pretty(state)?)?;
    file.sync_all()?;
    fs::rename(&tmp, state_path.join(STATE_FILE))?;
    // Plain-text lower path for the namespace helper script (no JSON parsing).
    let lower_tmp = state_path.join("lower.tmp");
    fs::write(&lower_tmp, mount_from_state(state_path, state).lower.as_os_str().as_encoded_bytes())?;
    fs::rename(&lower_tmp, state_path.join("lower"))?;
    Ok(())
}

fn read_state(state_path: &Path) -> anyhow::Result<State> {
    let path = state_path.join(STATE_FILE);
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn mount_from_state(state_path: &Path, state: &State) -> OverlayMount {
    OverlayMount {
        lower: state.lowers_root.join(state.lower_tree_ids.join("-")),
        upper: state_path.join("upper"),
        work: state_path.join("work"),
        merged: state.working_copy_path.clone(),
    }
}

/// Mount paths for a working copy without loading the jj store (launch path).
pub fn read_mount(state_path: &Path) -> anyhow::Result<OverlayMount> {
    Ok(mount_from_state(state_path, &read_state(state_path)?))
}

/// Points `lowers_root/<tree-key>` at an existing directory holding `tree`
/// (for example a warm snapshot with dependencies installed), so check-out of
/// that tree reuses it instead of materializing a copy. Keeps an existing entry.
pub fn link_lower(lowers_root: &Path, tree: &MergedTree, dir: &Path) -> anyhow::Result<PathBuf> {
    fs::create_dir_all(lowers_root)?;
    let link = lowers_root.join(tree_key(tree));
    if !link.is_dir() {
        let _ = fs::remove_file(&link);
        match std::os::unix::fs::symlink(dir, &link) {
            Ok(()) => {}
            Err(_) if link.is_dir() => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(link)
}

/// Writes `tree` into `lowers_root/<tree-key>` once; later forks reuse it.
/// Returns the directory and whether it had to be written.
pub async fn ensure_lower(lowers_root: &Path, tree: &MergedTree) -> anyhow::Result<(PathBuf, bool)> {
    let dir = lowers_root.join(tree_key(tree));
    if dir.is_dir() {
        // Lowers written before the mount point existed get it now.
        let _ = fs::create_dir(dir.join(".jj"));
        return Ok((dir, false));
    }
    fs::create_dir_all(lowers_root)?;
    let tmp = lowers_root.join(format!(".tmp-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&tmp)?;
    let store = tree.store().clone();
    let result = async {
        for (path, value) in tree.entries() {
            let value = value?;
            // Conflicted paths are left out; see the module docs.
            let Some(Some(value)) = value.as_resolved() else {
                continue;
            };
            let disk = path.to_fs_path(&tmp)?;
            if let Some(parent) = disk.parent() {
                fs::create_dir_all(parent)?;
            }
            match value {
                TreeValue::File { id, executable, .. } => {
                    let mut reader = store.read_file(&path, id).await?;
                    let mut bytes = Vec::new();
                    tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut bytes).await?;
                    fs::write(&disk, &bytes)?;
                    if *executable {
                        fs::set_permissions(&disk, fs::Permissions::from_mode(0o755))?;
                    }
                }
                TreeValue::Symlink(id) => {
                    let target = store.read_symlink(&path, id).await?;
                    std::os::unix::fs::symlink(target, &disk)?;
                }
                TreeValue::Tree(_) | TreeValue::GitSubmodule(_) => {}
            }
        }
        anyhow::Ok(())
    }
    .await;
    // The fork's enter script binds the workspace's real `.jj` over this
    // path; without it the mount fails and the agent can never relaunch.
    let result = result.and_then(|()| Ok(fs::create_dir(tmp.join(".jj"))?));
    if let Err(err) = result {
        let _ = fs::remove_dir_all(&tmp);
        return Err(err);
    }
    match fs::rename(&tmp, &dir) {
        Ok(()) => Ok((dir, true)),
        // Another fork materialized the same tree concurrently.
        Err(_) if dir.is_dir() => {
            let _ = fs::remove_dir_all(&tmp);
            Ok((dir, false))
        }
        Err(err) => Err(err.into()),
    }
}

fn reset_dir(path: &Path) -> anyhow::Result<()> {
    if path.exists() {
        crate::workspace::remove_managed_tree(path)
            .with_context(|| format!("remove {}", path.display()))?;
    }
    fs::create_dir_all(path)?;
    Ok(())
}

/// A jj working copy whose files live in an overlay fork.
pub struct OverlayWorkingCopy {
    store: Arc<Store>,
    state_path: PathBuf,
    state: State,
    workspace_name: WorkspaceNameBuf,
    operation_id: OperationId,
    tree: MergedTree,
    lower_tree: MergedTree,
}

impl OverlayWorkingCopy {
    pub fn name() -> &'static str {
        "rudder-overlay"
    }

    fn from_state(store: Arc<Store>, state_path: PathBuf, state: State) -> anyhow::Result<Self> {
        let tree = tree_from_ids(&store, &state.tree_ids, &state.conflict_labels)?;
        let lower_tree = tree_from_ids(&store, &state.lower_tree_ids, &state.lower_conflict_labels)?;
        Ok(Self {
            workspace_name: WorkspaceNameBuf::from(state.workspace_name.clone()),
            operation_id: OperationId::new(hex::decode(&state.operation_id)?),
            store,
            state_path,
            state,
            tree,
            lower_tree,
        })
    }

    pub fn mount(&self) -> OverlayMount {
        mount_from_state(&self.state_path, &self.state)
    }

    pub fn state_path(&self) -> &Path {
        &self.state_path
    }
}

impl WorkingCopy for OverlayWorkingCopy {
    fn name(&self) -> &str {
        Self::name()
    }

    fn workspace_name(&self) -> &WorkspaceName {
        &self.workspace_name
    }

    fn operation_id(&self) -> &OperationId {
        &self.operation_id
    }

    fn tree(&self) -> Result<&MergedTree, WorkingCopyStateError> {
        Ok(&self.tree)
    }

    fn sparse_patterns(&self) -> Result<&[RepoPathBuf], WorkingCopyStateError> {
        Ok(ROOT_PATTERNS.as_slice())
    }

    fn start_mutation(&self) -> Result<Box<dyn LockedWorkingCopy>, WorkingCopyStateError> {
        let lock_path = self.state_path.join(LOCK_FILE);
        let lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|err| state_err("open working copy lock", err))?;
        fs2::FileExt::lock_exclusive(&lock).map_err(|err| state_err("lock working copy", err))?;
        // Reload under the lock so a concurrent finish() is observed.
        let state = read_state(&self.state_path).map_err(|err| state_err("read working copy state", err))?;
        let wc = Self::from_state(self.store.clone(), self.state_path.clone(), state)
            .map_err(|err| state_err("load working copy state", err))?;
        Ok(Box::new(LockedOverlayWorkingCopy {
            old_operation_id: wc.operation_id.clone(),
            old_tree: wc.tree.clone(),
            wc,
            _lock: lock,
        }))
    }
}

static ROOT_PATTERNS: std::sync::LazyLock<Vec<RepoPathBuf>> =
    std::sync::LazyLock::new(|| vec![RepoPathBuf::root()]);

/// Factory registered under [`OverlayWorkingCopy::name`]. `lowers_root` is the
/// shared tree cache; forks of one repository should share it.
pub struct OverlayWorkingCopyFactory {
    pub lowers_root: Option<PathBuf>,
}

impl WorkingCopyFactory for OverlayWorkingCopyFactory {
    fn init_working_copy(
        &self,
        store: Arc<Store>,
        working_copy_path: PathBuf,
        state_path: PathBuf,
        operation_id: OperationId,
        workspace_name: WorkspaceNameBuf,
        _settings: &UserSettings,
    ) -> Result<Box<dyn WorkingCopy>, WorkingCopyStateError> {
        let init = || -> anyhow::Result<OverlayWorkingCopy> {
            let lowers_root = self
                .lowers_root
                .clone()
                .unwrap_or_else(|| state_path.join("lowers"));
            let tree = store.empty_merged_tree();
            pollster::block_on(ensure_lower(&lowers_root, &tree))?;
            fs::create_dir_all(&state_path)?;
            reset_dir(&state_path.join("upper"))?;
            reset_dir(&state_path.join("work"))?;
            let (tree_ids, conflict_labels) = tree_to_ids(&tree);
            let state = State {
                operation_id: operation_id.hex(),
                workspace_name: workspace_name.as_str().to_owned(),
                working_copy_path,
                lowers_root,
                lower_tree_ids: tree_ids.clone(),
                lower_conflict_labels: conflict_labels.clone(),
                tree_ids,
                conflict_labels,
            };
            write_state(&state_path, &state)?;
            OverlayWorkingCopy::from_state(store, state_path.clone(), state)
        };
        Ok(Box::new(init().map_err(|err| state_err("init overlay working copy", err))?))
    }

    fn load_working_copy(
        &self,
        store: Arc<Store>,
        working_copy_path: PathBuf,
        state_path: PathBuf,
        _settings: &UserSettings,
    ) -> Result<Box<dyn WorkingCopy>, WorkingCopyStateError> {
        let mut state = read_state(&state_path).map_err(|err| state_err("read working copy state", err))?;
        state.working_copy_path = working_copy_path;
        let wc = OverlayWorkingCopy::from_state(store, state_path, state)
            .map_err(|err| state_err("load working copy state", err))?;
        Ok(Box::new(wc))
    }
}

/// jj's default working-copy factories plus the overlay one.
pub fn working_copy_factories(lowers_root: Option<PathBuf>) -> jj_lib::workspace::WorkingCopyFactories {
    let mut factories = jj_lib::workspace::default_working_copy_factories();
    factories.insert(
        OverlayWorkingCopy::name().to_owned(),
        Box::new(OverlayWorkingCopyFactory { lowers_root }),
    );
    factories
}

pub struct LockedOverlayWorkingCopy {
    wc: OverlayWorkingCopy,
    old_operation_id: OperationId,
    old_tree: MergedTree,
    _lock: File,
}

impl LockedOverlayWorkingCopy {
    pub fn mount(&self) -> OverlayMount {
        self.wc.mount()
    }
}

fn other_snapshot(err: anyhow::Error) -> SnapshotError {
    SnapshotError::Other {
        message: format!("overlay snapshot failed: {err:#}"),
        err: err.into(),
    }
}

fn other_checkout(err: anyhow::Error) -> CheckoutError {
    CheckoutError::Other {
        message: format!("overlay check-out failed: {err:#}"),
        err: err.into(),
    }
}

fn is_opaque(path: &Path) -> bool {
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    let mut buf = [0u8; 4];
    // SAFETY: valid NUL-terminated strings and a buffer of the given length.
    let n = unsafe {
        libc::lgetxattr(
            c_path.as_ptr(),
            c"user.overlay.opaque".as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    n == 1 && buf[0] == b'y'
}

struct Walk<'a> {
    base: &'a MergedTree,
    builder: MergedTreeBuilder,
    options: &'a SnapshotOptions<'a>,
    stats: SnapshotStats,
}

impl Walk<'_> {
    /// Removes every base entry at or under `path`.
    fn remove_subtree(&mut self, path: &RepoPath) -> anyhow::Result<()> {
        let matcher = PrefixMatcher::new([path]);
        let removed: Vec<RepoPathBuf> = self
            .base
            .entries_matching(&matcher)
            .map(|(entry, _)| entry)
            .collect();
        for entry in removed {
            self.builder.set_or_remove(entry, Merge::absent());
        }
        Ok(())
    }

    /// `lowers` are this directory in each read-only layer that shows through
    /// (used for `.gitignore` lookups only).
    async fn dir(
        &mut self,
        upper: &Path,
        dir: &RepoPath,
        lowers: &[PathBuf],
        ignores: Arc<GitIgnoreFile>,
    ) -> anyhow::Result<()> {
        for entry in fs::read_dir(upper)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                return Err(SnapshotErrorBox(SnapshotError::InvalidUtf8Path { path: entry.file_name() }).into());
            };
            if dir.is_root() && (name == ".jj" || name == ".git") {
                continue;
            }
            let path = RepoPathBuf::from_internal_string(format!("{}{}", dir.to_internal_dir_string(), name))?;
            let disk = entry.path();
            let meta = fs::symlink_metadata(&disk)?;
            let file_type = meta.file_type();
            if file_type.is_char_device() && meta.rdev() == 0 {
                self.remove_subtree(&path)?;
            } else if file_type.is_dir() {
                let opaque = is_opaque(&disk);
                let base_value = self.base.path_value(&path).await?;
                if opaque || (base_value.is_present() && !base_value.is_tree()) {
                    self.remove_subtree(&path)?;
                }
                let child_lowers = if opaque { Vec::new() } else { child_layers(lowers, name) };
                let gitignore = ignore_file(&disk, &child_lowers);
                let child_ignores = ignores.chain_with_file(&path.to_internal_dir_string(), gitignore)?;
                Box::pin(self.dir(&disk, &path, &child_lowers, child_ignores)).await?;
            } else if file_type.is_file() || file_type.is_symlink() {
                self.file(&disk, &path, &meta, &ignores).await?;
            }
        }
        Ok(())
    }

    async fn file(
        &mut self,
        disk: &Path,
        path: &RepoPath,
        meta: &fs::Metadata,
        ignores: &GitIgnoreFile,
    ) -> anyhow::Result<()> {
        if let Some(progress) = self.options.progress {
            progress(path);
        }
        let base_value: MergedTreeValue = self.base.path_value(path).await?;
        if base_value.is_tree() {
            self.remove_subtree(path)?;
        }
        let tracked = base_value.is_present() && !base_value.is_tree();
        if !tracked && !self.options.force_tracking_matcher.matches(path) {
            if ignores.matches(path.as_internal_file_string()) {
                return Ok(());
            }
            if !self.options.start_tracking_matcher.matches(path) {
                self.stats
                    .untracked_paths
                    .insert(path.to_owned(), UntrackedReason::FileNotAutoTracked);
                return Ok(());
            }
            if meta.is_file() && meta.len() > self.options.max_new_file_size {
                self.stats.untracked_paths.insert(
                    path.to_owned(),
                    UntrackedReason::FileTooLarge {
                        size: meta.len(),
                        max_size: self.options.max_new_file_size,
                    },
                );
                return Ok(());
            }
        }
        let store = self.base.store().clone();
        let value = if meta.file_type().is_symlink() {
            let target = fs::read_link(disk)?;
            let target = target
                .to_str()
                .ok_or_else(|| SnapshotErrorBox(SnapshotError::InvalidUtf8SymlinkTarget { path: disk.to_owned() }))?;
            TreeValue::Symlink(store.write_symlink(path, target).await?)
        } else {
            let bytes = fs::read(disk)?;
            let id = store.write_file(path, &mut bytes.as_slice()).await?;
            let copy_id = match base_value.as_resolved() {
                Some(Some(TreeValue::File { copy_id, .. })) => copy_id.clone(),
                _ => CopyId::placeholder(),
            };
            TreeValue::File {
                id,
                executable: meta.permissions().mode() & 0o111 != 0,
                copy_id,
            }
        };
        self.builder.set_or_remove(path.to_owned(), Merge::normal(value));
        Ok(())
    }
}

/// Carries a typed `SnapshotError` through anyhow so it is returned as-is.
#[derive(Debug)]
struct SnapshotErrorBox(SnapshotError);
impl std::fmt::Display for SnapshotErrorBox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for SnapshotErrorBox {}

impl LockedOverlayWorkingCopy {
    async fn snapshot_inner(&mut self, options: &SnapshotOptions<'_>) -> anyhow::Result<(MergedTree, SnapshotStats)> {
        let mount = self.wc.mount();
        let base = self.wc.lower_tree.clone();
        let mut walk = Walk {
            base: &base,
            builder: MergedTreeBuilder::new(base.clone()),
            options,
            stats: SnapshotStats::default(),
        };
        let lowers = lower_layers(&mount.lower);
        let ignores = options.base_ignores.chain_with_file("", ignore_file(&mount.upper, &lowers))?;
        walk.dir(&mount.upper, RepoPath::root(), &lowers, ignores).await?;
        let stats = walk.stats;
        let tree = walk.builder.write_tree().await?;
        Ok((tree, stats))
    }

    async fn check_out_inner(&mut self, new_tree: MergedTree) -> anyhow::Result<CheckoutStats> {
        let mut stats = CheckoutStats::default();
        // Same content (a rebase onto a commit whose files the fork already
        // holds): keep the warm lower and the upper as they are. Swapping the
        // lower would drop the snapshot's dependencies and build output.
        if tree_to_ids(&new_tree) == tree_to_ids(&self.wc.tree) {
            self.set_tree(new_tree);
            write_state(&self.wc.state_path, &self.wc.state)?;
            return Ok(stats);
        }
        let mut diff = self.wc.tree.diff_stream(&new_tree, &EverythingMatcher);
        while let Some(entry) = diff.next().await {
            let values = entry.values?;
            if values.before.is_absent() {
                stats.added_files += 1;
            } else if values.after.is_absent() {
                stats.removed_files += 1;
            } else {
                stats.updated_files += 1;
            }
        }
        drop(diff);
        ensure_lower(&self.wc.state.lowers_root, &new_tree).await?;
        reset_dir(&self.wc.state_path.join("upper"))?;
        reset_dir(&self.wc.state_path.join("work"))?;
        self.set_tree(new_tree.clone());
        self.set_lower_tree(new_tree);
        write_state(&self.wc.state_path, &self.wc.state)?;
        Ok(stats)
    }

    fn set_tree(&mut self, tree: MergedTree) {
        let (ids, labels) = tree_to_ids(&tree);
        self.wc.state.tree_ids = ids;
        self.wc.state.conflict_labels = labels;
        self.wc.tree = tree;
    }

    fn set_lower_tree(&mut self, tree: MergedTree) {
        let (ids, labels) = tree_to_ids(&tree);
        self.wc.state.lower_tree_ids = ids;
        self.wc.state.lower_conflict_labels = labels;
        self.wc.lower_tree = tree;
    }
}

#[async_trait]
impl LockedWorkingCopy for LockedOverlayWorkingCopy {
    fn old_operation_id(&self) -> &OperationId {
        &self.old_operation_id
    }

    fn old_tree(&self) -> &MergedTree {
        &self.old_tree
    }

    async fn snapshot(
        &mut self,
        options: &SnapshotOptions,
    ) -> Result<(MergedTree, SnapshotStats), SnapshotError> {
        match self.snapshot_inner(options).await {
            Ok((tree, stats)) => {
                self.set_tree(tree.clone());
                Ok((tree, stats))
            }
            Err(err) => match err.downcast::<SnapshotErrorBox>() {
                Ok(typed) => Err(typed.0),
                Err(err) => Err(other_snapshot(err)),
            },
        }
    }

    async fn check_out(&mut self, commit: &Commit) -> Result<CheckoutStats, CheckoutError> {
        self.check_out_inner(commit.tree()).await.map_err(other_checkout)
    }

    fn rename_workspace(&mut self, new_workspace_name: WorkspaceNameBuf) {
        self.wc.state.workspace_name = new_workspace_name.as_str().to_owned();
        self.wc.workspace_name = new_workspace_name;
    }

    /// Records `commit`'s tree without touching files. The next snapshot still
    /// reads lower + upper, so it reports what is on disk.
    async fn reset(&mut self, commit: &Commit) -> Result<(), ResetError> {
        self.set_tree(commit.tree());
        Ok(())
    }

    async fn recover(&mut self, commit: &Commit) -> Result<(), ResetError> {
        self.set_tree(commit.tree());
        Ok(())
    }

    fn sparse_patterns(&self) -> Result<&[RepoPathBuf], WorkingCopyStateError> {
        Ok(ROOT_PATTERNS.as_slice())
    }

    async fn set_sparse_patterns(
        &mut self,
        new_sparse_patterns: Vec<RepoPathBuf>,
    ) -> Result<CheckoutStats, CheckoutError> {
        if new_sparse_patterns == *ROOT_PATTERNS {
            return Ok(CheckoutStats::default());
        }
        Err(other_checkout(anyhow::anyhow!(
            "sparse patterns are not supported by the overlay working copy"
        )))
    }

    async fn finish(
        mut self: Box<Self>,
        operation_id: OperationId,
    ) -> Result<Box<dyn WorkingCopy>, WorkingCopyStateError> {
        self.wc.state.operation_id = operation_id.hex();
        self.wc.operation_id = operation_id;
        write_state(&self.wc.state_path, &self.wc.state)
            .map_err(|err| state_err("write working copy state", err))?;
        let this = *self;
        Ok(Box::new(this.wc))
    }
}
