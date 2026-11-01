use super::*;
use std::{ffi::CString,fs,os::{fd::{AsRawFd,FromRawFd,OwnedFd},unix::fs::DirBuilderExt}};
struct Fixture { root:PathBuf }
impl Fixture {
 fn new()->Self {
  let root=std::env::temp_dir().join(format!("rudder-managed-startup-{}",uuid::Uuid::new_v4()));
  fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
  fs::create_dir(root.join(".rudder")).unwrap();fs::create_dir(root.join(".rudder/runs")).unwrap();
  fs::write(root.join("RUDDER.md"),b"legacy context must stay untouched\n").unwrap();
  fs::write(root.join(".rudder/graph.json"),b"invalid legacy graph\n").unwrap();
  fs::write(root.join(".rudder/runs/legacy.json"),b"invalid legacy run\n").unwrap();
  Self{root}
 }
 fn initial(&self)->ManagedAppInitialState {ManagedAppInitialState{repository:self.root.clone(),
  selection:InitialSelection{backend:Backend::Codex,model:"explicit-managed-model".into(),effort:Some(EffortLevel::High)},
  branch:"accepted-managed-baseline".into(),session_started_iso:"2026-10-31T00:00:00Z".into()}}
 fn watch(&self)->OwnedFd {
  let raw=unsafe{libc::inotify_init1(libc::IN_CLOEXEC|libc::IN_NONBLOCK)};assert!(raw>=0);
  let fd=unsafe{OwnedFd::from_raw_fd(raw)};
  for path in [&self.root,&self.root.join(".rudder"),&self.root.join(".rudder/runs")] {
   let c=CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
   assert!(unsafe{libc::inotify_add_watch(fd.as_raw_fd(),c.as_ptr(),libc::IN_OPEN|libc::IN_ACCESS|libc::IN_CREATE|libc::IN_DELETE|libc::IN_MOVED_FROM|libc::IN_MOVED_TO|libc::IN_CLOSE_WRITE)}>=0);
  }
  fd
 }
}
impl Drop for Fixture {fn drop(&mut self){let _=fs::remove_dir_all(&self.root);}}
fn assert_no_legacy_io(watch:&OwnedFd) {
 let mut bytes=[0u8;8192];let result=unsafe{libc::read(watch.as_raw_fd(),bytes.as_mut_ptr().cast(),bytes.len())};
 assert_eq!(result,-1,"managed App touched legacy repository files");
 assert_eq!(std::io::Error::last_os_error().kind(),std::io::ErrorKind::WouldBlock);
}
#[test]
fn managed_constructor_uses_explicit_data_without_legacy_files_or_implicit_config() {
 let fixture=Fixture::new();let watch=fixture.watch();let app=App::new_managed(fixture.initial());
 assert!(app.managed_project_mode&&app.engine_headless);assert_eq!(app.cwd,fixture.root);
 assert_eq!(app.backend,Backend::Codex);assert_eq!(app.model,"explicit-managed-model");assert_eq!(app.effort,Some(EffortLevel::High));
 assert_eq!(app.branch.as_deref(),Some("accepted-managed-baseline"));assert_eq!(app.session_started_iso,"2026-10-31T00:00:00Z");
 assert!(app.agents.is_empty()&&app.pending_migration_resumes.is_empty()&&app.task_history.is_empty());
 assert!(!app.gam_links_enabled&&!app.node_review_enabled&&!app.interactive_orchestrator&&!app.home_runtime&&!app.cloud_connected);
 assert!(app.board_url.is_none()&&app.gam_last.is_none()&&!app.perf.enabled());assert_no_legacy_io(&watch);
}
#[test]
fn managed_poll_never_runs_legacy_discovery_schedulers_hooks_or_file_housekeeping() {
 let fixture=Fixture::new();let mut app=App::new_managed(fixture.initial());
 app.last_steer_poll=Instant::now()-Duration::from_secs(10);app.last_cloud_check=Instant::now()-Duration::from_secs(10);
 app.last_workspace_gc=Instant::now()-Duration::from_secs(120);app.last_remote_state_check=Instant::now()-Duration::from_secs(120);
 let watch=fixture.watch();for _ in 0..3 {app.poll_agents();}
 assert!(app.agents.is_empty());assert_eq!(app.scheduler_tick,0);assert!(!app.dashboard_open_emitted);assert_no_legacy_io(&watch);
}
#[test]
fn managed_pane_output_remains_visible_without_treating_pty_exit_as_task_completion() {
 use rudder_native::pty_terminal::{TerminalCommand,TerminalPaneOptions};
 let fixture=Fixture::new();let mut app=App::new_managed(fixture.initial());
 let pane=TerminalPane::spawn_shell_or_command(Some(TerminalCommand::with_args("/bin/sh",["-c","printf managed-projection-text"])),
  TerminalPaneOptions{cwd:Some(fixture.root.clone()),..TerminalPaneOptions::default()}).unwrap();
 let run=crate::app_tests::test_agent_run_with_terminal(&app,pane);app.agents.push(run);
 let deadline=Instant::now()+Duration::from_secs(3);
 loop {app.poll_agents();if app.agents[0].terminal.as_ref().unwrap().output_log_snapshot().contains("managed-projection-text"){break;}
  assert!(Instant::now()<deadline,"actual managed pane output was not drained");std::thread::sleep(Duration::from_millis(5));}
 assert_eq!(app.agents[0].status,AgentStatus::Running);assert!(app.agents[0].terminal.is_some());
 assert!(app.agents[0].completed_at.is_none());assert_eq!(app.scheduler_tick,0);
 // Actual process reap is a separate operation, never inferred from output.
 loop {if app.agents[0].terminal.as_mut().unwrap().try_wait_after_output().unwrap().is_some(){break;}
  assert!(Instant::now()<deadline);std::thread::sleep(Duration::from_millis(5));}
 assert_eq!(app.agents[0].status,AgentStatus::Running);
}
