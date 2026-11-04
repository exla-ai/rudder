//! Compare and release the exact original build pin under the retention lock.
use super::*;

impl LocalStore {
    pub(crate) fn release_build_input_pin(&self, pin: &str, checkpoint: &ObjectRef) -> Result<()> {
        let suffix = pin.strip_prefix("build/input/").context("invalid original build input pin")?;
        Uuid::parse_str(suffix)?;
        ensure!(matches!(checkpoint.kind, ObjectKind::Checkpoint | ObjectKind::RunnableRoot), "invalid original build checkpoint");
        let _lock = self.lock()?;
        let mut state = self.read_state()?;
        ensure!(state.pins.get(pin) == Some(checkpoint), "original retained input pin/checkpoint changed before release");
        ensure!(!state.backed_retention.as_ref().is_some_and(|data|data.pins.contains_key(pin))
            && !state.environment_dependencies.as_ref().is_some_and(|data|data.pins.contains_key(pin)||data.holds.values().any(|h|h.pin==pin))
            && !state.source_dependencies.as_ref().is_some_and(|data|data.pins.contains_key(pin)||data.holds.values().any(|h|h.pin==pin)),
            "build input release cannot release backed/dependent retention");
        state.pins.remove(pin);
        self.commit_state(&mut state)
    }
}
