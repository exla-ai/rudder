//! Fixed provider-kernel DATA boundary, not a kernel qualification or authority.
//! Provider boot may populate this host subtree; managed userspace always sees
//! an empty read-only directory instead. No caller-selected exclusion/flag.
use anyhow::{ensure,Result};
use std::path::{Component,Path};
pub const PROVIDER_KERNEL_DIRECTORY:&str="/usr/lib/modules";
pub const PROVIDER_KERNEL_EMPTY_MODE:u32=0o555;
pub const PROVIDER_KERNEL_BOUNDARY_ID:&str="provider-kernel-empty-v1";
// Fly replaces this installed path with an alias to its private boot helper.
// The immutable runtime substitutes an empty, non-executable file instead.
pub const PROVIDER_SWAP_TOOL:&str="/usr/sbin/swapon";
pub const PROVIDER_SWAP_EMPTY_MODE:u32=0o444;

pub fn is_provider_kernel_path(path:&Path)->bool {path.starts_with(PROVIDER_KERNEL_DIRECTORY)||path.starts_with(PROVIDER_SWAP_TOOL)}
/// The source has already been canonicalized by the runtime validator. Only
/// identity mappings of ancestors may include this boundary; direct payload or
/// remapped ancestor/guest inputs refuse rather than expose uncaptured bytes.
pub fn requires_provider_kernel_mask(source:&Path,guest:&Path)->Result<bool> {
    requires_mask(source,guest,Path::new(PROVIDER_KERNEL_DIRECTORY))
}
pub fn requires_provider_swap_mask(source:&Path,guest:&Path)->Result<bool> {
    requires_mask(source,guest,Path::new(PROVIDER_SWAP_TOOL))
}
fn requires_mask(source:&Path,guest:&Path,boundary:&Path)->Result<bool> {
    for path in [source,guest] {ensure!(path.is_absolute()&&path!=Path::new("/")&&path.components().all(|c|matches!(c,Component::RootDir|Component::Normal(_))),"provider-kernel runtime path is unsafe");}
    ensure!(!is_provider_kernel_path(source)&&!is_provider_kernel_path(guest),"provider kernel payload is not an image runtime input");
    let source_parent=boundary.starts_with(source);let guest_parent=boundary.starts_with(guest);
    if source_parent||guest_parent {
        ensure!(source_parent&&guest_parent&&source==guest,"provider-kernel ancestor runtime cannot be remapped");return Ok(true);
    }Ok(false)
}

#[cfg(test)]
#[path="provider_kernel_tests.rs"]
mod tests;
