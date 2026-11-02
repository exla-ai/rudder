//! Actual Linux descriptor mechanics only. No Root proof, mount owner,
//! readiness acknowledgment, clock replacement or execution grant is created.
use super::*;
use std::os::unix::fs::symlink;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("rudder-runtime-component-{}", uuid::Uuid::new_v4()));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
    fn parent(&self) -> OwnedFd {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&self.0)
            .unwrap()
            .into()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn errno(error: &anyhow::Error) -> Option<i32> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<io::Error>())
        .and_then(io::Error::raw_os_error)
}

#[test]
fn component_symlink_role_is_distinct_from_actual_open_errno_and_no_link_is_followed() {
    let fixture = Fixture::new();
    fs::write(fixture.0.join("target"), b"private bytes must not be read").unwrap();
    symlink("target", fixture.0.join("timezone")).unwrap();
    let parent = fixture.parent();
    let role =
        readonly_component_at(&parent, "timezone", false, RuntimeComponent::Timezone).unwrap_err();
    assert_eq!(
        bootstrap_failure_check(&role).as_deref(),
        Some("RuntimeComponentRole(Timezone)")
    );
    assert_eq!(errno(&role), None);
    let diagnostic = retained_bootstrap_diagnostic(&role, BootstrapCheck::RuntimeMountComponents);
    assert_eq!(
        bootstrap_failure_check(&diagnostic).as_deref(),
        Some("RuntimeComponentRole(Timezone)")
    );
    assert!(diagnostic
        .chain()
        .all(|cause| cause.downcast_ref::<io::Error>().is_none()));

    fs::remove_file(fixture.0.join("timezone")).unwrap();
    let missing =
        readonly_component_at(&parent, "timezone", false, RuntimeComponent::Timezone).unwrap_err();
    assert_eq!(
        bootstrap_failure_check(&missing).as_deref(),
        Some("RuntimeComponentOpen(Timezone)")
    );
    assert_eq!(errno(&missing), Some(libc::ENOENT));
    assert_eq!(
        errno(&retained_bootstrap_diagnostic(
            &missing,
            BootstrapCheck::RuntimeMountComponents
        )),
        Some(libc::ENOENT)
    );

    // O_DIRECTORY|O_NOFOLLOW rejects even a symlink to a real directory.
    fs::create_dir(fixture.0.join("actual-dir")).unwrap();
    symlink("actual-dir", fixture.0.join("usr")).unwrap();
    let directory = readonly_component_at(&parent, "usr", true, RuntimeComponent::Usr).unwrap_err();
    assert_eq!(
        bootstrap_failure_check(&directory).as_deref(),
        Some("RuntimeComponentOpen(Usr)")
    );
    assert_eq!(errno(&directory), Some(libc::ENOTDIR));
}

#[test]
fn component_real_fd_mount_validation_distinguishes_role_fd_data_and_mount_checks() {
    let fixture = Fixture::new();
    let parent = fixture.parent();
    fs::write(fixture.0.join("loader-cache"), b"fixed fixture").unwrap();
    fs::create_dir(fixture.0.join("usr")).unwrap();
    let file = readonly_component_at(
        &parent,
        "loader-cache",
        false,
        RuntimeComponent::LoaderCache,
    )
    .unwrap();
    let directory = readonly_component_at(&parent, "usr", true, RuntimeComponent::Usr).unwrap();
    let actual = wire::fd_data(&file).unwrap();
    check_runtime_component_mount(&file, actual.mount_id, RuntimeComponent::LoaderCache).unwrap();
    check_runtime_component_mount(&directory, actual.mount_id, RuntimeComponent::Usr).unwrap();
    let mismatch = check_runtime_component_mount(
        &file,
        actual.mount_id.checked_add(1).unwrap(),
        RuntimeComponent::LoaderCache,
    )
    .unwrap_err();
    assert_eq!(
        bootstrap_failure_check(&mismatch).as_deref(),
        Some("RuntimeComponentMount(LoaderCache)")
    );
    assert_eq!(errno(&mismatch), None);
    let role = readonly_component_at(&parent, "usr", false, RuntimeComponent::Usr).unwrap_err();
    assert_eq!(
        bootstrap_failure_check(&role).as_deref(),
        Some("RuntimeComponentRole(Usr)")
    );

    let raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
    assert!(raw >= 0);
    let event = unsafe { OwnedFd::from_raw_fd(raw) };
    let data =
        check_runtime_component_mount(&event, actual.mount_id, RuntimeComponent::Usr).unwrap_err();
    assert_eq!(
        bootstrap_failure_check(&data).as_deref(),
        Some("RuntimeComponentFdData(Usr)")
    );
    assert_eq!(errno(&data), None);
    // Diagnostics leave the actual successful descriptors open and usable.
    assert_eq!(wire::fd_data(&file).unwrap(), actual);
}

