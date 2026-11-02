//! Static projection DATA tests, not Root/runtime/SDK authority. Synthetic
//! ledgers below cannot publish an ImageDataBuild or bypass its actual custody.
use super::*;
use std::os::unix::fs::PermissionsExt;

struct Fixture{path:PathBuf,store:LocalStore}
impl Fixture{fn new()->Self{let base=if Path::new("/workspace").is_dir(){PathBuf::from("/workspace")}else{std::env::temp_dir()};
    let path=base.join(format!("rudder-static-link-test-{}",uuid::Uuid::new_v4()));fs::create_dir(&path).unwrap();let store=LocalStore::open(path.join("cas")).unwrap();Self{path,store}}}
impl Drop for Fixture{fn drop(&mut self){let _=fs::remove_dir_all(&self.path);}}
fn budget()->ImagePreseedBudget{ImagePreseedBudget::begin(Duration::from_secs(20)).unwrap()}
fn data(f:&Fixture)->(Stamp,ImageFileData){
    let bytes=b"actual immutable fixture content";let p=f.path.join("content");fs::write(&p,bytes).unwrap();fs::set_permissions(&p,fs::Permissions::from_mode(0o444)).unwrap();let m=fs::symlink_metadata(&p).unwrap();
    // This is a DATA-only synthetic path mapping. Production stamp() still
    // requires actual Root ownership; no owner or ready/pin ACK is constructed.
    let path=PathBuf::from("/usr/share/zoneinfo/Etc/UTC");
    let s=Stamp{path:path.clone(),dev:m.dev(),ino:m.ino(),len:m.len(),mode:m.mode(),mtime:(m.mtime(),m.mtime_nsec()),ctime:(m.ctime(),m.ctime_nsec())};
    let content=f.store.put_blob(bytes).unwrap();let r=ImageFileData{path,sha256:format!("{:x}",Sha256::digest(bytes)),size_bytes:bytes.len()as u64,content};(s,r)
}
#[test]
fn fixed_static_file_table_matches_runtime_regular_component_contract(){
    let expected=rudder_engine::execution::runtime_image::COMPONENTS.into_iter().filter(|(_,_,_,dir)|!*dir).map(|(name,source,_,_)|(name,source)).collect::<Vec<_>>();
    assert_eq!(STATIC_FILES.to_vec(),expected);
}
#[test]
fn static_link_normalization_is_lexical_bounded_and_never_resolves_filesystem(){
    let source=Path::new("/etc/localtime");let target=Path::new("/usr/share/zoneinfo/Etc/UTC");
    for link in ["/usr/share/zoneinfo/Etc/UTC","/usr/./share/zoneinfo/Etc/UTC"]{assert_eq!(static_link_target(source,Path::new(link)).unwrap(),target);}
    assert_eq!(static_link_target(Path::new("/etc/ssl/openssl.cnf"),Path::new("./certs/ca-certificates.crt")).unwrap(),Path::new("/etc/ssl/certs/ca-certificates.crt"));
    // A lexically cancellable parent could have traversed an actual symlink.
    // Refuse EVERY ParentDir without probing host filesystem semantics.
    for link in ["../usr/share/zoneinfo/Etc/UTC","/usr/share/zoneinfo/../zoneinfo/Etc/UTC","/usr/foo/../bar","/usr/missing/../bar","a/..","/../usr/share/zoneinfo/Etc/UTC"]{
        assert!(static_link_target(source,Path::new(link)).is_err(),"{link}");
    }
    // Nonexistent target deliberately succeeds only as normalized DATA; the
    // separate captured-file ledger admission must then refuse it.
    assert_eq!(static_link_target(source,Path::new("/usr/no-such-captured-file")).unwrap(),Path::new("/usr/no-such-captured-file"));
    assert!(static_link_target(source,Path::new("../../etc/shadow")).is_err());
    assert!(static_link_target(Path::new("/etc/shadow"),target).is_err());
    assert!(static_link_target(source,Path::new(&"a".repeat(4097))).is_err());
    assert!(static_link_target(source,Path::new("")).is_err());assert!(static_link_target(source,Path::new("/usr/bad\0target")).is_err());
}
#[test]
fn static_alias_reuses_exact_cas_bytes_and_adds_declared_source_manifest_row(){
    let f=Fixture::new();let(s,r)=data(&f);let source=Path::new("/etc/localtime");
    let(node,alias,saved)=static_alias_data(source,&r.path,&[s.clone()],&[r.clone()]).unwrap();
    assert!(saved==s);assert_eq!(alias.path,source);assert_eq!(alias.sha256,r.sha256);assert_eq!(alias.size_bytes,r.size_bytes);assert_eq!(alias.content,r.content);
    assert_eq!(node,EntryNode::File{object:r.content.clone(),logical_size:r.size_bytes,mode:0o444});
    let lease=f.store.begin_staging(Duration::from_secs(30)).unwrap();let directories=LocalDirectoryStore::new(&f.store,&lease.id);let tree=PagedDirectory::new(&directories);
    let root=tree.build(0o555,[DirectoryEntry::new(b"timezone",node).unwrap()]).unwrap();
    assert!(matches!(tree.lookup(&root,b"timezone").unwrap().unwrap().node,EntryNode::File{..}));
    assert_eq!(content::read_file_range(&f.store,&alias.content,0,alias.size_bytes as usize).unwrap(),b"actual immutable fixture content");
    let mut files=vec![r.clone(),alias];files.sort_by(|a,b|a.path.cmp(&b.path));
    let manifest=ImageManifestData{format_version:1,installer_input:root,files};let raw=canonical_json(&manifest).unwrap();
    let decoded:ImageManifestData=serde_json::from_slice(&raw).unwrap();assert_eq!(decoded.files.len(),2);assert!(decoded.files.contains(&r));assert_eq!(canonical_json(&decoded).unwrap(),raw);
}
#[test]
fn static_alias_rejects_uncaptured_targets_missing_stamps_and_duplicate_records(){
    let f=Fixture::new();let(s,r)=data(&f);let source=Path::new("/etc/localtime");
    assert!(static_alias_data(source,&r.path,&[],&[r.clone()]).is_err());
    assert!(static_alias_data(source,&r.path,&[s.clone()],&[]).is_err());
    assert!(static_alias_data(source,&r.path,&[s.clone(),s.clone()],&[r.clone()]).is_err());
    assert!(static_alias_data(source,&r.path,&[s.clone()],&[r.clone(),r.clone()]).is_err());
    let mut changed=r.clone();changed.size_bytes+=1;assert!(static_alias_data(source,&r.path,&[s],&[changed]).is_err());
}
#[test]
fn static_alias_rejects_provider_host_special_and_noncanonical_target_paths(){
    let f=Fixture::new();let(s,r)=data(&f);
    for target in ["/dev/null","/etc/shadow","/run/host-timezone","/usr/lib/modules/a","/usr/sbin/swapon","/usr/../etc/shadow","relative"]{
        let mut s=s.clone();s.path=target.into();let mut r=r.clone();r.path=target.into();
        assert!(static_alias_data(Path::new("/etc/localtime"),Path::new(target),&[s],&[r]).is_err(),"{target}");
    }
    assert!(static_alias_data(Path::new("/etc/localtime.backup"),&r.path,&[s],&[r.clone()]).is_err());
}
#[test]
fn static_alias_rejects_link_directory_unreadable_or_mutable_captured_target(){
    let f=Fixture::new();let(s,r)=data(&f);
    for mode in [libc::S_IFLNK|0o777,libc::S_IFDIR|0o555,libc::S_IFREG|0o440,libc::S_IFREG|0o464]{
        let mut s=s.clone();s.mode=mode;assert!(static_alias_data(Path::new("/etc/localtime"),&r.path,&[s],&[r.clone()]).is_err());
    }
}
#[test]
fn cancellation_refuses_static_import_before_any_new_capture_and_never_renews_budget(){
    let f=Fixture::new();let original=budget();let cancel=Arc::new(AtomicBool::new(true));let mut build=ImageDataBuild::staged(f.store.clone(),original.clone(),cancel);
    assert!(build.import_static_file(Path::new("/etc/localtime")).is_err());assert!(original.remaining().is_err());
    assert!(build.stamps.is_empty()&&build.roots.is_empty()&&build.files.is_empty()&&!build.ready&&!build.pin_ack);
}
#[test]
#[ignore="requires actual fixed Linux /etc/localtime symlink to a Root-owned captured regular /usr target"]
fn actual_fixed_timezone_alias_materializes_captured_content_and_retains_original_custody(){
    let f=Fixture::new();let source=Path::new("/etc/localtime");let before=stamp(source).unwrap();assert_eq!(before.mode&libc::S_IFMT,libc::S_IFLNK);
    let target=static_link_target(source,&fs::read_link(source).unwrap()).unwrap();let captured=stamp(&target).unwrap();regular_readable(&captured).unwrap();
    let original=budget();let cancel=Arc::new(AtomicBool::new(false));let mut build=ImageDataBuild::staged(f.store.clone(),original.clone(),cancel.clone());
    LocalImageImport::stage_retaining(&mut build.bulk,f.store.clone(),Arc::new(ImportClock{original:original.clone(),cancelled:cancel})).unwrap();
    let file=OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC|libc::O_NONBLOCK).open(&target).unwrap();
    let(content,sha256)=build.imported.read_with(file,&captured,build.bulk.as_ref().unwrap(),&original,&build.cancelled).unwrap();
    build.stamps.push(captured.clone());build.files.push(ImageFileData{path:target.clone(),sha256:sha256.clone(),size_bytes:captured.len,content:content.clone()});
    build.roots.push(Root::capture(target.parent().unwrap()).unwrap());
    let existing=vec![installer_path(&target).unwrap()];let nlink=fs::symlink_metadata(&target).unwrap().nlink();assert_eq!(nlink,1,"fixed fixture target must have its sole physical alias captured");build.links.insert((captured.dev,captured.ino),(nlink,existing.clone()));
    let node=build.import_static_file(source).unwrap();assert_eq!(node,EntryNode::File{object:content.clone(),logical_size:captured.len,mode:captured.mode&0o7777});
    assert_eq!(build.files.len(),2);assert_eq!(build.files[0].path,target);assert_eq!(build.files[1].path,source);assert_eq!(build.files[1].sha256,sha256);
    assert_eq!(build.imported.bytes,captured.len);assert_eq!(build.imported.inodes.len(),1);assert_eq!(build.links[&(captured.dev,captured.ino)].1,existing);
    assert!(hardlink_topology(&build.links).unwrap().groups.is_empty());assert!(build.stamps.contains(&before));assert!(build.stamps.contains(&captured));
    assert!(build.roots.iter().any(|r|r.path==source.parent().unwrap()));build.check_deep_for_import().unwrap();
    assert!(stamp(source).unwrap()==before);assert!(stamp(&target).unwrap()==captured);assert!(!build.ready&&!build.pin_ack);assert!(build.pin_data().is_err());
}
