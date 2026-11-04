//! Bounded observations of existing job/release authority. These helpers do not
//! schedule work, authorize commands, or release actual worker admission.

pub mod waits;
pub use waits::{
    BoundedWaitRegistry, CancelWait, SettledOutcome, Subscription, WaitKind, WaitResult, WaitScope,
};
