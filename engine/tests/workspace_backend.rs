#![cfg(unix)]
use rudder_engine::{
    protocol::{CheckpointConsistency, ObjectRef, OwnerEpoch, FORMAT_VERSION},
    store::{CheckpointV2Manifest, EnvironmentManifest, LocalStore, SourceManifest},
    workspace::{
        CreateRequest, PrepareRequest, PreparedWorkspace, WorkspaceBackend, WorkspaceManager,
        WorkspaceProfile, WorkspaceStatus,
    },
};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};
use tempfile::TempDir;
use uuid::Uuid;

fn id() -> String {
    Uuid::new_v4().to_string()
}
fn owner() -> OwnerEpoch {
    OwnerEpoch {
        owner: "engine-owner".into(),
        epoch: 7,
    }
}
fn partition(value: &str) -> Vec<Vec<u8>> {
    value
        .split('/')
        .map(|component| component.as_bytes().to_vec())
        .collect()
}
fn profile() -> WorkspaceProfile {
    WorkspaceProfile {
        format_version: FORMAT_VERSION,
        prepared: vec![partition("node_modules")],
        scratch: vec![partition("target")],
        vcs_local: vec![partition(".git"), partition(".jj")],
    }
}
fn write(path: impl AsRef<Path>, bytes: &[u8]) {
    fs::create_dir_all(path.as_ref().parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

struct Fixture {
    _temp: TempDir,
    store: LocalStore,
    manager: WorkspaceManager,
    source: ObjectRef,
    environment: ObjectRef,
    input: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        Self::at(None)
    }
    fn at(root: Option<&Path>) -> Self {
        let temp = root
            .map(|root| {
                tempfile::Builder::new()
                    .prefix("rudder-workspace-test-")
                    .tempdir_in(root)
                    .unwrap()
            })
            .unwrap_or_else(|| tempfile::tempdir().unwrap());
        let input = temp.path().join("input");
        write(input.join("source"), b"base source");
        write(input.join("mapped"), &vec![b'A'; 4096]);
        write(input.join("deleted"), b"remove this");
        write(input.join("opaque/old"), b"hidden later");
        write(input.join("rename-me/leaf"), b"rename data");
        write(input.join(".git/config"), b"foreign recovery metadata");
        let store = LocalStore::open(temp.path().join("cas")).unwrap();
        let directory = store
            .import_tree(&input, CheckpointConsistency::Quiesced)
            .unwrap();
        let source = store
            .put_source(&SourceManifest {
                format_version: FORMAT_VERSION,
                directory,
                parents: vec![],
                consistency: CheckpointConsistency::Quiesced,
            })
            .unwrap();
        store.pin("fixture-source", &source).unwrap();
        let env_input = temp.path().join("environment");
        write(
            env_input.join("node_modules/package/index"),
            b"prepared dependency",
        );
        let env_dir = store
            .import_tree(&env_input, CheckpointConsistency::Quiesced)
            .unwrap();
        let environment = store
            .put_environment(&EnvironmentManifest {
                format_version: FORMAT_VERSION,
                directory: env_dir,
                platform: BTreeMap::from([
                    ("os".into(), std::env::consts::OS.into()),
                    ("arch".into(), std::env::consts::ARCH.into()),
                ]),
                installer_inputs: vec![],
                provenance: BTreeMap::new(),
            })
            .unwrap();
        store.pin("fixture-environment", &environment).unwrap();
        let manager = WorkspaceManager::open(temp.path().join("manager"), store.clone()).unwrap();
        Self {
            _temp: temp,
            store,
            manager,
            source,
            environment,
            input,
        }
    }
    fn prepare(&self, backend: WorkspaceBackend) -> PreparedWorkspace {
        self.manager
            .prepare(PrepareRequest {
                source: self.source.clone(),
                environment: Some(self.environment.clone()),
                profile: profile(),
                backend,
            })
            .unwrap()
    }
    fn create(&self, prepared: &PreparedWorkspace) -> CreateRequest {
        let request = CreateRequest {
            view_id: id(),
            operation_id: id(),
            prepared_id: prepared.prepared_id.clone(),
            owner: owner(),
        };
        self.manager.create(request.clone()).unwrap();
        request
    }
}

#[test]
fn creation_is_idempotent_fenced_and_does_not_enumerate_prepared_file_contents() {
    let fixture = Fixture::new();
    let prepared = fixture.prepare(WorkspaceBackend::EagerCopy);
    fs::set_permissions(&prepared.lower, fs::Permissions::from_mode(0)).unwrap();
    let request = fixture.create(&prepared);
    fs::set_permissions(&prepared.lower, fs::Permissions::from_mode(0o755)).unwrap();
    let first = fixture
        .manager
        .descriptor(&request.view_id, &owner())
        .unwrap();
    assert_eq!(
        fixture.manager.create(request.clone()).unwrap().revision,
        first.revision
    );
    assert_eq!(first.status, WorkspaceStatus::Created);
    assert_eq!(
        fs::read_dir(fixture.manager.execution_path(&request.view_id).unwrap())
            .unwrap()
            .count(),
        0
    );
    let mut wrong = request.clone();
    wrong.owner.epoch += 1;
    assert!(fixture.manager.create(wrong).is_err());
    let mut conflict = request.clone();
    conflict.operation_id = id();
    assert!(fixture.manager.create(conflict).is_err());
    assert!(fixture
        .manager
        .descriptor(
            &request.view_id,
            &OwnerEpoch {
                owner: "somebody-else".into(),
                epoch: 7
            }
        )
        .is_err());
    let next = OwnerEpoch {
        owner: "next-engine".into(),
        epoch: 8,
    };
    let operation = id();
    let reassigned = fixture
        .manager
        .reassign_owner(&request.view_id, &owner(), next.clone(), &operation)
        .unwrap();
    assert_eq!(
        fixture
            .manager
            .reassign_owner(&request.view_id, &owner(), next.clone(), &operation)
            .unwrap()
            .revision,
        reassigned.revision
    );
    assert!(fixture
        .manager
        .descriptor(&request.view_id, &owner())
        .is_err());
    assert_eq!(
        fixture
            .manager
            .descriptor(&request.view_id, &next)
            .unwrap()
            .owner,
        next
    );
}

#[test]
fn eager_private_materialization_never_changes_parent_sibling_or_prepared_cache() {
    let fixture = Fixture::new();
    let prepared = fixture.prepare(WorkspaceBackend::EagerCopy);
    let a = fixture.create(&prepared);
    let b = fixture.create(&prepared);
    let a_path = fixture.manager.materialize(&a.view_id, &owner()).unwrap();
    let b_path = fixture.manager.materialize(&b.view_id, &owner()).unwrap();
    assert!(a_path.inherited_bytes_copied && a_path.entry_count_work && a_path.vcs_remint_required);
    assert!(!a_path.path.join(".git").exists());
    write(a_path.path.join("source"), b"view a");
    write(
        a_path.path.join("node_modules/package/index"),
        b"private dependency",
    );
    assert_eq!(
        fs::read(b_path.path.join("source")).unwrap(),
        b"base source"
    );
    assert_eq!(
        fs::read(prepared.lower.join("node_modules/package/index")).unwrap(),
        b"prepared dependency"
    );
    assert_eq!(
        fs::read(fixture.input.join("source")).unwrap(),
        b"base source"
    );
    assert_eq!(
        fs::read(fixture.input.join(".git/config")).unwrap(),
        b"foreign recovery metadata"
    );
}

#[test]
fn changed_source_reuses_the_same_retained_environment_layer_without_recopying_dependencies() {
    let fixture = Fixture::new();
    let first = fixture.prepare(WorkspaceBackend::LinuxOverlay);
    assert_eq!(first.lower_layers.len(), 2);
    let mut source: SourceManifest =
        serde_json::from_slice(&fixture.store.read_object(&fixture.source).unwrap()).unwrap();
    let object = fixture.store.put_blob(b"changed source").unwrap();
    source.directory = fixture
        .store
        .update_path(
            &source.directory,
            &[b"source".to_vec()],
            Some(
                rudder_engine::store::DirectoryEntry::new(
                    b"source",
                    rudder_engine::store::EntryNode::File {
                        object,
                        mode: 0o644,
                    },
                )
                .unwrap(),
            ),
        )
        .unwrap();
    source.parents = vec![fixture.source.clone()];
    let source = fixture.store.put_source(&source).unwrap();
    let second = fixture
        .manager
        .prepare(PrepareRequest {
            source,
            environment: Some(fixture.environment.clone()),
            profile: profile(),
            backend: WorkspaceBackend::LinuxOverlay,
        })
        .unwrap();
    assert_ne!(first.lower_layers[0], second.lower_layers[0]);
    assert_eq!(first.lower_layers[1], second.lower_layers[1]);
    assert_eq!(
        fs::read(second.lower_layers[1].join("node_modules/package/index")).unwrap(),
        b"prepared dependency"
    );
    assert_eq!(
        fs::read(second.lower_layers[0].join("source")).unwrap(),
        b"changed source"
    );
}

#[test]
fn v2_checkpoint_separates_source_and_preserves_full_dirty_state_for_restore() {
    let fixture = Fixture::new();
    let prepared = fixture.prepare(WorkspaceBackend::EagerCopy);
    let view = fixture.create(&prepared);
    let materialized = fixture
        .manager
        .materialize(&view.view_id, &owner())
        .unwrap();
    write(materialized.path.join("source"), b"new source");
    write(
        materialized.path.join("node_modules/package/index"),
        b"private prepared mutation",
    );
    write(materialized.path.join("target/build"), b"private scratch");
    write(
        materialized.path.join(".git/index"),
        b"private VCS recovery data",
    );
    let operation = id();
    assert!(fixture
        .manager
        .checkpoint_quiesced(&view.view_id, &owner(), &operation, false, false, 42)
        .is_err());
    let receipt = fixture
        .manager
        .checkpoint_quiesced(&view.view_id, &owner(), &operation, false, true, 42)
        .unwrap();
    let repeated = fixture
        .manager
        .checkpoint_quiesced(&view.view_id, &owner(), &operation, false, true, 42)
        .unwrap();
    assert_eq!(receipt.checkpoint, repeated.checkpoint);
    assert!(fixture
        .manager
        .checkpoint_quiesced(&view.view_id, &owner(), &operation, false, true, 43)
        .is_err());
    let checkpoint: CheckpointV2Manifest = serde_json::from_slice(
        &fixture
            .store
            .read_object(receipt.checkpoint.as_ref().unwrap())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(checkpoint.format_version, 2);
    assert_eq!(checkpoint.knowledge_cursor, 42);
    let source: SourceManifest =
        serde_json::from_slice(&fixture.store.read_object(&checkpoint.source).unwrap()).unwrap();
    let output = fixture._temp.path().join("source-output");
    fixture
        .store
        .export_tree(&source.directory, &output)
        .unwrap();
    assert_eq!(fs::read(output.join("source")).unwrap(), b"new source");
    for excluded in ["node_modules", "target", ".git", ".jj"] {
        assert!(!output.join(excluded).exists());
    }
    let full = fixture._temp.path().join("full-recovery");
    fixture
        .store
        .export_tree(&checkpoint.view_root, &full)
        .unwrap();
    assert_eq!(
        fs::read(full.join(".git/index")).unwrap(),
        b"private VCS recovery data"
    );
    let restored = fixture
        .manager
        .prepare_checkpoint(
            receipt.checkpoint.as_ref().unwrap(),
            WorkspaceBackend::EagerCopy,
        )
        .unwrap();
    let restored_view = fixture.create(&restored);
    let restored_tree = fixture
        .manager
        .materialize(&restored_view.view_id, &owner())
        .unwrap();
    assert!(restored_tree.vcs_remint_required);
    assert!(!restored_tree.path.join(".git").exists());
    assert_eq!(
        fs::read(restored_tree.path.join("target/build")).unwrap(),
        b"private scratch"
    );
    assert_eq!(
        fs::read(restored_tree.path.join("node_modules/package/index")).unwrap(),
        b"private prepared mutation"
    );
}

#[test]
fn seal_and_destroy_are_idempotent_and_retain_checkpoint_closure() {
    let fixture = Fixture::new();
    let prepared = fixture.prepare(WorkspaceBackend::EagerCopy);
    let view = fixture.create(&prepared);
    let path = fixture
        .manager
        .materialize(&view.view_id, &owner())
        .unwrap()
        .path;
    write(path.join("source"), b"sealed");
    let seal_id = id();
    let seal = fixture
        .manager
        .checkpoint_quiesced(&view.view_id, &owner(), &seal_id, true, true, 7)
        .unwrap();
    assert_eq!(
        fixture
            .manager
            .checkpoint_quiesced(&view.view_id, &owner(), &seal_id, true, true, 7)
            .unwrap()
            .checkpoint,
        seal.checkpoint
    );
    assert!(fixture
        .manager
        .materialize(&view.view_id, &owner())
        .is_err());
    let destroy_id = id();
    let destroyed = fixture
        .manager
        .destroy(&view.view_id, &owner(), &destroy_id)
        .unwrap();
    assert_eq!(destroyed.checkpoint, seal.checkpoint);
    assert_eq!(
        fixture
            .manager
            .destroy(&view.view_id, &owner(), &destroy_id)
            .unwrap()
            .revision,
        destroyed.revision
    );
    fixture.store.gc(Duration::ZERO).unwrap();
    assert!(fixture
        .store
        .closure(seal.checkpoint.as_ref().unwrap())
        .is_ok());
    assert!(!fixture
        .manager
        .view_path(&view.view_id)
        .unwrap()
        .join("private")
        .exists());
    assert_eq!(
        fixture
            .manager
            .descriptor(&view.view_id, &owner())
            .unwrap()
            .status,
        WorkspaceStatus::Destroyed
    );
}

#[test]
fn manager_and_cleanup_reject_checkout_paths_symlinks_and_owner_mismatches() {
    let fixture = Fixture::new();
    assert!(WorkspaceManager::open(&fixture.input, fixture.store.clone()).is_err());
    assert!(fixture.input.join("source").exists());
    assert!(!fixture.input.join("manager.json").exists());
    let prepared = fixture.prepare(WorkspaceBackend::EagerCopy);
    let view = fixture.create(&prepared);
    let private = fixture
        .manager
        .view_path(&view.view_id)
        .unwrap()
        .join("private");
    fs::remove_dir_all(&private).unwrap();
    symlink(&fixture.input, &private).unwrap();
    assert!(fixture
        .manager
        .destroy(&view.view_id, &owner(), &id())
        .is_err());
    assert!(fixture.input.join("source").exists());
    let wrong = OwnerEpoch {
        owner: owner().owner,
        epoch: 99,
    };
    assert!(fixture.manager.materialize(&view.view_id, &wrong).is_err());
    assert!(fixture.manager.view_path("../input").is_err());
}

#[test]
fn profile_partition_conflicts_and_foreign_platform_environments_fail_explicitly() {
    let fixture = Fixture::new();
    let mut overlapping = profile();
    overlapping.scratch.push(partition("node_modules/cache"));
    assert!(overlapping.validate().is_err());
    let mut unsafe_profile = profile();
    unsafe_profile.prepared.push(vec![b"..".to_vec()]);
    assert!(unsafe_profile.validate().is_err());
    let mut environment: EnvironmentManifest =
        serde_json::from_slice(&fixture.store.read_object(&fixture.environment).unwrap()).unwrap();
    environment.platform.insert("os".into(), "other-os".into());
    let foreign = fixture.store.put_environment(&environment).unwrap();
    assert!(fixture
        .manager
        .prepare(PrepareRequest {
            source: fixture.source.clone(),
            environment: Some(foreign),
            profile: profile(),
            backend: WorkspaceBackend::EagerCopy
        })
        .is_err());
    let empty_profile = WorkspaceProfile::source_only();
    assert!(fixture
        .manager
        .prepare(PrepareRequest {
            source: fixture.source.clone(),
            environment: Some(fixture.environment.clone()),
            profile: empty_profile,
            backend: WorkspaceBackend::EagerCopy
        })
        .is_err());
}

#[test]
fn absolute_and_escaping_prepared_symlinks_fail_before_execution() {
    let fixture = Fixture::new();
    symlink("/some/foreign/checkout", fixture.input.join("absolute")).unwrap();
    let directory = fixture
        .store
        .import_tree(&fixture.input, CheckpointConsistency::Quiesced)
        .unwrap();
    let source = fixture
        .store
        .put_source(&SourceManifest {
            format_version: FORMAT_VERSION,
            directory,
            parents: vec![],
            consistency: CheckpointConsistency::Quiesced,
        })
        .unwrap();
    assert!(fixture
        .manager
        .prepare(PrepareRequest {
            source,
            environment: None,
            profile: profile(),
            backend: WorkspaceBackend::EagerCopy
        })
        .is_err());
}

#[cfg(not(target_os = "macos"))]
#[test]
fn apfs_is_not_silently_emulated_on_other_hosts() {
    let fixture = Fixture::new();
    let prepared = fixture.prepare(WorkspaceBackend::MacApfsClone);
    let view = fixture.create(&prepared);
    assert!(fixture
        .manager
        .materialize(&view.view_id, &owner())
        .is_err());
    assert_eq!(
        fixture
            .manager
            .descriptor(&view.view_id, &owner())
            .unwrap()
            .status,
        WorkspaceStatus::Created
    );
}

#[cfg(target_os = "macos")]
#[test]
fn native_apfs_clone_is_private_and_has_explicit_entry_work() {
    let fixture = Fixture::new();
    let prepared = fixture.prepare(WorkspaceBackend::MacApfsClone);
    let view = fixture.create(&prepared);
    let report = fixture
        .manager
        .materialize(&view.view_id, &owner())
        .unwrap();
    assert!(!report.inherited_bytes_copied && report.entry_count_work);
    write(report.path.join("source"), b"clone mutation");
    assert_eq!(
        fs::read(prepared.lower.join("source")).unwrap(),
        b"base source"
    );
}

#[cfg(target_os = "linux")]
mod namespaces {
    use super::*;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::{
        process::{Command, Stdio},
        time::Instant,
    };

    // A tiny executable linked to the REAL engine API gives namespace setup a
    // genuinely fresh single-threaded process (Rust's test harness has threads).
    fn helper(root: &Path) -> PathBuf {
        let source = root.join("helper.rs");
        let binary = root.join("helper");
        fs::write(&source, r#"
use std::{fs,io::Write,os::fd::AsRawFd,os::unix::{fs::{PermissionsExt,symlink},process::CommandExt},process::{Command,Stdio},time::Duration};
fn uring(){unsafe{
 #[repr(C)]#[derive(Default)]struct Sq{head:u32,tail:u32,mask:u32,entries:u32,flags:u32,dropped:u32,array:u32,reserved:u32,address:u64}
 #[repr(C)]#[derive(Default)]struct Cq{head:u32,tail:u32,mask:u32,entries:u32,overflow:u32,cqes:u32,flags:u32,reserved:u32,address:u64}
 #[repr(C)]#[derive(Default)]struct Params{sq_entries:u32,cq_entries:u32,flags:u32,cpu:u32,idle:u32,features:u32,wq_fd:u32,reserved:[u32;3],sq:Sq,cq:Cq}
 let mut p=Params::default();let ring=libc::syscall(libc::SYS_io_uring_setup,8,&mut p) as i32;if ring<0{fs::write("uring-unavailable",std::io::Error::last_os_error().to_string()).unwrap();return}
 let size=p.sq.array as usize+p.sq_entries as usize*4;let sq=libc::mmap(std::ptr::null_mut(),size,libc::PROT_READ|libc::PROT_WRITE,libc::MAP_SHARED,ring,0);
 let sqes=libc::mmap(std::ptr::null_mut(),p.sq_entries as usize*64,libc::PROT_READ|libc::PROT_WRITE,libc::MAP_SHARED,ring,0x10000000);assert_ne!(sq,libc::MAP_FAILED);assert_ne!(sqes,libc::MAP_FAILED);
 let output=fs::OpenOptions::new().create(true).read(true).write(true).truncate(true).open("uring-output").unwrap();let buffer=vec![b'U';4096];let sqe=sqes as *mut u8;
 std::ptr::write(sqe,23);std::ptr::write(sqe.add(1),1<<4);std::ptr::write_unaligned(sqe.add(4) as *mut i32,output.as_raw_fd());std::ptr::write_unaligned(sqe.add(16) as *mut u64,buffer.as_ptr() as u64);std::ptr::write_unaligned(sqe.add(24) as *mut u32,buffer.len() as u32);
 std::ptr::write_volatile((sq as *mut u8).add(p.sq.array as usize) as *mut u32,0);std::sync::atomic::fence(std::sync::atomic::Ordering::Release);std::ptr::write_volatile((sq as *mut u8).add(p.sq.tail as usize) as *mut u32,1);
 assert_eq!(libc::syscall(libc::SYS_io_uring_enter,ring,1,0,0,std::ptr::null::<u8>(),0),1);fs::write("uring-submitted",b"submitted async write; no userspace completion wait").unwrap();
 std::mem::forget(buffer);std::mem::forget(output); // Kernel cancellation/drain owns exit cleanup.
}}
fn writer(hold:bool){
 fs::write("atomic.tmp",b"edited source").unwrap();fs::rename("atomic.tmp","source").unwrap();
 let file=fs::OpenOptions::new().read(true).write(true).open("mapped").unwrap();unsafe{let p=libc::mmap(std::ptr::null_mut(),4096,libc::PROT_READ|libc::PROT_WRITE,libc::MAP_SHARED,file.as_raw_fd(),0);assert_ne!(p,libc::MAP_FAILED);*(p as *mut u8)=b'Z';assert_eq!(libc::msync(p,4096,libc::MS_SYNC),0);libc::munmap(p,4096);}
 fs::remove_file("deleted").unwrap();fs::remove_dir_all("opaque").unwrap();fs::create_dir("opaque").unwrap();fs::write("opaque/new",b"new").unwrap();
 assert_eq!(fs::rename("rename-me","renamed-lower").unwrap_err().raw_os_error(),Some(libc::EXDEV));fs::create_dir("upper-created").unwrap();fs::write("upper-created/leaf",b"rename data").unwrap();fs::rename("upper-created","renamed").unwrap();symlink("source","link").unwrap();fs::set_permissions("source",fs::Permissions::from_mode(0o751)).unwrap();
 fs::write("node_modules/package/index",b"dirty prepared").unwrap();fs::create_dir_all("target").unwrap();fs::write("target/build",b"dirty scratch").unwrap();fs::create_dir_all(".git").unwrap();fs::write(".git/index",b"recovery-only").unwrap();
 uring();
 let mut cmd=Command::new(std::env::current_exe().unwrap());cmd.arg("background").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());unsafe{cmd.pre_exec(||{if libc::setsid()<0{return Err(std::io::Error::last_os_error())}Ok(())});}let _background=cmd.spawn().unwrap();
 while !std::path::Path::new("background-ready").exists(){std::thread::sleep(Duration::from_millis(5));}
 if hold{fs::write("writer-ready",b"ready").unwrap();loop{std::thread::sleep(Duration::from_millis(10));}}
}
fn main(){let args:Vec<_>=std::env::args().collect();if args[1]=="writer"{writer(args.get(2).is_some_and(|s|s=="hold"));return}if args[1]=="background"{let mut f=fs::OpenOptions::new().create(true).append(true).open("background.log").unwrap();fs::write("background-ready",b"ready").unwrap();loop{f.write_all(b"x").unwrap();f.sync_data().unwrap();std::thread::sleep(Duration::from_millis(2));}}
 if args[1]=="tty"{use std::io::Read;unsafe{let mut t:libc::termios=std::mem::zeroed();assert_eq!(libc::tcgetattr(0,&mut t),0);libc::cfmakeraw(&mut t);assert_eq!(libc::tcsetattr(0,libc::TCSANOW,&t),0);}print!("PTY-READY");std::io::stdout().flush().unwrap();let mut b=[0u8;39];std::io::stdin().read_exact(&mut b).unwrap();std::io::stdout().write_all(&b).unwrap();std::io::stdout().flush().unwrap();return}
 if args[1]=="tty-signal"{print!("PTY-SIGNAL-READY");std::io::stdout().flush().unwrap();loop{std::thread::sleep(Duration::from_millis(20));}}
 let (code,report)=rudder_engine::workspace::linux::run_helper_report(&fs::read(&args[1]).unwrap());fs::write(&args[2],report).unwrap();std::process::exit(code);}
"#).unwrap();
        let dependencies = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let library = |name: &str| {
            let mut candidates: Vec<_> = fs::read_dir(&dependencies)
                .unwrap()
                .filter_map(|entry| entry.ok())
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(&format!("lib{name}-"))
                        && entry
                            .path()
                            .extension()
                            .is_some_and(|extension| extension == "rlib")
                })
                .collect();
            candidates.sort_by_key(|entry| entry.metadata().unwrap().modified().unwrap());
            candidates.pop().unwrap().path()
        };
        let output = Command::new("rustc")
            .arg("--edition=2021")
            .arg(&source)
            .arg("--extern")
            .arg(format!(
                "rudder_engine={}",
                library("rudder_engine").display()
            ))
            .arg("--extern")
            .arg(format!("libc={}", library("libc").display()))
            .arg("-L")
            .arg(format!("dependency={}", dependencies.display()))
            .arg("-o")
            .arg(&binary)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        binary
    }
    fn wait(child: &mut std::process::Child) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                return status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                panic!("helper exceeded isolated test deadline");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn result(path: &Path) -> serde_json::Value {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn linux_overlay_copy_up_whiteout_opaque_mmap_rename_and_remount_are_qualified_in_fresh_helper()
    {
        if !Path::new("/workspace").is_dir() {
            eprintln!("Linux overlay qualification unavailable: no dedicated /workspace volume");
            return;
        }
        let fixture = Fixture::at(Some(Path::new("/workspace")));
        let binary = helper(fixture._temp.path());
        let prepared = fixture.prepare(WorkspaceBackend::LinuxOverlay);
        let view = fixture.create(&prepared);
        let host_namespace = fs::read_link("/proc/self/ns/mnt").unwrap();
        let spec = fixture
            .manager
            .helper_spec(
                &view.view_id,
                &owner(),
                vec![binary.to_string_lossy().into_owned(), "writer".into()],
                None,
                true,
            )
            .unwrap();
        let spec_file = fixture._temp.path().join("spec.json");
        let result_file = fixture._temp.path().join("result.json");
        fs::write(&spec_file, serde_json::to_vec(&spec).unwrap()).unwrap();
        let mut child = Command::new(&binary)
            .arg(&spec_file)
            .arg(&result_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let status = wait(&mut child);
        let value = result(&result_file);
        assert_eq!(fs::read_link("/proc/self/ns/mnt").unwrap(), host_namespace);
        if let Some(reason) = value.get("unsupported") {
            eprintln!(
                "Linux overlay runtime qualification FAILED (explicit unsupported): {reason}"
            );
            assert_eq!(status.code(), Some(125));
            assert_eq!(
                fixture
                    .manager
                    .descriptor(&view.view_id, &owner())
                    .unwrap()
                    .status,
                WorkspaceStatus::Orphaned
            );
            return;
        }
        assert!(status.success());
        assert_eq!(value["qualification"]["private_pid_namespace"], true);
        assert_eq!(value["qualification"]["live_checkpoint"], false);
        assert_eq!(value["qualification"]["lower_directory_rename"], false);
        let checkpoint = fixture
            .manager
            .descriptor(&view.view_id, &owner())
            .unwrap()
            .latest_checkpoint
            .unwrap();
        let checkpoint: CheckpointV2Manifest =
            serde_json::from_slice(&fixture.store.read_object(&checkpoint).unwrap()).unwrap();
        let output = fixture._temp.path().join("logical-output");
        fixture
            .store
            .export_tree(&checkpoint.view_root, &output)
            .unwrap();
        assert_eq!(fs::read(output.join("source")).unwrap(), b"edited source");
        assert_eq!(fs::read(output.join("mapped")).unwrap()[0], b'Z');
        assert!(!output.join("deleted").exists());
        assert!(!output.join("opaque/old").exists());
        assert_eq!(fs::read(output.join("opaque/new")).unwrap(), b"new");
        assert_eq!(
            fs::read(output.join("renamed/leaf")).unwrap(),
            b"rename data"
        );
        assert_eq!(
            fs::read_link(output.join("link")).unwrap(),
            PathBuf::from("source")
        );
        assert_eq!(
            fs::metadata(output.join("source"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o751
        );
        assert_eq!(
            fs::read(prepared.lower.join("source")).unwrap(),
            b"base source"
        );
        assert!(prepared.lower.join("deleted").exists());
        if output.join("uring-submitted").exists() {
            let bytes = fs::read(output.join("uring-output")).unwrap();
            assert!(bytes.iter().all(|byte| *byte == b'U'));
            eprintln!("io_uring async WRITE submitted; terminal namespace drain + syncfs checkpointed {} bytes", bytes.len());
        } else {
            eprintln!(
                "io_uring qualification unavailable: {}",
                fs::read_to_string(output.join("uring-unavailable")).unwrap()
            );
        }
        // Re-enter the same retained upper/work in another fresh namespace.
        let remount = fixture.manager.helper_spec(&view.view_id, &owner(), vec!["/bin/sh".into(), "-c".into(), "test ! -e deleted && test ! -e opaque/old && test -e opaque/new && test -e renamed/leaf".into()], None, true).unwrap();
        fs::write(&spec_file, serde_json::to_vec(&remount).unwrap()).unwrap();
        let mut child = Command::new(&binary)
            .arg(&spec_file)
            .arg(&result_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        assert!(wait(&mut child).success());
    }

    #[test]
    fn terminal_checkpoint_stops_detached_descendants_and_preserves_full_private_state() {
        if !Path::new("/workspace").is_dir() {
            return;
        }
        let fixture = Fixture::at(Some(Path::new("/workspace")));
        let binary = helper(fixture._temp.path());
        let prepared = fixture.prepare(WorkspaceBackend::LinuxOverlay);
        let view = fixture.create(&prepared);
        let spec = fixture
            .manager
            .helper_spec(
                &view.view_id,
                &owner(),
                vec![
                    binary.to_string_lossy().into_owned(),
                    "writer".into(),
                    "hold".into(),
                ],
                None,
                true,
            )
            .unwrap();
        let spec_file = fixture._temp.path().join("spec.json");
        let result_file = fixture._temp.path().join("result.json");
        fs::write(&spec_file, serde_json::to_vec(&spec).unwrap()).unwrap();
        let mut child = Command::new(&binary)
            .arg(&spec_file)
            .arg(&result_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut observed = Vec::new();
        loop {
            if child.try_wait().unwrap().is_some() {
                let value = result(&result_file);
                if let Some(reason) = value.get("unsupported") {
                    eprintln!("terminal namespace test unsupported: {reason}");
                    return;
                }
                panic!("writer exited before terminal checkpoint");
            }
            let descendants = descendants(child.id());
            if descendants.len() >= 3 {
                for pid in descendants {
                    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
                    if fd >= 0 {
                        observed.push(unsafe { OwnedFd::from_raw_fd(fd as i32) });
                    }
                }
                if observed.len() >= 3 {
                    break;
                }
            }
            assert!(Instant::now() < deadline, "detached writer did not start");
            std::thread::sleep(Duration::from_millis(10));
        }
        let operation = id();
        let request = rudder_engine::workspace::linux::TerminalCheckpointRequest {
            view_id: view.view_id.clone(),
            owner: owner(),
            operation_id: operation.clone(),
            seal: true,
            knowledge_cursor: 99,
        };
        let receipt = rudder_engine::workspace::linux::request_terminal_checkpoint(
            &fixture.manager,
            request.clone(),
        )
        .unwrap();
        wait(&mut child);
        for process in observed {
            let mut descriptor = libc::pollfd {
                fd: process.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            assert_eq!(
                unsafe { libc::poll(&mut descriptor, 1, 0) },
                1,
                "an observed managed namespace process survived terminal checkpoint"
            );
        }
        assert_eq!(receipt.knowledge_cursor, Some(99));
        assert_eq!(
            fixture
                .manager
                .descriptor(&view.view_id, &owner())
                .unwrap()
                .status,
            WorkspaceStatus::Sealed
        );
        assert_eq!(
            rudder_engine::workspace::linux::request_terminal_checkpoint(&fixture.manager, request)
                .unwrap()
                .checkpoint,
            receipt.checkpoint
        );
    }

    #[test]
    fn helper_retains_real_pty_unicode_control_bytes_and_foreground_interrupt_semantics() {
        use std::io::{Read, Write};
        use std::os::unix::process::CommandExt;
        if !Path::new("/workspace").is_dir() {
            return;
        }
        let fixture = Fixture::at(Some(Path::new("/workspace")));
        let binary = helper(fixture._temp.path());
        let prepared = fixture.prepare(WorkspaceBackend::LinuxOverlay);
        for mode in ["tty", "tty-signal"] {
            let view = fixture.create(&prepared);
            let spec = fixture
                .manager
                .helper_spec(
                    &view.view_id,
                    &owner(),
                    vec![binary.to_string_lossy().into_owned(), mode.into()],
                    None,
                    true,
                )
                .unwrap();
            let spec_file = fixture._temp.path().join(format!("{mode}-spec.json"));
            let report_file = fixture._temp.path().join(format!("{mode}-report.json"));
            fs::write(&spec_file, serde_json::to_vec(&spec).unwrap()).unwrap();
            let mut master = 0;
            let mut slave = 0;
            let size = libc::winsize {
                ws_row: 30,
                ws_col: 100,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            assert_eq!(
                unsafe {
                    libc::openpty(
                        &mut master,
                        &mut slave,
                        std::ptr::null_mut(),
                        std::ptr::null(),
                        &size,
                    )
                },
                0
            );
            let mut master = unsafe { fs::File::from_raw_fd(master) };
            let slave = unsafe { fs::File::from_raw_fd(slave) };
            unsafe {
                libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
            }
            let mut command = Command::new(&binary);
            command
                .arg(&spec_file)
                .arg(&report_file)
                .stdin(Stdio::from(slave.try_clone().unwrap()))
                .stdout(Stdio::from(slave.try_clone().unwrap()))
                .stderr(Stdio::from(slave.try_clone().unwrap()));
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let mut child = command.spawn().unwrap();
            drop(slave);
            let marker = if mode == "tty" {
                "PTY-READY"
            } else {
                "PTY-SIGNAL-READY"
            };
            let mut output = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(15);
            while !String::from_utf8_lossy(&output).contains(marker) {
                let mut bytes = [0u8; 4096];
                if let Ok(count) = master.read(&mut bytes) {
                    output.extend_from_slice(&bytes[..count]);
                }
                if child.try_wait().unwrap().is_some() {
                    let report = result(&report_file);
                    if report.get("unsupported").is_some() {
                        eprintln!("PTY namespace qualification unsupported: {report}");
                        return;
                    }
                    panic!("PTY helper exited: {}", String::from_utf8_lossy(&output));
                }
                assert!(
                    Instant::now() < deadline,
                    "PTY helper did not become ready: {}",
                    String::from_utf8_lossy(&output)
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            if mode == "tty" {
                let payload = "日本語 👩🏽‍💻 é\x1b[1;5D END".as_bytes();
                assert_eq!(payload.len(), 39);
                master.write_all(payload).unwrap();
                while !output
                    .windows(payload.len())
                    .any(|window| window == payload)
                {
                    let mut bytes = [0u8; 4096];
                    if let Ok(count) = master.read(&mut bytes) {
                        output.extend_from_slice(&bytes[..count]);
                    }
                    assert!(
                        Instant::now() < deadline,
                        "PTY Unicode/control roundtrip timed out"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
                assert!(wait(&mut child).success());
            } else {
                master.write_all(&[3]).unwrap();
                let status = wait(&mut child);
                assert_eq!(
                    status.code(),
                    Some(130),
                    "Ctrl-C must reach the foreground agent, not kill the wrapper"
                );
            }
            let report = result(&report_file);
            assert!(
                report["checkpoint"]["checkpoint"].is_object(),
                "wrapper must survive to commit terminal checkpoint"
            );
            assert_eq!(
                fixture
                    .manager
                    .descriptor(&view.view_id, &owner())
                    .unwrap()
                    .status,
                WorkspaceStatus::Quiesced
            );
        }
    }

    #[test]
    fn killing_outer_helper_kills_namespace_writers_and_never_claims_a_checkpoint() {
        if !Path::new("/workspace").is_dir() {
            return;
        }
        let fixture = Fixture::at(Some(Path::new("/workspace")));
        let binary = helper(fixture._temp.path());
        let prepared = fixture.prepare(WorkspaceBackend::LinuxOverlay);
        let view = fixture.create(&prepared);
        let spec = fixture
            .manager
            .helper_spec(
                &view.view_id,
                &owner(),
                vec![
                    binary.to_string_lossy().into_owned(),
                    "writer".into(),
                    "hold".into(),
                ],
                None,
                true,
            )
            .unwrap();
        let spec_file = fixture._temp.path().join("crash-spec.json");
        let report_file = fixture._temp.path().join("crash-report.json");
        fs::write(&spec_file, serde_json::to_vec(&spec).unwrap()).unwrap();
        let mut child = Command::new(&binary)
            .arg(&spec_file)
            .arg(&report_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let observed = loop {
            if child.try_wait().unwrap().is_some() {
                let report = result(&report_file);
                if report.get("unsupported").is_some() {
                    eprintln!("crash namespace qualification unsupported: {report}");
                    return;
                }
                panic!("crash writer unexpectedly exited")
            }
            let pids = descendants(child.id());
            if pids.len() >= 3 {
                let expected_namespace =
                    fs::read_link(format!("/proc/{}/ns/pid_for_children", child.id())).unwrap();
                assert_ne!(
                    expected_namespace,
                    fs::read_link("/proc/self/ns/pid").unwrap(),
                    "crash witnesses must be inside the helper's private PID namespace"
                );
                let descriptors: Vec<_> = pids
                    .into_iter()
                    .filter_map(|pid| {
                        // A pidfd prevents reuse AFTER opening. Check identity on
                        // both sides as well, so a discovered PID cannot be reused
                        // for an unrelated process before pidfd_open.
                        let before = process_identity(pid)?;
                        if before.1 != expected_namespace {
                            return None;
                        }
                        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
                        if fd < 0 {
                            return None;
                        }
                        let descriptor = unsafe { OwnedFd::from_raw_fd(fd as i32) };
                        if process_identity(pid).as_ref() != Some(&before) {
                            return None;
                        }
                        let info = fs::read_to_string(format!(
                            "/proc/self/fdinfo/{}",
                            descriptor.as_raw_fd()
                        ))
                        .ok()?;
                        let bound = info.lines().find_map(|line| {
                            line.strip_prefix("Pid:")
                                .and_then(|value| value.trim().parse::<u32>().ok())
                        });
                        if bound != Some(pid) {
                            return None;
                        }
                        Some((pid, descriptor, process_observation(pid)))
                    })
                    .collect();
                if descriptors.len() >= 3 {
                    break descriptors;
                }
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        };
        // Match the helper's documented teardown bound for the WHOLE
        // namespace. Kernel I/O exit can wait beyond a short scheduling timer;
        // the only successful proof remains POLLIN on every exact pidfd.
        let teardown_deadline = Instant::now() + Duration::from_secs(30);
        child.kill().unwrap();
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(wait(&mut child).signal(), Some(libc::SIGKILL));
        for (pid, descriptor, before) in observed {
            loop {
                let mut poll = libc::pollfd {
                    fd: descriptor.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let result = unsafe { libc::poll(&mut poll, 1, 0) };
                if result == 1 && poll.revents & libc::POLLIN != 0 {
                    break;
                }
                assert_eq!(
                    poll.revents & libc::POLLNVAL,
                    0,
                    "invalid namespace pidfd witness for pid {pid}"
                );
                if result < 0 {
                    assert_eq!(
                        std::io::Error::last_os_error().kind(),
                        std::io::ErrorKind::Interrupted,
                        "poll namespace pidfd failed"
                    );
                }
                assert!(Instant::now()<teardown_deadline,
                    "namespace process survived outer-helper death: pid={pid} before={before} after={} pidfd={}",
                    process_observation(pid),
                    fs::read_to_string(format!("/proc/self/fdinfo/{}",descriptor.as_raw_fd())).unwrap_or_default());
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        assert!(fixture
            .manager
            .descriptor(&view.view_id, &owner())
            .unwrap()
            .latest_checkpoint
            .is_none());
        assert!(!report_file.exists());
        // A released flock alone is not a recovery proof: init closes files
        // before its kernel namespace shutdown has necessarily finished.
        let retry_report = fixture._temp.path().join("crash-retry-report.json");
        let mut retried = Command::new(&binary)
            .arg(&spec_file)
            .arg(&retry_report)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        assert_eq!(wait(&mut retried).code(), Some(125));
        assert!(result(&retry_report)["unsupported"]
            .as_str()
            .unwrap()
            .contains("completion proof"));
    }
    fn process_identity(pid: u32) -> Option<(u64, PathBuf)> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // comm may contain spaces or parentheses; field22 follows the final
        // comm delimiter and field3(state), so never split the entire line.
        let fields = stat.get(stat.rfind(')')? + 2..)?;
        let started = fields.split_whitespace().nth(19)?.parse().ok()?;
        let namespace = fs::read_link(format!("/proc/{pid}/ns/pid")).ok()?;
        Some((started, namespace))
    }

    fn process_observation(pid: u32) -> serde_json::Value {
        let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
        let fields: Vec<_> = status
            .lines()
            .filter(|line| {
                ["Name:", "State:", "PPid:", "NSpid:", "Threads:"]
                    .iter()
                    .any(|key| line.starts_with(key))
            })
            .collect();
        let tasks:Vec<_>=fs::read_dir(format!("/proc/{pid}/task")).into_iter().flatten().filter_map(|entry|entry.ok()).take(16).map(|entry|{
            let status=fs::read_to_string(entry.path().join("status")).unwrap_or_default();
            let fields:Vec<_>=status.lines().filter(|line|line.starts_with("State:")||line.starts_with("Name:")).collect();
            serde_json::json!({"tid":entry.file_name().to_string_lossy(),"status":fields,"wchan":fs::read_to_string(entry.path().join("wchan")).ok()})
        }).collect();
        serde_json::json!({"identity":process_identity(pid),"status":fields,"tasks":tasks})
    }

    fn descendants(root: u32) -> Vec<u32> {
        let mut all = Vec::new();
        let mut todo = vec![root];
        while let Some(pid) = todo.pop() {
            if let Ok(text) = fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")) {
                for pid in text
                    .split_whitespace()
                    .filter_map(|value| value.parse::<u32>().ok())
                {
                    all.push(pid);
                    todo.push(pid);
                }
            }
        }
        all
    }
}
