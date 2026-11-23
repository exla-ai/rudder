//! Product events and `/feedback`, emitted from the dashboard.
//!
//! The dashboard has no HTTP client and should never grow one for this: a stalled
//! POST inside the render loop is a worse bug than any missing metric. An event is
//! one line appended to the spool (`<rudder home>/telemetry/events.jsonl`), and
//! the `rudder` CLI that launched the dashboard sends it with src/analytics.ts
//! (`flushEventSpool`), so one telemetry implementation serves both halves of
//! Rudder and an event costs no process. A report from `/feedback` still goes to
//! a detached `rudder feedback`, which owns the local copy and the GitHub issue.
//!
//! Every emit here is best-effort and silent. If the spool can't be written,
//! telemetry is off, or the network is down, the dashboard behaves exactly the same.

use std::{
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

/// Nothing sends the spool while no CLI runs beside a dashboard, so it stops
/// growing here.
const SPOOL_LIMIT_BYTES: u64 = 256 * 1024;

/// Spool one product event for the dashboard rooted at `root`. Properties must be
/// small, non-identifying scalars; see the rules at the top of src/analytics.ts,
/// which enforces them again when it sends. `root` never leaves the machine: the
/// sender turns it into the hashed project id.
pub(crate) fn emit_event(root: &Path, name: &str, properties: serde_json::Value) {
    if cfg!(test) {
        return;
    }
    let Some(home) = crate::signals::rudder_home() else {
        return;
    };
    let _ = append_event(&home.join("telemetry").join("events.jsonl"), root, name, properties);
}

fn append_event(
    spool: &Path,
    root: &Path,
    name: &str,
    properties: serde_json::Value,
) -> std::io::Result<()> {
    if let Some(dir) = spool.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::metadata(spool).is_ok_and(|meta| meta.len() >= SPOOL_LIMIT_BYTES) {
        return Ok(());
    }
    let at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64);
    let mut line = serde_json::to_vec(&serde_json::json!({
        "event": name,
        "properties": properties,
        "root": root.to_string_lossy(),
        "at": at,
    }))?;
    line.push(b'\n');
    // One write on an O_APPEND file: dashboards sharing the spool never
    // interleave a line.
    OpenOptions::new().create(true).append(true).open(spool)?.write_all(&line)
}

/// Hand a feedback report to the CLI, which owns the local copy, the usage event,
/// and the GitHub issue. The context is passed as a FILE so a long report never
/// hits an argv limit and never lands in a process listing.
pub(crate) fn submit_feedback(
    repo_root: &Path,
    context: serde_json::Value,
) -> std::io::Result<PathBuf> {
    let dir = repo_root.join(".rudder").join("feedback");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("pending-{}.json", std::process::id()));
    std::fs::write(&path, serde_json::to_vec_pretty(&context)?)?;
    if cfg!(test) {
        return Ok(path);
    }
    let Some(rudder) = crate::locate_rudder_cli() else {
        return Ok(path);
    };
    let mut command = Command::new(rudder);
    command.arg("feedback").arg("--context").arg(&path);
    command.current_dir(repo_root);
    spawn_detached(command);
    Ok(path)
}

fn spawn_detached(mut command: Command) {
    if let Ok(mut child) = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        // Reap off-thread so a fire-and-forget emit never leaves a zombie.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    /// The line format src/analytics.ts `flushEventSpool` reads.
    #[test]
    fn a_spooled_event_is_one_json_line_the_cli_sends() {
        let dir = std::env::temp_dir().join(format!("rudder-event-spool-{}", std::process::id()));
        let spool = dir.join("telemetry").join("events.jsonl");
        for event in ["dashboard_opened", "merge_finished"] {
            super::append_event(&spool, Path::new("/repo/one"), event, serde_json::json!({ "ok": true }))
                .expect("append to the spool");
        }
        let text = std::fs::read_to_string(&spool).expect("read the spool");
        std::fs::remove_dir_all(&dir).expect("remove the test spool");
        let lines: Vec<serde_json::Value> =
            text.lines().map(|line| serde_json::from_str(line).expect("one JSON object per line")).collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["event"], "dashboard_opened");
        assert_eq!(lines[1]["event"], "merge_finished");
        assert_eq!(lines[0]["root"], "/repo/one");
        assert_eq!(lines[0]["properties"]["ok"], true);
        assert!(lines[0]["at"].as_u64().is_some_and(|at| at > 1_700_000_000_000));
    }
}
