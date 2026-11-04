//! Explicit registration of an already-owned preview process. No process is
//! spawned, no listener is inferred from cwd, and no network IO occurs here.
use super::*;
use anyhow::{Context, ensure};
use rudder_engine::journal::{Journal, OwnerFence};
use serde::Deserialize;

#[derive(Clone, Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all="camelCase")]
struct Scope { project_id:String, workspace_id:String, attempt_id:String, owner_epoch:u64 }
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all="camelCase")]
struct Registration { version:u32, request_id:String, action:String, scope:Scope, pid:u32, process_start:String, protocol:String }
const KIND:&str="private-preview-registration-v1";

pub(crate) fn register(app:&App, owner:&OwnerFence, journal:&mut Journal, request:&serde_json::Value)->Result<serde_json::Value> {
    ensure!(serde_json::to_vec(request)?.len()<=4096,"preview registration exceeds bound");
    let requested:Registration=serde_json::from_value(request.clone())?;
    ensure!(requested.version==1 && ["register","retire"].contains(&requested.action.as_str())
        && ["http","https"].contains(&requested.protocol.as_str())
        && uuid::Uuid::parse_str(&requested.request_id)?.to_string()==requested.request_id,"invalid preview registration");
    let grants=app.fabric_runtime.file_grants(app,owner);
    let scope=serde_json::to_value(&requested.scope)?;
    let grant=grants.as_array().context("current preview scopes are unavailable")?.iter()
        .find(|grant|grant["scope"]==scope).context("preview requires the exact current owned workspace")?;
    let run=app.agents.iter().find(|run|run.id==requested.scope.workspace_id).context("preview run is absent")?;
    let terminal=run.terminal.as_ref().context("preview root PTY is absent")?;
    ensure!(terminal.instance_id()==requested.scope.attempt_id,"preview attempt changed");
    let root_pid=terminal.child_process_id().context("preview root process is absent")?;
    let workspace=PathBuf::from(grant["root"].as_str().context("preview root is unavailable")?);
    ensure!(!requested.process_start.is_empty() && requested.process_start.len()<=32 && requested.process_start.bytes().all(|c|c.is_ascii_digit()),"invalid preview process start token");
    // Retirement is allowed after the registered process has exited. It still
    // requires the current exact owner/attempt and cannot retire another run.
    if requested.action=="register" { validate_process(requested.pid,root_pid,&workspace,&requested.process_start)?; }
    let result=serde_json::json!({"version":1,"role":"preview","action":requested.action,
        "owner":owner,"scope":scope,"workspacePath":workspace,"rootPid":root_pid,
        "pid":requested.pid,"processStart":requested.process_start,"protocol":requested.protocol});
    let mut bound=request.clone(); bound["owner"]=serde_json::to_value(owner)?;
    journal.record_surface_receipt(owner,&requested.request_id,KIND,&bound,&result,epoch_ms()?)
}

pub(crate) fn list(app:&App, owner:&OwnerFence, journal:&Journal)->Result<serde_json::Value> {
    let grants=app.fabric_runtime.file_grants(app,owner);
    let grants=grants.as_array().context("current preview scopes are unavailable")?;
    let mut latest=std::collections::BTreeMap::new();
    for value in journal.surface_receipts(owner,KIND,256)? {
        if value["owner"]!=serde_json::to_value(owner)? || value["role"]!="preview" { continue; }
        let key=serde_json::to_string(&serde_json::json!([value["scope"],value["pid"],value["processStart"]]))?;
        latest.entry(key).or_insert(value);
    }
    let mut output=Vec::new();
    for value in latest.into_values() {
        if value["action"]!="register" || !grants.iter().any(|g|g["scope"]==value["scope"] && g["root"]==value["workspacePath"]) { continue; }
        let Some(run_id)=value["scope"]["workspaceId"].as_str() else { continue; };
        let Some(run)=app.agents.iter().find(|run|run.id==run_id) else { continue; };
        let Some(root_pid)=run.terminal.as_ref().and_then(TerminalPane::child_process_id) else { continue; };
        let (Some(pid),Some(start),Some(root))=(value["pid"].as_u64(),value["processStart"].as_str(),value["workspacePath"].as_str()) else { continue; };
        if pid>u32::MAX as u64 || validate_process(pid as u32,root_pid,Path::new(root),start).is_err() { continue; }
        output.push(value); if output.len()==64 { break; }
    }
    Ok(serde_json::json!(output))
}
fn epoch_ms()->Result<i64> { Ok(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis().try_into()?) }

