//! Actual binary Git pack/index/loose stores and actual fixed Git/jj consumers.
//! No generated receipt or bool stands in for an imported object or process.
use super::*;
use gix::features::zlib::stream::deflate::{Compress,FlushCompress};
use gix::objs::Write as _;
use jj_lib::{repo::Repo,workspace::Workspace,config::{StackedConfig,ConfigLayer,ConfigSource},settings::UserSettings,backend::CommitId};
use std::{time::{Duration,Instant},panic::{catch_unwind,AssertUnwindSafe}};
fn clock()->FinalizationBudget{FinalizationBudget::begin(Duration::from_secs(30)).unwrap()}
fn settings()->UserSettings{let mut c=StackedConfig::with_defaults();let mut l=ConfigLayer::empty(ConfigSource::User);l.set_value("user.name","Packed profile fixture").unwrap();l.set_value("user.email","packed@invalid.example").unwrap();c.add_layer(l);UserSettings::from_config(c).unwrap()}
fn compress(raw:&[u8])->Vec<u8>{let mut c=Compress::new();let mut input=raw;let mut bytes=Vec::new();loop{let mut b=[0;65536];let a=c.total_in();let o=c.total_out();let status=c.compress(input,&mut b,FlushCompress::Finish).unwrap();let n=(c.total_in()-a)as usize;let m=(c.total_out()-o)as usize;bytes.extend_from_slice(&b[..m]);input=&input[n..];if status==Status::StreamEnd{assert!(input.is_empty());return bytes}assert!(n>0||m>0);}}
fn header(kind:u8,mut size:u64)->Vec<u8>{let mut first=(kind<<4)|(size as u8&15);size>>=4;if size>0{first|=128;}let mut b=vec![first];while size>0{let mut value=size as u8&127;size>>=7;if size>0{value|=128;}b.push(value);}b}
fn ofs(mut distance:u64)->Vec<u8>{let mut out=vec![distance as u8&127];while {distance>>=7;distance>0}{distance-=1;out.push(128|(distance as u8&127));}out.reverse();out}
fn variable(mut n:u64)->Vec<u8>{let mut out=Vec::new();loop{let mut b=(n&127)as u8;n>>=7;if n>0{b|=128;}out.push(b);if n==0{return out}}}
fn delta(base:&[u8],result:&[u8])->Vec<u8>{assert_eq!(base.len(),result.len());assert!(base.len()<256);let mut b=variable(base.len()as u64);b.extend(variable(result.len()as u64));b.extend_from_slice(&[1,result[0],0x91,1,(base.len()-1)as u8]);b}
fn index_for(entries:&[(ObjectId,u64,u32)],pack_hash:ObjectId)->Vec<u8>{let mut sorted=entries.to_vec();sorted.sort_by_key(|e|e.0);let mut raw=b"\xfftOc\0\0\0\x02".to_vec();let mut fan=[0u32;256];for(id,_,_)in &sorted{fan[id.as_bytes()[0]as usize]+=1;}let mut sum=0;for n in fan{sum+=n;raw.extend(sum.to_be_bytes());}for(id,_,_)in &sorted{raw.extend_from_slice(id.as_bytes());}for(_,_,crc)in &sorted{raw.extend(crc.to_be_bytes());}for(_,offset,_)in &sorted{raw.extend((*offset as u32).to_be_bytes());}raw.extend_from_slice(pack_hash.as_bytes());let digest=sha1(&raw).unwrap();raw.extend_from_slice(digest.as_bytes());raw}
struct Pair{_tmp:tempfile::TempDir,root:PathBuf,pack:PathBuf,index:PathBuf,first:ObjectId,second:ObjectId,base:Vec<u8>,result:Vec<u8>}
impl Pair{
 fn new(reference:bool)->Self{let base=b"hello packed base\n".to_vec();let mut result=base.clone();result[0]=b'H';Self::with_records(base,result,reference,None)}
 fn with_records(base:Vec<u8>,result:Vec<u8>,reference:bool,override_delta:Option<Vec<u8>>)->Self{
  let first=gix::objs::compute_hash(gix::hash::Kind::Sha1,Kind::Blob,&base).unwrap();let second=gix::objs::compute_hash(gix::hash::Kind::Sha1,Kind::Blob,&result).unwrap();
  let mut a=header(3,base.len()as u64);a.extend(compress(&base));let instructions=override_delta.unwrap_or_else(||delta(&base,&result));let mut b=header(if reference{7}else{6},instructions.len()as u64);
  if reference{b.extend_from_slice(first.as_bytes());}else{b.extend(ofs(a.len()as u64));}b.extend(compress(&instructions));
  let tmp=tempfile::tempdir().unwrap();let root=tmp.path().canonicalize().unwrap();let mut raw=b"PACK\0\0\0\x02\0\0\0\x02".to_vec();raw.extend_from_slice(&a);raw.extend_from_slice(&b);let hash=sha1(&raw).unwrap();raw.extend_from_slice(hash.as_bytes());
  let idx=index_for(&[(first,12,gix::features::hash::crc32(&a)),(second,12+a.len()as u64,gix::features::hash::crc32(&b))],hash);let pack=root.join(format!("pack-{hash}.pack"));let index=pack.with_extension("idx");fs::write(&pack,raw).unwrap();fs::write(&index,idx).unwrap();for p in [&pack,&index]{fs::set_permissions(p,fs::Permissions::from_mode(0o444)).unwrap();}Self{_tmp:tmp,root,pack,index,first,second,base,result}
 }
 fn owner(&self)->PackedHistory{let mut o=PackedHistory::stage(clock(),Arc::new(AtomicBool::new(false)));o.source=Some(HeldDir::open(self.root.clone()).unwrap());o.source_pack=Some(HeldFile::open(self.pack.clone()).unwrap());o.source_index=Some(HeldFile::open(self.index.clone()).unwrap());o.index_bytes=fs::read(&self.index).unwrap();o}
 fn checked(&self)->PackedHistory{let mut o=self.owner();o.parse_index(fs::metadata(&self.pack).unwrap().len()).unwrap();o.verify_pack_hash().unwrap();o.preflight().unwrap();o}
}
#[test]fn actual_ref_and_ofs_delta_records_verify_full_raw_oid_and_bound_work(){for reference in[false,true]{let f=Pair::new(reference);let mut o=f.checked();let second=o.ids[&f.second];assert_eq!(&**o.decode(second).unwrap(),&f.result);assert_eq!(&**o.decode(o.ids[&f.first]).unwrap(),&f.base);assert!(o.data.verification_work_bytes>0&&o.data.verification_work_bytes<=MAX_VERIFICATION_WORK);assert!(o.cache_bytes<=MAX_CACHE_BYTES);assert!(o.receipt().is_err());}}
#[test]fn delta_bomb_base_mismatch_and_invalid_instructions_fail_before_unbounded_allocation(){
 let mut bomb=variable(18);bomb.extend(variable(MAX_OBJECT_BYTES+1));let f=Pair::with_records(b"hello packed base\n".to_vec(),b"Hello packed base\n".to_vec(),false,Some(bomb));let mut o=f.owner();o.parse_index(fs::metadata(&f.pack).unwrap().len()).unwrap();o.verify_pack_hash().unwrap();assert!(o.preflight().is_err());assert!(o.active_bytes.is_empty()&&o.active_result.is_empty());
 let f=Pair::with_records(b"hello packed base\n".to_vec(),b"Hello packed base\n".to_vec(),true,Some(vec![17,18,0]));let mut o=f.owner();o.parse_index(fs::metadata(&f.pack).unwrap().len()).unwrap();assert!(o.preflight().is_err());
 let mut output=Vec::new();assert!(apply_delta(b"x",&[1,1,0],1,&mut output,&clock(),&AtomicBool::new(false)).is_err());assert!(apply_delta(b"x",&[1,1,0x91,2,1],1,&mut output,&clock(),&AtomicBool::new(false)).is_err());
}
#[test]fn malformed_index_crc_pack_checksum_and_object_identity_are_independent_refusals(){
 let f=Pair::new(false);let mut o=f.owner();o.index_bytes[8]^=1;assert!(o.parse_index(fs::metadata(&f.pack).unwrap().len()).is_err());
 let f=Pair::new(false);let mut o=f.owner();let n=o.index_bytes.len();o.index_bytes[1032+40]^=1;let digest=sha1(&o.index_bytes[..n-20]).unwrap();o.index_bytes[n-20..].copy_from_slice(digest.as_bytes());o.parse_index(fs::metadata(&f.pack).unwrap().len()).unwrap();assert!(o.preflight().is_err());
 let f=Pair::new(false);let mut o=f.owner();o.parse_index(fs::metadata(&f.pack).unwrap().len()).unwrap();fs::set_permissions(&f.pack,fs::Permissions::from_mode(0o600)).unwrap();let file=OpenOptions::new().write(true).open(&f.pack).unwrap();file.write_all_at(b"FAIL",0).unwrap();assert!(o.verify_pack_hash().is_err());
 let f=Pair::new(false);let mut o=f.checked();let i=o.ids[&f.second];o.records[i].id=ObjectId::from_hex(b"1111111111111111111111111111111111111111").unwrap();assert!(o.decode(i).is_err());assert!(o.receipt().is_err());
}
fn binary_owner(entries:Vec<(ObjectId,Vec<u8>)>)->(tempfile::TempDir,PackedHistory){
 let tmp=tempfile::tempdir().unwrap();let root=tmp.path().canonicalize().unwrap();let mut raw=b"PACK\0\0\0\x02".to_vec();raw.extend((entries.len()as u32).to_be_bytes());let mut records=Vec::new();for(id,bytes)in entries{records.push((id,raw.len()as u64,gix::features::hash::crc32(&bytes)));raw.extend(bytes);}let hash=sha1(&raw).unwrap();raw.extend_from_slice(hash.as_bytes());let pack=root.join(format!("pack-{hash}.pack"));let index=pack.with_extension("idx");fs::write(&pack,&raw).unwrap();fs::write(&index,index_for(&records,hash)).unwrap();for p in [&pack,&index]{fs::set_permissions(p,fs::Permissions::from_mode(0o444)).unwrap();}
 let mut o=PackedHistory::stage(clock(),Arc::new(AtomicBool::new(false)));o.source=Some(HeldDir::open(root).unwrap());o.source_pack=Some(HeldFile::open(pack).unwrap());o.source_index=Some(HeldFile::open(index.clone()).unwrap());o.index_bytes=fs::read(index).unwrap();o.parse_index(raw.len()as u64).unwrap();o.verify_pack_hash().unwrap();(tmp,o)
}
#[test]fn delta_cycles_external_bases_depth_and_work_limits_are_predecode_gates(){
 let a=ObjectId::from_hex(b"1111111111111111111111111111111111111111").unwrap();let b=ObjectId::from_hex(b"2222222222222222222222222222222222222222").unwrap();
 let reference=|id:ObjectId|{let mut r=header(7,4);r.extend_from_slice(id.as_bytes());r.extend(compress(&[1,1,1,b'a']));r};
 let(_f,mut o)=binary_owner(vec![(a,reference(b)),(b,reference(a))]);assert!(o.preflight().is_err());assert!(o.active_bytes.is_empty());
 let(_f,mut o)=binary_owner(vec![(a,reference(b))]);assert!(o.preflight().is_err());
 let mut entries=Vec::new();let mut previous=None;for n in 0..=MAX_DELTA_DEPTH+1{let id=ObjectId::from_hex(format!("{n:040x}").as_bytes()).unwrap();let raw=if let Some(p)=previous{reference(p)}else{let mut r=header(3,1);r.extend(compress(b"a"));r};entries.push((id,raw));previous=Some(id);}let(_f,mut o)=binary_owner(entries);assert!(o.preflight().is_err());
 let mut raw=header(3,MAX_OBJECT_BYTES+1);raw.extend(compress(b"a"));let(_f,mut o)=binary_owner(vec![(a,raw)]);assert!(o.preflight().is_err());assert!(o.active_bytes.is_empty());
 assert_eq!(MAX_DELTA_DEPTH,64);assert_eq!(MAX_DECODE_BUFFERS,128<<20);assert!(o.charge(MAX_VERIFICATION_WORK+1).is_err());assert!(record_header(&[0xb0;64],12).is_err());assert!(varint(&[0x80;32],0).is_err());
}
#[test]fn zlib_tail_and_delta_copy_overflow_cannot_hide_inside_valid_pack_checksums(){
 let id=gix::objs::compute_hash(gix::hash::Kind::Sha1,Kind::Blob,b"a").unwrap();let mut raw=header(3,1);raw.extend(compress(b"a"));raw.push(0);let(_f,mut o)=binary_owner(vec![(id,raw)]);o.preflight().unwrap();assert!(o.decode(0).is_err());assert!(o.receipt().is_err());
 let mut out=Vec::new();assert!(apply_delta(b"a",&[1,1,0x91,0xff,0xff],1,&mut out,&clock(),&AtomicBool::new(false)).is_err());
}
#[test]fn original_fd_replacement_and_cancellation_cannot_create_a_receipt(){
 let f=Pair::new(false);let o=f.checked();let original_inode=o.source_pack.as_ref().unwrap().file.metadata().unwrap().ino();fs::rename(&f.pack,f.root.join("old.pack")).unwrap();fs::copy(f.root.join("old.pack"),&f.pack).unwrap();assert!(o.current().is_err());assert_eq!(o.source_pack.as_ref().unwrap().file.metadata().unwrap().ino(),original_inode);
 let f=Pair::new(false);let mut o=f.checked();o.original.cancel();assert!(o.decode(o.ids[&f.second]).is_err());assert!(o.receipt().is_err());
 assert_eq!(GitImportLimits::default().max_total_bytes,384<<20);assert_eq!(MAX_ENCODED_BYTES,384<<20);assert_eq!(MAX_VERIFICATION_WORK,1<<30);
}
fn write(repo:&gix::Repository,kind:Kind,bytes:&[u8])->ObjectId{repo.objects.write_stream(kind,bytes.len()as u64,&mut &bytes[..]).unwrap()}
fn tree(repo:&gix::Repository,a:ObjectId,b:ObjectId)->ObjectId{let mut bytes=Vec::new();for(name,id)in[("a",a),("b",b)]{bytes.extend_from_slice(format!("100644 {name}\0").as_bytes());bytes.extend_from_slice(id.as_bytes());}write(repo,Kind::Tree,&bytes)}
fn commit(repo:&gix::Repository,tree:ObjectId,parent:Option<ObjectId>)->ObjectId{let mut b=format!("tree {tree}\n");if let Some(p)=parent{b.push_str(&format!("parent {p}\n"));}b.push_str("author A <a@invalid.example> 1 +0000\ncommitter A <a@invalid.example> 1 +0000\n\nactual history\n");write(repo,Kind::Commit,b.as_bytes())}
struct Fixture{_root:tempfile::TempDir,source:gix::Repository,_workspace:Workspace,target:Arc<jj_lib::repo::ReadonlyRepo>,head:ObjectId,parent:ObjectId,objects:Vec<ObjectId>,unrelated:ObjectId}
impl Fixture{
 fn new()->Self{let root=tempfile::tempdir().unwrap();let source=gix::init_bare(root.path().join("source.git")).unwrap();let directory=source.path().join("objects/pack");fs::create_dir_all(&directory).unwrap();let first=Pair::new(false);let second=Pair::with_records(b"other packed base\n".to_vec(),b"Other packed base\n".to_vec(),true,None);
  for f in[&first,&second]{fs::copy(&f.pack,directory.join(f.pack.file_name().unwrap())).unwrap();fs::copy(&f.index,directory.join(f.index.file_name().unwrap())).unwrap();}
  let old_tree=tree(&source,first.first,second.first);let parent=commit(&source,old_tree,None);let new_tree=tree(&source,first.second,second.second);let head=commit(&source,new_tree,Some(parent));fs::write(source.path().join("HEAD"),format!("{head}\n")).unwrap();let unrelated=write(&source,Kind::Blob,b"unreachable source bytes must never be copied");
  let path=root.path().join("target");fs::create_dir(&path).unwrap();let(workspace,target)=pollster::block_on(Workspace::init_internal_git(&settings(),&path)).unwrap();Self{_root:root,source,_workspace:workspace,target,head,parent,objects:vec![first.first,first.second,second.first,second.second,old_tree,parent,new_tree,head],unrelated}}
 fn backend(&self)->&GitBackend{self.target.store().backend_impl::<GitBackend>().unwrap()}
 fn pack_dir(&self)->PathBuf{self.backend().git_repo_path().join("objects/pack")}
 fn run(&self,out:&mut Option<RetainedGitImport>,original:&FinalizationBudget,cancel:Arc<AtomicBool>,guard:impl FnMut()->Result<()>)->Result<ImportedGitHeadData>{super::super::import_expected_git_head_profile_retaining(out,&self.source,self.backend(),&GitCommitId(self.head.to_string()),GitImportLimits::default(),GitHistoryProfile::PackedHistoryV2,original,cancel,guard)}
 fn has(&self,suffix:&str)->bool{fs::read_dir(self.pack_dir()).ok().is_some_and(|entries|entries.filter_map(|e|e.ok()).any(|e|e.path().extension().is_some_and(|x|x==suffix)))}
}
#[test]#[cfg(target_os="linux")]
fn actual_mixed_packs_and_loose_history_normalizes_once_and_jj_reads_exact_originals(){let f=Fixture::new();let before=pollster::block_on(f.target.loader().op_heads_store().get_op_heads()).unwrap();let mut out=None;let original=clock();let data=f.run(&mut out,&original,Arc::new(AtomicBool::new(false)),||Ok(())).unwrap();let owner=out.as_ref().unwrap();assert_eq!(owner.history_profile(),GitHistoryProfile::PackedHistoryV2);assert_eq!(data.objects,8);let packed=owner.packed_data().unwrap();assert!(packed.installed&&packed.canonical_child_reaped_and_all_pipes_eof);assert_eq!(packed.verification_work_limit_bytes,1<<30);assert!(packed.encoded_bytes<=384<<20&&packed.verification_work_bytes<=1<<30);
 let actual=gix::open(f.backend().git_repo_path()).unwrap();for id in &f.objects{let expected=f.source.find_object(*id).unwrap();let got=actual.find_object(*id).unwrap();assert_eq!(got.kind,expected.kind);assert_eq!(got.data,expected.data);}assert!(actual.find_object(f.unrelated).is_err());let commit=f.target.store().get_commit(&CommitId::new(f.head.as_bytes().to_vec())).unwrap();assert_eq!(commit.parent_ids(),&[CommitId::new(f.parent.as_bytes().to_vec())]);assert_eq!(pollster::block_on(f.target.loader().op_heads_store().get_op_heads()).unwrap(),before);assert!(!actual.path().join("objects/info/alternates").exists());assert!(f.has("keep")&&f.has("pack")&&f.has("idx"));}
