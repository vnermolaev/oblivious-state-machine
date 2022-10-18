use crate::state::{BoxedState, StateTypes};
use crate::state_machine::{StateMachineId, StateMachineRx, TimeBoundStateMachineRunner};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::mpsc;

#[cfg(feature = "tracing")]
use tracing::Span;

pub mod combined2;
pub mod combined3;

/// Specification to run an intermediate state machine.
pub struct IntermediateSpec<T0, T1> {
    /// Time budget for state machine to complete.
    time_budget: Duration,
    /// Converter from the final state of the current state machine to an initial state of the next state machine.
    converter: Option<Converter<T0, T1>>,
}

impl<T0, T1> IntermediateSpec<T0, T1> {
    pub fn new(time_budget: Duration, converter: Converter<T0, T1>) -> Self {
        Self {
            time_budget,
            converter: Some(converter),
        }
    }
}

/// Specification for the final state machine.
#[derive(Clone)]
pub struct FinalSpec {
    /// Time budget for state machine to complete.
    pub time_budget: Duration,
}

pub type Converter<T, S> = Box<dyn Fn(BoxedState<T>) -> Result<BoxedState<S>, ConversionError>>;

#[derive(Error, Debug)]
pub enum ConversionError {
    #[error(
        "State machine completed in unexpected state: `{terminal_desc}`. Expected: {expected}"
    )]
    UnexpectedFinalState {
        terminal_desc: String,
        expected: String,
    },
    #[error("Cannot construct `to` from `from`")]
    CannotConstructNextState { from: String, to: String },
}

/// Convenience function starting a new state machine with the given parameters.
pub(crate) fn start_new_sm<T: StateTypes>(
    state_machine_id: StateMachineId,
    initial_state: BoxedState<T>,
    time_budget: Duration,
    #[cfg(feature = "tracing")] span: Span,
) -> (TimeBoundStateMachineRunner<T>, StateMachineRx<T>) {
    let mut sm = TimeBoundStateMachineRunner::new(
        state_machine_id,
        initial_state,
        time_budget,
        #[cfg(feature = "tracing")]
        span,
    );

    let (tx, rx) = mpsc::unbounded_channel();

    sm.run(tx);

    (sm, rx)
}
