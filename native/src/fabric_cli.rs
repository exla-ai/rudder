//! Machine-readable entry point for immutable workspace and journal operations.
//! This does not instantiate App or a second scheduler.

use anyhow::{bail, Result};
use rudder_engine::{
    paths::FabricPaths,
    protocol::{ObjectRef, FORMAT_VERSION},
    service::{handle_store, transfer_reference, StoreOperation, StoreRequest, TransferSession},
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{self, BufRead, Read, Write};

pub(crate) fn run(args: &[String]) -> Result<()> {
    if args.first().is_some_and(|s| s == "broker") {
        return crate::agent_broker::run(&args[1..]);
    }
    if args.first().is_some_and(|s| s == "execution") {
        return crate::execution_cli::run(&args[1..]);
    }
    if args.first().is_some_and(|s| s == "rpc") {
        if args.len() != 1 {
            bail!("fabric rpc accepts no extra arguments");
        }
        return rpc();
    }
    if args.first().is_some_and(|s| s == "vfs") {
        return crate::vfs_cli::run(&args[1..]);
    }
    if args.first().is_some_and(|s| s == "workspace") {
        return crate::workspace_cli::run(&args[1..]);
    }
    if args.first().is_some_and(|s| s == "launch-error") {
        bail!("engine could not issue a scoped agent identity; inspect engine diagnostics");
    }
    if args.first().is_some_and(|s| s == "mcp") {
        return crate::agent_mcp::run(&args[1..]);
    }
    if args.first().is_some_and(|s| s == "capabilities") {
        println!(
            "{}",
            serde_json::json!({
                "format_version": FORMAT_VERSION,
                "object_hash": "sha256",
                "metadata_encoding": "canonical-json-v1",
                "authority": "local-only",
                "scheduler": false,
            })
        );
        return Ok(());
    }
    let mut raw = String::new();
    io::stdin()
        .take(16 * 1024 * 1024 + 1)
        .read_to_string(&mut raw)?;
    if raw.len() > 16 * 1024 * 1024 {
        bail!("fabric request exceeds 16 MiB");
    }
    let request: StoreRequest = serde_json::from_str(&raw)?;
    let home = super::signals::rudder_home()
        .ok_or_else(|| anyhow::anyhow!("Rudder home is unavailable"))?;
    let response = handle_store(&FabricPaths::new(&home), request)?;
    println!("{}", serde_json::to_string(&response)?);
    Ok(())
}

const RPC_REQUEST_HEADER: usize = 256 * 1024;
const RPC_RESPONSE_HEADER: usize = 32 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RpcRequest {
    format_version: u32,
    id: String,
    request: RpcOperation,
    binary_bytes: u64,
}
#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum RpcOperation {
    Store { request: StoreOperation },
    PinSource { root: ObjectRef },
    PinPartialSource { receipt_json: String },
    UnpinSource { pin: String },
    Inventory { root: ObjectRef },
    ReadBinary { object: ObjectRef },
    ReceiveBinary { lease_id: String, object: ObjectRef },
}

fn frame(output: &mut impl Write, id: &str, result: Result<Value>, bytes: &[u8]) -> Result<()> {
    let header = match result {
        Ok(result) => {
            json!({"format_version":1,"id":id,"ok":true,"result":result,"binary_bytes":bytes.len()})
        }
        Err(_) => {
            json!({"format_version":1,"id":id,"ok":false,"error":{"code":"local_request_failed","message":"Local CAS request failed; no transfer success is implied"},"binary_bytes":0})
        }
    };
    let encoded = serde_json::to_vec(&header)?;
    if encoded.len() > RPC_RESPONSE_HEADER {
        bail!("fabric RPC response exceeds header limit");
    }
    output.write_all(&encoded)?;
    output.write_all(b"\n")?;
    if header["ok"] == true {
        output.write_all(bytes)?;
    }
    output.flush()?;
    Ok(())
}

/// Each frame is one bounded UTF-8 JSON header, LF, then exactly binary_bytes
/// raw bytes. Binary bodies have no terminator and are never hex/JSON encoded.
/// This helper owns no App, scheduler, PTY, network credential or agent token.
fn rpc() -> Result<()> {
    let home = super::signals::rudder_home()
        .ok_or_else(|| anyhow::anyhow!("Rudder home is unavailable"))?;
    let mut session = TransferSession::open(&FabricPaths::new(&home))?;
    let input = io::stdin();
    let output = io::stdout();
    let mut reader = input.lock();
    let mut writer = output.lock();
    loop {
        let mut line = Vec::new();
        (&mut reader)
            .take(RPC_REQUEST_HEADER as u64 + 1)
            .read_until(b'\n', &mut line)?;
        if line.is_empty() {
            return session.finish();
        }
        if line.len() > RPC_REQUEST_HEADER || !line.ends_with(b"\n") {
            frame(&mut writer, "", Err(anyhow::anyhow!("invalid frame")), &[])?;
            return session.finish();
        }
        let request = match serde_json::from_slice::<RpcRequest>(&line) {
            Ok(request) => request,
            Err(_) => {
                frame(
                    &mut writer,
                    "",
                    Err(anyhow::anyhow!("invalid request")),
                    &[],
                )?;
                return session.finish();
            }
        };
        let valid = request.format_version == FORMAT_VERSION
            && !request.id.is_empty()
            && request.id.len() <= 128
            && request
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b));
        let valid_body = match &request.request {
            RpcOperation::ReceiveBinary { object, .. } => {
                transfer_reference(object).is_ok() && request.binary_bytes == object.size_bytes
            }
            _ => request.binary_bytes == 0,
        };
        if !valid || !valid_body {
            frame(
                &mut writer,
                &request.id,
                Err(anyhow::anyhow!("invalid frame")),
                &[],
            )?;
            // An invalid length cannot safely resynchronize the next frame.
            return session.finish();
        }
        let mut body = vec![0; request.binary_bytes as usize];
        if reader.read_exact(&mut body).is_err() {
            frame(
                &mut writer,
                &request.id,
                Err(anyhow::anyhow!("incomplete body")),
                &[],
            )?;
            return session.finish();
        }
        let mut outgoing = Vec::new();
        let result = (|| -> Result<Value> {
            Ok(match request.request {
                RpcOperation::Store { request } => session.call(StoreRequest {
                    format_version: FORMAT_VERSION,
                    request,
                })?,
                RpcOperation::PinSource { root } => json!({"owned_pin":session.pin_source(&root)?}),
                RpcOperation::PinPartialSource { receipt_json } => {
                    if receipt_json.len() > 64 * 1024 {
                        bail!("partial receipt exceeds byte limit");
                    }
                    let record: rudder_engine::delta::PartialTreeReceipt =
                        serde_json::from_str(&receipt_json)?;
                    serde_json::to_value(session.pin_partial_source(&record)?)?
                }
                RpcOperation::UnpinSource { pin } => {
                    session.unpin_source(&pin)?;
                    json!({"released":true})
                }
                RpcOperation::Inventory { root } => json!({"objects":session.inventory(&root)?}),
                RpcOperation::ReadBinary { object } => {
                    outgoing = session.read(&object)?;
                    json!({"object":object})
                }
                RpcOperation::ReceiveBinary { lease_id, object } => {
                    json!({"object":object,"children":session.receive(&lease_id,&object,&body)?})
                }
            })
        })();
        frame(&mut writer, &request.id, result, &outgoing)?;
    }
}