#[test]#[cfg(target_os="linux")]
fn actual_late_cancel_and_panic_retain_original_installed_fds_without_ack_or_replay(){let f=Fixture::new();let mut out=None;let original=clock();let cancel=Arc::new(AtomicBool::new(false));assert!(f.run(&mut out,&original,cancel.clone(),||{if f.has("idx"){cancel.store(true,Ordering::Release);anyhow::bail!("late original publication guard");}Ok(())}).is_err());assert!(f.has("idx")&&out.as_ref().unwrap().data().is_err()&&out.as_ref().unwrap().packed_data().is_err());let packed=out.as_ref().unwrap().packed.as_ref().unwrap();assert!(packed.installed.iter().all(Option::is_some));assert!(f.run(&mut out,&clock(),Arc::new(AtomicBool::new(false)),||Ok(())).is_err());
 let f=Fixture::new();let mut out=None;let result=catch_unwind(AssertUnwindSafe(||{let _=f.run(&mut out,&clock(),Arc::new(AtomicBool::new(false)),||{if f.has("pack"){panic!("actual publication unwind")}Ok(())});}));assert!(result.is_err());assert!(out.as_ref().unwrap().data().is_err());assert!(out.as_ref().unwrap().packed.as_ref().unwrap().installed[1].is_some());}
