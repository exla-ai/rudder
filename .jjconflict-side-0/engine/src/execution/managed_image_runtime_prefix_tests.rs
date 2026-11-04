use super::*;
#[test]
fn only_the_exact_declared_managed_image_source_boundary_is_public_runtime() {
    for accepted in ["/opt/rudder-managed-runtime/v1","/opt/rudder-managed-runtime/v1/codex","/opt/rudder-managed-runtime/v1/deps/library"] {assert!(managed_image_runtime(Path::new(accepted)));assert!(public_runtime(Path::new(accepted)));}
    for refused in ["/opt/rudder-managed-runtime","/opt/rudder-managed-runtime/v10","/opt/rudder-managed-runtime/v1-other","/opt/rudder-managed-runtime/v2","/opt/rudder-managed-runtime/v1/../v2","opt/rudder-managed-runtime/v1"] {assert!(!managed_image_runtime(Path::new(refused)));assert!(!public_runtime(Path::new(refused)));}
}
