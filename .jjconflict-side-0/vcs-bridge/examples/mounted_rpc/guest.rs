use super::wire;
use anyhow::{Result, bail, ensure};
use rudder_vcs_bridge::command_transport::*;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

const CLIENT: &str = "/opt/runtime/jj-command/client.json";
fn client_path() -> std::path::PathBuf {
    std::env::var_os("RUDDER_JJ_CLIENT_CONFIG")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| CLIENT.into())
}
fn pause(marker: &str) -> Result<()> {
    println!("VCS-FIXTURE:{marker}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    ensure!(
        std::io::stdin().read_line(&mut line)? > 0 && line.trim() == "continue",
        "host did not confirm fixture transition"
    );
    Ok(())
}
fn write(file: &mut File, number: u8) -> Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(format!("export const n = {number};\n").as_bytes())?;
    file.sync_all()?;
    Ok(())
}
fn jj(args: &[&str]) -> Result<String> {
    let output = Command::new(
        std::env::var_os("RUDDER_JJ_BIN").unwrap_or_else(|| "/opt/runtime/rudder-jj".into()),
    )
    .args(args)
    .env("RUDDER_JJ_CLIENT_CONFIG", client_path())
    .stdin(Stdio::null())
    .output()?;
    ensure!(
        output.status.success(),
        "guest jj {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout)?;
    println!(
        "{}",
        serde_json::json!({"kind":"guest-jj-command","args":args,"stdout":text,"stderr":String::from_utf8_lossy(&output.stderr)})
    );
    Ok(text)
}
fn commit(log: &str) -> Result<String> {
    log.split_whitespace()
        .find(|word| word.len() == 40 && word.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(String::from)
        .ok_or_else(|| anyhow::anyhow!("full commit id absent from fixed log"))
}
fn spawn_writer() -> Result<std::process::Child> {
    Command::new("/opt/runtime/qualify-vcs")
        .arg("writer")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(Into::into)
}
fn forged_scope(client: &CommandClientConfig, field: &str) -> Result<()> {
    let mut identity: CommandIdentity = wire::read_json(&client.identity_file)?;
    match field {
        "epoch" => identity.binding.owner.epoch += 1,
        "attempt" => identity.binding.attempt_id = uuid::Uuid::new_v4().to_string(),
        _ => bail!("unknown fixture forgery"),
    }
    let path = Path::new("/tmp").join(format!("forged-{field}.json"));
    wire::write_json(&path, &identity)?;
    let mut config = client.clone();
    config.binding = identity.binding;
    config.identity_file = path;
    let forged = CommandClient::new(config)?;
    ensure!(
        forged
            .execute(
                &uuid::Uuid::new_v4().to_string(),
                SemanticCommand::Status {}
            )
            .is_err(),
        "stale {field} accepted"
    );
    Ok(())
}
pub fn run(mode: &str) -> Result<()> {
    ensure!(
        unsafe { libc::geteuid() } == 1500 && unsafe { libc::getegid() } == 1500,
        "guest UID/GID changed"
    );
    let status = fs::read_to_string("/proc/self/status")?;
    ensure!(
        status.contains("CapEff:\t0000000000000000") && status.contains("NoNewPrivs:\t1"),
        "guest capability boundary absent"
    );
    // Waiting for the bound nonempty WC also ensures these host fixture files
    // exist; denial must not be proved with a missing-file negative control.
    wire::wait_file(&client_path(), 25)?;
    for path in serde_json::from_str::<Vec<String>>(&std::env::var("FORBIDDEN_PATHS")?)? {
        ensure!(fs::read(&path).is_err(), "private host file readable");
        ensure!(
            fs::read(format!("/proc/1/root{path}")).is_err(),
            "private file readable via proc root"
        );
    }
    let config: CommandClientConfig = wire::read_json(&client_path())?;
    let encoded = serde_json::to_string(&config)?;
    ensure!(
        !encoded.contains("cas_root")
            && !encoded.contains("private_repository")
            && !encoded.contains(".jj"),
        "host paths leaked in guest command config"
    );
    forged_scope(&config, "epoch")?;
    forged_scope(&config, "attempt")?;
    let mut source = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/workspace/source.ts")?;
    write(&mut source, 2)?;
    jj(&["status"])?;
    if mode == "epoch" || mode == "native-epoch" {
        let _writer = spawn_writer()?;
        wire::wait_file(Path::new("/workspace/writer.log"), 5)?;
        println!(
            "{}",
            serde_json::json!({"kind":"mounted-vcs-epoch-guest","stale_epoch_denied":true,"stale_attempt_denied":true,"writer_ready":true,"outside_authority_invalidation_pending":true})
        );
        println!("VCS-FIXTURE:EPOCH_READY");
        std::io::stdout().flush()?;
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    }
    if mode != "native-live" {
        pause("CAPTURE_READY")?;
    }
    write(&mut source, 3)?;
    if mode != "native-live" {
        pause("CAPTURE_LATER")?;
    }
    let diff = jj(&["diff"])?;
    ensure!(
        diff.contains("+export const n = 3;") && diff.contains("-export const n = 1;"),
        "open-FD change missing from actual jj diff"
    );
    let parent = commit(&jj(&["log", "--limit", "5"])?)?;
    jj(&["new", "--message=private mounted child"])?;
    jj(&["edit", &parent])?;
    ensure!(
        jj(&["log", "--limit", "5"])?.contains(&parent),
        "selected parent disappeared from private history"
    );
    // `log` is topological; the first row is not a WC pointer. The trusted
    // outer controller checks the exact private WC commit under its writer
    // lock before acknowledging this transition.
    pause(&format!("EDIT_SELECTED:{parent}"))?;
    // Reject dangerous argv before any host dispatcher or snapshot is invoked.
    let rejected = Command::new("/opt/runtime/rudder-jj")
        .args(["--config", "ui.editor='touch /tmp/escaped'", "status"])
        .env("RUDDER_JJ_CLIENT_CONFIG", client_path())
        .output()?;
    ensure!(
        !rejected.status.success() && !Path::new("/tmp/escaped").exists(),
        "untrusted CLI configuration escaped typed grammar"
    );
    write(&mut source, 4)?;
    if mode == "native-live" {
        jj(&["--version"])?;
        jj(&["--help"])?;
        jj(&["new", "--message=actual native scoped provider"])?;
        write(&mut source, 5)?;
        let _writer = spawn_writer()?;
        wire::wait_file(Path::new("/workspace/writer.log"), 5)?;
        println!(
            "{}",
            serde_json::json!({"kind":"mounted-native-vcs-guest","uid":unsafe{libc::geteuid()},"gid":unsafe{libc::getegid()},"nonempty_status_diff_new":true,"same_tree_edit":true,"same_open_fd_across_observations":true,"stale_epoch_denied":true,"stale_attempt_denied":true,"arbitrary_config_denied":true,"offline_version_help":true})
        );
        pause("FINALIZE")?;
        return Ok(());
    }
    pause("ARM_LOST_COMMAND")?;
    let client = CommandClient::new(config)?;
    let id = uuid::Uuid::new_v4().to_string();
    let command = SemanticCommand::New {
        message: "same UUID across lost completed reply".into(),
    };
    ensure!(
        client.execute(&id, command.clone()).is_err(),
        "fault injector did not drop completed command reply"
    );
    pause(&format!("COMMAND_REPLY_LOST:{id}"))?;
    let inspected = client.inspect(&id)?;
    let retried = client.execute(&id, command)?;
    ensure!(
        inspected.state == CommandState::Completed && inspected.exit_code == Some(0),
        "lost command not durably completed"
    );
    ensure!(
        serde_json::to_value(&inspected)? == serde_json::to_value(&retried)?,
        "same command UUID replayed after service restart"
    );
    // A post-observation write through the ORIGINAL FD must survive into the
    // terminal checkpoint while remaining outside the latest jj observation.
    write(&mut source, 5)?;
    let _writer = spawn_writer()?;
    wire::wait_file(Path::new("/workspace/writer.log"), 5)?;
    println!(
        "{}",
        serde_json::json!({"kind":"mounted-vcs-guest","uid":unsafe{libc::geteuid()},"gid":unsafe{libc::getegid()},"nonempty_status_diff_new":true,"same_tree_edit":true,"same_open_fd_across_observations":true,"stale_epoch_denied":true,"stale_attempt_denied":true,"arbitrary_config_denied":true,"completed_reply_loss_id":id,"completed_reply_retry_no_replay":true,"lost_reply_backend":retried.backend})
    );
    pause("FINALIZE")?;
    source.seek(SeekFrom::Start(0))?;
    let mut bytes = String::new();
    source.read_to_string(&mut bytes)?;
    ensure!(
        bytes == "export const n = 5;\n",
        "FD changed during finalization"
    );
    Ok(())
}
pub fn writer() -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open("/workspace/writer.log")?;
    loop {
        file.write_all(b"writer\n")?;
        file.flush()?;
        thread::sleep(Duration::from_millis(5));
    }
}