#[cfg(target_os="linux")]
fn stat(pid:u32)->Result<(u32,String)> {
    use std::os::unix::fs::MetadataExt;
    let path=PathBuf::from(format!("/proc/{pid}"));
    ensure!(fs::metadata(&path)?.uid()==unsafe{libc::geteuid()},"preview process belongs to another user");
    let bytes=fs::read(path.join("stat"))?; ensure!(bytes.len()<=8192,"process stat exceeds bound");
    let text=std::str::from_utf8(&bytes)?;
    let end=text.rfind(')').context("invalid process stat")?;
    let fields:Vec<_>=text[end+1..].split_whitespace().take(32).collect();
    ensure!(fields.len()>19 && !matches!(fields[0],"Z"|"X"),"preview process has exited");
    Ok((fields[1].parse()?,fields[19].to_owned()))
}
#[cfg(target_os="linux")]
fn validate_process(pid:u32,root_pid:u32,workspace:&Path,start:&str)->Result<()> {
    ensure!(pid>1 && pid!=root_pid,"register an owned preview child, not the backend root");
    let (mut parent,token)=stat(pid)?; ensure!(token==start,"preview process identity changed");
    let root=workspace.canonicalize()?;
    let cwd=fs::read_link(format!("/proc/{pid}/cwd"))?.canonicalize()?;
    ensure!(cwd.starts_with(&root),"preview process left the owned workspace");
    let mut visited=std::collections::BTreeSet::new(); let mut owned=false;
    for _ in 0..32 {
        if parent==root_pid { stat(root_pid)?; owned=true; break; }
        if parent<=1 || !visited.insert(parent) { break; }
        parent=stat(parent)?.0;
    }
    ensure!(owned && stat(pid)?.1==start,"preview process is not a current owned PTY descendant");
    Ok(())
}
#[cfg(not(target_os="linux"))]
fn validate_process(_:u32,_:u32,_:&Path,_:&str)->Result<()> { anyhow::bail!("explicit preview process registration requires the Linux process identity adapter") }

#[cfg(all(test,target_os="linux"))]
mod tests {
    use super::*;
    struct ChildFixture { root:PathBuf, child:std::process::Child }
    impl Drop for ChildFixture {
        fn drop(&mut self) {
            let _=self.child.kill(); let _=self.child.wait();
            let _=fs::remove_dir_all(&self.root);
            let _=fs::remove_dir("/workspace/rudder-scratch/n13-private-terminal");
        }
    }
    #[test]
    fn preview_registration_validates_real_child_custody_and_pid_reuse_token() {
        let root=PathBuf::from("/workspace/rudder-scratch/n13-private-terminal")
            .join(format!("custody-{}",uuid::Uuid::new_v4())); fs::create_dir_all(&root).unwrap();
        let child=std::process::Command::new("/bin/sleep").arg("30").current_dir(&root).spawn().unwrap();
        let mut fixture=ChildFixture {root,child};
        let pid=fixture.child.id(); let token=stat(pid).unwrap().1;
        assert!(validate_process(pid,std::process::id(),&fixture.root,&token).is_ok());
        assert!(validate_process(pid,std::process::id(),&fixture.root,"0").is_err());
        assert!(validate_process(pid,pid,&fixture.root,&token).is_err());
        assert!(validate_process(pid,1,&fixture.root,&token).is_err());
        fixture.child.kill().unwrap(); fixture.child.wait().unwrap();
        assert!(validate_process(pid,std::process::id(),&fixture.root,&token).is_err());
    }
}
