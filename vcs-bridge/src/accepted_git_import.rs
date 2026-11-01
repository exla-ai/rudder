//! Bounded, off-App import of exact Git object bytes, never acceptance authority.
//! Caller holds the actual source/target owners and source-cleanliness lock.
//! Errors may leave immutable objects/jj import effects in the owned target;
//! neither an error nor this DATA result publishes a bookmark or Journal fact.

use crate::GitCommitId;
use anyhow::{Context, Result, ensure};
use gix::{ObjectId, objs::{Kind, Write as _}};
use jj_lib::{backend::CommitId, git_backend::GitBackend};
use rudder_engine::execution::finalization_budget::FinalizationBudget;
use std::{collections::{BTreeMap, BTreeSet, VecDeque}, fs, io::{self, Read}, path::Path};

#[derive(Clone, Copy, Debug)]
pub struct GitImportLimits {
    pub max_objects: u64,
    pub max_edges: u64,
    pub max_total_bytes: u64,
    pub max_object_bytes: u64,
}
impl Default for GitImportLimits {
    fn default() -> Self {
        Self { max_objects: 100_000, max_edges: 1_000_000,
            max_total_bytes: 384 * 1024 * 1024, max_object_bytes: 32 * 1024 * 1024 }
    }
}
impl GitImportLimits {
    fn validate(self) -> Result<()> {
        ensure!((1..=1_000_000).contains(&self.max_objects)
            && (1..=4_000_000).contains(&self.max_edges)
            && (1..=4 * 1024 * 1024 * 1024).contains(&self.max_total_bytes)
            && (1..=256 * 1024 * 1024).contains(&self.max_object_bytes),
            "Git import limits exceed the bounded profile");
        Ok(())
    }
}

/// Current import DATA, deliberately non-Serde and not an accepted-source proof.
#[derive(Debug, PartialEq, Eq)]
pub struct ImportedGitHeadData {
    pub head: GitCommitId,
    pub objects: u64,
    pub commits: u64,
    pub trees: u64,
    pub blobs: u64,
    pub bytes: u64,
}

fn absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(_) => anyhow::bail!("Git import requires a self-contained, non-shallow store"),
    }
}

fn check_odb(repo: &gix::Repository, budget: &FinalizationBudget) -> Result<()> {
    budget.remaining()?;
    ensure!(repo.object_hash() == gix::hash::Kind::Sha1, "Git import requires SHA1");
    ensure!(repo.namespace().is_none(), "Git import requires the unnamespaced repository");
    // The ODB map contains only enabled replacements. Refuse the actual
    // namespace even when replacement lookup is disabled, and include packed
    // refs. An unreadable/malformed matching ref is also refusal, never empty.
    let references = repo.references()
        .map_err(|_| anyhow::anyhow!("Git replacement references cannot be inspected"))?;
    let mut replacements = references.prefixed("refs/replace/")
        .map_err(|_| anyhow::anyhow!("Git replacement namespace cannot be inspected"))?;
    ensure!(replacements.next().is_none(), "Git import refuses replacement references");
    budget.remaining()?;
    let expected = repo.common_dir().join("objects");
    ensure!(fs::symlink_metadata(&expected)?.is_dir()
        && repo.objects.store_ref().path().canonicalize()? == expected.canonicalize()?,
        "Git object storage is redirected");
    for path in [repo.shallow_file(), expected.join("info/alternates"), expected.join("info/http-alternates")] {
        absent(&path)?;
    }
    ensure!(repo.objects.store_ref().replacements().next().is_none()
        && repo.objects.store_ref().alternate_db_paths()?.is_empty(),
        "Git import refuses replacements and alternate stores");
    budget.remaining()?;
    Ok(())
}

fn check_target(target: &GitBackend, repo: &gix::Repository, budget: &FinalizationBudget) -> Result<()> {
    budget.remaining()?;
    let path = target.git_repo_path();
    let store = path.parent().context("internal Git store has no parent")?;
    let marker = store.join("git_target");
    let marker_metadata = fs::symlink_metadata(&marker)?;
    ensure!(path.file_name().is_some_and(|name| name == "git")
        && fs::symlink_metadata(path)?.is_dir() && path.canonicalize()? == path
        && marker_metadata.is_file() && marker_metadata.len() == 3
        && fs::read(marker)? == b"git", "target must be the owned internal Git backend");
    absent(&path.join("commondir"))?;
    check_odb(repo, budget)
}

fn check_head(source: &gix::Repository, expected: ObjectId, budget: &FinalizationBudget) -> Result<()> {
    budget.remaining()?;
    // Do not peel tags or execute a command while resolving the expected HEAD.
    let current = source.head().map_err(|_| anyhow::anyhow!("source Git HEAD cannot be resolved"))?;
    ensure!(current.id().map(|id| id.detach()) == Some(expected),
        "source Git HEAD differs from the original expected commit");
    budget.remaining()?;
    Ok(())
}

fn schedule(id: ObjectId, kind: Kind, known: &mut BTreeMap<ObjectId, Kind>,
    pending: &mut VecDeque<ObjectId>, limits: GitImportLimits) -> Result<()> {
    if let Some(old) = known.get(&id) {
        ensure!(*old == kind, "Git closure uses one object with conflicting kinds");
    } else {
        ensure!((known.len() as u64) < limits.max_objects, "Git import object-count limit exceeded");
        known.insert(id, kind);
        pending.push_back(id);
    }
    Ok(())
}

