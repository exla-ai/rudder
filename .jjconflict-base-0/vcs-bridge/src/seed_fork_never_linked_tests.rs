//! Original-factory failure/link-intent regressions, written UNRUN. These
//! integrity-only private fixtures issue no Native never-started authority.
use super::*;
use rudder_engine::{protocol::{CheckpointConsistency,ObjectKind,OwnerEpoch},store::LocalStore};
use std::{sync::atomic::AtomicUsize,time::Duration};

fn budget()->FinalizationBudget{FinalizationBudget::begin(Duration::from_secs(30)).unwrap()}
struct Fixture {
    _temp:tempfile::TempDir,root:PathBuf,template_root:PathBuf,template:VerifiedMetadataTemplate,binding:AttemptBinding,
}
impl Fixture {
    fn new()->Self {
        let temp=tempfile::Builder::new().permissions(fs::Permissions::from_mode(0o700))
            .prefix("rudder-never-linked-").tempdir_in("/workspace").unwrap();
        let root=temp.path().canonicalize().unwrap();let input=root.join("input");
        for rel in [".git/objects/aa",".jj/repo/store",".jj/working_copy"]{fs::create_dir_all(input.join(rel)).unwrap();}
        fs::write(input.join(".git/HEAD"),b"ref: refs/heads/main\n").unwrap();
        fs::write(input.join(".git/config"),SAFE_GIT_CONFIG).unwrap();
        fs::write(input.join(".git/objects/aa/fixture"),b"original object fixture").unwrap();
        fs::write(input.join(".jj/repo/store/git_target"),b"../../../.git").unwrap();
        fs::write(input.join(".jj/working_copy/type"),b"local").unwrap();
        let baseline=crate::CommitIdentity{commit:crate::GitCommitId("a".repeat(40)),change:crate::JjChangeId("b".repeat(32)),parents:vec![],
            tree:crate::JjTreeIdentity{terms:vec![crate::GitTreeId("c".repeat(40))],labels:vec![]}};
        let operation=crate::JjOperationId("d".repeat(128));let template_root=root.join("seed");
        prepare_metadata_template(&input,&template_root,baseline.clone(),operation.clone()).unwrap();
        let store=LocalStore::open(root.join("cas")).unwrap();
        let seed=store.import_tree(&template_root,CheckpointConsistency::Quiesced).unwrap();store.pin("fixture-seed",&seed).unwrap();
        let template=VerifiedMetadataTemplate::open_retained_inner(&store,&seed,&template_root,&template_root.join("objects"),false).unwrap();
        let data=|text:&str|ObjectRef::from_bytes(ObjectKind::Blob,text.as_bytes());
        let binding=AttemptBinding{format_version:1,project_id:"never-linked-fixture".into(),owner:OwnerEpoch{owner:"fixture".into(),epoch:1},
            run_id:"run".into(),run_epoch:1,attempt_id:uuid::Uuid::new_v4().to_string(),view_id:"view".into(),seed,baseline,
            baseline_operation:operation,projection_policy:data("policy"),profile:data("profile")};
        Self{_temp:temp,root,template_root,template,binding}
    }
    fn prepare(&self,name:&str,owner:&mut Option<PrivateMetadataForkOwner>)->Result<PrivateMetadataFork>{
        fork_private_metadata_retaining(owner,&self.template,&self.root.join(name),self.binding.clone(),None,&budget(),&AtomicBool::new(false))
    }
    fn failed_at(&self,point:FactoryFault)->PrivateMetadataForkOwner {
        FACTORY_FAULT.with(|slot|slot.set(Some(point)));let mut owner=None;
        assert!(self.prepare("fork",&mut owner).is_err());owner.unwrap()
    }
    fn failed_copy(&self)->PrivateMetadataForkOwner {
        let source=self.template_root.join("metadata/.git/HEAD");let size=fs::metadata(&source).unwrap().len() as usize;
        fs::write(source,vec![b'x';size]).unwrap();let mut owner=None;
        assert!(self.prepare("fork",&mut owner).is_err());let owner=owner.unwrap();
        assert!(matches!(owner.pending.as_ref().map(|job|&job.source),Some(CopySource::File(_))));owner
    }
}
#[test]
fn original_never_attempted_permit_releases_a_failed_root_capture_without_path_certification(){
    let f=Fixture::new();let mut owner=f.failed_at(FactoryFault::AfterOwnerStaged);let original=budget();let mut out=None;
    assert!(owner.private.as_ref().unwrap().dirs.is_empty());
    owner.release_never_linked_retaining(&mut out,&original,&AtomicBool::new(false),|binding|{ensure!(binding==&f.binding,"wrong original binding");Ok(())}).unwrap();
    let proof=out.unwrap();assert_eq!(proof.binding(),&f.binding);assert_eq!(proof.seed(),&f.binding.seed);
    assert_eq!(proof.seed_manifest(),f.template.identity());assert_eq!(proof.manifest(),f.template.identity());
    assert_eq!(proof.original_shared_objects_root(),f.template.object_mount);
    assert!(owner.source.is_none());assert!(owner.alternate_install_permit.is_none());assert!(owner.never_linked_released);
    // TEST-owned source teardown; this API never removes source/history.
    fs::remove_dir_all(&f.template_root).unwrap();proof.check_current(&budget(),&AtomicBool::new(false)).unwrap();
}
#[test]
fn partial_metadata_output_history_and_actual_private_fd_survive_source_release(){
    let f=Fixture::new();let mut owner=f.failed_copy();let job=owner.pending.as_ref().unwrap();
    let private_inode=identity(&job.output).unwrap();let bytes=fs::read(fd_path(&job.output)).unwrap();let original=budget();let mut out=None;
    owner.release_never_linked_retaining(&mut out,&original,&AtomicBool::new(false),|_|Ok(())).unwrap();
    let proof=out.unwrap();let partial=proof.pending.as_ref().unwrap();assert_eq!(identity(&partial.output).unwrap(),private_inode);
    assert_eq!(fs::read(fd_path(&partial.output)).unwrap(),bytes);assert!(partial.parent.private);
    assert!(owner.source.is_none()&&owner.pending.is_none()&&owner.private.is_none());
    fs::remove_dir_all(&f.template_root).unwrap();proof.check_current(&budget(),&AtomicBool::new(false)).unwrap();
}
#[test]
fn claimed_intent_before_inline_blocks_proof_even_when_every_absence_flag_is_empty(){
    let f=Fixture::new();let mut owner=f.failed_at(FactoryFault::AfterAlternateIntent);
    assert!(owner.alternates.is_none()&&owner.pending.is_none());assert!(!f.root.join("fork/.git/objects/info/alternates").exists());
    assert!(owner.alternate_install_intent.is_some()&&owner.alternate_install_permit.is_none());
    let callbacks=AtomicUsize::new(0);let mut out=None;
    assert!(owner.release_never_linked_retaining(&mut out,&budget(),&AtomicBool::new(false),|_|{callbacks.fetch_add(1,Ordering::SeqCst);Ok(())}).is_err());
    assert_eq!(callbacks.load(Ordering::SeqCst),0);assert!(out.is_none()&&owner.source.is_some());
}
#[test]
fn installed_then_unlinked_alternate_never_turns_into_a_never_linked_receipt(){
    let f=Fixture::new();let mut owner=f.failed_at(FactoryFault::AfterAlternateInstalled);
    let path=f.root.join("fork/.git/objects/info/alternates");assert!(owner.alternates.is_some());
    fs::remove_file(&path).unwrap();assert_eq!(owner.alternates.as_ref().unwrap().file.metadata().unwrap().nlink(),0);
    let mut out=None;assert!(owner.release_never_linked_retaining(&mut out,&budget(),&AtomicBool::new(false),|_|Ok(())).is_err());
    assert!(out.is_none()&&owner.source.is_some());
}
#[test]
fn start_copy_cannot_bypass_the_original_alternate_install_intent(){
    let f=Fixture::new();let mut owner=f.failed_copy();let mut out=None;let original=budget();
    // Settle only this failed test's private pending carrier into an explicit
    // retained variable, rather than pretending its absence proves anything.
    let _original_pending=owner.pending.take();
    assert!(owner.inline(".git/objects/info/alternates",b"/foreign\n".to_vec(),&original,&AtomicBool::new(false)).is_err());
    assert!(owner.alternate_install_permit.is_some()&&owner.alternate_install_intent.is_none());
    assert!(owner.begin_original_alternate_install().is_ok());assert!(owner.begin_original_alternate_install().is_err());
    assert!(owner.release_never_linked_retaining(&mut out,&original,&AtomicBool::new(false),|_|Ok(())).is_err());
}
#[test]
fn callback_error_or_panic_preserves_original_source_pending_and_request(){
    let f=Fixture::new();let mut owner=f.failed_copy();let original=budget();let mut out=None;
    let caught=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||{
        let _=owner.release_never_linked_retaining(&mut out,&original,&AtomicBool::new(false),|_|panic!("native quiescence not available"));
    }));
    assert!(caught.is_err()&&out.is_none()&&owner.source.is_some()&&owner.pending.is_some());assert!(owner.never_linked_release.is_some());
    assert!(!owner.is_ready());
    assert!(owner.reconcile_never_linked_retaining(&mut out,&budget(),&AtomicBool::new(false),|_|Ok(())).is_err());
    assert!(owner.reconcile_never_linked_retaining(&mut out,&original,&AtomicBool::new(false),|_|anyhow::bail!("actual original joins unavailable")).is_err());
    assert!(out.is_none()&&owner.source.is_some()&&owner.pending.is_some());
    owner.reconcile_never_linked_retaining(&mut out,&original,&AtomicBool::new(false),|_|Ok(())).unwrap();
    assert!(out.is_some()&&owner.source.is_none());
}
#[test]
fn observed_cancellation_is_absorbing_and_cannot_get_a_new_cleanup_deadline(){
    let f=Fixture::new();let mut owner=f.failed_copy();let original=budget();let mut out=None;
    assert!(owner.release_never_linked_retaining(&mut out,&original,&AtomicBool::new(true),|_|Ok(())).is_err());
    assert!(original.remaining().is_err()&&out.is_none()&&owner.source.is_some());
    assert!(owner.reconcile_never_linked_retaining(&mut out,&original,&AtomicBool::new(false),|_|Ok(())).is_err());
    assert!(owner.reconcile_never_linked_retaining(&mut out,&budget(),&AtomicBool::new(false),|_|Ok(())).is_err());
    assert!(owner.source.is_some()&&owner.pending.is_some());
}
#[test]
fn actual_ack_is_stored_before_a_late_original_budget_fence(){
    let f=Fixture::new();let mut owner=f.failed_copy();let original=budget();let mut out=None;
    FACTORY_FAULT.with(|slot|slot.set(Some(FactoryFault::FenceAfterReleaseAck)));
    assert!(owner.release_never_linked_retaining(&mut out,&original,&AtomicBool::new(false),|_|Ok(())).is_err());
    assert!(out.is_some()&&owner.source.is_none()&&owner.pending.is_none()&&owner.never_linked_released);
    out.as_ref().unwrap().check_current(&budget(),&AtomicBool::new(false)).unwrap();
    assert!(owner.reconcile_never_linked_retaining(&mut out,&original,&AtomicBool::new(false),|_|Ok(())).is_err());
}
