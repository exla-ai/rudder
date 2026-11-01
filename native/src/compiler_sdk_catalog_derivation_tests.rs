//! Constructed member DATA only. No VerifiedBakedInputs, Root proof, SDK floor
//! or original helper ACK can be made by these fixtures.
use super::*;
use crate::image_preseed_data::ImageFileData;
use rudder_engine::execution::compiler_sdk::{compiler_sdk_profiles_data,C1_PROFILE};
fn spec()->&'static CompilerSdkToolSpecData{compiler_sdk_profiles_data().iter().find(|p|p.profile_id==C1_PROFILE).unwrap().tools.iter().find(|s|s.name=="cc").unwrap()}
fn reference(label:&[u8])->rudder_engine::protocol::ObjectRef{rudder_engine::protocol::ObjectRef::from_bytes(rudder_engine::protocol::ObjectKind::Blob,label)}
fn row()->ImageFileData{ImageFileData{path:PathBuf::from("/rudder-compiler-c1/prepared/usr/bin/cc"),sha256:digest(b"constructed member"),size_bytes:b"constructed member".len()as u64,content:reference(b"constructed member")}}
#[test]fn c1_selected_path_matches_only_same_actual_installer_content_size_and_immutable_mode(){
 let row=row();let actual=CompilerFileBindingData{content:row.content.clone(),logical_size:row.size_bytes,mode:0o555};let member=member_data("/rudder-compiler-c1",spec(),&[row.clone()],actual.clone()).unwrap();assert_eq!(member.name,"cc");assert_eq!(member.content,row.content);assert_eq!(member.raw_sha256,row.sha256);
 for change in 0..3{let mut bad=actual.clone();match change{0=>bad.content=reference(b"different"),1=>bad.logical_size+=1,_=>bad.mode=0o755};assert!(member_data("/rudder-compiler-c1",spec(),&[row.clone()],bad).is_err());}
}
#[test]fn c1_cannot_resolve_a2_prefixed_duplicate_missing_or_foreign_manifest_member(){
 let row=row();let actual=CompilerFileBindingData{content:row.content.clone(),logical_size:row.size_bytes,mode:0o555};assert!(member_data("/rudder-compiler-v1",spec(),&[row.clone()],actual.clone()).is_err());assert!(member_data("/rudder-compiler-c1",spec(),&[row.clone(),row.clone()],actual.clone()).is_err());assert!(member_data("/rudder-compiler-c1",spec(),&[],actual).is_err());
}
