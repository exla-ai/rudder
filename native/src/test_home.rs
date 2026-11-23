//! Every native test process gets its own Rudder home. Tests that never set
//! RUDDER_HOME used to resolve the user's real `~/.rudder` and leave hook
//! configs, notify scripts and signal files there (a thousand of them in
//! `signals/`). Before `main` runs, RUDDER_HOME points at a temp dir for this
//! process; a test that picks its own home still sets RUDDER_HOME itself. The
//! dir is removed when the process exits, and `sweep_dead_test_cwds` removes
//! it after a test process that was killed (it shares the per-App test dir
//! prefix).

/// The Rudder home a test process shares unless a test picks its own.
pub(crate) fn shared_test_rudder_home() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("rudder-test-cwd-{}-home", std::process::id()))
}

/// Whether this test runs on the shared home. State one test would leak into
/// the next through it (the last /gam pair) is neither read nor written.
pub(crate) fn on_shared_test_rudder_home() -> bool {
    cfg!(test) && std::env::var_os("RUDDER_HOME").is_some_and(|home| home == shared_test_rudder_home().as_os_str())
}

// The same hook a constructor attribute uses: the loader runs it before `main`,
// while the process is still single-threaded, so setting the env is safe.
#[cfg(all(test, unix))]
#[used]
#[cfg_attr(target_os = "linux", link_section = ".init_array")]
#[cfg_attr(target_os = "macos", link_section = "__DATA,__mod_init_func")]
static SET_TEST_RUDDER_HOME: extern "C" fn() = set_test_rudder_home;

#[cfg(all(test, unix))]
extern "C" fn set_test_rudder_home() {
    std::env::set_var("RUDDER_HOME", shared_test_rudder_home());
    unsafe { libc::atexit(remove_test_rudder_home) };
}

#[cfg(all(test, unix))]
extern "C" fn remove_test_rudder_home() {
    let _ = std::fs::remove_dir_all(shared_test_rudder_home());
}

#[cfg(all(test, unix))]
mod tests {
    #[test]
    fn tests_never_resolve_the_users_rudder_home() {
        // From the password database: other tests move HOME meanwhile.
        let user_home = unsafe {
            let entry = libc::getpwuid(libc::getuid());
            assert!(!entry.is_null(), "no password entry");
            std::ffi::CStr::from_ptr((*entry).pw_dir).to_str().unwrap().to_string()
        };
        let home = crate::signals::rudder_home().expect("a rudder home");
        assert_ne!(home, std::path::Path::new(&user_home).join(".rudder"));
    }
}
