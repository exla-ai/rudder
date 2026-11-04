//! Off-loop normal retirement of an owned local child and its enrolled workers.
//! This proves only the owners actually transferred here, not an unenrolled
//! descendant, a provider VM, a Root service or a whole managed-machine census.
use crate::{
    execution::{
        finalization_budget::FinalizationBudget,
        handoff::{OwnedWorkerJoin, WorkerJoined},
    },
    journal::{Journal, OwnerFence, RuntimeObservation},
    protocol::{ObjectKind, ObjectRef},
};
use anyhow::{ensure, Context, Result};
use std::{
    process::{Child, ExitStatus},
    time::Duration,
};

/// The actual resource owner stays inside the drain on every failure. Its
/// checkpoint implementation must quiesce writers and durably pin the closure
/// before returning. Retirement must be idempotent for the supplied drain ID,
/// retain partial custody on error and honor the SAME original deadline.
pub trait RuntimeRetirement {
    fn checkpoint(&mut self, drain_id: &str, original: &FinalizationBudget) -> Result<ObjectRef>;
    fn retire(&mut self, drain_id: &str, original: &FinalizationBudget) -> Result<()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeDrainPhase {
    ClosingAdmission,
    Checkpointed,
    ProcessJoined,
    WorkersJoined,
    TerminalPersisted,
    OwnerReleased,
    Retired,
}

/// Explicit reconciliation of a failed/unknown checkpoint, never permission
/// inferred from a timeout. Pinned requires the actual resource owner to retain
/// the named closure; RetrySafe requires proof the old attempt did not commit
/// or that the resource owner deduplicates the original drain ID.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RuntimeCheckpointResolution {
    Pinned {
        checkpoint: ObjectRef,
        evidence: serde_json::Value,
    },
    RetrySafe {
        evidence: serde_json::Value,
    },
}

/// An actual Child, not a PID or tombstone, supplies physical process custody.
/// Dropping this carrier does not stop, join or retire anything. Keep it through
/// failure; a replacement carrier cannot reset its deadline or checkpoint.
#[must_use = "retain the original child, workers, checkpoint and resources until retired"]
pub struct LocalRuntimeDrain<R> {
    id: String,
    owner: OwnerFence,
    observation: RuntimeObservation,
    child: Child,
    workers: Vec<OwnedWorkerJoin>,
    joined: Vec<WorkerJoined>,
    resources: R,
    original: FinalizationBudget,
    checkpoint: Option<ObjectRef>,
    checkpoint_failed: bool,
    exit: Option<ExitStatus>,
    phase: RuntimeDrainPhase,
}

