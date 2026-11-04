//! Real CAS/paged-directory subset tests; no Root or SDK grant is fabricated.
use super::*;
use crate::directory::{DirectoryEntry,EntryNode,LocalDirectoryStore};
use std::time::Duration;
fn blob(store:&LocalStore,s:&str)->ObjectRef{store.put_blob(s.as_bytes()).unwrap()}
fn dir(store:&LocalStore,stage:&str,entries:Vec<(&str,EntryNode)>)->ObjectRef{PagedDirectory::new(&LocalDirectoryStore::new(store,stage)).build(0o555,entries.into_iter().map(|(n,e)|DirectoryEntry::new(n.as_bytes(),e).unwrap()).collect::<Vec<_>>()).unwrap()}
fn file(r:ObjectRef)->EntryNode{EntryNode::File{logical_size:r.size_bytes,object:r,mode:0o555}}
fn graph(store:&LocalStore,stage:&str,compiler:&str,lib:&str,header:&str,buck:&str,path:&str,config:&str,provenance:&str)->ObjectRef{
    let tools=dir(store,stage,vec![(path,file(blob(store,compiler)))]);
    let libs=dir(store,stage,vec![("libc.so",file(blob(store,lib)))]);
    let headers=dir(store,stage,vec![("stdio.h",file(blob(store,header)))]);
    let prepared=dir(store,stage,vec![("tools",EntryNode::Directory{object:tools}),("lib",EntryNode::Directory{object:libs}),("include",EntryNode::Directory{object:headers}),("compiler.json",file(blob(store,config)))]);
    dir(store,stage,vec![("prepared",EntryNode::Directory{object:prepared}),("buck2",file(blob(store,buck))),("construction.json",file(blob(store,provenance)))])
}
fn derive(store:&LocalStore,root:&ObjectRef)->CompilerSdkSemanticData{let members:BTreeSet<_>=store.closure(root).unwrap().into_iter().collect();derive_members(&LocalDirectoryStore::read_only(store),root,|r|members.contains(r)).unwrap().0}
#[test]fn actual_same_sdk_tree_keeps_semantic_root_across_recipe_workload_oci_and_control_changes(){
    let temp=tempfile::tempdir_in("/workspace").unwrap();let store=LocalStore::open(temp.path().join("cas")).unwrap();let stage=store.begin_staging(Duration::from_secs(30)).unwrap();
    let base=graph(&store,&stage.id,"rustc","libc","headers","static-buck2","rustc","{}","recipeA/workloadA/ociA/configA/nativeA/registrationA");let semantic=derive(&store,&base);
    for changed in ["recipeB/workloadA/ociA/configA/nativeA/registrationA","recipeA/workloadB/ociA/configA/nativeA/registrationA","recipeA/workloadA/ociB/configA/nativeA/registrationA","recipeA/workloadA/ociA/configB/nativeA/registrationA","recipeA/workloadA/ociA/configA/nativeB/registrationA","recipeA/workloadA/ociA/configA/nativeA/registrationB"]{
        let root=graph(&store,&stage.id,"rustc","libc","headers","static-buck2","rustc","{}",changed);assert_ne!(root,base);assert_eq!(derive(&store,&root),semantic);
    }
}
#[test]fn actual_compiler_library_headers_frontend_path_and_sdk_configuration_change_semantic_root(){
    let temp=tempfile::tempdir_in("/workspace").unwrap();let store=LocalStore::open(temp.path().join("cas")).unwrap();let stage=store.begin_staging(Duration::from_secs(30)).unwrap();
    let root=graph(&store,&stage.id,"rustc","libc","headers","buck2","rustc","{}","construction");let base=derive(&store,&root);
    for (compiler,lib,header,buck,path,config)in[("rustc2","libc","headers","buck2","rustc","{}"),("rustc","libc2","headers","buck2","rustc","{}"),("rustc","libc","headers2","buck2","rustc","{}"),("rustc","libc","headers","buck22","rustc","{}"),("rustc","libc","headers","buck2","renamed-rustc","{}"),("rustc","libc","headers","buck2","rustc","{\"target\":\"changed\"}")]{
        let root=graph(&store,&stage.id,compiler,lib,header,buck,path,config,"construction");assert_ne!(derive(&store,&root).directory,base.directory);
    }
}
#[test]fn actual_subset_derivation_refuses_wrong_type_nonmember_and_substituted_data(){
    let temp=tempfile::tempdir_in("/workspace").unwrap();let store=LocalStore::open(temp.path().join("cas")).unwrap();let stage=store.begin_staging(Duration::from_secs(30)).unwrap();let root=graph(&store,&stage.id,"rustc","libc","headers","buck2","rustc","{}","recipe");let reader=LocalDirectoryStore::read_only(&store);
    assert!(derive_members(&reader,&root,|_|false).is_err());let mut data=derive(&store,&root);data.prepared=ObjectRef::from_bytes(ObjectKind::Directory,b"not-original");assert!(data.validate_data().is_err());
    let wrong=dir(&store,&stage.id,vec![("prepared",EntryNode::Symlink{target_hex:hex::encode(b"somewhere")}),("buck2",file(blob(&store,"buck2")))]);assert!(derive_members(&reader,&wrong,|_|true).is_err());
    let empty=dir(&store,&stage.id,vec![]);let wrong=dir(&store,&stage.id,vec![("prepared",EntryNode::Directory{object:empty.clone()}),("buck2",EntryNode::Directory{object:empty})]);assert!(derive_members(&reader,&wrong,|_|true).is_err());
}
