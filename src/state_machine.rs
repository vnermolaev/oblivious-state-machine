use crate::state::{BoxedState, Delivery, Transition};
use crate::{Event, StateMachineTypes};
use futures::Stream;
use std::fmt;
use std::fmt::Formatter;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use thiserror::Error;

pub struct StateMachine<T: StateMachineTypes> {
    id: StateMachineId,
    /// Messages scheduled for consumption by the inner state machine.
    // TODO Consider bounding this by using e.g. arrayvec.
    buffer: Vec<T::I>,
    ///
    waker: Option<Waker>,
    /// Describes the phase of this state machine lifecycle.
    phase: StateMachinePhase<T>,
}

impl<T: StateMachineTypes> StateMachine<T> {
    pub fn new(id: StateMachineId, producer: StateProducer<T>) -> Self {
        log::trace!("{id}: Creating `Pending`");

        Self {
            id,
            buffer: Vec::new(),
            waker: None,
            phase: StateMachinePhase::Pending(Pending { producer }),
        }
    }

    pub fn new_with_message(id: StateMachineId, message: T::I, producer: StateProducer<T>) -> Self {
        log::trace!("{id}: Creating `Pending` with message: {message:?}");

        Self {
            id,
            buffer: vec![message],
            waker: None,
            phase: StateMachinePhase::Pending(Pending { producer }),
        }
    }

    pub fn new_with_state(id: StateMachineId, state: BoxedState<T>) -> Self {
        log::trace!("{id}: Creating `Active` with state: {}", state.describe());

        Self {
            id,
            buffer: Vec::new(),
            waker: None,
            phase: StateMachinePhase::Active(Active {
                is_state_initialized: false,
                state: Some(state),
            }),
        }
    }
}

/// Function that attempts to construct the initial state by looking at all buffered messages,
/// it may happen that one or more messages must be removed when constructing the initial state.
/// `Vec` is used instead of a slice to provide access to `Vec` specific methods, such as `remove`, `retain`, etc.
pub type StateProducer<T> = Box<
    dyn Fn(&StateMachineId, &mut Vec<<T as StateMachineTypes>::I>) -> Option<BoxedState<T>> + Send,
>;

/// State machine progresses through phases: `Pending`, `Active`, and  `Terminated`.
enum StateMachinePhase<T: StateMachineTypes> {
    /// During `Pending` phase no actual state is maintained, because it cannot be produced
    /// from the available messages just yet.
    Pending(Pending<T>),

    /// During `Active` phase an actual state is maintained and attempted to be advanced as far as possible.
    Active(Active<T>),

    /// State machine has produced the final result, polling it will produce an error.
    Terminated,
}

/// `Pending` state  of the state machine.
pub(crate) struct Pending<T: StateMachineTypes> {
    producer: StateProducer<T>,
}

/// `Active` state of the state machine.
pub(crate) struct Active<T: StateMachineTypes> {
    is_state_initialized: bool,
    /// Current `State` of this state machine.
    /// `Option` is required to easier consume/advance the state when it is possible.
    state: Option<BoxedState<T>>,
}

impl<T: StateMachineTypes> StateMachine<T> {
    fn poll(&mut self, cx: &Context<'_>) -> Poll<Result<Event<T>, StateMachineError>> {
        match self.phase {
            StateMachinePhase::Pending(ref mut p) => match p.poll(&self.id, &mut self.buffer) {
                Poll::Ready(a) => {
                    log::trace!("{} `Pending` transitions to `Active`", self.id);
                    self.phase = StateMachinePhase::Active(a);

                    // If `Pending` transitions to `Active`, the inner state of `Active` may be immediately advanceable.
                    self.poll(cx)
                }
                Poll::Pending => {
                    // The buffer contains insufficient messages to construct the initial state.
                    // Save the waker to wake yourself up when a new message arrives.
                    self.waker = Some(cx.waker().clone());

                    Poll::Pending
                }
            },
            StateMachinePhase::Active(ref mut a) => match a.poll(&self.id, &mut self.buffer) {
                Poll::Ready(event @ Event::Output(_)) => Poll::Ready(Ok(event)),
                Poll::Ready(event @ Event::Completion { .. }) => {
                    log::trace!("{} `Active` transitions to `Terminated`", self.id);

                    // Progress from `Active` to `Terminated`.
                    self.phase = StateMachinePhase::Terminated;

                    Poll::Ready(Ok(event))
                }
                Poll::Ready(event @ Event::Error(_)) => Poll::Ready(Ok(event)),
                Poll::Pending => {
                    // The buffer contain insufficient messages to advance the current state further.
                    // Save the waker to wake yourself up when a new message arrives.
                    self.waker = Some(cx.waker().clone());

                    Poll::Pending
                }
            },
            StateMachinePhase::Terminated => {
                Poll::Ready(Err(StateMachineError::PolledTerminated(self.id.clone())))
            }
        }
    }

