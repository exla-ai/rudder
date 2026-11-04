//! App-loop adoption of an explicitly selected, reconciled migration snapshot.
//! Inspection and owner transfer happen off App before this typed data arrives.
use super::*;

#[cfg(test)]
#[path = "v4_migration_integration_tests.rs"]
mod integration_tests;

pub(crate) fn apply(app: &mut App, records: Vec<serde_json::Value>) -> Result<()> {
    anyhow::ensure!(records.len() <= 1024 && app.agents.iter().all(|run|run.terminal.is_none()),
        "migration adoption requires a new App without live PTYs");
    let mut ids = std::collections::HashSet::new();
    let agents = records.into_iter().map(|record| {
        let id = record["id"].as_str().context("migration record has no identity")?.to_owned();
        anyhow::ensure!(ids.insert(id), "duplicate migration run projection");
        let mut run = crate::gitio::agent_from_run_record(&app.cwd, record)
            .context("migration record cannot be restored by this App")?;
        anyhow::ensure!(run.terminal.is_none(), "migration adoption cannot reconstruct a live terminal");
        if matches!(run.status, AgentStatus::Running | AgentStatus::Orphaned) {
            run.status = AgentStatus::Paused;
        }
        run.needs_permission = false; run.needs_user_input = false;
        Ok(run)
    }).collect::<Result<Vec<_>>>()?;
    app.agents = agents;
    // Keep recovered plans/history visible, but opening a migrated checkout is
    // never approval to schedule queued nodes or resume a historical session.
    for plan in &mut app.plans { plan.awaiting_approval = true; }
    app.pending_migration_resumes.clear();
    app.migration_resumes_attempted = true;
    app.agent_row_map.clear();
    app.notice = Some("Migrated sessions restored. Resume paused work explicitly.".into());
    app.dirty = true;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn migration_adoption_app_preserves_sessions_and_requires_explicit_resume() {
        let mut app = App::new();
        let workspace = app.cwd.join("retained-workspace");
        apply(&mut app, vec![serde_json::json!({"id":"retained","task":"retain source and session",
            "status":"paused","backend":"codex","model":"gpt-5.4","effort":"high",
            "workspace":{"enabled":true,"path":workspace,"jjChangeId":"opaque-change"},
            "session":{"nativeSessionId":"actual-retained-session"},
            "migrationRecovery":{"format_version":1,"automatic_resume":false}})]).unwrap();
        assert_eq!(app.agents.len(),1);
        assert_eq!(app.agents[0].status,AgentStatus::Paused);
        assert_eq!(app.agents[0].session_id.as_deref(),Some("actual-retained-session"));
        assert_eq!(app.agents[0].cwd,workspace);
        assert!(app.agents[0].terminal.is_none());
        assert!(app.plans.iter().all(|plan|plan.awaiting_approval));
        assert!(app.migration_resumes_attempted);
        app.resume_migrated_agents();
        assert!(app.agents[0].terminal.is_none());
    }
}
