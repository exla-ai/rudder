//! Deterministic packing DATA only, no image/Source authority from references.
use super::*;
use rudder_engine::protocol::ObjectKind;
fn r(i:usize,size:u64)->ObjectRef{let mut r=ObjectRef::from_bytes(ObjectKind::Blob,i.to_string().as_bytes());r.digest=format!("{i:064x}");r.size_bytes=size;r}
#[test]
fn image_batch_packing_respects_reference_count_and_byte_cap(){let refs=(0..1025).map(|i|r(i,1)).collect::<Vec<_>>();let m=r(9999,1);assert_eq!(image_batch_end(&refs,0,&m).unwrap(),1024);assert_eq!(image_batch_end(&refs,1024,&m).unwrap(),1025);
    let refs=(0..9).map(|i|r(i,4194304)).collect::<Vec<_>>();assert_eq!(image_batch_end(&refs,0,&m).unwrap(),8);}
#[test]
fn image_batch_packing_isolates_only_the_exact_large_manifest(){let m=r(1,5673335);let refs=vec![r(0,1),m.clone(),r(2,2)];assert_eq!(image_batch_end(&refs,0,&m).unwrap(),1);assert_eq!(image_batch_end(&refs,1,&m).unwrap(),2);assert_eq!(image_batch_end(&refs,2,&m).unwrap(),3);assert!(image_batch_end(&[r(7,4194305)],0,&m).is_err());}
#[test]
fn image_batch_synthetic_distribution_covers_measured_union_without_claiming_measured_packing(){
    // Actual Root diagnostic totals; object SIZE DISTRIBUTION here is synthetic.
    let count=22175usize;let total=1968404726u64;let manifest=r(count-1,6000000);let each=(total-manifest.size_bytes)/(count as u64-1);let extra=(total-manifest.size_bytes)%(count as u64-1);
    let mut refs=(0..count-1).map(|i|r(i,each+if i==0{extra}else{0})).collect::<Vec<_>>();refs.push(manifest.clone());assert_eq!(refs.len(),count);assert_eq!(refs.iter().map(|r|r.size_bytes).sum::<u64>(),total);
    let(mut cursor,mut batches)=(0,0);while cursor<refs.len(){let end=image_batch_end(&refs,cursor,&manifest).unwrap();assert_eq!(end,image_batch_end(&refs,cursor,&manifest).unwrap());assert!(end-cursor<=1024);assert!(refs[cursor..end].iter().map(|r|r.size_bytes).sum::<u64>()<=33554432);if refs[cursor..end].contains(&manifest){assert_eq!(end-cursor,1);}cursor=end;batches+=1;}assert_eq!(cursor,count);assert!(batches>1); // No latency or actual-image batch-count claim.
}
