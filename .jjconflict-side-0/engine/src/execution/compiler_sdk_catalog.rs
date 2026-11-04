//! Finite immutable catalogue/overlay DATA, never an SDK mount, Root grant or
//! execution policy. Every supplied catalogue must exactly match one descriptor.
use super::*;
use crate::protocol::HashAlgorithm;
pub const A2_PROFILE:&str="rudder-linux-rust-typescript-v1";
pub const C1_PROFILE:&str="rudder-linux-rust-cxx-typescript-c1";
pub const C1_ARTIFACT_MANIFEST:&str="bb70f8795da717a73f9d9ded535149f1fb4abff84374b12188361f1cc4a8106a";
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub struct CompilerSdkToolSpecData{pub name:&'static str,pub relative_path:&'static str,pub compiler_entrypoint:Option<&'static str>}
const fn tool(name:&'static str,path:&'static str)->CompilerSdkToolSpecData{CompilerSdkToolSpecData{name,relative_path:path,compiler_entrypoint:None}}
const A2_TOOLS:[CompilerSdkToolSpecData;5]=[tool("buck2","buck2"),tool("rustc","prepared/tools/rust/bin/rustc"),tool("node","prepared/tools/node"),
 CompilerSdkToolSpecData{name:"typescript",relative_path:"prepared/tools/node",compiler_entrypoint:Some("prepared/tools/typescript/lib/tsc.js")},tool("python","prepared/tools/python/bin/python3")];
const C1_TOOLS:[CompilerSdkToolSpecData;9]=[A2_TOOLS[0],A2_TOOLS[1],A2_TOOLS[2],A2_TOOLS[3],A2_TOOLS[4],
 tool("cargo","prepared/tools/rust/bin/cargo"),tool("cc","prepared/usr/bin/cc"),tool("cxx","prepared/usr/bin/c++"),tool("ar","prepared/usr/bin/ar")];
#[derive(Clone,Copy,Debug)]
struct Pin{kind:ObjectKind,digest:&'static str,size:u64}
impl Pin{fn data(self)->ObjectRef{ObjectRef{kind:self.kind,algorithm:HashAlgorithm::Sha256,digest:self.digest.into(),size_bytes:self.size}}}
#[derive(Clone,Copy,Debug)]
pub struct CompilerSdkProfileData{pub profile_id:&'static str,pub artifact_directory:&'static str,pub cas_directory:&'static str,
 pub source_mount:&'static str,pub guest_mount:&'static str,pub tools:&'static[CompilerSdkToolSpecData],artifact_image:&'static str,cas_image:&'static str,
 artifact_manifest:Pin,inventory:Pin,manifest:Pin,installer:Pin,source_binding:&'static str}
const fn blob(digest:&'static str,size:u64)->Pin{Pin{kind:ObjectKind::Blob,digest,size}}
const fn directory(digest:&'static str,size:u64)->Pin{Pin{kind:ObjectKind::Directory,digest,size}}
static PROFILES:[CompilerSdkProfileData;2]=[
 CompilerSdkProfileData{profile_id:A2_PROFILE,artifact_directory:"/rudder-compiler-v1",cas_directory:"/var/lib/rudder-compiler-cas-v1",source_mount:SDK_SOURCE,guest_mount:SDK_GUEST,tools:&A2_TOOLS,
  artifact_image:"registry.fly.io/rudder-workers-exla@sha256:88d25bcd2d359fa6c829a9126d96011b17d197b23307f997e2406e00bc5d562f",cas_image:"registry.fly.io/rudder-workers-exla@sha256:fdb88bde30c78585e9c932c434bf8ed5c82f47ceac41c0ff23fbe9962147860f",
  artifact_manifest:blob(ARTIFACT_MANIFEST,190431),inventory:blob("e2d9b70e1f90e56ec69cf0dd7cfd6f5ba75e0d7a71af9fd214c1369ab906ad9e",176107),manifest:blob("914d7fa727db69b40b30060ab92216e0bebf02aa032c21c39633ae0acee708ee",315690),installer:directory("ba237b8a5cbf5b20d9a0c479d82624e2a98e012e7291c32b631425185ce3ef0b",230),source_binding:"b8469769a78dd68ce7b730a2dc865b13c22d63e538b0166ba65b094a279bbc4c"},
 CompilerSdkProfileData{profile_id:C1_PROFILE,artifact_directory:"/rudder-compiler-c1",cas_directory:"/var/lib/rudder-compiler-c1-cas-v1",source_mount:SDK_SOURCE,guest_mount:SDK_GUEST,tools:&C1_TOOLS,
  artifact_image:"registry.fly.io/rudder-workers-exla@sha256:469e5b3667cd673bab2674fcb30021f2c731555a8cc3326519dbb7c4cab73273",cas_image:"registry.fly.io/rudder-workers-exla@sha256:19ce471b0c894dcd820135d1d97d2a1f5a355938cd339acedd8373efa86e070e",
  artifact_manifest:blob(C1_ARTIFACT_MANIFEST,1065125),inventory:blob("4f8d57a0ffd78335a9ca9863d26040379d65dc8942fa32d41e217f898612f832",850145),manifest:blob("d60afb84b6e373fece5fd2a0babb1b66bd0f4c637efddf766f59a180e39ec54a",1830173),installer:directory("7deff164fb058949352597c656660878bbcb8d19d2ef9b6a6c23bba9bbf261e5",230),source_binding:"301ec13415fa57e2623571adce9327b4e406d558698a1fa46adfb7e04f5fbbc0"},
];
impl CompilerSdkProfileData{
 /// Recipe/catalogue DATA only. A known tuple never substitutes for actual
 /// retained verified graph, original Root admission or completed role proof.
 pub fn catalog_data(&self)->CompilerSdkCatalogData{CompilerSdkCatalogData{format_version:1,profile_id:self.profile_id.into(),artifact_image:self.artifact_image.into(),cas_image:self.cas_image.into(),artifact_manifest:self.artifact_manifest.data(),inventory:self.inventory.data(),manifest:self.manifest.data(),installer_input:self.installer.data(),source_binding_sha256:self.source_binding.into()}}
}
pub fn compiler_sdk_profiles_data()->&'static[CompilerSdkProfileData]{&PROFILES}
impl CompilerSdkCatalogData{
 pub fn profile_data(&self)->Result<&'static CompilerSdkProfileData>{let profile=PROFILES.iter().find(|p|p.profile_id==self.profile_id).ok_or_else(||anyhow::anyhow!("compiler SDK profile differs"))?;
  ensure!(self==&profile.catalog_data(),"compiler SDK immutable content/source tuple differs");for reference in[&self.artifact_manifest,&self.inventory,&self.manifest,&self.installer_input]{reference.validate()?;}Ok(profile)}
}
/// Content metadata only. The Root producer independently resolves each member
/// through the SAME actual VerifiedBakedInputs; this vector is not that owner.
#[derive(Clone,Debug,PartialEq,Eq)]
pub struct CompilerSdkToolMemberData{pub name:String,pub relative_path:String,pub content:ObjectRef,pub logical_size:u64,pub mode:u32,pub raw_sha256:String}
impl CompilerSdkCatalogData{
 pub fn toolchains_from_members_data(&self,semantic:&CompilerSdkSemanticData,members:&[CompilerSdkToolMemberData])->Result<BTreeMap<String,ToolchainIdentity>>{
  let profile=self.profile_data()?;semantic.validate_data()?;ensure!(members.len()==profile.tools.len(),"SDK exact member identity set differs");let mut by_name=BTreeMap::new();for member in members{ensure!(by_name.insert(member.name.as_str(),member).is_none(),"SDK duplicate selected identity");}
  let mut tools=BTreeMap::new();for spec in profile.tools{let member=*by_name.get(spec.name).ok_or_else(||anyhow::anyhow!("SDK actual selected member absent"))?;member.content.validate()?;
   ensure!(member.relative_path==spec.relative_path&&matches!(member.content.kind,ObjectKind::Blob|ObjectKind::File)&&member.logical_size>0&&member.logical_size<=2*1024*1024*1024&&member.mode==0o555
    &&(member.content.kind!=ObjectKind::Blob||member.content.size_bytes==member.logical_size)&&member.raw_sha256.len()==64&&member.raw_sha256.bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b)),"SDK actual selected member type/mode/hash differs");
   if spec.name=="buck2"{ensure!(member.content==semantic.buck2&&member.logical_size==semantic.buck2_logical_size&&member.mode==semantic.buck2_mode,"SDK frontend differs from actual semantic member");}
   let mut configuration=BTreeMap::from([("executable_raw_sha256".into(),member.raw_sha256.clone())]);if let Some(entry)=spec.compiler_entrypoint{configuration.insert("compiler_entrypoint".into(),entry.into());}
   tools.insert(spec.name.into(),ToolchainIdentity{version:semantic.directory.digest.clone(),closure:semantic.directory.clone(),executable:member.content.clone(),executable_path:spec.relative_path.into(),configuration});
  }Ok(tools)
 }
}
