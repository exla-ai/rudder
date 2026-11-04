//! Restart and terminal result delivery over the existing journal schema.
//! These methods require the trusted engine's exclusive OS lock. They neither
//! acquire fleet ownership nor prove that an external process was fenced.
use crate::journal::*;
use anyhow::{ensure, Context, Result};
use rusqlite::{params, TransactionBehavior};
use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
pub mod process;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionDelivery {
    /// Completion order, independent of acceptance order. A late completion
    /// must remain visible after a newer accepted command was delivered.
    pub sequence: u64,
    pub receipt: CommandReceipt,
}

impl Journal {
    /// Explicit process-restart entry point. Opening a journal or reconnecting
    /// a surface must NOT call this: the trusted engine holds the exclusive
    /// local owner lock and reconciles old external custody before replacement.
    /// Legacy `recover` remains the effect-only contract for existing callers.
    pub fn recover_runtime_restart(&mut self, owner: &OwnerFence, now_ms: i64) -> Result<Recovery> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_owner(&tx, owner)?;
        super::invalidate_recovered_runtimes(&tx, owner, now_ms)?;
        tx.commit()?;
        self.recover(owner, now_ms)
    }

    /// Bounded adoption of terminal receipts written by older engines. Repeat
    /// while this returns limit. No completed command is executed again.
    pub fn backfill_completion_outbox(
        &mut self,
        owner: &OwnerFence,
        limit: usize,
        now_ms: i64,
    ) -> Result<usize> {
        ensure!(
            (1..=256).contains(&limit),
            "completion backfill must be 1..256"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_owner(&tx, owner)?;
        let ids = {
            let mut statement = tx.prepare("SELECT c.command_id FROM commands c WHERE c.project_id=?1 AND c.state='completed' AND NOT EXISTS(SELECT 1 FROM events e WHERE e.project_id=c.project_id AND e.kind='command-result-ready' AND json_extract(e.payload_json,'$.command_id')=c.command_id) ORDER BY c.accepted_sequence LIMIT ?2")?;
            let ids = statement
                .query_map(params![owner.project_id, limit as i64], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            ids
        };
        for id in &ids {
            super::super::commands::publish_completion(&tx, id, now_ms)?;
        }
        tx.commit()?;
        Ok(ids.len())
    }

    /// At-least-once terminal receipts. The consumer must deduplicate by
    /// command_id and acknowledge only after its own durable application.
    /// Disconnecting or reading this page never acknowledges anything.
    pub fn completion_outbox(
        &self,
        owner: &OwnerFence,
        consumer: &str,
        limit: usize,
    ) -> Result<Vec<CompletionDelivery>> {
        assert_owner(&self.conn, owner)?;
        valid_id(consumer)?;
        ensure!((1..=256).contains(&limit), "completion page must be 1..256");
        let after = completion_cursor(&self.conn, &owner.project_id, consumer)?;
        let mut statement = self.conn.prepare("SELECT sequence,payload_json FROM events WHERE project_id=?1 AND kind='command-result-ready' AND sequence>?2 ORDER BY sequence LIMIT ?3")?;
        let rows = statement
            .query_map(
                params![owner.project_id, sql_u64(after)?, limit as i64],
                |row| Ok((db_u64(row, 0)?, row.get::<_, String>(1)?)),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(sequence, encoded)| {
                let payload: serde_json::Value = serde_json::from_str(&encoded)?;
                let receipt: CommandReceipt = serde_json::from_value(payload["receipt"].clone())?;
                ensure!(
                    receipt.project_id == owner.project_id
                        && receipt.state == CommandState::Completed,
                    "invalid completion outbox receipt"
                );
                Ok(CompletionDelivery { sequence, receipt })
            })
            .collect()
    }

    /// Acknowledge the processed prefix, never a send or an interrupt request.
    /// The cursor is an event in the same durable local journal, not UI memory.
    pub fn acknowledge_completions(
        &mut self,
        owner: &OwnerFence,
        consumer: &str,
        through: u64,
        now_ms: i64,
    ) -> Result<()> {
        valid_id(consumer)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_owner(&tx, owner)?;
        let previous = completion_cursor(&tx, &owner.project_id, consumer)?;
        if through <= previous {
            return Ok(());
        }
        let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM events WHERE project_id=?1 AND sequence=?2 AND kind='command-result-ready')", params![owner.project_id,sql_u64(through)?], |row| row.get(0))?;
        ensure!(
            exists,
            "completion acknowledgement is not an outbox sequence"
        );
        append_event(
            &tx,
            &owner.project_id,
            "command-results-acknowledged",
            &owner.owner_id,
            &serde_json::json!({"consumer":consumer,"through":through}),
            now_ms,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Use after recover/reconcile and proven external fencing, BEFORE admitting
    /// a replacement owner. The check and epoch change share one transaction.
    /// Pending old-attempt commands receive durable rejected terminal receipts.
    pub fn transfer_recovered_owner(
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
        require_settled_effects(&tx, &expected.project_id, false)?;
        let unresolved_runtime:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_observations WHERE project_id=?1 AND (json_extract(observation_json,'$.pty_owned')=1 OR json_extract(observation_json,'$.phase') NOT IN('closed','completed','failed')))",[&expected.project_id],|row|row.get(0))?;
        ensure!(
            !unresolved_runtime,
            "original runtime custody requires reconciliation before replacement ownership"
        );
        let next = expected
            .epoch
            .checked_add(1)
            .context("authority epoch exhausted")?;
        tx.execute(
            "UPDATE projects SET owner_id=?1,owner_epoch=?2,owner_active=1 WHERE project_id=?3",
            params![new_owner, sql_u64(next)?, expected.project_id],
        )?;
        bump_run_epochs(&tx, &expected.project_id)?;
        super::super::commands::retire_pending_targets(
            &tx,
            &expected.project_id,
            None,
            &expected.owner_id,
            now_ms,
        )?;
        append_event(
            &tx,
            &expected.project_id,
            "owner-transferred",
            new_owner,
            &serde_json::json!({"previous":expected,"epoch":next,"reconciled":true}),
            now_ms,
        )?;
        tx.commit()?;
        Ok(OwnerFence {
            project_id: expected.project_id.clone(),
            owner_id: new_owner.into(),
            epoch: next,
        })
    }

    /// Normal retirement refuses unresolved work and live runtime observations.
    /// The caller still must retain and physically join its actual process and
    /// worker owners before calling; database state alone cannot prove a join.
    pub fn release_drained_owner(&mut self, owner: &OwnerFence, now_ms: i64) -> Result<()> {
        self.release_drained_owner_inner(owner, now_ms, None)
    }

    pub fn release_drained_owner_with_budget(
        &mut self,
        owner: &OwnerFence,
        now_ms: i64,
        original: &crate::execution::finalization_budget::FinalizationBudget,
    ) -> Result<()> {
        self.release_drained_owner_inner(owner, now_ms, Some(original))
    }

    fn release_drained_owner_inner(
        &mut self,
        owner: &OwnerFence,
        now_ms: i64,
        original: Option<&crate::execution::finalization_budget::FinalizationBudget>,
    ) -> Result<()> {
        let check_deadline = || -> Result<()> {
            if let Some(original) = original {
                original.remaining()?;
            }
            Ok(())
        };
        check_deadline()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let next = owner
            .epoch
            .checked_add(1)
            .context("authority epoch exhausted")?;
        let state = project_state(&tx, &owner.project_id)?;
        // Several original runtimes can finish one shared project drain. Only
        // its actual durable release, at precisely the next epoch, is reusable.
        // A tombstone, lease expiry or a later/replacement owner is insufficient.
        if !state.owner_active
            && state.owner.owner_id == owner.owner_id
            && state.owner.epoch == next
        {
            let released:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM events WHERE project_id=?1 AND kind='owner-released' AND principal=?2 AND json_extract(payload_json,'$.epoch')=?3 AND json_extract(payload_json,'$.drained')=1)",params![owner.project_id,owner.owner_id,sql_u64(next)?],|row|row.get(0))?;
            ensure!(released, "owner lacks an original normal drain release");
            check_deadline()?;
            return Ok(());
        }
        assert_owner(&tx, owner)?;
        require_settled_effects(&tx, &owner.project_id, true)?;
        let live: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_observations WHERE project_id=?1 AND (json_extract(observation_json,'$.pty_owned')=1 OR json_extract(observation_json,'$.phase') NOT IN('closed','completed','failed')))", [&owner.project_id], |row| row.get(0))?;
        ensure!(!live, "runtime owners have not drained");
        tx.execute(
            "UPDATE projects SET owner_epoch=?1,owner_active=0 WHERE project_id=?2",
            params![sql_u64(next)?, owner.project_id],
        )?;
        bump_run_epochs(&tx, &owner.project_id)?;
        append_event(
            &tx,
            &owner.project_id,
            "owner-released",
            &owner.owner_id,
            &serde_json::json!({"epoch":next,"drained":true}),
            now_ms,
        )?;
        check_deadline()?;
        tx.commit()?;
        Ok(())
    }
}