#[test]#[cfg(target_os="linux")]
fn actual_owner_refusal_and_original_cancellation_keep_children_for_cleanup_only(){
 let f=Fixture::new();let original=clock();let cancel=Arc::new(AtomicBool::new(false));let mut canonical=canonical::CanonicalInput::stage(original.clone(),cancel);let mut calls=0;
 assert!(canonical.produce(&f.source,f.backend(),f.head,&mut||{calls+=1;if calls==3{original.cancel();anyhow::bail!("actual original owner cancellation after spawn")}Ok(())}).is_err());assert_eq!(calls,3);assert!(canonical.original_child_retained_for_test());
 canonical.request_kill_retaining();let end=Instant::now()+Duration::from_secs(5);while !canonical.poll_settlement().unwrap_or(false){assert!(Instant::now()<end,"actual cleanup child not settled");std::thread::sleep(Duration::from_millis(2));}assert!(!f.has("idx"));assert!(!canonical.settled_data());
}
#[test]#[cfg(target_os="linux")]
fn actual_changed_head_alternates_and_shallow_never_publish(){for case in["head","alternate","shallow"]{let f=Fixture::new();match case{"head"=>fs::write(f.source.path().join("HEAD"),format!("{}\n",f.parent)).unwrap(),"alternate"=>{fs::create_dir_all(f.source.path().join("objects/info")).unwrap();fs::write(f.source.path().join("objects/info/alternates"),b"/outside\n").unwrap();},_=>fs::write(f.source.path().join("shallow"),format!("{}\n",f.parent)).unwrap()};let mut out=None;assert!(f.run(&mut out,&clock(),Arc::new(AtomicBool::new(false)),||Ok(())).is_err());assert!(!f.has("idx"));assert!(out.as_ref().unwrap().data().is_err());}}