impl<R: RuntimeRetirement> LocalRuntimeDrain<R> {
    /// Infallible custody transfer. All fallible identity/time/database checks
    /// happen later with these actual owners already retained in this value.
    pub fn new(
        owner: OwnerFence,
        observation: RuntimeObservation,
        child: Child,
        workers: Vec<OwnedWorkerJoin>,
        resources: R,
        original: FinalizationBudget,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            owner,
            observation,
            child,
            workers,
            joined: Vec::new(),
            resources,
            original,
            checkpoint: None,
            checkpoint_failed: false,
            exit: None,
            phase: RuntimeDrainPhase::ClosingAdmission,
        }
    }

    pub fn phase(&self) -> RuntimeDrainPhase {
        self.phase
    }
    pub fn original_process_id(&self) -> u32 {
        self.child.id()
    }
    pub fn checkpoint_reference(&self) -> Option<&ObjectRef> {
        self.checkpoint.as_ref()
    }
    pub fn exit_status(&self) -> Option<ExitStatus> {
        self.exit
    }
    pub fn original_budget(&self) -> &FinalizationBudget {
        &self.original
    }

    pub fn reconcile_checkpoint_failure(
        &mut self,
        journal: &mut Journal,
        resolution: &RuntimeCheckpointResolution,
        now_ms: i64,
    ) -> Result<()> {
        use crate::journal::*;
        self.original.remaining()?;
        ensure!(
            self.phase == RuntimeDrainPhase::ClosingAdmission && self.checkpoint_failed,
            "original checkpoint is not awaiting reconciliation"
        );
        match resolution {
            RuntimeCheckpointResolution::Pinned {
                checkpoint,
                evidence,
            } => {
                ensure!(
                    !evidence.is_null(),
                    "checkpoint reconciliation requires evidence"
                );
                checkpoint.validate()?;
                ensure!(
                    checkpoint.kind == ObjectKind::Checkpoint,
                    "reconciled runtime checkpoint has wrong kind"
                );
            }
            RuntimeCheckpointResolution::RetrySafe { evidence } => ensure!(
                !evidence.is_null(),
                "checkpoint safe retry requires evidence"
            ),
        }
        let tx = journal
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_owner(&tx, &self.owner)?;
        assert_run(
            &tx,
            &self.owner.project_id,
            &self.observation.run_id,
            self.observation.run_epoch,
        )?;
        append_event(
            &tx,
            &self.owner.project_id,
            "runtime-checkpoint-reconciled",
            &self.owner.owner_id,
            &serde_json::json!({"drain_id":self.id,"resolution":resolution}),
            now_ms,
        )?;
        self.original.remaining()?;
        tx.commit()?;
        if let RuntimeCheckpointResolution::Pinned { checkpoint, .. } = resolution {
            self.checkpoint = Some(checkpoint.clone());
            self.phase = RuntimeDrainPhase::Checkpointed;
        }
        self.checkpoint_failed = false;
        Ok(())
    }

    /// Poll OFF the App loop. A stop request is closing this owned child's
    /// input, never a join. try_wait reaps the actual Child, then each original
    /// worker must physically join, including TLS, before terminal persistence.
    pub fn advance_off_loop(
        &mut self,
        journal: &mut Journal,
        now_ms: i64,
    ) -> Result<RuntimeDrainPhase> {
        if self.phase == RuntimeDrainPhase::Retired {
            return Ok(self.phase);
        }
        self.original.remaining()?;
        if self.phase == RuntimeDrainPhase::ClosingAdmission {
            journal.close_runtime_admission(
                &self.owner,
                &self.observation,
                &self.id,
                now_ms,
                &self.original,
            )?;
            ensure!(
                !self.checkpoint_failed,
                "original checkpoint failed; reconcile it before any retry"
            );
            // A failed/unknown checkpoint is never automatically issued again.
            self.checkpoint_failed = true;
            let checkpoint = self.resources.checkpoint(&self.id, &self.original)?;
            checkpoint.validate()?;
            ensure!(
                checkpoint.kind == ObjectKind::Checkpoint,
                "runtime drain requires a checkpoint root"
            );
            self.checkpoint = Some(checkpoint);
            self.checkpoint_failed = false;
            self.phase = RuntimeDrainPhase::Checkpointed;
            self.original.remaining()?;
        }
        if self.phase == RuntimeDrainPhase::Checkpointed {
            // EOF is cooperative retirement for an owned local process. Backend
            // adapters needing RPC shutdown must send it before transferring
            // custody; this carrier never manufactures an adapter ACK.
            self.child.stdin.take();
            let Some(exit) = self
                .child
                .try_wait()
                .context("join original runtime child")?
            else {
                return Ok(self.phase);
            };
            self.exit = Some(exit);
            self.phase = RuntimeDrainPhase::ProcessJoined;
            self.original.remaining()?;
        }
        if self.phase == RuntimeDrainPhase::ProcessJoined {
            while self.joined.len() < self.workers.len() {
                self.original.remaining()?;
                let worker = &self.workers[self.joined.len()];
                worker.start_join_observer()?;
                let Some(joined) = worker.try_joined()? else {
                    return Ok(self.phase);
                };
                self.joined.push(joined);
            }
            self.phase = RuntimeDrainPhase::WorkersJoined;
        }
        if self.phase == RuntimeDrainPhase::WorkersJoined {
            self.original.remaining()?;
            journal.persist_drained_runtime(
                &self.owner,
                &self.observation,
                &self.id,
                self.checkpoint
                    .as_ref()
                    .context("original checkpoint absent")?,
                self.child.id(),
                self.exit.unwrap(),
                now_ms,
                &self.original,
            )?;
            self.phase = RuntimeDrainPhase::TerminalPersisted;
        }
        if self.phase == RuntimeDrainPhase::TerminalPersisted {
            self.original.remaining()?;
            journal.release_drained_owner_with_budget(&self.owner, now_ms, &self.original)?;
            self.phase = RuntimeDrainPhase::OwnerReleased;
        }
        if self.phase == RuntimeDrainPhase::OwnerReleased {
            self.original.remaining()?;
            // The original lock must still be held by the enclosing engine.
            // Also refuse cleanup if a caller nevertheless admitted a later
            // owner while this resource carrier was retained after an error.
            journal.release_drained_owner_with_budget(&self.owner, now_ms, &self.original)?;
            self.resources.retire(&self.id, &self.original)?;
            self.original.remaining()?;
            self.phase = RuntimeDrainPhase::Retired;
        }
        Ok(self.phase)
    }

    pub fn await_retired_off_loop(
        &mut self,
        journal: &mut Journal,
        now_ms: impl Fn() -> i64,
    ) -> Result<()> {
        loop {
            if self.advance_off_loop(journal, now_ms())? == RuntimeDrainPhase::Retired {
                return Ok(());
            }
            std::thread::sleep(self.original.phase_limit(Duration::from_millis(2))?);
        }
    }
}

