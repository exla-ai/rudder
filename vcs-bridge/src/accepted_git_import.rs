//! Bounded, off-App import of exact Git object bytes, never acceptance authority.
//! Caller holds the actual source/target owners and source-cleanliness lock.
//! Errors may leave immutable objects/jj import effects in the owned target;
//! neither an error nor this DATA result publishes a bookmark or Journal fact.

use crate::GitCommitId;
use anyhow::{Context, Result, ensure};
use gix::{ObjectId, objs::Kind};
use jj_lib::{backend::CommitId, git_backend::GitBackend};
use rudder_engine::execution::finalization_budget::FinalizationBudget;
use std::{collections::{BTreeMap, BTreeSet, VecDeque}, fs, io, path::Path, sync::{Arc,atomic::{AtomicBool,Ordering}}, time::Instant};

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
#[derive(Clone,Debug,PartialEq,Eq,serde::Serialize)]
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
        .context("Git replacement references cannot be inspected")?;
    let mut replacements = references.prefixed("refs/replace/")
        .context("Git replacement namespace cannot be inspected")?;
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
    let current = source.head().context("source Git HEAD cannot be resolved")?;
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

/// Copy the complete commit/tree/blob closure, then import that exact commit
/// into jj's extra metadata. No refs, configs, hooks or source files are copied;
/// no Git subprocess/network is used. The supplied budget is ORIGINAL, shared
/// with the caller: no new deadline is created here. Local gix/jj
/// blocking calls are cooperative, not preemptible; retain the actual worker
/// and target until return even when cancellation/deadline has fired.
#[path="accepted_git_pack.rs"]
mod pack;
/// Diagnostic DATA only. A progress record never acknowledges an import,
/// accepted source, seed, Root effect or permission to retry a partial target.
#[derive(Clone,Debug,serde::Serialize)]
pub struct GitImportProgress {
 pub scope:&'static str,pub stage:&'static str,pub current_oid:Option<String>,pub current_kind:Option<String>,pub current_size:u64,
 pub verified_objects:u64,pub verified_bytes:u64,pub packed_objects:u64,pub packed_bytes:u64,
 pub pending_objects:usize,pub edges:u64,pub elapsed_ms:u128,pub acknowledged:bool,pub error_chain:Option<String>,
 pub pack_stage:Option<&'static str>,pub pack_bytes:u64,pub pack_synced:bool,pub index_synced:bool,pub keep_synced:bool,
 pub directory_syncs:u64,pub pack_install_attempted:bool,pub index_install_attempted:bool,pub keep_install_attempted:bool,
 pub pack_installed:bool,pub index_installed:bool,pub keep_installed:bool,pub installed_paths:Vec<std::path::PathBuf>,pub temporary_paths:Vec<std::path::PathBuf>,pub pack_count:usize,
}
/// Actual partial pack/index/keep/parent FDs survive errors and unwind when the
/// caller stages this outside its operation/catch. No Drop cleanup or replay.
pub struct RetainedGitImport {
 original:FinalizationBudget,cancelled:Arc<AtomicBool>,workspace:Option<jj_lib::workspace::Workspace>,repository:Option<Arc<jj_lib::repo::ReadonlyRepo>>,attempted:bool,pack:Option<pack::PackBuilder>,packs:Vec<pack::PackBuilder>,data:ImportedGitHeadData,
 stage:&'static str,current_oid:Option<String>,current_kind:Option<String>,current_size:u64,
 verified_objects:u64,verified_bytes:u64,pending:usize,edges:u64,started:Instant,ack:bool,error_chain:Option<String>,
}
impl RetainedGitImport {
 pub(crate) fn stage_initial_repository_retaining(out:&mut Option<Self>,workspace:jj_lib::workspace::Workspace,repository:Arc<jj_lib::repo::ReadonlyRepo>,expected:&GitCommitId,budget:&FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<()> {
  ensure!(out.is_none(),"initial Git import carrier already staged");*out=Some(Self::capture(expected,budget,cancelled));let owner=out.as_mut().unwrap();owner.workspace=Some(workspace);owner.repository=Some(repository);owner.stage="initialized-private-repository";owner.tick()
 }
 fn capture(expected:&GitCommitId,budget:&FinalizationBudget,cancelled:Arc<AtomicBool>)->Self{Self{original:budget.clone(),cancelled,workspace:None,repository:None,attempted:false,pack:None,packs:Vec::new(),
  data:ImportedGitHeadData{head:expected.clone(),objects:0,commits:0,trees:0,blobs:0,bytes:0},stage:"validate",current_oid:None,current_kind:None,current_size:0,verified_objects:0,verified_bytes:0,pending:0,edges:0,started:Instant::now(),ack:false,error_chain:None}}
 pub fn progress(&self)->GitImportProgress {let all=self.packs.iter().chain(self.pack.iter()).collect::<Vec<_>>();GitImportProgress{scope:"verified-Git-history-import-DATA-only",stage:self.stage,current_oid:self.current_oid.clone(),current_kind:self.current_kind.clone(),current_size:self.current_size,
  verified_objects:self.verified_objects,verified_bytes:self.verified_bytes,packed_objects:self.data.objects,packed_bytes:self.data.bytes,pending_objects:self.pending,edges:self.edges,elapsed_ms:self.started.elapsed().as_millis(),acknowledged:self.ack,error_chain:self.error_chain.clone(),
  pack_stage:all.last().map(|p|p.stage),pack_bytes:all.iter().map(|p|p.bytes).sum(),pack_synced:!all.is_empty()&&all.iter().all(|p|p.pack_synced),index_synced:!all.is_empty()&&all.iter().all(|p|p.index_synced),keep_synced:!all.is_empty()&&all.iter().all(|p|p.keep_synced),directory_syncs:all.iter().map(|p|p.directory_syncs).sum(),
  pack_install_attempted:all.iter().any(|p|p.pack_install_attempted),index_install_attempted:all.iter().any(|p|p.index_install_attempted),keep_install_attempted:all.iter().any(|p|p.keep_install_attempted),pack_installed:!all.is_empty()&&all.iter().all(|p|p.pack_installed),index_installed:!all.is_empty()&&all.iter().all(|p|p.index_installed),keep_installed:!all.is_empty()&&all.iter().all(|p|p.keep_installed),installed_paths:all.iter().flat_map(|p|p.paths().into_iter().flatten().map(Path::to_path_buf)).collect(),temporary_paths:all.iter().flat_map(|p|p.temporary_paths().into_iter().map(Path::to_path_buf)).collect(),pack_count:all.len()}}

 pub fn data(&self)->Result<&ImportedGitHeadData>{ensure!(self.ack,"original Git import ACK absent");Ok(&self.data)}
 fn tick(&self)->Result<()>{if self.cancelled.load(Ordering::Acquire){self.original.cancel();anyhow::bail!("original Git import cancelled");}self.original.remaining()?;Ok(())}
}
/// Compatibility DATA wrapper. Production initial-source owners use the
/// retaining entry below; this wrapper cannot retain failure FDs for a caller.
pub fn import_expected_git_head(
    source: &gix::Repository,
    target: &GitBackend,
    expected: &GitCommitId,
    limits: GitImportLimits,
    budget: &FinalizationBudget,
) -> Result<ImportedGitHeadData> {
    let mut owner=None;
    import_expected_git_head_retaining(&mut owner,source,target,expected,limits,budget,
        Arc::new(AtomicBool::new(false)),||Ok(()))
}
#[allow(clippy::too_many_arguments)]
pub fn import_expected_git_head_retaining(out:&mut Option<RetainedGitImport>,source:&gix::Repository,target:&GitBackend,
 expected:&GitCommitId,limits:GitImportLimits,budget:&FinalizationBudget,cancelled:Arc<AtomicBool>,mut check_owner:impl FnMut()->Result<()>)->Result<ImportedGitHeadData>{
 if out.is_none(){*out=Some(RetainedGitImport::capture(expected,budget,cancelled.clone()));}
 let retained=out.as_mut().unwrap();ensure!(!retained.attempted&&retained.original.constraint()==budget.constraint()&&Arc::ptr_eq(&retained.cancelled,&cancelled)&&retained.data.head==*expected,"original Git import carrier differs or attempted; no replay");retained.attempted=true;
 let owner=out.as_mut().unwrap();let result=owner.run(source,target,expected,limits,&mut check_owner);
 if let Err(error)=&result{owner.error_chain=Some(format!("{error:#}").chars().take(4096).collect());}result
}
impl RetainedGitImport {
 fn run(&mut self,source:&gix::Repository,target:&GitBackend,expected:&GitCommitId,limits:GitImportLimits,check_owner:&mut impl FnMut()->Result<()>)->Result<ImportedGitHeadData>{
    let original=self.original.clone();let budget=&original;self.tick()?;check_owner()?;
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
    self.stage="capture-pack";pack::PackBuilder::capture_retaining(&mut self.pack,&destination.common_dir().join("objects"),limits,budget.clone(),self.cancelled.clone())?;check_owner()?;self.stage="verify-closure";
    let mut edges = 0_u64;
    while let Some(id) = pending.pop_front() {
        self.tick()?;self.current_oid=Some(id.to_string());self.pending=pending.len();
        check_head(source, head, budget)?;
        let header = source.find_header(id)?;
        budget.remaining()?;
        let size = header.size();self.current_size=size;self.current_kind=Some(format!("{:?}",header.kind()));
        ensure!(header.kind() == known[&id] && size <= limits.max_object_bytes
            && size <= limits.max_total_bytes.saturating_sub(self.data.bytes),
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
                    .context("malformed Git commit")?;
                commit.author().context("malformed Git commit author")?;
                commit.committer().context("malformed Git committer")?;
                ensure!(commit.parents.len() <= 1024, "Git commit has too many parents");
                add(commit.tree(), Kind::Tree)?;
                for parent in commit.parents() { add(parent, Kind::Commit)?; }
                self.data.commits += 1;
            }
            Kind::Tree => {
                let mut names = BTreeSet::new();
                let mut previous = None;
                for entry in gix::objs::TreeRefIter::from_bytes(&object.data) {
                    let entry = entry.context("malformed Git tree")?;
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
                self.data.trees += 1;
            }
            Kind::Blob => self.data.blobs += 1,
            _ => anyhow::bail!("unsupported Git closure object kind"),
        }
        check_head(source, head, budget)?;
        self.verified_objects+=1;self.verified_bytes=self.verified_bytes.checked_add(size).context("verified Git byte-count overflow")?;self.edges=edges;
        if !self.pack.as_ref().unwrap().can_fit(size)? {self.stage="seal-pack-shard";self.pack.as_mut().unwrap().seal()?;self.packs.push(self.pack.take().unwrap());ensure!(self.packs.len()<128,"Git pack shard count exceeds bounded FD profile");
         pack::PackBuilder::capture_retaining(&mut self.pack,&destination.common_dir().join("objects"),limits,budget.clone(),self.cancelled.clone())?;self.stage="verify-closure";}
        self.pack.as_mut().unwrap().add(id,object.kind,&object.data).with_context(||format!("append verified Git object {id} ({:?}, {size} bytes)",object.kind))?;
        self.tick()?;
        check_head(source, head, budget)?;
        self.data.objects += 1;
        self.data.bytes = self.data.bytes.checked_add(size).context("Git byte-count overflow")?;
    }
    check_odb(source, budget)?;
    check_head(source, head, budget)?;
    check_target(target, &destination, budget)?;
    self.stage="install-pack";let cancelled=self.cancelled.clone();
    let mut guard=||->Result<()> {if cancelled.load(Ordering::Acquire){budget.cancel();anyhow::bail!("original Git import publication cancelled");}budget.remaining()?;check_owner()?;check_odb(source,budget)?;check_head(source,head,budget)?;check_target(target,&destination,budget)};
    self.pack.as_mut().unwrap().seal()?;self.packs.push(self.pack.take().unwrap());
    // The ENTIRE closure is verified and all shards sealed before any index is visible.
    for pack in &mut self.packs{pack.publish(&mut guard)?;}self.tick()?;self.stage="jj-import-head";
    let commit = CommitId::new(head.as_bytes().to_vec());
    target.import_head_commits([&commit])?;
    budget.remaining()?;
    check_head(source, head, budget)?;
    check_odb(source, budget)?;
    check_target(target, &destination, budget)?;
    for pack in &self.packs{pack.current()?;}self.tick()?;check_owner()?;self.pending=0;self.stage="complete";self.ack=true;Ok(self.data.clone())
 }
}
