//! Explicit, cooperative managed-writer floor. This is recovery evidence, not
//! launch authority. The trusted caller holds its actual checkout owner lock
//! and establishes quiescence before preparing. Old TS writers and a different
//! local home are not physically revoked by a database schema or this record.
use super::*;
use std::path::{Component, PathBuf};

pub const MANAGED_WRITER_PROTOCOL: u32 = 1;

/// Historical presence evidence only; no live fence, source, session or grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedAttemptRecovery {
    Observed {
        run_id: String,
        attempt_id: String,
        run_epoch: u64,
        sequence: u64,
    },
    Malformed {
        sequence: u64,
    },
}
#[derive(Debug, Clone)]
pub struct ManagedRecoveryCursor {
    receipt_sha256: String,
    after: u64,
}
pub struct ManagedAttemptRecoveryPage {
    pub observations: Vec<ManagedAttemptRecovery>,
    pub next: Option<ManagedRecoveryCursor>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ObservedAttemptPayload {
    run_id: String,
    instance_id: String,
    epoch: u64,
}

/// Canonical identities are supplied by the trusted owner off its App actor.
/// No path or project is inferred from an environment variable by this API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedProjectNamespace {
    pub format_version: u32,
    pub project_id: String,
    pub home: PathBuf,
    pub journal: PathBuf,
    pub repository: PathBuf,
    pub directory: PathBuf,
}
impl ManagedProjectNamespace {
    pub fn directory_for(home: &Path, project_id: &str) -> PathBuf {
        home.join("fabric/managed-projects")
            .join(hex::encode(Sha256::digest(project_id.as_bytes())))
    }
    pub fn validate(&self) -> Result<()> {
        valid_id(&self.project_id)?;
        ensure!(
            self.format_version == 1,
            "unsupported managed namespace format"
        );
        for path in [&self.home, &self.journal, &self.repository, &self.directory] {
            ensure!(
                path.is_absolute()
                    && path.as_os_str().len() <= 4096
                    && path
                        .components()
                        .all(|part| matches!(part, Component::RootDir | Component::Normal(_))),
                "managed namespace requires explicit normalized absolute identities"
            );
        }
        ensure!(
            self.journal == self.home.join("fabric/journal.sqlite")
                && self.directory == Self::directory_for(&self.home, &self.project_id),
            "managed namespace differs from its home/project identity"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedProjectPrepare {
    pub request_id: String,
    pub namespace: ManagedProjectNamespace,
}
impl ManagedProjectPrepare {
    pub fn validate(&self) -> Result<()> {
        self.namespace.validate()?;
        ensure!(
            Uuid::parse_str(&self.request_id).is_ok_and(|id| id.to_string() == self.request_id),
            "managed prepare request must be a canonical reserved UUID"
        );
        ensure!(
            crate::protocol::canonical_json(self)?.len() <= 32 * 1024,
            "managed prepare request exceeds bound"
        );
        Ok(())
    }
}

/// Historical receipt. Its recorded owner is never a newly issued live fence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedProjectReceipt {
    pub format_version: u32,
    pub minimum_writer_protocol: u32,
    pub request: ManagedProjectPrepare,
    pub prepared_owner: OwnerFence,
    pub sequence: u64,
    pub prepared_at_ms: i64,
}
impl ManagedProjectReceipt {
    pub fn validate(&self) -> Result<()> {
        self.request.validate()?;
        ensure!(
            self.format_version == 1
                && self.minimum_writer_protocol == MANAGED_WRITER_PROTOCOL
                && self.prepared_owner.project_id == self.request.namespace.project_id
                && self.prepared_owner.epoch > 0
                && self.sequence > 0,
            "invalid or unsupported managed writer receipt"
        );
        valid_id(&self.prepared_owner.owner_id)?;
        Ok(())
    }
}

fn read_receipt(conn: &Connection, project_id: &str) -> Result<Option<ManagedProjectReceipt>> {
    let version: u32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version == 4 {
        return Ok(None);
    }
    ensure!(version == 5, "unsupported managed writer schema");
    let row: Option<(String, String, u32)> = conn.query_row(
        "SELECT request_json,receipt_json,minimum_writer_protocol FROM managed_projects WHERE project_id=?1",
        [project_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).optional()?;
    let Some((request, receipt, minimum)) = row else {
        return Ok(None);
    };
    ensure!(
        request.len() <= 32 * 1024 && receipt.len() <= 64 * 1024,
        "managed writer record exceeds bound"
    );
    let receipt: ManagedProjectReceipt = serde_json::from_str(&receipt)?;
    receipt.validate()?;
    ensure!(
        receipt.request.namespace.project_id == project_id
            && minimum == receipt.minimum_writer_protocol
            && crate::protocol::canonical_json(&receipt.request)? == request.as_bytes(),
        "managed writer record identity is inconsistent"
    );
    Ok(Some(receipt))
}

impl Journal {
    /// Bounded, project/receipt-bound historical recovery scan. The opaque
    /// cursor cannot choose an arbitrary lower sequence or another project.
    /// Invalid/oversize event payloads remain explicit malformed observations.
    /// Compatible callers cap the number of pages and expose budget exhaustion
    /// as quarantine. Pre-floor observations are never returned.
    pub fn managed_recovery_attempts(
        &self,
        receipt: &ManagedProjectReceipt,
        cursor: Option<&ManagedRecoveryCursor>,
        limit: usize,
    ) -> Result<ManagedAttemptRecoveryPage> {
        receipt.validate()?;
        ensure!(
            (1..=256).contains(&limit)
                && self.database_path()? == receipt.request.namespace.journal
                && self
                    .managed_project(&receipt.request.namespace.project_id)?
                    .as_ref()
                    == Some(receipt),
            "managed recovery requires the actual prepared receipt and bounded page"
        );
        let digest = hex::encode(Sha256::digest(crate::protocol::canonical_json(receipt)?));
        let after = if let Some(cursor) = cursor {
            ensure!(
                cursor.receipt_sha256 == digest && cursor.after >= receipt.sequence,
                "managed recovery cursor belongs to another prepared receipt"
            );
            cursor.after
        } else {
            receipt.sequence
        };
        let mut statement=self.conn.prepare("SELECT sequence,CASE WHEN length(CAST(payload_json AS BLOB))<=8192 THEN payload_json ELSE NULL END FROM events WHERE project_id=?1 AND sequence>?2 AND kind='run-attempt-observed' ORDER BY sequence LIMIT ?3")?;
        let mut rows = statement
            .query_map(
                params![
                    receipt.request.namespace.project_id,
                    sql_u64(after)?,
                    (limit + 1) as i64
                ],
                |row| Ok((db_u64(row, 0)?, row.get::<_, Option<String>>(1)?)),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let more = rows.len() > limit;
        if more {
            rows.pop();
        }
        let next = if more {
            Some(ManagedRecoveryCursor {
                receipt_sha256: digest,
                after: rows.last().context("empty managed recovery page")?.0,
            })
        } else {
            None
        };
        let observations = rows
            .into_iter()
            .map(|(sequence, raw)| {
                let parsed = raw
                    .and_then(|raw| serde_json::from_str::<ObservedAttemptPayload>(&raw).ok())
                    .filter(|value| {
                        valid_id(&value.run_id).is_ok()
                            && value.run_id.len() <= 256
                            && !value.run_id.chars().any(char::is_control)
                            && !value.run_id.contains(['/', '\\'])
                            && !matches!(value.run_id.as_str(), "." | "..")
                            && Uuid::parse_str(&value.instance_id)
                                .is_ok_and(|id| id.to_string() == value.instance_id)
                            && value.epoch > 0
                            && value.epoch <= i64::MAX as u64
                    });
                match parsed {
                    Some(value) => ManagedAttemptRecovery::Observed {
                        run_id: value.run_id,
                        attempt_id: value.instance_id,
                        run_epoch: value.epoch,
                        sequence,
                    },
                    None => ManagedAttemptRecovery::Malformed { sequence },
                }
            })
            .collect();
        Ok(ManagedAttemptRecoveryPage { observations, next })
    }
    /// Read-only recovery lookup, including after an owner restart. This never
    /// revives the historical fence, upgrades schema4 or authorizes a launch.
    pub fn managed_project(&self, project_id: &str) -> Result<Option<ManagedProjectReceipt>> {
        valid_id(project_id)?;
        read_receipt(&self.conn, project_id)
    }

    /// Off-App. Caller holds its actual exclusive owner lock and has quiesced
    /// compatible/known legacy writers. Schema5 is installed only in this
    /// explicit opt-in transaction, atomically with the owner-fenced receipt.
    /// Ordinary schema4 opens and projects are left at schema4 until opt-in.
    pub fn prepare_managed_project(
        &mut self,
        owner: &OwnerFence,
        request: &ManagedProjectPrepare,
        now_ms: i64,
    ) -> Result<ManagedProjectReceipt> {
        request.validate()?;
        ensure!(
            request.namespace.project_id == owner.project_id
                && self.database_path()? == request.namespace.journal,
            "managed prepare differs from the owned project/database"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_owner(&tx, owner)?;
        if let Some(receipt) = read_receipt(&tx, &owner.project_id)? {
            ensure!(
                receipt.request == *request,
                "managed project already prepared by a different request/identity"
            );
            tx.commit()?;
            return Ok(receipt);
        }
        tx.execute_batch(
            r#"
CREATE TABLE IF NOT EXISTS managed_projects(
 project_id TEXT PRIMARY KEY REFERENCES projects(project_id),
 minimum_writer_protocol INTEGER NOT NULL CHECK(minimum_writer_protocol>0),
 request_json TEXT NOT NULL CHECK(length(request_json)<=32768),
 receipt_json TEXT NOT NULL CHECK(length(receipt_json)<=65536));
PRAGMA user_version=5;
"#,
        )?;
        let sequence = append_event(
            &tx,
            &owner.project_id,
            "managed-project-prepared",
            &owner.owner_id,
            &serde_json::to_value(request)?,
            now_ms,
        )?;
        let receipt = ManagedProjectReceipt {
            format_version: 1,
            minimum_writer_protocol: MANAGED_WRITER_PROTOCOL,
            request: request.clone(),
            prepared_owner: owner.clone(),
            sequence,
            prepared_at_ms: now_ms,
        };
        receipt.validate()?;
        tx.execute("INSERT INTO managed_projects(project_id,minimum_writer_protocol,request_json,receipt_json) VALUES(?1,?2,?3,?4)",
            params![owner.project_id, MANAGED_WRITER_PROTOCOL, String::from_utf8(crate::protocol::canonical_json(request)?)?,
                String::from_utf8(crate::protocol::canonical_json(&receipt)?)?])?;
        tx.commit()?;
        Ok(receipt)
    }
}
