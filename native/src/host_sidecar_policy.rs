//! Exact immutable packaging DATA only. No Root/transport/engine owner factory.
use super::*;

pub(super) const FILES:[&str;27]=[
    "host-control-core.mjs","host-control.mjs",
    "node_modules/ws/LICENSE","node_modules/ws/README.md","node_modules/ws/browser.js","node_modules/ws/index.js",
    "node_modules/ws/lib/buffer-util.js","node_modules/ws/lib/constants.js","node_modules/ws/lib/event-target.js",
    "node_modules/ws/lib/extension.js","node_modules/ws/lib/limiter.js","node_modules/ws/lib/permessage-deflate.js",
    "node_modules/ws/lib/receiver.js","node_modules/ws/lib/sender.js","node_modules/ws/lib/stream.js",
    "node_modules/ws/lib/subprotocol.js","node_modules/ws/lib/validation.js","node_modules/ws/lib/websocket-server.js",
    "node_modules/ws/lib/websocket.js","node_modules/ws/node_modules/bufferutil/index.js",
    "node_modules/ws/node_modules/utf-8-validate/index.js","node_modules/ws/package.json","node_modules/ws/wrapper.mjs",
    "package-lock.json","package.json","root-upload.mjs","runtime-qualification.mjs",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Pin {sidecar_manifest_sha256:String,sidecar_script_sha256:String,sidecar_core_sha256:String,files:Vec<SidecarFile>}

pub(super) fn validate_manifest(manifest:&SidecarManifest)->Result<()> {
    ensure!(manifest.version==1&&manifest.files.len()==FILES.len(),"host sidecar manifest exact closure invalid");
    let mut total=0u64;
    for (entry,expected) in manifest.files.iter().zip(FILES) {
        // Exact UTF-8 spelling/order refuses duplicates, traversal, aliases,
        // omitted core/stubs and an undeclared same-cardinality replacement.
        ensure!(entry.path.to_str()==Some(expected),"host sidecar manifest path/order differs");sha(&entry.sha256)?;
        ensure!(entry.size_bytes>0&&entry.size_bytes<=8*1024*1024,"host sidecar file size invalid");
        total=total.checked_add(entry.size_bytes).context("host sidecar bytes overflow")?;
        ensure!(total<=8*1024*1024,"host sidecar dependencies exceed bound");
    }Ok(())
}

pub(super) fn validate_pin(raw:&[u8],manifest_raw:&[u8],manifest:&SidecarManifest)->Result<()> {
    ensure!(!raw.is_empty()&&raw.len()<=64*1024&&manifest_raw.len()<=32*1024,"host sidecar pin exceeds bound");
    validate_manifest(manifest)?;let pin:Pin=serde_json::from_slice(raw)?;
    for digest in [&pin.sidecar_manifest_sha256,&pin.sidecar_script_sha256,&pin.sidecar_core_sha256]{sha(digest)?;}
    ensure!(pin.sidecar_manifest_sha256==format!("{:x}",Sha256::digest(manifest_raw))&&pin.files==manifest.files,
        "host sidecar policy differs from raw manifest/complete closure");
    ensure!(manifest.files[0].path==Path::new("host-control-core.mjs")&&manifest.files[1].path==Path::new("host-control.mjs")
        &&pin.sidecar_core_sha256==manifest.files[0].sha256&&pin.sidecar_script_sha256==manifest.files[1].sha256,
        "host sidecar policy source hashes differ");Ok(())
}

#[cfg(test)]
#[path="host_sidecar_policy_tests.rs"]
mod tests;
