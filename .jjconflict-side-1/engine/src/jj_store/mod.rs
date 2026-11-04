//! jj-lib storage backend on Rudder's content store.
//!
//! Every jj object (file, symlink, tree, commit) is one immutable blob in a
//! [`LocalStore`]. A jj object id is the raw SHA-256 of that blob, so it is
//! exactly the Rudder `ObjectRef` digest. File ids therefore equal the blob
//! digest Rudder already uses for the same bytes, and file contents are shared
//! with Rudder snapshots for free. Trees and commits are canonical JSON blobs.
//!
//! Public API (for the VFS working copy and launch wiring):
//!
//! - [`BACKEND_NAME`] (`"rudder"`) is written to `.jj/repo/store/type`.
//! - [`init_workspace`] creates a jj workspace + repo whose objects live in the
//!   store at `store_root`. It uses jj's default local working copy; a VFS
//!   working copy can instead call `Workspace::init_with_factories` with
//!   [`initializer`] as the backend initializer.
//! - [`initializer`] returns a jj `BackendInitializer` for `ReadonlyRepo::init`
//!   or `Workspace::init_with_factories`.
//! - [`store_factories`] (or [`register`] on existing factories) lets
//!   `Workspace::load` / `RepoLoader::init_from_file_system` open such repos.
//!   The store location is recorded in `.jj/repo/store/rudder_store`.
//! - [`RudderBackend`] is the `jj_lib::backend::Backend` implementation.
//!
//! Limits: copy tracking is unsupported (like jj's simple backend). Objects
//! are not pinned, so `LocalStore::gc` would sweep them; do not run store GC on
//! a store holding a jj repo until retention for jj heads is added. Git interop
//! stays at the edges: export through jj's git backend, not here.

use crate::protocol::{ObjectKind, ObjectRef};
use crate::store::LocalStore;
use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt as _};
use jj_lib::backend::{
    make_root_commit, Backend, BackendError, BackendInitError, BackendLoadError, BackendResult,
    ChangeId, Commit, CommitId, CopyHistory, CopyId, CopyRecord, FileId, MillisSinceEpoch,
    RelatedCopy, SecureSig, Signature, SigningFn, SymlinkId, Timestamp, Tree, TreeId, TreeValue,
};
use jj_lib::conflict_labels::ConflictLabels;
use jj_lib::index::Index;
use jj_lib::merge::MergeBuilder;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::{ReadonlyRepo, StoreFactories};
use jj_lib::repo_path::{RepoPath, RepoPathBuf, RepoPathComponentBuf};
use jj_lib::settings::UserSettings;
use jj_lib::signing::Signer;
use jj_lib::workspace::Workspace;
use pollster::FutureExt as _;
use serde::{Deserialize, Serialize};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::io::{AsyncRead, AsyncReadExt as _};

#[cfg(test)]
mod tests;

/// Name written to `.jj/repo/store/type`.
pub const BACKEND_NAME: &str = "rudder";
/// File in jj's store directory holding the absolute `LocalStore` root.
pub const STORE_POINTER_FILE: &str = "rudder_store";

const COMMIT_ID_LENGTH: usize = 32;
const CHANGE_ID_LENGTH: usize = 16;

/// jj `Backend` over a Rudder [`LocalStore`].
#[derive(Debug)]
pub struct RudderBackend {
    store: LocalStore,
    root_commit_id: CommitId,
    root_change_id: ChangeId,
    empty_tree_id: TreeId,
}

impl RudderBackend {
    /// Opens a backend on `store`, writing the empty tree if it is missing.
    pub fn new(store: LocalStore) -> anyhow::Result<Self> {
        let empty = tree_bytes(&Tree::default())?;
        let empty_ref = store.put_blob(&empty)?;
        Ok(Self {
            store,
            root_commit_id: CommitId::from_bytes(&[0; COMMIT_ID_LENGTH]),
            root_change_id: ChangeId::from_bytes(&[0; CHANGE_ID_LENGTH]),
            empty_tree_id: TreeId::new(hex::decode(&empty_ref.digest)?),
        })
    }

    /// The underlying Rudder content store.
    pub fn store(&self) -> &LocalStore {
        &self.store
    }

