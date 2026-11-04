//! Real local filesystem/gix/jj fixtures. No subprocess, network or Native
//! backend-drain authority. The ignored fixture alone tests a real RO mount.
use super::*;
use gix::objs::Write as _;
use jj_lib::{backend::CommitId,config::{ConfigLayer,ConfigSource,StackedConfig},git_backend::GitBackend,
    ref_name::WorkspaceName,repo::{Repo,ReadonlyRepo},settings::UserSettings,
    workspace::{Workspace,default_working_copy_factories}};
use rudder_engine::{protocol::{CheckpointConsistency,ObjectKind,OwnerEpoch},store::LocalStore};
use std::time::Duration;

const PACK_ID:&str="1e1d6799c57d6d7bfe7d6c71b81e1be66f8658c4";
const PACK_HEX:&str="5041434b0000000200000005b801789c2b484cce4e4d5148ad484d2e2d494cca495548aa2c492de60200737e08fc37789c2b294a4cce4e4d01000bbf02dfa304789c3334320002859cccbc6c068b8613d17261caeb66861e3ab2f1a1f7d626cfb79c860606e6a6a60a254589c9d9a9290c6d87aa574d52795dc0f27982e88dfb520c621aa9a5004e561a0c9d0a789c95cb410ac2301046e17d4e91bd204931b6011157de6332f983c1a40de9547a7c0b9ec0b7fa364f3aa02d02a6c93817ae1618a34994928b9ead41f0ccde5c30846154b4c96be9fa9977d93af42dfdf0c8f3874a8e67ec545bc15d5b7d32478a975ab308fe7954237e236a624693038156943c437d01625039cbc608789c25cb410ac2301046e17d4e317b4112daa60988b8f21e93e44f89346d89a3d4db5bf1adbecd5bc30351a8b72387ecb389661c86ce251bd96a04f6ce3a37f4ba0367f65ec96703c5b5d6224a78a22790907e9cd0e85e767935d025ff712bcb9be792ced8b96e33ae64e8a48f946a102e0b121dabfa0247c52b741e1d6799c57d6d7bfe7d6c71b81e1be66f8658c4";
const INDEX_HEX:&str="ff744f6300000002000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000010000000100000001000000010000000100000001000000010000000100000001000000010000000100000001000000010000000100000001000000010000000100000001000000010000000100000001000000010000000100000001000000010000000100000002000000020000000200000002000000020000000200000002000000020000000200000002000000020000000200000002000000020000000300000003000000040000000400000004000000040000000400000004000000040000000400000004000000040000000400000004000000040000000400000004000000040000000400000004000000040000000400000004000000040000000400000004000000040000000400000004000000040000000400000004000000040000000400000004000000040000000400000004000000040000000400000004000000040000000400000004000000040000000400000004000000040000000400000004000000040000000400000004000000040000000400000004000000040000000400000004000000040000000400000004000000040000000400000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000050000000500000005000000051ebe88055b61ee7d0faff5d9c10eb9cc904e2b273880c85b1e5623ae9955c2c4b1e14bb58249ed09467abf9f1c175538d6ca60eba986885403eafa99480b8a9d26fd96ad730ec4c09433ac75409ca22686c27baa9224eb7004f39015d8df1a001628657557cddb0a183e4dc3f9b4aa69c8fd395406eadc6b0000003e0000002e0000008b000001080000000c1e1d6799c57d6d7bfe7d6c71b81e1be66f8658c42a128482b901aca95c63c096c22e42c441fc814e";
const PACKED_COMMIT:&str="467abf9f1c175538d6ca60eba986885403eafa99";
const PACKED_TREE:&str="1ebe88055b61ee7d0faff5d9c10eb9cc904e2b27";
const PACKED_BLOB:&str="86c27baa9224eb7004f39015d8df1a0016286575";
fn budget()->FinalizationBudget{FinalizationBudget::begin(Duration::from_secs(30)).unwrap()}
fn settings()->UserSettings{
    let mut config=StackedConfig::with_defaults();let mut layer=ConfigLayer::empty(ConfigSource::User);
    layer.set_value("user.name","Fork localization fixture").unwrap();layer.set_value("user.email","fixture@invalid.example").unwrap();
    config.add_layer(layer);UserSettings::from_config(config).unwrap()
}
fn update_wc(workspace:&Workspace,repo:&ReadonlyRepo,commit:&jj_lib::commit::Commit){
    let mut locked=workspace.working_copy().start_mutation().unwrap();
    pollster::block_on(locked.reset(commit)).unwrap();pollster::block_on(locked.finish(repo.op_id().clone())).unwrap();
}
struct Fixture {
    _temp:tempfile::TempDir,root:PathBuf,seed_root:PathBuf,fork:PathBuf,shared:PathBuf,
    owner:Option<PrivateMetadataForkOwner>,proof:Option<LocalizedMetadataFork>,binding:AttemptBinding,original_mode:u32,
}
impl Fixture {
    fn new(mounted:bool)->Self{
        let temp=tempfile::Builder::new().permissions(fs::Permissions::from_mode(0o700))
            .prefix("rudder-fork-localization-").tempdir_in("/workspace").unwrap();
        let root=temp.path().canonicalize().unwrap();
        let input=root.join("initial-history");fs::create_dir(&input).unwrap();
        let (workspace,repo)=pollster::block_on(Workspace::init_internal_git(&settings(),&input)).unwrap();
        let backend=repo.store().backend_impl::<GitBackend>().unwrap();let git=backend.git_repo_path();
        let pack_dir=git.join("objects/pack");fs::create_dir_all(&pack_dir).unwrap();
        fs::write(pack_dir.join(format!("pack-{PACK_ID}.pack")),hex::decode(PACK_HEX).unwrap()).unwrap();
        fs::write(pack_dir.join(format!("pack-{PACK_ID}.idx")),hex::decode(INDEX_HEX).unwrap()).unwrap();
        let id=CommitId::new(hex::decode(PACKED_COMMIT).unwrap());backend.import_head_commits([&id]).unwrap();
        let packed=repo.store().get_commit(&id).unwrap();
        let mut tx=repo.start_transaction();pollster::block_on(tx.repo_mut().add_head(&packed)).unwrap();
        tx.repo_mut().set_wc_commit(WorkspaceName::DEFAULT.to_owned(),packed.id().clone()).unwrap();
        let accepted=pollster::block_on(tx.commit("retain packed accepted history")).unwrap();update_wc(&workspace,&accepted,&packed);
        fs::write(git.join("HEAD"),format!("{PACKED_COMMIT}\n")).unwrap();
        fs::create_dir_all(git.join("refs/tags")).unwrap();fs::write(git.join("refs/tags/seeded"),"480b8a9d26fd96ad730ec4c09433ac75409ca226\n").unwrap();
        let seed_root=root.join("seed");
        let manifest=prepare_internal_metadata_template(&input,&seed_root,crate::CommitIdentity::from_jj(&packed),crate::JjOperationId(accepted.op_id().hex())).unwrap();
        let store=LocalStore::open(root.join("cas")).unwrap();
        let seed=store.import_tree(&seed_root,CheckpointConsistency::Quiesced).unwrap();store.pin("retained-fixture-seed",&seed).unwrap();
        let shared=if mounted{
            let target=root.join("shared-objects");fs::create_dir(&target).unwrap();
            let from=CString::new(seed_root.join("objects").as_os_str().as_bytes()).unwrap();let to=CString::new(target.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe{libc::mount(from.as_ptr(),to.as_ptr(),std::ptr::null(),libc::MS_BIND,std::ptr::null())},0,"actual bind mount required");
            assert_eq!(unsafe{libc::mount(std::ptr::null(),to.as_ptr(),std::ptr::null(),libc::MS_BIND|libc::MS_REMOUNT|libc::MS_RDONLY,std::ptr::null())},0,"actual RO remount required");target
        }else{seed_root.join("objects")};
        // Private crate-only integrity test seam. Release callers cannot open
        // non-RO templates for this factory. No Root/Native proof is invented.
        let template=VerifiedMetadataTemplate::open_retained_inner(&store,&seed,&seed_root,&shared,mounted).unwrap();
        let data=|label:&str|ObjectRef::from_bytes(ObjectKind::Blob,label.as_bytes());
        let binding=AttemptBinding{format_version:1,project_id:"localization-fixture".into(),owner:OwnerEpoch{owner:"fixture".into(),epoch:1},
            run_id:"run".into(),run_epoch:1,attempt_id:uuid::Uuid::new_v4().to_string(),view_id:"fixture-view".into(),seed,
            baseline:manifest.baseline.clone(),baseline_operation:manifest.operation.clone(),projection_policy:data("policy"),profile:data("profile")};
        let fork=root.join("fork");let mut owner=None;
        let receipt=fork_private_metadata_retaining(&mut owner,&template,&fork,binding.clone(),None,&budget(),&AtomicBool::new(false)).unwrap();
        assert_eq!(receipt.binding,binding);assert_eq!(receipt.kernel_readonly_objects,mounted);assert!(owner.as_ref().unwrap().is_ready());
        let original_mode=fs::metadata(fork.join(".git/config")).unwrap().mode()&0o777;
        drop(workspace);drop(accepted);drop(repo);
        Self{_temp:temp,root,seed_root,fork,shared,owner,proof:None,binding,original_mode}
    }
    fn private_commit(&self,description:&str)->(String,String){
        let workspace=Workspace::load(&settings(),&self.fork,&StoreFactories::default(),&default_working_copy_factories()).unwrap();
        let repo=pollster::block_on(workspace.repo_loader().load_at_head()).unwrap();
        let old=repo.store().get_commit(repo.view().get_wc_commit_id(WorkspaceName::DEFAULT).unwrap()).unwrap();
        let mut tx=repo.start_transaction();let commit=pollster::block_on(tx.repo_mut().new_commit(vec![old.id().clone()],old.tree())
            .set_description(description).write()).unwrap();
        tx.repo_mut().set_wc_commit(WorkspaceName::DEFAULT.to_owned(),commit.id().clone()).unwrap();
        let changed=pollster::block_on(tx.commit(description)).unwrap();update_wc(&workspace,&changed,&commit);
        (commit.id().hex(),changed.op_id().hex())
    }
    fn read_status_history(&self,expected:&str,older:&str){
        let workspace=Workspace::load(&settings(),&self.fork,&StoreFactories::default(),&default_working_copy_factories()).unwrap();
        let repo=pollster::block_on(workspace.repo_loader().load_at_head()).unwrap();
        let wc=repo.store().get_commit(repo.view().get_wc_commit_id(WorkspaceName::DEFAULT).unwrap()).unwrap();
        assert_eq!(wc.id().hex(),expected);
        assert_eq!(crate::JjTreeIdentity::from_jj(workspace.working_copy().tree().unwrap()),crate::JjTreeIdentity::from_jj(&wc.tree()));
        assert_eq!(workspace.working_copy().operation_id(),repo.op_id());
        assert_eq!(repo.store().get_commit(&CommitId::new(hex::decode(older).unwrap())).unwrap().id().hex(),older);
        let git=gix::open(self.fork.join(".git")).unwrap();assert_eq!(git.head().unwrap().id().unwrap().detach().to_string(),PACKED_COMMIT);
        assert_eq!(git.find_object(ObjectId::from_hex(PACKED_BLOB.as_bytes()).unwrap()).unwrap().data,b"packed executable bytes\n");
        let tree=git.find_object(ObjectId::from_hex(PACKED_TREE.as_bytes()).unwrap()).unwrap();
        let modes:Vec<_>=gix::objs::TreeRefIter::from_bytes(&tree.data).map(|e|e.unwrap().mode.kind()).collect();
        assert_eq!(modes,vec![gix::objs::tree::EntryKind::Link,gix::objs::tree::EntryKind::BlobExecutable]);
        assert_eq!(fs::metadata(self.fork.join(".git/config")).unwrap().mode()&0o777,self.original_mode);
    }
    fn localize(&mut self,out:&mut Option<LocalizedMetadataFork>,original:&FinalizationBudget)->Result<()> {
        let expected=self.binding.clone();self.owner.as_mut().unwrap().localize_retaining(out,ForkLocalizationLimits::default(),original,&AtomicBool::new(false),move |actual|{
            ensure!(actual==&expected,"actual fixture binding differs");Ok(())})
    }
}
#[test]
fn packed_old_jj_operations_private_current_and_new_commit_survive_source_removal(){
    let mut f=Fixture::new(false);let (current,operation)=f.private_commit("private before detach");
    let mut out=None;f.localize(&mut out,&budget()).unwrap();let proof=out.unwrap();
    assert_eq!(proof.binding(),&f.binding);assert_eq!(proof.seed(),&f.binding.seed);assert_eq!(proof.original_shared_objects_root(),f.shared);
    assert!(f.owner.as_ref().unwrap().source.is_none());assert!(f.owner.as_ref().unwrap().pending.is_none());
    assert!(!f.fork.join(".git/objects/info/alternates").exists());
    assert!(f.fork.join(format!(".git/objects/pack/pack-{PACK_ID}.pack")).is_file());
    // Removal is TEST-OWNED source teardown, never an API cleanup action.
    fs::remove_dir_all(&f.shared).unwrap();assert!(!f.shared.exists());
    proof.check_current(&budget(),&AtomicBool::new(false)).unwrap();f.read_status_history(&current,PACKED_COMMIT);
    let loader=RepoLoader::init_from_file_system(&settings(),&f.fork.join(".jj/repo"),&StoreFactories::default()).unwrap();
    assert_eq!(pollster::block_on(loader.load_operation(&jj_lib::op_store::OperationId::new(hex::decode(&operation).unwrap()))).unwrap().id().hex(),operation);
    drop(loader);let (new,_) =f.private_commit("new commit after original source is gone");
    f.read_status_history(&new,&current);proof.check_current(&budget(),&AtomicBool::new(false)).unwrap();
}
#[test]
fn foreign_replacement_alternate_is_never_detached_even_with_identical_ready_data(){
    let mut f=Fixture::new(false);let path=f.fork.join(".git/objects/info/alternates");let bytes=fs::read(&path).unwrap();
    fs::rename(&path,path.with_file_name("original-held")).unwrap();fs::write(&path,&bytes).unwrap();
    let mut out=None;assert!(f.localize(&mut out,&budget()).is_err());assert!(out.is_none());assert_eq!(fs::read(path).unwrap(),bytes);
}
#[test]
fn a_well_hashed_but_incomplete_git_closure_cannot_remove_the_original_alternate(){
    let mut f=Fixture::new(false);let git=gix::open(f.fork.join(".git")).unwrap();
    let broken=format!("tree {}\nauthor A <a@invalid.example> 1 +0000\ncommitter A <a@invalid.example> 1 +0000\n\nmissing tree\n","f".repeat(40));
    git.objects.write_stream(Kind::Commit,broken.len() as u64,&mut broken.as_bytes()).unwrap();drop(git);
    let mut out=None;assert!(f.localize(&mut out,&budget()).is_err());assert!(out.is_none());assert!(f.fork.join(".git/objects/info/alternates").exists());
}
#[test]
fn cancellation_and_callback_failure_bind_the_original_attempt_before_io(){
    let mut f=Fixture::new(false);let original=budget();let mut out=None;let cancelled=AtomicBool::new(true);
    assert!(f.owner.as_mut().unwrap().localize_retaining(&mut out,ForkLocalizationLimits::default(),&original,&cancelled,|_|Ok(())).is_err());
    assert!(out.is_none());assert!(f.fork.join(".git/objects/info/alternates").exists());assert!(original.remaining().is_err());
    assert!(f.owner.as_mut().unwrap().reconcile_localization_retaining(&mut out,ForkLocalizationLimits::default(),&original,&AtomicBool::new(false),|_|Ok(())).is_err());
    assert!(f.owner.as_mut().unwrap().localize_retaining(&mut out,ForkLocalizationLimits::default(),&budget(),&AtomicBool::new(false),|_|Ok(())).is_err());
    assert!(f.owner.as_mut().unwrap().reconcile_localization_retaining(&mut out,ForkLocalizationLimits::default(),&budget(),&AtomicBool::new(false),|_|Ok(())).is_err());
    let mut other=Fixture::new(false);let mut none=None;
    assert!(other.owner.as_mut().unwrap().localize_retaining(&mut none,ForkLocalizationLimits::default(),&budget(),&AtomicBool::new(false),|_|anyhow::bail!("actual owner not quiescent")).is_err());
    assert!(none.is_none());assert!(other.fork.join(".git/objects/info/alternates").exists());
}
#[test]
fn a_quiescence_callback_panic_retains_the_original_request_and_explicit_recovery(){
    let mut f=Fixture::new(false);let original=budget();let mut out=None;
    let caught=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||{
        let _=f.owner.as_mut().unwrap().localize_retaining(&mut out,ForkLocalizationLimits::default(),&original,&AtomicBool::new(false),|_|{
            panic!("fixture callback panic after original owner staged")});
    }));
    assert!(caught.is_err());assert!(out.is_none());assert!(!f.owner.as_ref().unwrap().is_ready());
    assert!(f.owner.as_ref().unwrap().source.is_some());assert!(f.owner.as_ref().unwrap().failed);
    f.owner.as_mut().unwrap().reconcile_localization_retaining(&mut out,ForkLocalizationLimits::default(),&original,&AtomicBool::new(false),|_|Ok(())).unwrap();
    out.unwrap().check_current(&budget(),&AtomicBool::new(false)).unwrap();
}
#[test]
fn actual_install_lost_ack_retains_original_temporary_and_requires_explicit_reconcile(){
    let mut f=Fixture::new(false);let original=budget();f.owner.as_mut().unwrap().fault=Some(Fault::AfterRename);let mut out=None;
    assert!(f.localize(&mut out,&original).is_err());assert!(out.is_none());
    assert!(f.owner.as_ref().unwrap().pending.is_some());assert!(f.fork.join(".git/objects/info/alternates").exists());
    f.owner.as_mut().unwrap().reconcile_localization_retaining(&mut out,ForkLocalizationLimits::default(),&original,&AtomicBool::new(false),|_|Ok(())).unwrap();
    out.unwrap().check_current(&budget(),&AtomicBool::new(false)).unwrap();
}
#[test]
fn actual_unlink_lost_ack_is_reconciled_only_from_original_unlinked_inode_and_intent(){
    let mut f=Fixture::new(false);let original=budget();f.owner.as_mut().unwrap().fault=Some(Fault::AfterUnlink);let mut out=None;
    assert!(f.localize(&mut out,&original).is_err());assert!(out.is_none());assert!(!f.fork.join(".git/objects/info/alternates").exists());
    assert_eq!(f.owner.as_ref().unwrap().alternates.as_ref().unwrap().file.metadata().unwrap().nlink(),0);
    assert!(f.owner.as_ref().unwrap().source.is_some());
    f.owner.as_mut().unwrap().reconcile_localization_retaining(&mut out,ForkLocalizationLimits::default(),&original,&AtomicBool::new(false),|_|Ok(())).unwrap();
    assert!(f.owner.as_ref().unwrap().source.is_none());out.unwrap().check_current(&budget(),&AtomicBool::new(false)).unwrap();
}
#[test]
fn cancellation_after_unlink_keeps_unknown_durability_and_source_custody(){
    let mut f=Fixture::new(false);let original=budget();let cancellation=AtomicBool::new(false);let mut out=None;
    // Once the original alternate is unlinked, the callback can fence the
    // operation. No proof is issued until all original directory syncs finish.
    let path=f.fork.join(".git/objects/info/alternates");let seen=AtomicBool::new(false);
    let result=f.owner.as_mut().unwrap().localize_retaining(&mut out,ForkLocalizationLimits::default(),&original,&cancellation,|_|{
        if !path.exists(){seen.store(true,Ordering::Release);original.cancel();}Ok(())});
    assert!(result.is_err());assert!(seen.load(Ordering::Acquire));assert!(out.is_none());
    assert!(f.owner.as_ref().unwrap().source.is_some(),"uncertain durability must keep original source custody");
}
#[test]
fn genuine_ack_is_already_retained_when_the_original_budget_fences_after_ack(){
    let mut f=Fixture::new(false);let original=budget();let mut out=None;
    f.owner.as_mut().unwrap().fault=Some(Fault::FenceAfterAck);
    assert!(f.localize(&mut out,&original).is_err());assert!(original.remaining().is_err());
    assert!(out.is_some(),"completed original unlink/fsync ACK must survive postcheck failure");
    assert!(f.owner.as_ref().unwrap().source.is_none());assert!(f.owner.as_ref().unwrap().localized);
    out.as_ref().unwrap().check_current(&budget(),&AtomicBool::new(false)).unwrap();
    assert!(f.owner.as_mut().unwrap().reconcile_localization_retaining(&mut out,ForkLocalizationLimits::default(),&budget(),&AtomicBool::new(false),|_|Ok(())).is_err());
}
#[test]
fn byte_limits_and_changed_history_during_copy_preserve_the_original_link(){
    let mut f=Fixture::new(false);let mut out=None;let limits=ForkLocalizationLimits{max_copy_bytes:1,..Default::default()};
    assert!(f.owner.as_mut().unwrap().localize_retaining(&mut out,limits,&budget(),&AtomicBool::new(false),|_|Ok(())).is_err());
    assert!(out.is_none());assert!(f.fork.join(".git/objects/info/alternates").exists());
    let mut other=Fixture::new(false);let mut result=None;let original=budget();let path=other.fork.join(".git/HEAD");let old=fs::read(&path).unwrap();let mut changed=false;
    let owner=other.owner.as_mut().unwrap();
    assert!(owner.localize_retaining(&mut result,ForkLocalizationLimits::default(),&original,&AtomicBool::new(false),|_|{
        if !changed&&path.parent().unwrap().join("objects/pack").exists(){fs::write(&path,b"ref: refs/heads/foreign\n")?;changed=true;}Ok(())}).is_err());
    assert!(result.is_none());assert!(other.fork.join(".git/objects/info/alternates").exists());assert_ne!(fs::read(path).unwrap(),old);
}
#[test]
fn an_existing_private_hardlink_to_the_shared_seed_is_not_independent_storage(){
    let mut f=Fixture::new(false);let target=f.fork.join(".git/objects/pack");
    fs::create_dir(&target).unwrap();fs::set_permissions(&target,fs::Permissions::from_mode(0o700)).unwrap();
    let name=format!("pack-{PACK_ID}.pack");fs::hard_link(f.shared.join("pack").join(&name),target.join(&name)).unwrap();
    let mut out=None;assert!(f.localize(&mut out,&budget()).is_err());assert!(out.is_none());
    assert!(f.fork.join(".git/objects/info/alternates").exists());
}
#[test]
fn local_proof_cannot_survive_a_new_alternate_or_object_substitution(){
    let mut f=Fixture::new(false);let mut out=None;f.localize(&mut out,&budget()).unwrap();let proof=out.unwrap();
    let path=f.fork.join(".git/objects/info/alternates");fs::write(&path,b"/foreign/objects\n").unwrap();
    assert!(proof.check_current(&budget(),&AtomicBool::new(false)).is_err());fs::remove_file(path).unwrap();
    let path=f.fork.join(format!(".git/objects/pack/pack-{PACK_ID}.pack"));let replacement=path.with_extension("foreign");
    fs::write(&replacement,fs::read(&path).unwrap()).unwrap();fs::rename(replacement,path).unwrap();
    assert!(proof.check_current(&budget(),&AtomicBool::new(false)).is_err());
}
#[test]
#[ignore="root CAP_SYS_ADMIN; disposable namespace only; real normal unmount/history fixture"]
fn actual_ro_mount_can_normally_unmount_before_git_jj_status_history_and_new_commit(){
    assert_eq!(unsafe{libc::geteuid()},0);assert_eq!(std::env::var("RUDDER_VCS_LOCALIZATION_MOUNT_FIXTURE").as_deref(),Ok("1"));
    assert_eq!(unsafe{libc::unshare(libc::CLONE_NEWNS)},0,"actual private mount namespace required");
    let slash=CString::new("/").unwrap();assert_eq!(unsafe{libc::mount(std::ptr::null(),slash.as_ptr(),std::ptr::null(),libc::MS_REC|libc::MS_PRIVATE,std::ptr::null())},0);
    let mut f=Box::new(Fixture::new(true));
    let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||{
        let (current,_)=f.private_commit("private before real normal unmount");
        let expected=f.binding.clone();let original=budget();
        {let value=&mut *f;value.owner.as_mut().unwrap().localize_retaining(&mut value.proof,ForkLocalizationLimits::default(),&original,&AtomicBool::new(false),|actual|{
            ensure!(actual==&expected,"actual original fixture differs");Ok(())}).unwrap();}
        let proof=f.proof.as_ref().unwrap();
        assert!(proof.kernel_readonly_objects());let shared=CString::new(f.shared.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe{libc::umount2(shared.as_ptr(),0)},0,"normal unmount; never lazy/force");fs::remove_dir(&f.shared).unwrap();
        proof.check_current(&budget(),&AtomicBool::new(false)).unwrap();f.read_status_history(&current,PACKED_COMMIT);
        let (next,_)=f.private_commit("new after actual mount deletion");f.read_status_history(&next,&current);
        proof.check_current(&budget(),&AtomicBool::new(false)).unwrap();
    }));
    if result.is_err(){let _retained=Box::leak(f);panic!("actual fixture mount/fork carrier retained; no normal cleanup claim");}
}