fn completion_cursor(conn: &rusqlite::Connection, project: &str, consumer: &str) -> Result<u64> {
    Ok(conn.query_row("SELECT COALESCE(MAX(CAST(json_extract(payload_json,'$.through') AS INTEGER)),0) FROM events WHERE project_id=?1 AND kind='command-results-acknowledged' AND json_extract(payload_json,'$.consumer')=?2", params![project,consumer], |row| db_u64(row,0))?)
}

fn require_settled_effects(
    conn: &rusqlite::Connection,
    project: &str,
    include_pending: bool,
) -> Result<()> {
    let unresolved: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM effects e JOIN commands c ON c.command_id=e.command_id WHERE c.project_id=?1 AND (e.state IN('started','uncertain') OR (?2 AND e.state='pending')))", params![project,include_pending], |row| row.get(0))?;
    ensure!(
        !unresolved,
        "external effects require reconciliation before owner retirement or replacement"
    );
    Ok(())
}

pub(in crate::journal) fn require_runtime_admission(
    conn: &rusqlite::Connection,
    project: &str,
    run: &str,
    epoch: u64,
) -> Result<()> {
    let closed: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM events WHERE project_id=?1 AND kind='runtime-drain-started' AND json_extract(payload_json,'$.run_id')=?2 AND json_extract(payload_json,'$.run_epoch')=?3)", params![project,run,sql_u64(epoch)?], |row| row.get(0))?;
    ensure!(!closed, "runtime admission closed for intentional drain");
    Ok(())
}

#[cfg(test)]
mod tests;
