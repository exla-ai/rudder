//! Tests the anchored cleanup mechanism, not a fake startup/join capability.
//! Actual completed-probe cleanup is required by the separate ignored CLI test.
use super::*;
use std::os::unix::fs::DirBuilderExt;
struct TestDirectory(PathBuf);
impl TestDirectory {
    fn path(&self)->&Path {&self.0}
    fn create()->Result<Self> {
        let parent=if Path::new("/workspace").is_dir(){PathBuf::from("/workspace")}else{std::env::temp_dir()};
        let path=parent.join(format!("rudder-kernel-retirement-test-{}",uuid::Uuid::new_v4()));
        fs::DirBuilder::new().mode(0o700).create(&path)?;Ok(Self(path))
    }
}
impl Drop for TestDirectory {fn drop(&mut self){let _=fs::remove_dir_all(&self.0);}}
fn fixture()->Result<(TestDirectory,DirectoryAnchor)> {
    let base=TestDirectory::create()?;
    let own=base.path().join("original-private-fixture");fs::create_dir(&own)?;fs::set_permissions(&own,fs::Permissions::from_mode(0o700))?;
    let parent=open_original_directory(base.path())?;let file=open_original_directory(&own)?;
    Ok((base,DirectoryAnchor::capture(parent,file,b"original-private-fixture".to_vec(),Role::Root)?))
}
fn budget()->Result<FinalizationBudget>{FinalizationBudget::begin(Duration::from_secs(30))}
#[test]
fn provider_kernel_retirement_removes_only_anchored_original_tree_and_fsyncs_parent()->Result<()> {
    let (base,anchor)=fixture()?;let own=base.path().join("original-private-fixture");
    fs::create_dir(own.join("child"))?;fs::set_permissions(own.join("child"),fs::Permissions::from_mode(0o700))?;
    fs::write(own.join("child/own"),b"fixture-only")?;fs::set_permissions(own.join("child/own"),fs::Permissions::from_mode(0o444))?;
    fs::write(base.path().join("protected-sibling"),b"keep exact sibling")?;
    let original=budget()?;let cancel=AtomicBool::new(false);let mut tree=Tree::capture(&anchor,&original,&cancel)?;
    tree.remove(&anchor,&original,&cancel)?;
    assert!(tree.root_attempted&&tree.root_removed&&tree.synced&&tree.entries.iter().all(|e|e.attempted&&e.removed));
    assert_eq!(anchor.file.metadata()?.nlink(),0);assert!(!own.exists());
    assert_eq!(fs::read(base.path().join("protected-sibling"))?,b"keep exact sibling");Ok(())
}
#[test]
fn provider_kernel_retirement_replacement_keeps_original_fd_and_refuses_both_trees()->Result<()> {
    let (base,anchor)=fixture()?;let own=base.path().join("original-private-fixture");
    fs::write(own.join("original"),b"original")?;fs::rename(&own,base.path().join("moved-original"))?;
    fs::create_dir(&own)?;fs::set_permissions(&own,fs::Permissions::from_mode(0o700))?;fs::write(own.join("replacement"),b"replacement")?;
    assert!(Tree::capture(&anchor,&budget()?,&AtomicBool::new(false)).is_err());
    assert!(anchor.file.metadata()?.nlink()>0);
    assert_eq!(fs::read(base.path().join("moved-original/original"))?,b"original");
    assert_eq!(fs::read(own.join("replacement"))?,b"replacement");Ok(())
}
#[test]
fn provider_kernel_retirement_refuses_foreign_symlink_and_hardlink_without_deletion()->Result<()> {
    for symlink in [true,false] {
        let (base,anchor)=fixture()?;let own=base.path().join("original-private-fixture");let foreign=base.path().join("foreign");
        fs::write(&foreign,b"never touch")?;fs::set_permissions(&foreign,fs::Permissions::from_mode(0o600))?;
        if symlink { std::os::unix::fs::symlink(&foreign,own.join("foreign-alias"))?; }
        else { fs::hard_link(&foreign,own.join("foreign-alias"))?; }
        assert!(Tree::capture(&anchor,&budget()?,&AtomicBool::new(false)).is_err());
        assert!(fs::symlink_metadata(own.join("foreign-alias")).is_ok());assert_eq!(fs::read(&foreign)?,b"never touch");
    }Ok(())
}
#[test]
fn provider_kernel_retirement_parent_sync_failure_keeps_actual_unlink_ack_without_success()->Result<()> {
    let (base,anchor)=fixture()?;let original=budget()?;let cancel=AtomicBool::new(false);
    let mut tree=Tree::capture(&anchor,&original,&cancel)?;tree.fail_parent_sync=true;
    assert!(tree.remove(&anchor,&original,&cancel).is_err());
    assert!(tree.root_attempted&&tree.root_removed&&!tree.synced);assert_eq!(anchor.file.metadata()?.nlink(),0);
    assert!(!base.path().join("original-private-fixture").exists());
    assert!(tree.remove(&anchor,&original,&cancel).is_err(),"absence is not a new cleanup ACK or permission to replay");Ok(())
}
#[test]
fn provider_kernel_retirement_cancelled_original_budget_prevents_unlink()->Result<()> {
    let (base,anchor)=fixture()?;let original=budget()?;let cancel=AtomicBool::new(false);
    let mut tree=Tree::capture(&anchor,&original,&cancel)?;cancel.store(true,Ordering::Release);
    assert!(tree.remove(&anchor,&original,&cancel).is_err());assert!(!tree.root_attempted&&!tree.root_removed);
    assert!(base.path().join("original-private-fixture").is_dir());Ok(())
}
#[test]
fn provider_kernel_retirement_does_not_remove_a_live_or_replaced_broker_lock()->Result<()> {
    let (base,_anchor)=fixture()?;let lock_path=base.path().join("owned.lock");
    let file=OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&lock_path)?;file.try_lock_exclusive()?;
    let parent=open_original_directory(base.path())?;let original=open_beneath(&parent,b"owned.lock",false)?;
    let anchor=FileAnchor::capture(parent,original,b"owned.lock".to_vec())?;
    assert!(anchor.lock_original().is_err(),"a close flag cannot substitute for original acceptor join/lock release");
    drop(file);let actual=anchor.lock_original()?;
    fs::rename(&lock_path,base.path().join("moved.lock"))?;
    let _replacement=OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&lock_path)?;
    assert!(anchor.check().is_err());assert!(actual.metadata()?.nlink()>0);assert!(lock_path.is_file());Ok(())
}
#[test]
fn provider_kernel_retirement_receipt_is_send_sync_without_serde_constructor(){fn shared<T:Send+Sync+'static>(){}shared::<KernelProbeFixtureRetired>();}