struct BudgetReader<'a> { bytes: &'a [u8], budget: &'a FinalizationBudget }
impl Read for BudgetReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.budget.remaining().map_err(|_| io::Error::other("Git import budget retired"))?;
        let count = output.len().min(self.bytes.len()).min(64 * 1024);
        output[..count].copy_from_slice(&self.bytes[..count]);
        self.bytes = &self.bytes[count..];
        Ok(count)
    }
}

/// Copy the complete commit/tree/blob closure, then import that exact commit
/// into jj's extra metadata. No refs, configs, hooks or source files are copied;
/// no Git subprocess/network is used. The supplied budget is ORIGINAL, shared
/// with the caller: no new deadline is created here. Local gix/jj
/// blocking calls are cooperative, not preemptible; retain the actual worker
/// and target until return even when cancellation/deadline has fired.
pub fn import_expected_git_head(
    source: &gix::Repository,
    target: &GitBackend,
    expected: &GitCommitId,
    limits: GitImportLimits,
    budget: &FinalizationBudget,
) -> Result<ImportedGitHeadData> {
    budget.remaining()?;
    expected.validate()?;
    limits.validate()?;
    let head = ObjectId::from_hex(expected.0.as_bytes())?;
    check_odb(source, budget)?;
    check_head(source, head, budget)?;
    let destination = target.git_repo();
    check_target(target, &destination, budget)?;
    ensure!(source.objects.store_ref().path().canonicalize()?
        != destination.objects.store_ref().path().canonicalize()?, "source and target Git stores must differ");
    let mut known = BTreeMap::new();
    let mut pending = VecDeque::new();
    schedule(head, Kind::Commit, &mut known, &mut pending, limits)?;
    let mut data = ImportedGitHeadData { head: expected.clone(), objects: 0, commits: 0, trees: 0, blobs: 0, bytes: 0 };
    let mut edges = 0_u64;
    while let Some(id) = pending.pop_front() {
        check_head(source, head, budget)?;
        let header = source.find_header(id)?;
        budget.remaining()?;
        let size = header.size();
        ensure!(header.kind() == known[&id] && size <= limits.max_object_bytes
            && size <= limits.max_total_bytes.saturating_sub(data.bytes),
            "Git import object kind/size exceeds admission");
        let object = source.find_object(id)?;
        budget.remaining()?;
        ensure!(object.kind == header.kind() && object.data.len() as u64 == size,
            "Git object changed during import");
        ensure!(gix::objs::compute_hash(gix::hash::Kind::Sha1, object.kind, &object.data)? == id,
            "source Git bytes differ from their raw object identity");
        budget.remaining()?;
        let mut add = |child, kind| -> Result<()> {
            budget.remaining()?;
            edges = edges.checked_add(1).context("Git edge-count overflow")?;
            ensure!(edges <= limits.max_edges, "Git import edge-count limit exceeded");
            schedule(child, kind, &mut known, &mut pending, limits)
        };
        match object.kind {
            Kind::Commit => {
                let commit = gix::objs::CommitRef::from_bytes(&object.data)
                    .map_err(|_| anyhow::anyhow!("malformed Git commit"))?;
                commit.author().map_err(|_| anyhow::anyhow!("malformed Git commit author"))?;
                commit.committer().map_err(|_| anyhow::anyhow!("malformed Git committer"))?;
                ensure!(commit.parents.len() <= 1024, "Git commit has too many parents");
                add(commit.tree(), Kind::Tree)?;
                for parent in commit.parents() { add(parent, Kind::Commit)?; }
                data.commits += 1;
            }
            Kind::Tree => {
                let mut names = BTreeSet::new();
                let mut previous = None;
                for entry in gix::objs::TreeRefIter::from_bytes(&object.data) {
                    let entry = entry.map_err(|_| anyhow::anyhow!("malformed Git tree"))?;
                    let name: &[u8] = entry.filename.as_ref();
                    ensure!(!name.is_empty() && name != b"." && name != b".."
                        && !name.contains(&b'/') && names.insert(name)
                        && previous.is_none_or(|old| old < entry), "malformed Git tree namespace/order");
                    previous = Some(entry);
                    use gix::objs::tree::EntryKind;
                    let kind = match entry.mode.kind() {
                        EntryKind::Tree => Kind::Tree,
                        EntryKind::Blob | EntryKind::BlobExecutable | EntryKind::Link => Kind::Blob,
                        EntryKind::Commit => anyhow::bail!("Gitlinks are unsupported by accepted source import"),
                    };
                    add(entry.oid.to_owned(), kind)?;
                }
                data.trees += 1;
            }
            Kind::Blob => data.blobs += 1,
            _ => anyhow::bail!("unsupported Git closure object kind"),
        }
        check_head(source, head, budget)?;
        let mut reader = BudgetReader { bytes: &object.data, budget };
        let written = destination.objects.write_stream(object.kind, size, &mut reader)
            .map_err(|_| anyhow::anyhow!("internal Git object write failed"))?;
        budget.remaining()?;
        ensure!(reader.bytes.is_empty() && written == id, "target Git write changed raw object identity");
        check_head(source, head, budget)?;
        data.objects += 1;
        data.bytes = data.bytes.checked_add(size).context("Git byte-count overflow")?;
    }
    check_odb(source, budget)?;
    check_head(source, head, budget)?;
    check_target(target, &destination, budget)?;
    let commit = CommitId::new(head.as_bytes().to_vec());
    target.import_head_commits([&commit])?;
    budget.remaining()?;
    check_head(source, head, budget)?;
    check_odb(source, budget)?;
    check_target(target, &destination, budget)?;
    Ok(data)
}