#[test]
fn component_labels_are_closed_over_exact_components_and_cancel_keeps_original_clock() {
    let components = COMPONENTS
        .iter()
        .map(|(name, _, _, _)| runtime_component(name).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        components,
        vec![
            RuntimeComponent::Usr,
            RuntimeComponent::Package,
            RuntimeComponent::Model,
            RuntimeComponent::Certificates,
            RuntimeComponent::Alternatives,
            RuntimeComponent::LoaderCache,
            RuntimeComponent::Timezone,
            RuntimeComponent::SslConfig,
            RuntimeComponent::PythonSite
        ]
    );
    let error = runtime_component("private/credential/path").unwrap_err();
    assert_eq!(
        bootstrap_failure_check(&error).as_deref(),
        Some("RuntimeComponentSet")
    );
    let original = FinalizationBudget::begin(Duration::from_secs(30)).unwrap();
    let constraint = original.constraint();
    original.cancel();
    let error = proof_check(
        BootstrapCheck::RuntimeComponentClock(RuntimeComponent::Timezone),
        || {
            original.remaining()?;
            Ok(())
        },
    )
    .unwrap_err();
    let error = bootstrap_failure_context(error, BootstrapCheck::RuntimeMountComponents);
    assert_eq!(
        bootstrap_failure_check(&error).as_deref(),
        Some("RuntimeComponentClock(Timezone)")
    );
    assert_eq!(original.constraint(), constraint);
    assert!(original.remaining().is_err());
}

// compiler_sdk_helper's run_namespace_retaining imports this exact generic
// three-argument helper through super::*. Buck2 is not a runtime component.
#[test]
fn sdk_buck2_generic_three_argument_descriptor_helper_keeps_its_original_contract() {
    let fixture = Fixture::new();
    let parent = fixture.parent();
    fs::write(fixture.0.join("buck2"), b"SDK fixture executable bytes").unwrap();
    let buck2 = readonly_at(&parent, "buck2", false).unwrap();
    assert!(!wire::fd_data(&buck2).unwrap().directory);
    let flags = unsafe { libc::fcntl(buck2.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0 && flags & libc::O_PATH != 0 && flags & libc::O_NOFOLLOW != 0);
    assert_ne!(
        unsafe { libc::fcntl(buck2.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
        0
    );
    fs::remove_file(fixture.0.join("buck2")).unwrap();
    symlink("missing-target", fixture.0.join("buck2")).unwrap();
    let role = readonly_at(&parent, "buck2", false).unwrap_err();
    assert!(bootstrap_failure_check(&role).is_none());
    assert_eq!(errno(&role), None);
    fs::remove_file(fixture.0.join("buck2")).unwrap();
    let missing = readonly_at(&parent, "buck2", false).unwrap_err();
    // Preserve SDK's existing generic open error; errno enhancement is scoped
    // to readonly_component_at and does not change an SDK failure contract.
    assert!(bootstrap_failure_check(&missing).is_none());
    assert_eq!(errno(&missing), None);
}
