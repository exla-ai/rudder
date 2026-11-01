//! Fixed verification source DATA inside an ACTUAL new FreshLocalNamespace.
//! The profile is not namespace/source/SDK authority. Only the existing genuine
//! preparation/environment/client chain can consume it before the first head.
use super::*;
use rudder_native::{host_ingress::SdkInitialSourceProfile,root_storage_bootstrap::runtime_image::CompilerSdkClient};
use rudder_engine::build::PreparationSpec;
use std::collections::BTreeMap;
pub(super) struct FixedInitialSource {preparation:Arc<FreshProjectPreparation>,sdk:Option<Arc<CompilerSdkClient>>,original:FinalizationBudget,
    files:Option<BTreeMap<String,Vec<u8>>>,ready:bool}
impl FixedInitialSource {
    pub(super) fn capture_retaining(out:&mut Option<Self>,environment:&Arc<ManagedFreshEnvironment>,original:&FinalizationBudget,cancel:&AtomicBool)->Result<()> {
        ensure!(out.is_none(),"fixed initial source carrier occupied");let config=environment.machine().config();
        let Some(profile)=config.data().sdk_initial_source else{return Ok(())};
        *out=Some(Self{preparation:environment.preparation().clone(),sdk:None,original:original.clone(),files:None,ready:false});let held=out.as_mut().unwrap();
        time(original,cancel)?;environment.current(original,cancel)?;held.preparation.assert_current()?;
        ensure!(profile==SdkInitialSourceProfile::FixedSuiteV1&&config.data().runtime_qualification.is_none()&&config.data().compiler_qualification.is_none(),"fixed initial source is ordinary-only");
        let backing=environment.runtime_backing().context("fixed initial source needs actual original runtime backing")?;
        held.sdk=Some(backing.compiler_sdk_client(original,cancel)?);let sdk=held.sdk.as_ref().unwrap();
        sdk.runtime_client().assert_environment_scope(config,held.preparation.registration())?;sdk.current(original)?;sdk.require_ordinary_execution()?;
        held.files=Some(files_data(sdk.preparation_spec_data()?)?);environment.current(original,cancel)?;held.preparation.assert_current()?;time(original,cancel)?;held.ready=true;Ok(())
    }
    pub(super) fn files(&self,cancel:&AtomicBool)->Result<&BTreeMap<String,Vec<u8>>>{time(&self.original,cancel)?;ensure!(self.ready,"original fixed initial source incomplete");self.preparation.assert_current()?;
        let sdk=self.sdk.as_ref().context("original fixed-source SDK absent")?;sdk.current(&self.original)?;sdk.require_ordinary_execution()?;self.files.as_ref().context("fixed source bytes absent")}
}
/// Pure bounded DATA, not a root, owner, accepted source or Root grant factory.
fn files_data(sdk:&PreparationSpec)->Result<BTreeMap<String,Vec<u8>>>{sdk.validate()?;let buck=sdk.toolchains.get("buck2").context("actual SDK frontend identity absent")?;
    let sha=buck.configuration.get("executable_raw_sha256").context("actual SDK frontend raw SHA absent")?;
    ensure!(sha.len()==64&&sha.bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b)),"SDK frontend SHA malformed");
    let mut files=BTreeMap::from([
        ("lib.rs".into(),b"pub fn answer() -> u32 { 42 }\n".to_vec()),
        ("main.ts".into(),b"export const answer: number = 42;\n".to_vec()),
        ("sdk-probes/proc.rs".into(),b"pub const PROBE: &str = include_str!(\"/proc/sys/kernel/random/uuid\");\n".to_vec()),
        ("sdk-probes/job.rs".into(),b"pub const PROBE: &str = include_str!(\"/workspace/job.json\");\n".to_vec()),
    ]);
    files.insert("rudder.toml".into(),policy_data(sdk,&buck.version,sha,&[("library","rust","lib.rs"),("javascript","typescript","main.ts")])?);
    files.insert("sdk-probes/proc.toml".into(),policy_data(sdk,&buck.version,sha,&[("denied","rust","sdk-probes/proc.rs")])?);
    files.insert("sdk-probes/job.toml".into(),policy_data(sdk,&buck.version,sha,&[("denied","rust","sdk-probes/job.rs")])?);
    ensure!(files.len()==7&&files.values().map(Vec::len).sum::<usize>()<=32*1024,"fixed SDK source suite bound");Ok(files)
}
fn policy_data(sdk:&PreparationSpec,release:&str,sha:&str,targets:&[(&str,&str,&str)])->Result<Vec<u8>> {
    let q=|s:&str|serde_json::to_string(s);let mut raw=format!("format_version=1\nname=\"operator-sdk-suite-v1\"\n[platform]\nos={}\narchitecture={}\nabi={}\n[buck2]\nrelease={}\nexecutable_sha256={}\n[policy]\nversion=\"operator-sdk-source-v1\"\neffects=[]\n",q(&sdk.platform.os)?,q(&sdk.platform.architecture)?,q(&sdk.platform.abi)?,q(release)?,q(sha)?);
    ensure!(sdk.platform.properties.is_empty(),"fixed initial source does not silently omit SDK platform properties");
    for(name,language,path)in targets{raw.push_str(&format!("[[targets]]\nname={}\nlanguage={}\nentrypoint={}\nsources=[{}]\n",q(name)?,q(language)?,q(path)?,q(path)?));}
    let profile=rudder_engine::build::BuildProfile::from_toml(&raw)?;ensure!(profile.platform==sdk.platform&&profile.buck2.executable_sha256==sha,"fixed policy lost actual SDK identity");Ok(raw.into_bytes())
}

#[cfg(test)]#[path="managed_sdk_initial_source_tests.rs"]mod tests;
