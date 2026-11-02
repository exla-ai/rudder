//! Diagnostic classification only. No fabricated Root owner, mount, readiness
//! acknowledgment or resource disposition is produced by these tests.
use super::*;

#[derive(Debug)]
struct PrivateCause(Arc<std::sync::atomic::AtomicUsize>);
impl std::fmt::Display for PrivateCause {
    fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        panic!("private error formatting must never run");
    }
}
impl std::error::Error for PrivateCause {}
impl Drop for PrivateCause {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn retained_sdk_diagnostic_preserves_io_kind_without_formatting_or_dropping_original_cause() {
    let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let original = anyhow::Error::new(io::Error::new(
        io::ErrorKind::PermissionDenied,
        PrivateCause(drops.clone()),
    ));
    let diagnostic = retained_bootstrap_diagnostic(&original, BootstrapCheck::SdkSourceBinding);
    assert_eq!(
        bootstrap_failure_check(&diagnostic).as_deref(),
        Some("SdkSourceBinding")
    );
    let os = diagnostic
        .chain()
        .find_map(|cause| cause.downcast_ref::<io::Error>())
        .unwrap();
    assert_eq!(os.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(os.raw_os_error(), None);
    // The returned diagnostic carries no private inner payload. The original
    // still holds that payload, matching SDK capture's retained failure slot.
    assert!(os.get_ref().is_none());
    assert!(original
        .downcast_ref::<io::Error>()
        .unwrap()
        .get_ref()
        .unwrap()
        .is::<PrivateCause>());
    drop(diagnostic);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(original);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn retained_sdk_diagnostic_preserves_errno_and_precise_inner_check_over_prepare_phase() {
    let original = proof_check(BootstrapCheck::EngineProtectedProc, || -> Result<()> {
        Err(io::Error::from_raw_os_error(libc::EACCES)).context("private source path")
    })
    .unwrap_err();
    let diagnostic = retained_bootstrap_diagnostic(&original, BootstrapCheck::SdkQualification);
    // A mount's enclosing SDK phase must not overwrite a narrower check.
    let diagnostic = bootstrap_failure_context(diagnostic, BootstrapCheck::RuntimeMountSdk);
    assert_eq!(
        bootstrap_failure_check(&diagnostic).as_deref(),
        Some("EngineProtectedProc")
    );
    let os = diagnostic
        .chain()
        .find_map(|cause| cause.downcast_ref::<io::Error>())
        .unwrap();
    assert_eq!(os.raw_os_error(), Some(libc::EACCES));
    assert_eq!(os.kind(), io::ErrorKind::PermissionDenied);
}

#[test]
fn prepare_validation_diagnostic_keeps_original_clock_cancelled_and_never_formats_private_cause() {
    let original = FinalizationBudget::begin(Duration::from_secs(30)).unwrap();
    let constraint = original.constraint();
    original.cancel();
    let error = original.remaining().unwrap_err();
    let diagnostic =
        retained_bootstrap_diagnostic(&error, BootstrapCheck::RuntimeMountSelectedElves);
    assert_eq!(
        bootstrap_failure_check(&diagnostic).as_deref(),
        Some("RuntimeMountSelectedElves")
    );
    assert!(diagnostic
        .chain()
        .all(|cause| cause.downcast_ref::<io::Error>().is_none()));
    assert_eq!(original.constraint(), constraint);
    assert!(original.remaining().is_err());

    let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let error = anyhow::Error::new(PrivateCause(drops.clone()));
    let error = bootstrap_failure_context(error, BootstrapCheck::RuntimeMountManifest);
    let diagnostic = retained_bootstrap_diagnostic(&error, BootstrapCheck::RuntimeMountSdk);
    assert_eq!(
        bootstrap_failure_check(&diagnostic).as_deref(),
        Some("RuntimeMountManifest")
    );
    assert!(error.downcast_ref::<PrivateCause>().is_some());
    drop(diagnostic);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(error);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
