use super::*;
#[test]
fn provider_kernel_only_exact_subtree_is_excluded() {
    for path in ["/usr/lib/modules","/usr/lib/modules/6.12.114-fly","/usr/lib/modules/any/kernel.ko"] {assert!(is_provider_kernel_path(Path::new(path)));}
    for path in ["/usr/lib","/usr/lib64/modules","/usr/lib/modules-extra","/usr/share/modules","/opt/runtime/modules"] {assert!(!is_provider_kernel_path(Path::new(path)));}
}
#[test]
fn provider_kernel_masks_actual_usr_ancestors_and_preserves_other_runtime() {
    for path in ["/usr","/usr/lib"] {assert!(requires_provider_kernel_mask(Path::new(path),Path::new(path)).unwrap());}
    for path in ["/usr/bin","/usr/lib64","/usr/lib/x86_64-linux-gnu","/etc/ssl/certs","/opt/runtime/model/v1"] {assert!(!requires_provider_kernel_mask(Path::new(path),Path::new(path)).unwrap());}
}
#[test]
fn provider_kernel_direct_and_remapped_sources_or_guests_refuse() {
    for (source,guest) in [("/usr/lib/modules","/opt/runtime/modules"),("/usr/lib/modules/6.12.114-fly/kernel.ko","/usr/bin/data"),("/usr","/opt/runtime/system"),("/usr/lib","/usr/local/lib"),("/usr/share","/usr/lib"),("/usr/share","/usr/lib/modules"),("/opt/runtime","/usr")] {assert!(requires_provider_kernel_mask(Path::new(source),Path::new(guest)).is_err(),"{source} -> {guest}");}
}
#[test]
fn provider_kernel_policy_has_no_traversal_root_or_relative_escape() {
    for (source,guest) in [("/usr/lib/../lib","/usr/lib"),("/usr","/usr/lib/.."),("/","/usr"),("usr/lib","/usr/lib"),("/usr","usr")] {assert!(requires_provider_kernel_mask(Path::new(source),Path::new(guest)).is_err());}
}
#[test]
fn provider_swap_tool_is_exactly_masked_without_expanding_other_exclusions() {
    for p in ["/usr/sbin/swapon","/usr/sbin/swapon/private"] {assert!(is_provider_kernel_path(Path::new(p)));}
    for p in ["/usr/sbin","/usr/sbin/swapoff","/usr/sbin/swapon-extra","/usr/bin/swapon"] {assert!(!is_provider_kernel_path(Path::new(p)));}
    for p in ["/usr","/usr/sbin"] {assert!(requires_provider_swap_mask(Path::new(p),Path::new(p)).unwrap());}
    for p in ["/usr/bin","/usr/lib","/opt/runtime"] {assert!(!requires_provider_swap_mask(Path::new(p),Path::new(p)).unwrap());}
    for (s,g) in [("/usr/sbin/swapon","/usr/bin/tool"),("/usr/sbin","/opt/runtime"),("/opt/runtime","/usr/sbin"),("/usr","/opt/runtime")] {assert!(requires_provider_swap_mask(Path::new(s),Path::new(g)).is_err());}
}
