//! Durable local authority, command inbox/outbox and addressed agent mailboxes.
//!
//! This database belongs on one machine's local filesystem. It is not a fleet
//! coordinator, a lease service or a database to share over NFS. Authority
//! management methods are for the trusted engine, which must hold its OS owner
//! lock and quiesce legacy writers before claiming ownership. Agent ingress
//! uses issued capabilities, never a caller-supplied author or role.
//! The engine must keep this database inaccessible to untrusted workers;
//! arbitrary same-UID filesystem access can bypass any in-process API policy.
//!
//! A committed SQLite receipt proves durable intent, not an external effect or
//! model compliance. Claim before acting, and reconcile uncertain claims after
//! a crash. External tools must enforce fencing too; SQLite cannot retract an
//! already issued network request or kill an escaped process.

use anyhow::{bail, ensure, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;
use uuid::Uuid;

use crate::protocol::ObjectRef;

mod commands;
mod decisions;
mod mailbox;
mod schema;
mod runtime;
mod vcs;

pub use commands::{
    CommandReceipt, CommandRequest, CommandState, EffectClaim, Reconciliation, Recovery,
};
pub use decisions::{
    AcceptedDecision, DecisionRequest, IntegrationEvidence, IntegrationReceipt, IntegrationRequest,
};
pub use mailbox::{MailboxCursor, MessageEnvelope, MessageIntent, MessagePriority, MessageRequest};
pub use runtime::{DiscoveredAgent, RuntimeObservation, RuntimePhase, RuntimeSnapshot, RUNTIME_MAX_AGE_MS};

pub use vcs::{
    PreparedVcsOperation, VcsIntentReceipt, VcsIntentRequest, VcsIntentState,
    VcsPublicationObservation, VcsPublisher, VcsRunFence,
};

pub type TopicVersions = BTreeMap<String, u64>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerFence {
    pub project_id: String,
    pub owner_id: String,
    pub epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectState {
    pub owner: OwnerFence,
    pub owner_active: bool,
    pub integration_head: String,
    pub sequence: u64,
}

/// Immutable grant issued by the trusted authority. No implicit wildcards.
/// Operations include `command:<kind>`, `message:send`, `mailbox:read`,
/// `mailbox:ack` and `decision:accept`. Recipient, scope and topic grants are
/// exact strings. Mailbox access is always restricted to the grant's own run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityGrant {
    pub principal: String,
    pub run_id: String,
    pub run_epoch: u64,
    pub operations: BTreeSet<String>,
    pub recipients: BTreeMap<String, u64>,
    pub scopes: BTreeSet<String>,
    pub topics: BTreeSet<String>,
    pub expires_at_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalEvent {
    pub event_id: String,
    pub project_id: String,
    pub sequence: u64,
    pub kind: String,
    pub principal: String,
    pub payload: Value,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentIdentity {
    pub project_id: String,
    pub run_id: String,
    pub run_epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityIdentity {
    pub project_id: String,
    pub principal: String,
    pub run_id: String,
    pub run_epoch: u64,
    pub owner_epoch: u64,
}

pub struct Journal {
    conn: Connection,
}

impl Journal {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_busy_timeout(path, Duration::from_secs(5))
    }

    /// The agent API uses a shorter lock wait for bounded inbox awaits. This
    /// changes contention waiting only, never durability or transaction mode.
    pub fn open_with_busy_timeout(path: impl AsRef<Path>, busy_timeout: Duration) -> Result<Self> {
        // WAL-reset corruption is fixed in 3.51.3 and newer (and selected
        // backports). We deliberately require the current branch rather than
        // maintaining a permissive version table for old system libraries.
        ensure!(
            rusqlite::version_number() >= 3_051_003,
            "SQLite >= 3.51.3 is required for WAL safety"
        );
        let path = path.as_ref();
        ensure!(
            !path.as_os_str().is_empty()
                && path != Path::new(":memory:")
                && !path.to_string_lossy().starts_with("file:"),
            "durable journal requires a filesystem path, not a temporary database or URI"
        );
        let conn = Connection::open(path).context("open local engine journal")?;
        #[cfg(unix)]
        if path != Path::new(":memory:") {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        conn.busy_timeout(busy_timeout)?;
        conn.execute_batch(
            "PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;",
        )?;
        let mode: String = conn.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        ensure!(mode == "wal", "durable journal requires WAL mode");
        let mut journal = Self { conn };
        schema::initialize(&mut journal.conn)?;
        Ok(journal)
    }

    /// Creates only a local authority. Existing ownership must be transferred
    /// explicitly; reopening a file never silently grants authority.
    pub fn create_project(
        &mut self,
        project_id: &str,
        owner_id: &str,
        initial_head: &str,
        now_ms: i64,
    ) -> Result<OwnerFence> {
        valid_id(project_id)?;
        valid_id(owner_id)?;
        valid_id(initial_head)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("INSERT INTO projects(project_id,authority_kind,owner_id,owner_epoch,owner_active,integration_head,event_seq) VALUES(?1,'local',?2,1,1,?3,0)", params![project_id,owner_id,initial_head])?;
        append_event(
            &tx,
            project_id,
            "project-created",
            owner_id,
            &serde_json::json!({"head":initial_head,"authority":"local"}),
            now_ms,
        )?;
        tx.commit()?;
        Ok(OwnerFence {
            project_id: project_id.into(),
            owner_id: owner_id.into(),
            epoch: 1,
        })
    }

    pub fn project_state(&self, project_id: &str) -> Result<ProjectState> {
        project_state(&self.conn, project_id)
    }

    /// Caller must already hold the exclusive OS engine lock. No timestamp or
    /// advisory timeout can authorize this operation. Old adapter tokens are
    /// invalidated by increasing every run epoch in the same transaction.
    pub fn transfer_owner(
        &mut self,
        expected: &OwnerFence,
        new_owner: &str,
        now_ms: i64,
    ) -> Result<OwnerFence> {
        valid_id(new_owner)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state = project_state(&tx, &expected.project_id)?;
        ensure!(state.owner == *expected, "stale authority epoch");
        let next = expected
            .epoch
            .checked_add(1)
            .context("authority epoch exhausted")?;
        tx.execute(
            "UPDATE projects SET owner_id=?1,owner_epoch=?2,owner_active=1 WHERE project_id=?3",
            params![new_owner, sql_u64(next)?, expected.project_id],
        )?;
        bump_run_epochs(&tx, &expected.project_id)?;
        commands::retire_pending_targets(&tx, &expected.project_id, None, &expected.owner_id, now_ms)?;
        append_event(
            &tx,
            &expected.project_id,
            "owner-transferred",
            new_owner,
            &serde_json::json!({"previous":expected,"epoch":next}),
            now_ms,
        )?;
        tx.commit()?;
        Ok(OwnerFence {
            project_id: expected.project_id.clone(),
            owner_id: new_owner.into(),
            epoch: next,
        })
    }

    pub fn release_owner(&mut self, expected: &OwnerFence, now_ms: i64) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_owner(&tx, expected)?;
        let next = expected
            .epoch
            .checked_add(1)
            .context("authority epoch exhausted")?;
        tx.execute(
            "UPDATE projects SET owner_epoch=?1,owner_active=0 WHERE project_id=?2",
            params![sql_u64(next)?, expected.project_id],
        )?;
        bump_run_epochs(&tx, &expected.project_id)?;
        commands::retire_pending_targets(&tx, &expected.project_id, None, &expected.owner_id, now_ms)?;
        append_event(
            &tx,
            &expected.project_id,
            "owner-released",
            &expected.owner_id,
            &serde_json::json!({"epoch":next}),
            now_ms,
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn register_run(&mut self, owner: &OwnerFence, run_id: &str, now_ms: i64) -> Result<u64> {
        valid_id(run_id)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_owner(&tx, owner)?;
        tx.execute(
            "INSERT INTO runs(project_id,run_id,epoch) VALUES(?1,?2,1)",
            params![owner.project_id, run_id],
        )?;
        append_event(
            &tx,
            &owner.project_id,
            "run-registered",
            &owner.owner_id,
            &serde_json::json!({"run_id":run_id,"epoch":1}),
            now_ms,
        )?;
        tx.commit()?;
        Ok(1)
    }

    pub fn run_epoch(&self, project_id: &str, run_id: &str) -> Result<u64> {
        run_epoch(&self.conn, project_id, run_id)
    }

    /// Trusted local actor inventory used when minting exact peer grants.
    /// Agent ingress uses find_agents and cannot enumerate ungranted peers.
    pub fn owned_runs(&self, owner: &OwnerFence) -> Result<Vec<AgentIdentity>> {
        assert_owner(&self.conn, owner)?;
        let mut statement = self.conn.prepare("SELECT run_id,epoch FROM runs WHERE project_id=?1 ORDER BY run_id")?;
        let rows = statement.query_map([&owner.project_id], |row| Ok(AgentIdentity {
            project_id: owner.project_id.clone(), run_id: row.get(0)?, run_epoch: db_u64(row, 1)?,
        }))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Trusted runtime observation of a PTY/backend-session instance. Repeated
    /// heartbeats are idempotent; replacing an instance fences its old tokens.
    /// First registration/binding must precede capability issuance.
    pub fn observe_run_attempt(
        &mut self,
        owner: &OwnerFence,
        run_id: &str,
        instance_id: &str,
        now_ms: i64,
    ) -> Result<u64> {
        valid_id(run_id)?;
        valid_id(instance_id)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_owner(&tx, owner)?;
        let old: Option<(u64, Option<String>)> = tx
            .query_row(
                "SELECT epoch,instance_id FROM runs WHERE project_id=?1 AND run_id=?2",
                params![owner.project_id, run_id],
                |row| Ok((db_u64(row, 0)?, row.get(1)?)),
            )
            .optional()?;
        let epoch = match old {
            Some((epoch, Some(old))) if old == instance_id => return Ok(epoch),
            Some((epoch, Some(_))) => {
                let next = epoch.checked_add(1).context("run epoch exhausted")?;
                tx.execute(
                    "UPDATE runs SET epoch=?1,instance_id=?2 WHERE project_id=?3 AND run_id=?4",
                    params![sql_u64(next)?, instance_id, owner.project_id, run_id],
                )?;
                next
            }
            Some((epoch, None)) => {
                tx.execute(
                    "UPDATE runs SET instance_id=?1 WHERE project_id=?2 AND run_id=?3",
                    params![instance_id, owner.project_id, run_id],
                )?;
                epoch
            }
            None => {
                tx.execute(
                    "INSERT INTO runs(project_id,run_id,epoch,instance_id) VALUES(?1,?2,1,?3)",
                    params![owner.project_id, run_id, instance_id],
                )?;
                1
            }
        };
        commands::retire_pending_targets(&tx, &owner.project_id, Some(run_id), &owner.owner_id, now_ms)?;
        append_event(
            &tx,
            &owner.project_id,
            "run-attempt-observed",
            &owner.owner_id,
            &serde_json::json!({"run_id":run_id,"instance_id":instance_id,"epoch":epoch}),
            now_ms,
        )?;
        tx.commit()?;
        Ok(epoch)
    }

    pub fn advance_run_epoch(
        &mut self,
        owner: &OwnerFence,
        run_id: &str,
        expected_epoch: u64,
        now_ms: i64,
    ) -> Result<u64> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_owner(&tx, owner)?;
        assert_run(&tx, &owner.project_id, run_id, expected_epoch)?;
        let next = expected_epoch
            .checked_add(1)
            .context("run epoch exhausted")?;
        tx.execute(
            "UPDATE runs SET epoch=?1 WHERE project_id=?2 AND run_id=?3",
            params![sql_u64(next)?, owner.project_id, run_id],
        )?;
        commands::retire_pending_targets(&tx, &owner.project_id, Some(run_id), &owner.owner_id, now_ms)?;
        append_event(
            &tx,
            &owner.project_id,
            "run-epoch-advanced",
            &owner.owner_id,
            &serde_json::json!({"run_id":run_id,"epoch":next}),
            now_ms,
        )?;
        tx.commit()?;
        Ok(next)
    }

    /// Returns the secret once. Only its SHA-256 hash is retained in SQLite.
    /// Persist/transport this token through protected engine channels only.
    pub fn issue_capability(
        &mut self,
        owner: &OwnerFence,
        grant: &CapabilityGrant,
        now_ms: i64,
    ) -> Result<String> {
        valid_id(&grant.principal)?;
        ensure!(!grant.operations.is_empty(), "capability has no operations");
        ensure!(
            grant.expires_at_ms.is_none_or(|at| at > now_ms),
            "capability already expired"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_owner(&tx, owner)?;
        assert_run(&tx, &owner.project_id, &grant.run_id, grant.run_epoch)?;
        for (recipient, epoch) in &grant.recipients {
            assert_run(&tx, &owner.project_id, recipient, *epoch)?;
        }
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let digest = token_hash(&token);
        tx.execute("INSERT INTO capabilities(token_hash,project_id,grant_json,revoked,issued_at) VALUES(?1,?2,?3,0,?4)",params![digest,owner.project_id,json(grant)?,now_ms])?;
        append_event(
            &tx,
            &owner.project_id,
            "capability-issued",
            &owner.owner_id,
            &serde_json::json!({"token_hash":digest,"grant":grant}),
            now_ms,
        )?;
        tx.commit()?;
        Ok(token)
    }

    pub fn revoke_capability(
        &mut self,
        owner: &OwnerFence,
        token: &str,
        now_ms: i64,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_owner(&tx, owner)?;
        ensure!(
            tx.execute(
                "UPDATE capabilities SET revoked=1 WHERE project_id=?1 AND token_hash=?2",
                params![owner.project_id, token_hash(token)]
            )? == 1,
            "unknown capability"
        );
        append_event(
            &tx,
            &owner.project_id,
            "capability-revoked",
            &owner.owner_id,
            &serde_json::json!({"token_hash":token_hash(token)}),
            now_ms,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Only self and explicitly granted recipients are discoverable. Labels
    /// and richer presence metadata belong to a separate scoped projection.
    pub fn find_agents(&self, token: &str, now_ms: i64) -> Result<Vec<AgentIdentity>> {
        let tx = self.conn.unchecked_transaction()?;
        let auth = authenticate(&tx, token, now_ms)?;
        authorize(&tx, &auth, "agents:find")?;
        let mut runs = auth.grant.recipients.clone();
        runs.insert(auth.grant.run_id, auth.grant.run_epoch);
        let agents = runs
            .into_iter()
            .filter_map(
                |(run_id, epoch)| match run_epoch(&tx, &auth.project, &run_id) {
                    Ok(current) if current != epoch => None,
                    Err(error) => Some(Err(error)),
                    Ok(_) => Some(Ok(AgentIdentity {
                        run_epoch: epoch,
                        project_id: auth.project.clone(),
                        run_id,
                    })),
                },
            )
            .collect::<Result<Vec<_>>>()?;
        tx.commit()?;
        Ok(agents)
    }

    pub fn authenticate_agent(&self, token: &str, now_ms: i64) -> Result<CapabilityIdentity> {
        let tx = self.conn.unchecked_transaction()?;
        let auth = authenticate(&tx, token, now_ms)?;
        assert_run(&tx, &auth.project, &auth.grant.run_id, auth.grant.run_epoch)?;
        let owner_epoch = project_state(&tx, &auth.project)?.owner.epoch;
        let identity = CapabilityIdentity {
            project_id: auth.project,
            principal: auth.grant.principal,
            run_id: auth.grant.run_id,
            run_epoch: auth.grant.run_epoch,
            owner_epoch,
        };
        tx.commit()?;
        Ok(identity)
    }

    /// Read-only, exact-attempt authorization for the helper that owns a view.
    /// This grants neither a checkpoint nor a new owner/scheduler. The scope
    /// commits every workspace binding field, including profile and namespace.
    pub fn authorize_workspace(&self, token: &str, binding: &crate::vfs::rpc::ObservationBinding, now_ms: i64) -> Result<CapabilityIdentity> {
        binding.validate()?;
        let scope = crate::execution::workspace_scope(binding)?;
        let tx = self.conn.unchecked_transaction()?;
        let auth = authenticate(&tx, token, now_ms)?;
        authorize(&tx, &auth, "workspace:observe")?;
        ensure!(auth.grant.scopes.contains(&scope), "workspace is outside capability");
        let project = project_state(&tx, &auth.project)?;
        ensure!(project.owner.owner_id == binding.owner.owner && project.owner.epoch == binding.owner.epoch, "workspace owner changed");
        let instance: Option<String> = tx.query_row("SELECT instance_id FROM runs WHERE project_id=?1 AND run_id=?2", params![auth.project,auth.grant.run_id], |row| row.get(0))?;
        ensure!(instance.as_deref() == Some(binding.attempt.execution_id.as_str()), "workspace execution attempt changed");
        let identity = CapabilityIdentity { project_id: auth.project, principal: auth.grant.principal, run_id: auth.grant.run_id, run_epoch: auth.grant.run_epoch, owner_epoch: project.owner.epoch };
        tx.commit()?;
        Ok(identity)
    }

    /// Trusted engine projection. Do not expose a raw project-wide journal to
    /// an agent capability; use its scoped inbox instead.
    pub fn events(&self, project_id: &str, after: u64, limit: usize) -> Result<Vec<JournalEvent>> {
        ensure!((1..=1000).contains(&limit), "event page must be 1..1000");
        let mut stmt=self.conn.prepare("SELECT event_id,project_id,sequence,kind,principal,payload_json,created_at FROM events WHERE project_id=?1 AND sequence>?2 ORDER BY sequence LIMIT ?3")?;
        let rows = stmt.query_map(params![project_id, sql_u64(after)?, limit as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                db_u64(r, 2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, i64>(6)?,
            ))
        })?;
        rows.map(|row| {
            let (id, p, sequence, kind, principal, payload, created_at_ms) = row?;
            Ok(JournalEvent {
                event_id: id,
                project_id: p,
                sequence,
                kind,
                principal,
                payload: serde_json::from_str(&payload)?,
                created_at_ms,
            })
        })
        .collect()
    }
}

fn valid_id(value: &str) -> Result<()> {
    ensure!(
        !value.trim().is_empty() && value.len() <= 1024,
        "invalid identifier"
    );
    Ok(())
}
fn sql_u64(value: u64) -> Result<i64> {
    i64::try_from(value).context("integer exceeds SQLite range")
}
fn db_u64(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}
fn db_optional_u64(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Option<u64>> {
    row.get::<_, Option<i64>>(index)?
        .map(|value| {
            u64::try_from(value).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    index,
                    rusqlite::types::Type::Integer,
                    Box::new(error),
                )
            })
        })
        .transpose()
}
fn json<T: Serialize>(value: &T) -> Result<String> {
    // Command/message bodies may contain floats. Canonicalize map order for
    // idempotency without imposing the stricter CAS object metadata contract.
    fn sort(value: Value) -> Value {
        match value {
            Value::Object(map) => {
                let sorted: BTreeMap<_, _> = map.into_iter().map(|(k, v)| (k, sort(v))).collect();
                Value::Object(sorted.into_iter().collect())
            }
            Value::Array(items) => Value::Array(items.into_iter().map(sort).collect()),
            other => other,
        }
    }
    Ok(serde_json::to_string(&sort(serde_json::to_value(value)?))?)
}
fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}
fn project_state(conn: &Connection, project: &str) -> Result<ProjectState> {
    conn.query_row("SELECT owner_id,owner_epoch,owner_active,integration_head,event_seq FROM projects WHERE project_id=?1 AND authority_kind='local'",[project],|r|Ok(ProjectState{owner:OwnerFence{project_id:project.into(),owner_id:r.get(0)?,epoch:db_u64(r,1)?},owner_active:r.get(2)?,integration_head:r.get(3)?,sequence:db_u64(r,4)?})).context("unknown local project")
}
fn assert_owner(conn: &Connection, fence: &OwnerFence) -> Result<()> {
    let state = project_state(conn, &fence.project_id)?;
    ensure!(
        state.owner_active && state.owner == *fence,
        "stale authority epoch"
    );
    Ok(())
}
fn run_epoch(conn: &Connection, project: &str, run: &str) -> Result<u64> {
    conn.query_row(
        "SELECT epoch FROM runs WHERE project_id=?1 AND run_id=?2",
        params![project, run],
        |r| db_u64(r, 0),
    )
    .context("unknown project run")
}
fn assert_run(conn: &Connection, project: &str, run: &str, epoch: u64) -> Result<()> {
    ensure!(
        epoch > 0 && run_epoch(conn, project, run)? == epoch,
        "stale run epoch"
    );
    Ok(())
}
fn bump_run_epochs(tx: &Transaction<'_>, project: &str) -> Result<()> {
    let exhausted: i64 = tx.query_row(
        "SELECT count(*) FROM runs WHERE project_id=?1 AND epoch>=9223372036854775807",
        [project],
        |r| r.get(0),
    )?;
    ensure!(exhausted == 0, "run epoch exhausted");
    tx.execute(
        "UPDATE runs SET epoch=epoch+1 WHERE project_id=?1",
        [project],
    )?;
    Ok(())
}
struct Auth {
    project: String,
    grant: CapabilityGrant,
}
fn authenticate(conn: &Connection, token: &str, now_ms: i64) -> Result<Auth> {
    authenticate_hash(conn, &token_hash(token), now_ms)
}
fn authenticate_hash(conn: &Connection, hash: &str, now_ms: i64) -> Result<Auth> {
    let row: Option<(String, String, bool)> = conn
        .query_row(
            "SELECT project_id,grant_json,revoked FROM capabilities WHERE token_hash=?1",
            [hash],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let (project, grant, revoked) = row.context("invalid capability")?;
    let grant: CapabilityGrant = serde_json::from_str(&grant)?;
    ensure!(
        !revoked && grant.expires_at_ms.is_none_or(|at| at > now_ms),
        "revoked or expired capability"
    );
    ensure!(
        project_state(conn, &project)?.owner_active,
        "project authority is inactive"
    );
    Ok(Auth { project, grant })
}
fn authorize(conn: &Connection, auth: &Auth, operation: &str) -> Result<()> {
    assert_run(
        conn,
        &auth.project,
        &auth.grant.run_id,
        auth.grant.run_epoch,
    )?;
    ensure!(
        auth.grant.operations.contains(operation),
        "capability does not grant operation"
    );
    Ok(())
}
fn authorize_target(
    conn: &Connection,
    auth: &Auth,
    run: &str,
    epoch: u64,
    scope: &str,
) -> Result<()> {
    ensure!(
        auth.grant.recipients.get(run) == Some(&epoch),
        "recipient is outside capability"
    );
    ensure!(
        auth.grant.scopes.contains(scope),
        "scope is outside capability"
    );
    assert_run(conn, &auth.project, run, epoch)
}
fn append_event(
    tx: &Transaction<'_>,
    project: &str,
    kind: &str,
    principal: &str,
    payload: &Value,
    now_ms: i64,
) -> Result<u64> {
    let sequence: u64 = tx.query_row(
        "UPDATE projects SET event_seq=event_seq+1 WHERE project_id=?1 AND event_seq<9223372036854775807 RETURNING event_seq",
        [project],
        |r| db_u64(r, 0),
    )?;
    tx.execute("INSERT INTO events(project_id,sequence,event_id,kind,principal,payload_json,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![project,sql_u64(sequence)?,Uuid::new_v4().to_string(),kind,principal,json(payload)?,now_ms])?;
    Ok(sequence)
}