impl Journal {
    fn close_runtime_admission(
        &mut self,
        owner: &OwnerFence,
        observation: &RuntimeObservation,
        drain_id: &str,
        now_ms: i64,
        original: &FinalizationBudget,
    ) -> Result<()> {
        use crate::journal::*;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_owner(&tx, owner)?;
        assert_run(
            &tx,
            &owner.project_id,
            &observation.run_id,
            observation.run_epoch,
        )?;
        let attempt: Option<String> = tx.query_row(
            "SELECT instance_id FROM runs WHERE project_id=?1 AND run_id=?2",
            params![owner.project_id, observation.run_id],
            |row| row.get(0),
        )?;
        ensure!(
            attempt.as_deref() == Some(&observation.attempt_id),
            "drain belongs to a replaced attempt"
        );
        let existing_drain: Option<String> = tx.query_row("SELECT json_extract(payload_json,'$.drain_id') FROM events WHERE project_id=?1 AND kind='runtime-drain-started' AND json_extract(payload_json,'$.run_id')=?2 AND json_extract(payload_json,'$.run_epoch')=?3 LIMIT 1", params![owner.project_id,observation.run_id,sql_u64(observation.run_epoch)?], |row| row.get(0)).optional()?;
        if let Some(existing_drain) = existing_drain {
            ensure!(
                existing_drain == drain_id,
                "original drain already owns runtime admission"
            );
            return Ok(());
        }
        let ids = {
            let mut statement = tx.prepare("SELECT c.command_id FROM commands c JOIN effects e ON c.command_id=e.command_id WHERE c.project_id=?1 AND c.target_run=?2 AND c.target_epoch=?3 AND c.state='accepted' AND e.state='pending' ORDER BY c.accepted_sequence")?;
            let ids = statement
                .query_map(
                    params![
                        owner.project_id,
                        observation.run_id,
                        sql_u64(observation.run_epoch)?
                    ],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            ids
        };
        for id in ids {
            crate::journal::commands::complete(
                &tx,
                &id,
                &serde_json::json!({"status":"rejected","reason":"runtime intentionally paused before dispatch"}),
                now_ms,
            )?;
        }
        append_event(
            &tx,
            &owner.project_id,
            "runtime-drain-started",
            &owner.owner_id,
            &serde_json::json!({"drain_id":drain_id,"run_id":observation.run_id,"run_epoch":observation.run_epoch,"attempt_id":observation.attempt_id}),
            now_ms,
        )?;
        original.remaining()?;
        tx.commit()?;
        Ok(())
    }

    fn persist_drained_runtime(
        &mut self,
        owner: &OwnerFence,
        observation: &RuntimeObservation,
        drain_id: &str,
        checkpoint: &ObjectRef,
        process_id: u32,
        exit: ExitStatus,
        now_ms: i64,
        original: &FinalizationBudget,
    ) -> Result<()> {
        use crate::journal::*;
        let mut terminal = observation.clone();
        terminal.phase = RuntimePhase::Closed;
        terminal.pty_owned = false;
        terminal.active_turn_id = None;
        terminal.capabilities.retain(|kind| kind == "inspect");
        super::super::validate_observation(&terminal)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_owner(&tx, owner)?;
        assert_run(&tx, &owner.project_id, &terminal.run_id, terminal.run_epoch)?;
        let changed = tx.execute("UPDATE runtime_observations SET observation_json=?1,observed_at=?2 WHERE project_id=?3 AND run_id=?4 AND run_epoch=?5 AND owner_epoch=?6 AND instance_id=?7", params![json(&terminal)?,now_ms,owner.project_id,terminal.run_id,sql_u64(terminal.run_epoch)?,sql_u64(owner.epoch)?,terminal.attempt_id])?;
        ensure!(
            changed == 1,
            "original runtime observation was replaced before terminal persistence"
        );
        append_event(
            &tx,
            &owner.project_id,
            "runtime-observed",
            &owner.owner_id,
            &serde_json::to_value(&terminal)?,
            now_ms,
        )?;
        append_event(
            &tx,
            &owner.project_id,
            "runtime-paused",
            &owner.owner_id,
            &serde_json::json!({"drain_id":drain_id,"run_id":terminal.run_id,"run_epoch":terminal.run_epoch,"attempt_id":terminal.attempt_id,"state":"paused","process":null,"original_process_id":process_id,"exit_code":exit.code(),"checkpoint":checkpoint}),
            now_ms,
        )?;
        original.remaining()?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "process_tests.rs"]
mod tests;