    /// Rudder object reference for a jj object id (any of file, tree, commit).
    pub fn object_ref(&self, id_hex: &str) -> BackendResult<Option<ObjectRef>> {
        let mut reference = ObjectRef::from_bytes(ObjectKind::Blob, &[]);
        reference.digest = id_hex.to_owned();
        let path = self.store.object_path(&reference).map_err(to_other)?;
        match std::fs::metadata(&path) {
            Ok(meta) => {
                reference.size_bytes = meta.len();
                Ok(Some(reference))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(to_other(err)),
        }
    }

    fn read(&self, id: &impl jj_lib::object_id::ObjectId) -> BackendResult<Vec<u8>> {
        let not_found = || BackendError::ObjectNotFound {
            object_type: id.object_type(),
            hash: id.hex(),
            source: "not in Rudder store".into(),
        };
        if id.as_bytes().len() != 32 {
            return Err(not_found());
        }
        let reference = self.object_ref(&id.hex())?.ok_or_else(not_found)?;
        self.store.read_object(&reference).map_err(|err| BackendError::ReadObject {
            object_type: id.object_type(),
            hash: id.hex(),
            source: err.into(),
        })
    }

    fn write(&self, bytes: &[u8]) -> BackendResult<Vec<u8>> {
        let reference = self.store.put_blob(bytes).map_err(to_other)?;
        hex::decode(&reference.digest).map_err(to_other)
    }
}

/// Backend initializer for `ReadonlyRepo::init` / `Workspace::init_*`. Records
/// `store_root` in jj's store directory so the repo can be reopened.
pub fn initializer(
    store_root: PathBuf,
) -> impl Fn(&UserSettings, &Path) -> Result<Box<dyn Backend>, BackendInitError> {
    move |_settings, jj_store_path| {
        let init = || -> anyhow::Result<Box<dyn Backend>> {
            let store = LocalStore::open(&store_root)?;
            std::fs::write(
                jj_store_path.join(STORE_POINTER_FILE),
                store.root().as_os_str().as_encoded_bytes(),
            )?;
            Ok(Box::new(RudderBackend::new(store)?))
        };
        init().map_err(|err| BackendInitError(err.into()))
    }
}

/// Adds the Rudder backend loader to `factories`.
pub fn register(factories: &mut StoreFactories) {
    factories.add_backend(
        BACKEND_NAME,
        Box::new(|_settings, jj_store_path| {
            let load = || -> anyhow::Result<Box<dyn Backend>> {
                let root = std::fs::read_to_string(jj_store_path.join(STORE_POINTER_FILE))?;
                Ok(Box::new(RudderBackend::new(LocalStore::open(root.trim_end())?)?))
            };
            load().map_err(|err| BackendLoadError(err.into()))
        }),
    );
}

/// jj's default store factories plus the Rudder backend.
pub fn store_factories() -> StoreFactories {
    let mut factories = StoreFactories::default();
    register(&mut factories);
    factories
}

/// Initializes a jj workspace at `workspace_root` (which must exist) with its
/// repo objects in the Rudder store at `store_root`.
pub fn init_workspace(
    settings: &UserSettings,
    workspace_root: &Path,
    store_root: &Path,
) -> anyhow::Result<(Workspace, Arc<ReadonlyRepo>)> {
    let init = initializer(store_root.to_path_buf());
    let signer = Signer::from_settings(settings)?;
    Ok(Workspace::init_with_backend(settings, workspace_root, &init, signer).block_on()?)
}

fn to_other(err: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> BackendError {
    BackendError::Other(err.into())
}

#[async_trait]
impl Backend for RudderBackend {
    fn name(&self) -> &str {
        BACKEND_NAME
    }
    fn commit_id_length(&self) -> usize {
        COMMIT_ID_LENGTH
    }
    fn change_id_length(&self) -> usize {
        CHANGE_ID_LENGTH
    }
    fn root_commit_id(&self) -> &CommitId {
        &self.root_commit_id
    }
    fn root_change_id(&self) -> &ChangeId {
        &self.root_change_id
    }
    fn empty_tree_id(&self) -> &TreeId {
        &self.empty_tree_id
    }
    fn concurrency(&self) -> usize {
        1
    }

    async fn read_file(
        &self,
        _path: &RepoPath,
        id: &FileId,
    ) -> BackendResult<Pin<Box<dyn AsyncRead + Send>>> {
        Ok(Box::pin(Cursor::new(self.read(id)?)))
    }

    async fn write_file(
        &self,
        _path: &RepoPath,
        contents: &mut (dyn AsyncRead + Send + Unpin),
    ) -> BackendResult<FileId> {
        let mut bytes = Vec::new();
        contents.read_to_end(&mut bytes).await.map_err(to_other)?;
        Ok(FileId::new(self.write(&bytes)?))
    }

    async fn read_symlink(&self, _path: &RepoPath, id: &SymlinkId) -> BackendResult<String> {
        String::from_utf8(self.read(id)?).map_err(to_other)
    }

    async fn write_symlink(&self, _path: &RepoPath, target: &str) -> BackendResult<SymlinkId> {
        Ok(SymlinkId::new(self.write(target.as_bytes())?))
    }

    async fn read_copy(&self, _id: &CopyId) -> BackendResult<CopyHistory> {
        Err(BackendError::Unsupported("the Rudder backend doesn't track copies".into()))
    }

    async fn write_copy(&self, _copy: &CopyHistory) -> BackendResult<CopyId> {
        Err(BackendError::Unsupported("the Rudder backend doesn't track copies".into()))
    }

    async fn get_related_copies(&self, _copy_id: &CopyId) -> BackendResult<Vec<RelatedCopy>> {
        Err(BackendError::Unsupported("the Rudder backend doesn't track copies".into()))
    }

    async fn read_tree(&self, _path: &RepoPath, id: &TreeId) -> BackendResult<Tree> {
        let stored: StoredTree = serde_json::from_slice(&self.read(id)?).map_err(to_other)?;
        stored.into_tree()
    }

    async fn write_tree(&self, _path: &RepoPath, tree: &Tree) -> BackendResult<TreeId> {
        Ok(TreeId::new(self.write(&tree_bytes(tree).map_err(to_other)?)?))
    }

    async fn read_commit(&self, id: &CommitId) -> BackendResult<Commit> {
        if *id == self.root_commit_id {
            return Ok(make_root_commit(self.root_change_id.clone(), self.empty_tree_id.clone()));
        }
        let stored: StoredCommit = serde_json::from_slice(&self.read(id)?).map_err(to_other)?;
        stored.into_commit()
    }

    async fn write_commit(
        &self,
        mut commit: Commit,
        sign_with: Option<&mut SigningFn>,
    ) -> BackendResult<(CommitId, Commit)> {
        if commit.parents.is_empty() {
            return Err(to_other("cannot write a commit with no parents"));
        }
        let mut stored = StoredCommit::from_commit(&commit);
        if let Some(sign) = sign_with {
            let data = serde_json::to_vec(&stored).map_err(to_other)?;
            let sig = sign(&data).map_err(to_other)?;
            stored.secure_sig = Some(hex::encode(&sig));
            commit.secure_sig = Some(SecureSig { data, sig });
        }
        let bytes = serde_json::to_vec(&stored).map_err(to_other)?;
        Ok((CommitId::new(self.write(&bytes)?), commit))
    }

    fn get_copy_records(
        &self,
        _paths: Option<&[RepoPathBuf]>,
        _root: &CommitId,
        _head: &CommitId,
    ) -> BackendResult<BoxStream<'_, BackendResult<CopyRecord>>> {
        Ok(stream::empty().boxed())
    }

    fn gc(&self, _index: &dyn Index, _keep_newer: SystemTime) -> BackendResult<()> {
        Ok(())
    }
}

// Stored formats. Field order is fixed, so serde_json output is canonical.

#[derive(Serialize, Deserialize)]
struct StoredTree {
    entries: Vec<(String, StoredValue)>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredValue {
    File { id: String, executable: bool, copy_id: String },
    Symlink(String),
    Tree(String),
}

fn tree_bytes(tree: &Tree) -> anyhow::Result<Vec<u8>> {
    let mut entries = Vec::new();
    for entry in tree.entries() {
        let value = match entry.value() {
            TreeValue::File { id, executable, copy_id } => StoredValue::File {
                id: id.hex(),
                executable: *executable,
                copy_id: copy_id.hex(),
            },
            TreeValue::Symlink(id) => StoredValue::Symlink(id.hex()),
            TreeValue::Tree(id) => StoredValue::Tree(id.hex()),
            TreeValue::GitSubmodule(_) => anyhow::bail!("the Rudder backend can't store submodules"),
        };
        entries.push((entry.name().as_internal_str().to_owned(), value));
    }
    Ok(serde_json::to_vec(&StoredTree { entries })?)
}

fn decode_hex(hex: &str) -> BackendResult<Vec<u8>> {
    hex::decode(hex).map_err(to_other)
}

impl StoredTree {
    fn into_tree(self) -> BackendResult<Tree> {
        let mut entries = Vec::with_capacity(self.entries.len());
        for (name, value) in self.entries {
            let value = match value {
                StoredValue::File { id, executable, copy_id } => TreeValue::File {
                    id: FileId::new(decode_hex(&id)?),
                    executable,
                    copy_id: CopyId::new(decode_hex(&copy_id)?),
                },
                StoredValue::Symlink(id) => TreeValue::Symlink(SymlinkId::new(decode_hex(&id)?)),
                StoredValue::Tree(id) => TreeValue::Tree(TreeId::new(decode_hex(&id)?)),
            };
            entries.push((RepoPathComponentBuf::new(name).map_err(to_other)?, value));
        }
        Ok(Tree::from_sorted_entries(entries))
    }
}

#[derive(Serialize, Deserialize)]
struct StoredSignature {
    name: String,
    email: String,
    millis: i64,
    tz_offset: i32,
}

impl StoredSignature {
    fn from(sig: &Signature) -> Self {
        Self {
            name: sig.name.clone(),
            email: sig.email.clone(),
            millis: sig.timestamp.timestamp.0,
            tz_offset: sig.timestamp.tz_offset,
        }
    }
    fn into_signature(self) -> Signature {
        Signature {
            name: self.name,
            email: self.email,
            timestamp: Timestamp {
                timestamp: MillisSinceEpoch(self.millis),
                tz_offset: self.tz_offset,
            },
        }
    }
}

#[derive(Serialize, Deserialize)]
struct StoredCommit {
    parents: Vec<String>,
    predecessors: Vec<String>,
    root_tree: Vec<String>,
    conflict_labels: Vec<String>,
    change_id: String,
    description: String,
    author: StoredSignature,
    committer: StoredSignature,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    secure_sig: Option<String>,
}

impl StoredCommit {
    fn from_commit(commit: &Commit) -> Self {
        Self {
            parents: commit.parents.iter().map(|id| id.hex()).collect(),
            predecessors: commit.predecessors.iter().map(|id| id.hex()).collect(),
            root_tree: commit.root_tree.iter().map(|id| id.hex()).collect(),
            conflict_labels: if commit.conflict_labels.is_resolved() {
                Vec::new()
            } else {
                commit.conflict_labels.iter().cloned().collect()
            },
            change_id: commit.change_id.hex(),
            description: commit.description.clone(),
            author: StoredSignature::from(&commit.author),
            committer: StoredSignature::from(&commit.committer),
            secure_sig: None,
        }
    }

    fn into_commit(mut self) -> BackendResult<Commit> {
        let secure_sig = match self.secure_sig.take() {
            Some(sig) => Some(SecureSig {
                data: serde_json::to_vec(&self).map_err(to_other)?,
                sig: decode_hex(&sig)?,
            }),
            None => None,
        };
        let ids = |hexes: Vec<String>| -> BackendResult<Vec<CommitId>> {
            hexes.iter().map(|h| Ok(CommitId::new(decode_hex(h)?))).collect()
        };
        let root_tree: MergeBuilder<TreeId> = self
            .root_tree
            .iter()
            .map(|h| Ok(TreeId::new(decode_hex(h)?)))
            .collect::<BackendResult<Vec<_>>>()?
            .into_iter()
            .collect();
        Ok(Commit {
            parents: ids(self.parents)?,
            predecessors: ids(self.predecessors)?,
            root_tree: root_tree.build(),
            conflict_labels: ConflictLabels::from_vec(self.conflict_labels).into_merge(),
            change_id: ChangeId::new(decode_hex(&self.change_id)?),
            description: self.description,
            author: self.author.into_signature(),
            committer: self.committer.into_signature(),
            secure_sig,
        })
    }
}