    pub fn deliver(&mut self, message: T::I) {
        log::trace!("{} New message delivered: {message:?}", self.id);
        self.buffer.push(message);

        if let Some(waker) = self.waker.take() {
            log::trace!("{} Waking", self.id);
            waker.wake();
        }
    }
}

impl<T: StateMachineTypes> Pending<T> {
    fn poll(&mut self, id: &StateMachineId, buffer: &mut Vec<T::I>) -> Poll<Active<T>> {
        log::trace!("{id} Polling `Pending`");

        // Check if the current set of messages is sufficient to produce an initial state.
        if let Some(s) = (self.producer)(id, buffer) {
            // Construct `Active` state machine.
            let active = Active {
                is_state_initialized: false,
                state: Some(s),
            };
            Poll::Ready(active)
        } else {
            Poll::Pending
        }
    }
}

impl<T: StateMachineTypes> Active<T> {
    const STATE_PRESENT: &'static str = "`Active` SM must maintain a state";

    fn poll(&mut self, id: &StateMachineId, buffer: &mut Vec<T::I>) -> Poll<Event<T>> {
        log::trace!("{id} Polling `Active`");

        let mut state = self.state.take().expect(Self::STATE_PRESENT);

        // Advance the state as long as it is possible.
        loop {
            // If state is not initialized, do so.
            if !self.is_state_initialized {
                log::trace!("{id} Initializing state {}", state.describe());

                self.is_state_initialized = true;
                let messages = state.initialize();
                if !messages.is_empty() {
                    self.state = Some(state);
                    return Poll::Ready(Event::Output(messages));
                }
            }

            // If messages are buffered, try to deliver them.
            if !buffer.is_empty() {
                log::trace!("{id} Delivering buffered messages: {:?}", buffer);

                let (mut schedule, errors) = buffer.drain(..).fold(
                    (Vec::new(), Vec::new()),
                    |(mut schedule, mut errors), message| {
                        match state.deliver(message) {
                            Delivery::Delivered => {}
                            Delivery::Unexpected(message) => {
                                // Queue Unexpected messages only for later delivery.
                                schedule.push(message)
                            }
                            Delivery::Error(e) => errors.push(e),
                        }
                        (schedule, errors)
                    },
                );

                if !errors.is_empty() {
                    return Poll::Ready(Event::Error(errors));
                }

                buffer.append(&mut schedule);
                log::trace!("{id} Still buffered messages: {:?}", buffer);
            }

            // If the state can be advanced, advance it.
            match state.prepare_advance() {
                Ok(true) => {
                    log::trace!("{id} State can advance");

                    let state_desc = state.describe();

                    match state.advance() {
                        Transition::Next(s) => {
                            log::trace!("{id} {state_desc} -> {}", s.describe());

                            state = s;
                            self.is_state_initialized = false;
                        }
                        Transition::Final(state) => {
                            log::trace!("{id} {state_desc} => {}", state.describe());

                            return Poll::Ready(Event::Completion {
                                state,
                                buffer: buffer.drain(..).collect(),
                            });
                        }
                    }
                }
                Ok(false) => {
                    // Current buffer is insufficient for further advancement,
                    // thus the task shall return `Poll::Pending` until a new messages arrives.
                    log::trace!(
                        "{id} Current buffer is insufficient to advance the state `{}` further",
                        state.describe()
                    );

                    // Save the current state.
                    self.state = Some(state);

                    return Poll::Pending;
                }
                Err(e) => return Poll::Ready(Event::Error(e)),
            }
        }
    }
}

impl<T: StateMachineTypes> Unpin for StateMachine<T> {}

impl<T: StateMachineTypes> Stream for StateMachine<T> {
    type Item = Event<T>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        log::trace!("Polling state machine as stream");

        match self.as_mut().poll(cx) {
            Poll::Ready(Ok(event)) => Poll::Ready(Some(event)),
            Poll::Ready(Err(StateMachineError::PolledTerminated(id))) => {
                log::trace!("{id} Machine terminated; stream will always return None");

                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// State machine ID
#[derive(Clone, Debug)]
pub struct StateMachineId(pub String);

impl From<&str> for StateMachineId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl fmt::Display for StateMachineId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "[{}]", self.0)
    }
}
#[derive(Debug, Error)]
pub enum StateMachineError {
    #[error("{0} terminated and cannot be polled")]
    PolledTerminated(StateMachineId),
}
