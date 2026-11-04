//! Existing App actor hook. No PTY writes, channel permission bypass, scheduler
//! or model work occurs here; dispatch uses the existing qualified AgentLink.
use super::*;

fn allowed(slot: &RuntimeLink, status: AgentStatus, pty_owned: bool, permission: bool) -> bool {
    matches!(status, AgentStatus::Running | AgentStatus::Done)
        && pty_owned
        && !permission
        && slot.qualified
        && !slot.closed
        && !slot.draining
        && !slot.uncertain
        && !slot.transport_uncertain
        && slot.active.is_none()
        && slot.interrupted.is_none()
        && matches!(slot.phase, RuntimePhase::Waiting | RuntimePhase::Completed)
        && slot.thread.is_some()
        && matches!(slot.profile.as_str(), "codex-app-server" | "claude-sdk")
}

impl RuntimeState {
    pub(super) fn deliver_idle_mailboxes(
        &mut self,
        app: &mut App,
        journal: &mut Journal,
        owner: &OwnerFence,
    ) -> Result<()> {
        let targets: Vec<_> = self
            .links
            .iter()
            .filter_map(|(id, slot)| {
                let run = app.agents.iter().find(|run| run.id == *id)?;
                let owned = run
                    .terminal
                    .as_ref()
                    .is_some_and(|pane| pane.instance_id() == slot.attempt);
                (slot.link.is_some() && allowed(slot, run.status, owned, run.needs_permission))
                    .then(|| (id.clone(), slot.attempt.clone()))
            })
            .collect();
        let mut started = 0;
        for (run, attempt) in targets {
            let now = epoch_ms()?;
            let Some(original) = journal.claim_idle_mailbox_delivery(owner, &run, &attempt, now)?
            else {
                continue;
            };
            let (claim, accepted) = original.into_effect();
            started += 1;
            self.dispatch(app, journal, claim, accepted, epoch_ms()?)?;
            if started == 8 {
                break;
            }
        }
        Ok(())
    }
}

pub(super) fn message(claim: &EffectClaim) -> Result<Option<PeerMessage>> {
    if claim.payload.get("automatic_mailbox_delivery") != Some(&serde_json::json!(1)) {
        return Ok(None);
    }
    let message: rudder_engine::journal::MailboxDeliveryNotice =
        serde_json::from_value(claim.payload["mailbox_notice"].clone())?;
    anyhow::ensure!(
        claim.kind == "deliver"
            && claim.principal == message.author
            && claim.sender_run == message.sender_run
            && claim.sender_epoch == message.sender_epoch
            && claim.owner.project_id == message.project_id
            && claim.target_run == message.recipient_run
            && claim.target_epoch == message.recipient_epoch,
        "automatic mailbox origin differs"
    );
    // The bounded notification names the genuine envelope. Full peer content
    // stays in its scoped durable inbox until the model chooses to read it.
    Ok(Some(PeerMessage {
        sender_run_id: message.sender_run,
        message_id: message.message_id,
        text: serde_json::to_string(&serde_json::json!({
            "principal":message.author,"sender_epoch":message.sender_epoch,
            "recipient_epoch":message.recipient_epoch,"intent":message.intent,
            "notice":"A peer message is waiting in your Rudder inbox. Use agents_inbox to read it, then agents_ack with its message_id after receipt. This notification is not an acknowledgment.",
        }))?,
    }))
}

pub(super) fn still_allowed(
    slot: &RuntimeLink,
    run: &AgentRun,
    owned: bool,
    claim: &EffectClaim,
) -> bool {
    claim.payload.get("automatic_mailbox_delivery") != Some(&serde_json::json!(1))
        || (allowed(slot, run.status, owned, run.needs_permission)
            && claim
                .payload
                .get("expected_attempt_id")
                .and_then(serde_json::Value::as_str)
                == Some(slot.attempt.as_str())
            && claim
                .payload
                .get("expected_thread_id")
                .and_then(serde_json::Value::as_str)
                == slot.thread.as_deref()
            && claim
                .payload
                .get("expected_turn_id")
                .is_some_and(serde_json::Value::is_null))
}

#[cfg(test)]
#[path = "fabric_mailbox_delivery_tests.rs"]
mod tests;
