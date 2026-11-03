use super::*;
use crate::managed_task_launch::{ManagedTaskNetworkSelection, SelectedTaskCredentialKind};
use std::{fs::OpenOptions, io::Write, os::unix::fs::{OpenOptionsExt, PermissionsExt}, sync::Arc};

struct Fixture { root: PathBuf, runtime: ManagedProfileLoaderRuntime }
impl Fixture {
    fn new(profiles: serde_json::Value) -> Self {
        let root = PathBuf::from("/workspace").join(format!("rudder-profile-loader-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap(); fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let mut store = OpenOptions::new().write(true).create_new(true).mode(0o600).open(root.join("auth-profiles.json")).unwrap();
        store.write_all(serde_json::to_vec(&serde_json::json!({"version":1,"profiles":profiles})).unwrap().as_slice()).unwrap();
        let runtime = ManagedProfileLoaderRuntime {
            node: PathBuf::from(std::env::var_os("RUDDER_PROFILE_LOADER_NODE").expect("Root must supply actual absolute Node path")).canonicalize().unwrap(),
            helper: PathBuf::from(std::env::var_os("RUDDER_PROFILE_LOADER_HELPER").expect("Root must compile and supply actual standalone profile CLI")).canonicalize().unwrap(),
            rudder_home: root.clone(),
        };
        Self { root, runtime }
    }
    fn script(&mut self, body: &str) {
        let path = self.root.join("fault-helper.mjs");
        let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path).unwrap();
        file.write_all(body.as_bytes()).unwrap(); self.runtime.helper = path;
    }
}
impl Drop for Fixture { fn drop(&mut self) { fs::remove_dir_all(&self.root).unwrap(); } }
fn budget() -> FinalizationBudget { FinalizationBudget::begin(Duration::from_secs(5)).unwrap() }
fn selected(backend: Backend) -> ManagedTaskOptIn {
    ManagedTaskOptIn::selected("selected".into(), backend, SelectedTaskCredentialKind::ApiKeyV1,
        ManagedTaskNetworkSelection::OrdinaryNetworkV1).unwrap()
}
fn load_error(f: &Fixture, selection: &ManagedTaskOptIn) -> ManagedProfileLoadError {
    match load_selected_managed_task_profile(&f.runtime, selection, &AtomicBool::new(false), &budget()) {
        Ok(_) => panic!("fault must not construct a credential owner"), Err(error) => error,
    }
}
const GOOD_METADATA: &str = r#"{"ok":true,"metadata":{"formatVersion":1,"profileId":"selected","backend":"codex","credentialKind":"api_key","provider":"openai"}}"#;

#[test]
fn actual_node_private_store_pipe_frame_and_selected_owner_for_both_backends() {
    for (backend, provider, key_text) in [(Backend::Codex, "openai", "FIXTURE_CODEX_KEY"), (Backend::Claude, "anthropic", "FIXTURE_CLAUDE_KEY")] {
        let f = Fixture::new(serde_json::json!({"selected":{"mode":"api_key","provider":provider,"apiKey":key_text},
            "other":{"type":"api_key","provider":"openai","key":"FIXTURE_OTHER_KEY"}}));
        let cancelled = AtomicBool::new(false); let original = budget();
        let inspected = inspect_selected_managed_task_profile(&f.runtime, "selected", backend, &cancelled, &original).unwrap();
        assert_eq!(inspected.kind, ManagedProfileCredentialKind::ApiKeyV1);
        let result = run(&f.runtime, "selected", backend, true, &cancelled, &original).unwrap();
        assert!(result.status.success()); assert!(result.error.0.is_empty());
        assert_eq!(metadata(&result, "selected", backend).unwrap(), inspected);
        assert_eq!(key(&result.key.0).unwrap(), key_text);
        assert!(load_selected_managed_task_profile(&f.runtime, &selected(backend), &cancelled, &original).is_ok());
    }
}

#[test]
fn metadata_kind_precedes_optin_and_oauth_token_never_fall_back_to_other_api_profile() {
    for (credential, expected_kind, expected_error) in [
        (serde_json::json!({"type":"oauth","provider":"openai-codex","access":"FIXTURE_ACCESS","refresh":"FIXTURE_REFRESH","expires":123456}), ManagedProfileCredentialKind::OAuthV1, ManagedProfileLoadError::UnsupportedOAuth),
        (serde_json::json!({"type":"token","provider":"openai","token":"FIXTURE_TOKEN"}), ManagedProfileCredentialKind::TokenV1, ManagedProfileLoadError::UnsupportedToken),
    ] {
        let f = Fixture::new(serde_json::json!({"selected":credential,"another":{"type":"api_key","provider":"openai","key":"FIXTURE_NOT_SELECTED"}}));
        assert_eq!(inspect_selected_managed_task_profile(&f.runtime, "selected", Backend::Codex, &AtomicBool::new(false), &budget()).unwrap().kind, expected_kind);
        assert_eq!(load_error(&f, &selected(Backend::Codex)), expected_error);
    }
}

#[test]
fn exact_selection_keyref_and_backend_mismatch_never_create_credentials() {
    for profiles in [serde_json::json!({"other":{"type":"api_key","provider":"openai","key":"FIXTURE_OTHER"}}),
        serde_json::json!({"selected":{"type":"api_key","provider":"anthropic","key":"FIXTURE_WRONG_BACKEND"}}),
        serde_json::json!({"selected":{"type":"api_key","provider":"openai","keyRef":{"source":"env","provider":"openai","id":"OPENAI_API_KEY"}}})] {
        let f = Fixture::new(profiles); assert_eq!(load_error(&f, &selected(Backend::Codex)), ManagedProfileLoadError::Rejected);
    }
}

#[test]
fn bad_exit_and_malformed_or_trailing_secret_frames_are_rejected_after_real_wait() {
    let mut bad_exit = Fixture::new(serde_json::json!({}));
    bad_exit.script(&format!("process.stdout.write({GOOD_METADATA:?}); process.exitCode=9;"));
    assert_eq!(inspect_selected_managed_task_profile(&bad_exit.runtime, "selected", Backend::Codex, &AtomicBool::new(false), &budget()).unwrap_err(), ManagedProfileLoadError::Protocol);
    for frame in ["[0,0,0,1,255]", "[0,0,0,1,120,0,0,0,1,121]"] {
        let mut f = Fixture::new(serde_json::json!({}));
        f.script(&format!("import fs from 'node:fs'; if(process.argv[2]==='handoff-api-key'){{fs.writeSync(3,Buffer.from({frame}));fs.closeSync(3);}} process.stdout.write({GOOD_METADATA:?});"));
        assert_eq!(load_error(&f, &selected(Backend::Codex)), ManagedProfileLoadError::Credential);
    }
}

#[test]
fn stderr_overflow_and_error_bodies_return_static_errors_only() {
    let mut f = Fixture::new(serde_json::json!({}));
    f.script("import fs from 'node:fs'; fs.writeSync(2,'FIXTURE_DO_NOT_EXPOSE'.repeat(300)); setInterval(()=>{},100);");
    let error = inspect_selected_managed_task_profile(&f.runtime, "selected", Backend::Codex, &AtomicBool::new(false), &budget()).unwrap_err();
    assert_eq!(error, ManagedProfileLoadError::Overflow); assert!(!format!("{error:?} {error}").contains("FIXTURE_DO_NOT_EXPOSE"));
    let mut f = Fixture::new(serde_json::json!({}));
    f.script("process.stdout.write(JSON.stringify({ok:false,error:{code:'FIXTURE_SECRET_CODE',message:'FIXTURE_SECRET_BODY'}}));process.exitCode=1;");
    let error = inspect_selected_managed_task_profile(&f.runtime, "selected", Backend::Codex, &AtomicBool::new(false), &budget()).unwrap_err();
    assert_eq!(error, ManagedProfileLoadError::Rejected); assert!(!error.to_string().contains("FIXTURE_SECRET"));
}

#[test]
fn deadline_kills_and_reaps_actual_helper_before_return_and_does_not_renew_budget() {
    let mut f = Fixture::new(serde_json::json!({}));
    f.script(&format!("import fs from 'node:fs';fs.writeFileSync(process.env.RUDDER_HOME+'/child-pid',String(process.pid));process.stdout.write({GOOD_METADATA:?});setInterval(()=>{{}},100);"));
    let original = FinalizationBudget::begin(Duration::from_millis(700)).unwrap();
    let error = inspect_selected_managed_task_profile(&f.runtime, "selected", Backend::Codex, &AtomicBool::new(false), &original).unwrap_err();
    assert_eq!(error, ManagedProfileLoadError::Deadline); assert!(original.remaining().is_err());
    if let Ok(pid) = fs::read_to_string(f.root.join("child-pid")) {
        let pid: i32 = pid.parse().unwrap(); assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }
}

#[test]
fn queued_cancellation_never_spawns_and_inflight_cancellation_reaps_before_return() {
    let mut f = Fixture::new(serde_json::json!({}));
    f.script(&format!("import fs from 'node:fs';fs.writeFileSync(process.env.RUDDER_HOME+'/child-pid',String(process.pid));process.stdout.write({GOOD_METADATA:?});setInterval(()=>{{}},100);"));
    assert_eq!(inspect_selected_managed_task_profile(&f.runtime, "selected", Backend::Codex, &AtomicBool::new(true), &budget()).unwrap_err(), ManagedProfileLoadError::Cancelled);
    assert!(!f.root.join("child-pid").exists());
    let cancelled = Arc::new(AtomicBool::new(false)); let original = budget();
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| inspect_selected_managed_task_profile(&f.runtime, "selected", Backend::Codex, &cancelled, &original));
        for _ in 0..300 { if f.root.join("child-pid").exists() { break; } std::thread::sleep(Duration::from_millis(10)); }
        cancelled.store(true, Ordering::Release);
        assert_eq!(worker.join().unwrap().unwrap_err(), ManagedProfileLoadError::Cancelled);
    });
    if let Ok(pid) = fs::read_to_string(f.root.join("child-pid")) {
        assert_eq!(unsafe { libc::kill(pid.parse().unwrap(), 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }
}

#[test]
fn child_environment_contains_only_explicit_normal_store_location() {
    let mut f = Fixture::new(serde_json::json!({}));
    f.script(&format!("import fs from 'node:fs';fs.writeFileSync(process.env.RUDDER_HOME+'/environment.json',JSON.stringify(Object.keys(process.env).sort()));process.stdout.write({GOOD_METADATA:?});"));
    inspect_selected_managed_task_profile(&f.runtime, "selected", Backend::Codex, &AtomicBool::new(false), &budget()).unwrap();
    assert_eq!(fs::read_to_string(f.root.join("environment.json")).unwrap(), "[\"RUDDER_HOME\"]");
}

#[test]
fn private_home_must_match_the_ts_utf8_and_trim_resolved_directory() {
    use std::os::unix::ffi::OsStringExt;
    let mut f = Fixture::new(serde_json::json!({}));
    let normal = f.root.join("profiles"); fs::create_dir(&normal).unwrap();
    fs::set_permissions(&normal,fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(normal.join("auth-profiles.json"), br#"{"version":1,"profiles":{"selected":{"type":"api_key","provider":"openai","key":"FIXTURE_WRONG_TRIMMED_STORE"}}}"#).unwrap();
    fs::set_permissions(normal.join("auth-profiles.json"),fs::Permissions::from_mode(0o600)).unwrap();
    for name in [std::ffi::OsString::from("profiles "), std::ffi::OsString::from("profiles\u{feff}"),
        std::ffi::OsString::from_vec(b"profiles-invalid-\xff".to_vec())] {
        let path = f.root.join(name); fs::create_dir(&path).unwrap();
        fs::set_permissions(&path,fs::Permissions::from_mode(0o700)).unwrap();
        f.runtime.rudder_home = path;
        assert_eq!(load_error(&f, &selected(Backend::Codex)), ManagedProfileLoadError::Runtime);
    }
}

#[test]
fn reader_originally_at3_is_replaced_only_in_designated_child_and_parent_stays_cloexec() {
    let (original_reader, original_writer) = pipe().unwrap();
    let r = unsafe { libc::fcntl(original_reader.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10) };
    let w = unsafe { libc::fcntl(original_writer.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10) };
    assert!(r >= 10 && w >= 10); let reader = unsafe { OwnedFd::from_raw_fd(r) }; let writer = unsafe { OwnedFd::from_raw_fd(w) };
    drop(original_reader); drop(original_writer);
    let pid = unsafe { libc::fork() }; assert!(pid >= 0);
    if pid == 0 { unsafe {
        if libc::dup2(r,3) < 0 || libc::dup2(w,4) < 0 { libc::_exit(1); }
        if inherit_writer(3,4).is_err() || libc::fcntl(3,libc::F_GETFD) & libc::FD_CLOEXEC != 0 { libc::_exit(2); }
        if libc::write(3,b"ok".as_ptr().cast(),2) != 2 { libc::_exit(3); }
        libc::_exit(0);
    } }
    drop(writer);
    let mut status = 0; assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid); assert_eq!(status, 0);
    assert_ne!(unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC, 0);
    let mut bytes = [0u8;2]; assert_eq!(unsafe { libc::read(reader.as_raw_fd(),bytes.as_mut_ptr().cast(),2) }, 2); assert_eq!(&bytes,b"ok");
}

fn snapshot_selection()->ManagedTaskOptIn {
    ManagedTaskOptIn::selected("selected".into(),Backend::Codex,SelectedTaskCredentialKind::SelectedProfileV1,ManagedTaskNetworkSelection::OrdinaryNetworkV1).unwrap()
}
fn snapshot_fixture_token()->String {
    const T:&[u8]=b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let exp=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()+3600;
    let bytes=serde_json::json!({"exp":exp,"https://api.openai.com/auth":{"chatgpt_account_id":"fixture-account","chatgpt_plan_type":"plus"}}).to_string().into_bytes();
    let(mut bits,mut count)=(0u32,0u32);let mut encoded=String::new();for b in bytes {bits=(bits<<8)|b as u32;count+=8;while count>=6{count-=6;encoded.push(T[((bits>>count)&63)as usize]as char);}}
    if count>0{encoded.push(T[((bits<<(6-count))&63)as usize]as char);}format!("e30.{encoded}.c2ln")
}
#[test]
fn actual_node_codex_access_snapshot_uses_selected_account_and_jwt_not_store_mtime_or_refresh() {
    let token=snapshot_fixture_token();
    let f=Fixture::new(serde_json::json!({"selected":{"type":"oauth","provider":"openai-codex","access":token,"refresh":"FIXTURE_REFRESH_NEVER_HANDOFF","idToken":"FIXTURE_ID_NEVER_HANDOFF","expires":1,"accountId":"fixture-account"},
        "other":{"type":"api_key","provider":"openai","key":"FIXTURE_NOT_SELECTED"}}));
    let original=budget();let cancel=AtomicBool::new(false);let before=original.remaining().unwrap();
    let metadata=inspect_selected_managed_task_profile(&f.runtime,"selected",Backend::Codex,&cancel,&original).unwrap();
    assert!(metadata.supports_codex_access_snapshot());
    let captured=run_verb(&f.runtime,"selected",Backend::Codex,"handoff-codex-oauth-access-snapshot",&cancel,&original).unwrap();
    assert!(captured.status.success());let frame=std::str::from_utf8(&captured.key.0[4..]).unwrap();
    assert!(!frame.contains("FIXTURE_REFRESH_NEVER_HANDOFF"));assert!(!frame.contains("FIXTURE_ID_NEVER_HANDOFF"));
    assert!(!frame.contains("FIXTURE_NOT_SELECTED"));
    assert!(load_selected_managed_task_profile(&f.runtime,&snapshot_selection(),&cancel,&original).is_ok());
    assert!(original.remaining().unwrap()<=before,"handoff never resets original budget");
}
#[test]
fn actual_node_access_snapshot_refuses_foreign_account_and_explicit_snapshot_never_falls_back_to_api_key() {
    let f=Fixture::new(serde_json::json!({"selected":{"type":"oauth","provider":"openai-codex","access":snapshot_fixture_token(),"refresh":"FIXTURE_REFRESH","accountId":"foreign-account"}}));
    assert_eq!(load_error(&f,&snapshot_selection()),ManagedProfileLoadError::Rejected);
    let f=Fixture::new(serde_json::json!({"selected":{"type":"api_key","provider":"openai","key":"FIXTURE_KEY"}}));
    let snapshot_only=ManagedTaskOptIn::selected("selected".into(),Backend::Codex,SelectedTaskCredentialKind::CodexOAuthAccessSnapshotV1,ManagedTaskNetworkSelection::OrdinaryNetworkV1).unwrap();
    assert_eq!(load_error(&f,&snapshot_only),ManagedProfileLoadError::Selection);
}