#[test]#[cfg(target_os="linux")]
#[ignore="actual >384MiB reconstructed history; bounded owned /workspace fixture under original30s"]
fn actual_repetitive_large_history_uses_encoded_plane_without_raising_raw_v1_limit(){
 let root=tempfile::tempdir_in("/workspace").unwrap();let source=gix::init_bare(root.path().join("source.git")).unwrap();let mut bytes=vec![b'x';4<<20];let mut parent=None;let mut reconstructed=0u64;
 for n in 0..104u64{bytes[..8].copy_from_slice(&n.to_le_bytes());let blob=write(&source,Kind::Blob,&bytes);let mut tree_bytes=b"100644 tracked\0".to_vec();tree_bytes.extend_from_slice(blob.as_bytes());let tree=write(&source,Kind::Tree,&tree_bytes);let id=commit(&source,tree,parent);parent=Some(id);reconstructed+=bytes.len()as u64;}
 let head=parent.unwrap();fs::write(source.path().join("HEAD"),format!("{head}\n")).unwrap();let target=root.path().join("target");fs::create_dir(&target).unwrap();let(_workspace,repo)=pollster::block_on(Workspace::init_internal_git(&settings(),&target)).unwrap();let backend=repo.store().backend_impl::<GitBackend>().unwrap();let original=clock();let constraint=original.constraint();let mut owner=None;
 let data=super::super::import_expected_git_head_profile_retaining(&mut owner,&source,backend,&GitCommitId(head.to_string()),GitImportLimits::default(),GitHistoryProfile::PackedHistoryV2,&original,Arc::new(AtomicBool::new(false)),||Ok(())).unwrap();
 let packed=owner.as_ref().unwrap().packed_data().unwrap();assert!(data.bytes>=reconstructed&&data.bytes>384<<20);assert!(packed.encoded_bytes<384<<20&&packed.verification_work_bytes<=1<<30);assert!(packed.canonical_child_reaped_and_all_pipes_eof);assert_eq!(original.constraint(),constraint);assert_eq!(packed.original_constraint,constraint);original.remaining().unwrap();assert_eq!(GitImportLimits::default().max_total_bytes,384<<20);assert_eq!(data.commits,104);assert_eq!(repo.store().get_commit(&CommitId::new(head.as_bytes().to_vec())).unwrap().parent_ids().len(),1);
}

#[test]fn multi_byte_ofs_distances_and_pack_size_headers_roundtrip_without_payload_allocation(){
 for distance in[1,127,128,129,16384,1<<20,MAX_ENCODED_BYTES-1]{let mut bytes=header(6,12345);bytes.extend(ofs(distance));let(base,size,used)=record_header(&bytes,distance+12).unwrap();assert!(matches!(base,Base::Offset(12)));assert_eq!(size,12345);assert_eq!(used,bytes.len());}
}
