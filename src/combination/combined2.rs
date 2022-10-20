use crate::combination::{ConversionError, Converter, FinalSpec, IntermediateSpec};
use crate::start_new_sm;
use crate::state::{BoxedState, StateTypes};
use crate::state_machine::{
    Either, StateMachineError, StateMachineRx, TimeBoundStateMachineRunner,
};
use std::any::type_name;
use thiserror::Error;

#[cfg(feature = "tracing")]
use tracing::Span;

pub enum Combined<T0, T1>
where
    T0: StateTypes,
    T1: StateTypes,
{
    SM0 {
        sm: TimeBoundStateMachineRunner<T0>,
        rx: StateMachineRx<T0>,
        converter: Converter<T0, T1>,
        t1: FinalSpec,
    },
    SM1 {
        sm: TimeBoundStateMachineRunner<T1>,
        rx: StateMachineRx<T1>,
    },
}

pub enum CombinedOut<T0, T1>
where
    T0: StateTypes,
    T1: StateTypes,
{
    SM0(Vec<<T0 as StateTypes>::Out>),
    SM1(Vec<<T1 as StateTypes>::Out>),
}

#[derive(Error, Debug)]
pub enum CombinedError<T0, T1>
where
    T0: StateTypes,
    T1: StateTypes,
{
    #[error(transparent)]
    SM0(StateMachineError<T0>),
    #[error(transparent)]
    SM1(StateMachineError<T1>),
    #[error(transparent)]
    ConversionError(ConversionError),
}

pub type CombinedResult<T0, T1> = Result<BoxedState<T1>, CombinedError<T0, T1>>;

pub enum CombinedIn<T0, T1>
where
    T0: StateTypes,
    T1: StateTypes,
{
    SM0(<T0 as StateTypes>::In),
    SM1(<T1 as StateTypes>::In),
}

impl<T0, T1> Combined<T0, T1>
where
    T0: StateTypes,
    T1: StateTypes,
{
    pub fn new(
        initial_state: BoxedState<T0>,
        t0_t1: IntermediateSpec<T0, T1>,
        t1: FinalSpec,
        #[cfg(feature = "tracing")] span: Span,
    ) -> Self {
        let (sm, rx) = start_new_sm(
            type_name::<Self>().into(),
            initial_state,
            t0_t1.time_budget,
            #[cfg(feature = "tracing")]
            span,
        );
        let converter = t0_t1.converter.expect("Converter must be present");
        Self::SM0 {
            sm,
            rx,
            converter,
            t1,
        }
    }

    /// Polls for either a set of messages from some state machine,
    /// or a result of overall progression through all state machines.
    /// The result is constituted by either a terminal state of the final state machine,
    /// or any intermediate error, e.g., intermediate sm machine error or a conversion error
    /// between terminal state of one state machine and an initial state of another one.
    pub async fn recv(&mut self) -> Option<Either<CombinedOut<T0, T1>, CombinedResult<T0, T1>>> {
        match self {
            Self::SM0 {
                ref mut rx,
                converter,
                t1,
                ..
            } => match rx.recv().await {
                // Observe, that `map` is not applied on Option, because if the option contains
                // a result, then it will be used for construction of the next state
                // and not returned from this function.
                Some(Either::Result {
                    from,
                    result: Ok(t),
                    #[cfg(feature = "tracing")]
                    span,
                }) => {
                    // Current state machine succeeded with an Ok result,
                    // use its result to construct a state for the next sm.
                    let s = match converter(t) {
                        Ok(s) => s,
                        Err(err) => {
                            return Some(Either::Result {
                                from,
                                result: Err(CombinedError::ConversionError(err)),
                                #[cfg(feature = "tracing")]
                                span,
                            })
                        }
                    };
                    let (sm, rx) = start_new_sm(
                        type_name::<Self>().into(),
                        s,
                        t1.time_budget,
                        #[cfg(feature = "tracing")]
                        span,
                    );
                    *self = Combined::SM1 { sm, rx };
                    None
                }
                other @ Some(_) => other.map(|either| {
                    either
                        .map_messages(CombinedOut::SM0)
                        .map_result(|res| match res {
                            // map_err?
                            Ok(_) => unreachable!("This case has been handled outside"),
                            Err(err) => Err(CombinedError::SM0(err)),
                        })
                }),
                None => None,
            },
            Self::SM1 { ref mut rx, .. } => match rx.recv().await {
                success @ Some(Either::Result { result: Ok(_), .. }) => success.map(|either| {
                    either
                        .map_messages(CombinedOut::SM1)
                        .map_result(|res| match res {
                            // map_err?
                            Ok(t) => Ok(t),
                            Err(_) => unreachable!("This case has been handled outside"),
                        })
                }),
                other @ Some(_) => other.map(|either| {
                    either
                        .map_messages(CombinedOut::SM1)
                        .map_result(|res| match res {
                            Ok(_) => unreachable!("This case has been handled outside"),
                            Err(err) => Err(CombinedError::SM1(err)),
                        })
                }),
                None => None,
            },
        }
    }

    pub fn deliver(&self, message: CombinedIn<T0, T1>) -> Result<(), CombinedIn<T0, T1>> {
        match (self, message) {
            (Self::SM0 { sm, .. }, CombinedIn::SM0(m)) => sm.deliver(m).map_err(CombinedIn::SM0),
            (Self::SM1 { sm, .. }, CombinedIn::SM1(m)) => sm.deliver(m).map_err(CombinedIn::SM1),
            _ => panic!("Incorrect kind message for the current sm"),
        }
    }
}
