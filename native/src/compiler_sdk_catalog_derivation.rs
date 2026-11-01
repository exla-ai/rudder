//! Metadata derivation child of the genuine Root SDK owner. This never creates
//! a Root, floor, execution, retention or client capability from catalogue DATA.
use super::*;
use rudder_engine::{execution::compiler_sdk::{CompilerSdkSemanticData,CompilerSdkToolMemberData,CompilerSdkToolSpecData},
 store::{ImageImportFence,backed_inputs::CompilerFileBindingData}};

pub(super) fn derive_toolchains_data(inputs:&VerifiedBakedInputs,catalog:&CompilerSdkCatalogData,semantic:&CompilerSdkSemanticData,
    manifest:&crate::image_preseed_data::ImageManifestData,original:&dyn ImageImportFence)->Result<BTreeMap<String,ToolchainIdentity>>{
    original.remaining()?;catalog.validate_data()?;semantic.validate_data()?;inputs.current(original)?;
    let profile=catalog.profile_data()?;
    ensure!(manifest.format_version==1&&manifest.installer_input==catalog.installer_input,"SDK full manifest differs from selected catalogue");
    let binding=inputs.binding_data()?;ensure!(binding.inventory==catalog.inventory&&binding.manifest==catalog.manifest&&binding.installer_input==catalog.installer_input,"SDK selected files differ from original retained whole graph");
    let mut members=Vec::new();
    for spec in profile.tools {
        original.remaining()?;let actual=inputs.compiler_file_content_data(catalog,spec.relative_path,original)?;
        members.push(member_data(profile.artifact_directory,spec,&manifest.files,actual)?);
        // The entrypoint is separate from the interpreter. Full prepared root
        // remains the identity, and the original graph must contain this script.
        if let Some(script)=spec.compiler_entrypoint {
            let actual=inputs.compiler_file_content_data(catalog,script,original)?;
            let file=unique_file(profile.artifact_directory,script,&manifest.files)?;
            ensure!(actual.content==file.content&&actual.logical_size==file.size_bytes&&matches!(actual.mode,0o444|0o555),"SDK compiler script differs from original immutable entry");
        }
    }
    let tools=catalog.toolchains_from_members_data(semantic,&members)?;inputs.current(original)?;original.remaining()?;Ok(tools)
}
fn unique_file<'a>(artifact_directory:&str,relative:&str,files:&'a[crate::image_preseed_data::ImageFileData])->Result<&'a crate::image_preseed_data::ImageFileData>{
    let expected=Path::new(artifact_directory).join(relative);let mut found=files.iter().filter(|f|f.path==expected);
    let file=found.next().context("SDK complete manifest lacks selected member")?;ensure!(found.next().is_none(),"SDK complete manifest duplicates selected member");Ok(file)
}
fn member_data(artifact_directory:&str,spec:&CompilerSdkToolSpecData,files:&[crate::image_preseed_data::ImageFileData],actual:CompilerFileBindingData)->Result<CompilerSdkToolMemberData>{
    let file=unique_file(artifact_directory,spec.relative_path,files)?;
    ensure!(actual.content==file.content&&actual.logical_size==file.size_bytes&&actual.mode==0o555,"SDK full manifest and actual installer member differ");
    Ok(CompilerSdkToolMemberData{name:spec.name.into(),relative_path:spec.relative_path.into(),content:actual.content,logical_size:actual.logical_size,mode:actual.mode,raw_sha256:file.sha256.clone()})
}
#[cfg(test)]#[path="compiler_sdk_catalog_derivation_tests.rs"]mod tests;
