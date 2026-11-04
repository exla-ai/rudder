//! Compiler SDK identity DATA only. Real execution additionally requires the
//! original Root runtime capsule and held SDK floor descriptor. No JSON grant.
use anyhow::{ensure,Result};
use crate::{build::ToolchainIdentity,protocol::{ObjectKind,ObjectRef}};
use serde::{Serialize,Deserialize};
use std::collections::BTreeMap;

#[path="compiler_sdk_semantics.rs"]
mod semantic;
pub use semantic::CompilerSdkSemanticData;
#[path="compiler_sdk_catalog.rs"]mod catalog;
pub use catalog::{compiler_sdk_profiles_data,CompilerSdkProfileData,CompilerSdkToolSpecData,CompilerSdkToolMemberData,A2_PROFILE,C1_PROFILE,C1_ARTIFACT_MANIFEST};

pub const SDK_SOURCE:&str="/opt/rudder-worker/compiler-sdk-v1";
pub const SDK_GUEST:&str="/opt/runtime/sdk/v1";
pub const ARTIFACT_MANIFEST:&str="add56ea6ade5411145b026094f63d8d5838e38b58d397d09749027090d346d0c";
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompilerSdkCatalogData {
    pub format_version:u32,pub profile_id:String,pub artifact_image:String,pub cas_image:String,
    pub artifact_manifest:ObjectRef,pub inventory:ObjectRef,pub manifest:ObjectRef,pub installer_input:ObjectRef,
    pub source_binding_sha256:String,
}
impl CompilerSdkCatalogData {
    pub fn validate_data(&self)->Result<()> {self.profile_data()?;Ok(())}

}
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompilerSdkMountData {
    pub catalog:CompilerSdkCatalogData,pub semantic:CompilerSdkSemanticData,pub floor:super::runtime_image::RuntimeFdData,
    pub toolchains:BTreeMap<String,ToolchainIdentity>,
}
impl CompilerSdkMountData {
    pub fn validate_data(&self)->Result<()> {
        self.catalog.validate_data()?;self.semantic.validate_data()?;ensure!(self.floor.directory&&self.floor.mount_id>0,"compiler SDK floor identity absent");
        let expected=self.catalog.profile_data()?.tools;
        ensure!(self.toolchains.len()==expected.len(),"compiler SDK selected tool set differs");
        for spec in expected {let name=spec.name;let path=spec.relative_path;
            let tool=self.toolchains.get(name).ok_or_else(||anyhow::anyhow!("compiler SDK selected tool absent"))?;
            ensure!(tool.closure==self.semantic.directory&&tool.version==self.semantic.directory.digest&&tool.executable_path==path
                &&matches!(tool.executable.kind,ObjectKind::Blob|ObjectKind::File),"compiler SDK tool identity is not its complete closure");
            tool.executable.validate()?;
            ensure!(tool.configuration.get("executable_raw_sha256").is_some_and(|s|s.len()==64&&s.bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b)))
                &&tool.configuration.len()==(if spec.compiler_entrypoint.is_some(){2}else{1})
                &&spec.compiler_entrypoint.is_none_or(|entry|tool.configuration.get("compiler_entrypoint").map(String::as_str)==Some(entry)),"compiler SDK semantic configuration contains unrelated control input");
            if name=="buck2"{ensure!(tool.executable==self.semantic.buck2,"SDK static frontend content differs from its actual semantic member");}
        }Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn catalog()->CompilerSdkCatalogData {
        let r=|kind,digest:&str,size_bytes|ObjectRef{kind,algorithm:crate::protocol::HashAlgorithm::Sha256,digest:digest.into(),size_bytes};
        CompilerSdkCatalogData{format_version:1,profile_id:"rudder-linux-rust-typescript-v1".into(),
            artifact_image:"registry.fly.io/rudder-workers-exla@sha256:88d25bcd2d359fa6c829a9126d96011b17d197b23307f997e2406e00bc5d562f".into(),
            cas_image:"registry.fly.io/rudder-workers-exla@sha256:fdb88bde30c78585e9c932c434bf8ed5c82f47ceac41c0ff23fbe9962147860f".into(),
            artifact_manifest:r(ObjectKind::Blob,ARTIFACT_MANIFEST,190431),inventory:r(ObjectKind::Blob,"e2d9b70e1f90e56ec69cf0dd7cfd6f5ba75e0d7a71af9fd214c1369ab906ad9e",176107),
            manifest:r(ObjectKind::Blob,"914d7fa727db69b40b30060ab92216e0bebf02aa032c21c39633ae0acee708ee",315690),
            installer_input:r(ObjectKind::Directory,"ba237b8a5cbf5b20d9a0c479d82624e2a98e012e7291c32b631425185ce3ef0b",230),
            source_binding_sha256:"b8469769a78dd68ce7b730a2dc865b13c22d63e538b0166ba65b094a279bbc4c".into()}
    }
    #[test]fn compiler_catalog_is_exact_distinct_data_and_refuses_runtime_or_old_images(){
        let c=catalog();c.validate_data().unwrap();let mut wrong=c.clone();wrong.artifact_manifest.digest="0".repeat(64);assert!(wrong.validate_data().is_err());
        let mut wrong=c;wrong.cas_image=wrong.artifact_image.clone();assert!(wrong.validate_data().is_err());
    }
    #[test]fn compiler_tool_identity_rejects_incomplete_sdk_and_ephemeral_configuration(){
        let c=catalog();let(semantic,_)=CompilerSdkSemanticData::from_members_data(c.installer_input.clone(),c.artifact_manifest.clone(),c.artifact_manifest.size_bytes,0o555).unwrap();let mut mount=CompilerSdkMountData{catalog:c.clone(),semantic,floor:super::super::runtime_image::RuntimeFdData{device:1,inode:2,mount_id:3,directory:true},toolchains:BTreeMap::new()};
        assert!(mount.validate_data().is_err());
        for(name,path)in[("buck2","buck2"),("rustc","prepared/tools/rust/bin/rustc"),("node","prepared/tools/node"),("typescript","prepared/tools/node"),("python","prepared/tools/python/bin/python3")]{
            mount.toolchains.insert(name.into(),ToolchainIdentity{version:mount.semantic.directory.digest.clone(),closure:mount.semantic.directory.clone(),executable:c.artifact_manifest.clone(),executable_path:path.into(),
                configuration:BTreeMap::from([("executable_raw_sha256".into(),"a".repeat(64))])});
            if name=="typescript"{mount.toolchains.get_mut(name).unwrap().configuration.insert("compiler_entrypoint".into(),"prepared/tools/typescript/lib/tsc.js".into());}
        }
        mount.validate_data().unwrap();mount.toolchains.get_mut("rustc").unwrap().configuration.insert("registration_proof_sha256".into(),"a".repeat(64));assert!(mount.validate_data().is_err());
    }
}

#[cfg(test)]#[path="compiler_sdk_catalog_tests.rs"]mod catalog_tests;
