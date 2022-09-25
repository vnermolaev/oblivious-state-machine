use crate::feed::{Feed, FeedError};
use crate::state::{BoxedState, DeliveryStatus, StateTypes, Transition};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::time;

/// A time-bound state machine runner.
/// Runs a state machine for at most as long as `[time_budget_ms]` milliseconds.
/// It is a convenient interface to deliver and receive messages from the inner state machine.
pub struct TimeBoundStateMachineRunner<Types: StateTypes> {
    pub id: StateMachineId,
    initial_state: Option<BoxedState<Types>>,
    feed: Option<mpsc::UnboundedReceiver<Types::In>>,
    feeder: mpsc::UnboundedSender<Types::In>,
    time_budget: Duration,
}

impl<Types> TimeBoundStateMachineRunner<Types>
where
    Types: 'static + StateTypes,
    Types::In: Send,
    Types::Out: Send,
    Types::Err: Send,
{
    pub fn new(
        id: StateMachineId,
        initial_state: BoxedState<Types>,
        time_budget: Duration,
    ) -> Self {
        log::debug!(
            "[{id:?}] Time-bound state machine runner for state machine [{id:?}] is initializing with budget {:?}",
            time_budget
        );

        let (feeder, feed) = mpsc::unbounded_channel();

        Self {
            id,
            initial_state: Some(initial_state),
            feed: Some(feed),
            feeder,
            time_budget,
        }
    }

    pub fn run(&mut self, state_machine_sink: StateMachineTx<Types>) {
        // Prepare state machine.
        let initial_state = self
            .initial_state
            .take()
            .expect("An initial state is expected");

        let feed = self
            .feed
            .take()
            .expect("A channel for state machine communication is expected");

        // State machine is ready.
        let state_machine =
            StateMachine::new(self.id.clone(), initial_state, feed, state_machine_sink);

        let time_budget = self.time_budget;

        tokio::spawn(async move {
            state_machine.run_with_timeout(time_budget).await;
        });
    }

    /// Attempts a message delivery, if unsuccessful, returns message back as Err(message).
    pub fn deliver(&self, message: Types::In) -> Result<(), Types::In> {
        self.feeder.send(message).map_err(|err| err.0)
    }
}

pub enum Either<M, R> {
    Messages { from: StateMachineId, messages: M },
    Result { from: StateMachineId, result: R },
}

pub type TimeBoundStateMachineResult<T> = Result<BoxedState<T>, StateMachineError<T>>;

/// State machine that takes an initial states and attempts to advance it until the terminal state.
pub struct StateMachine<Types: StateTypes> {
    /// id for this state machine.
    id: StateMachineId,

    /// State is wrapped into [InnerState] that also maintains a flag whether the state has been initialized.
    state: BoxedState<Types>,

    /// [Feed] combining delayed messages and the external feed.
    feed: Feed<Types::In>,

    /// Channel to report outgoing messages and the result.
    state_machine_tx: StateMachineTx<Types>,
}

impl<Types> StateMachine<Types>
where
    Types: 'static + StateTypes,
    Types::In: Send,
{
    pub fn new(
        id: StateMachineId,
        initial_state: BoxedState<Types>,
        feed: mpsc::UnboundedReceiver<Types::In>,
        state_machine_tx: StateMachineTx<Types>,
    ) -> Self {
        log::debug!(
            "[{id:?}] State machine has been initialized at <{}>",
            initial_state.desc()
        );

        Self {
            id,
            state: initial_state,
            feed: Feed::new(feed),
            state_machine_tx,
        }
    }

    pub async fn run_with_timeout(mut self, time_budget: Duration) {
        let result = match time::timeout(time_budget, self.run()).await {
            Ok(Ok(())) => Ok(self.state),
            Ok(Err(StateMachineDriverError::StateError(error))) => Err(StateMachineError::State {
                error,
                state: self.state,
                feed: self.feed,
            }),
            Ok(Err(StateMachineDriverError::IncomingCommunication(err))) => {
                Err(StateMachineError::IncomingCommunication(err))
            }
            Ok(Err(StateMachineDriverError::OutgoingCommunication(err))) => {
                Err(StateMachineError::OutgoingCommunication(err))
            }
            Err(_) => Err(StateMachineError::Timeout {
                time_budget,
                state: self.state,
                feed: self.feed,
            }),
        };

        self.state_machine_tx
            .send(Either::Result {
                from: self.id.clone(),
                result,
            })
            .unwrap_or_else(|_| panic!("[{:?}] State machine result receiver dropped", self.id));
    }

    async fn run(&mut self) -> Result<(), StateMachineDriverError<Types>> {
        let Self {
            id,
            ref mut state,
            ref mut feed,
            ref mut state_machine_tx,
        } = self;

        log::debug!("[{id:?}] State machine is running");

        let mut is_state_initialized = false;

        loop {
            // Try to Initialize the state.
            if !is_state_initialized {
                log::debug!("[{:?}] Initializing", id);

                let messages = state.initialize();

                state_machine_tx
                    .send(Either::Messages{ from: id.clone(), messages })
                    .map_err(|err| {
                        if let Either::Messages{messages, ..} = err.0 {
                            StateMachineDriverError::OutgoingCommunication(messages)
                        } else {
                            panic!("[BUG] Sending Either::Messages failed, but reports not being able to send other kind of a message");
                        }

                    })?;

                is_state_initialized = true;
            }

            // Attempt to advance.
            log::debug!("[{id:?}] State advance attempt");
            let advanced = state.advance().map_err(|err| {
                log::debug!("[{id:?}] State advance attempt failed with: {err:?}");
                StateMachineDriverError::StateError(err)
            })?;

            match advanced {
                Transition::Same => {
                    log::debug!("[{id:?}] State requires more input");

                    // No advancement. Try to deliver a message.
                    let message = feed
                        .next()
                        .await
                        .map_err(StateMachineDriverError::IncomingCommunication)?;

                    match state.deliver(message) {
                        DeliveryStatus::Delivered => {
                            log::debug!("[{id:?}] Message has been delivered");
                        }
                        DeliveryStatus::Unexpected(message) => {
                            log::debug!("[{id:?}] Unexpected message. Storing for future attempts");
                            feed.delay(message);
                        }
                        DeliveryStatus::Error(err) => {
                            Err(StateMachineDriverError::StateError(err))?;
                        }
                    }
                }
                Transition::Next(next) => {
                    log::debug!("[{id:?}] State has been advanced to <{}>", next.desc());

                    // Update the current state.
                    *state = next;
                    is_state_initialized = false;

                    // Refresh the feed.
                    feed.refresh();
                }
                Transition::Terminal => {
                    log::debug!("[{id:?}] State is terminal. Completing...");
                    break;
                }
            }
        }

        log::debug!("[{id:?}] State machine has completed");
        Ok(())
    }
}

pub type StateMachineTx<Types> = mpsc::UnboundedSender<
    Either<Vec<<Types as StateTypes>::Out>, TimeBoundStateMachineResult<Types>>,
>;
pub type StateMachineRx<Types> = mpsc::UnboundedReceiver<
    Either<Vec<<Types as StateTypes>::Out>, TimeBoundStateMachineResult<Types>>,
>;

#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct StateMachineId(String);

impl From<&str> for StateMachineId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl From<String> for StateMachineId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl StateMachineId {
    pub fn id(&self) -> &str {
        &self.0
    }
}

/// Possible reasons for erroneous run of the state machine.
/// This enum provides a rich context in which the state machine run.
#[derive(Error)]
pub enum StateMachineError<Types: StateTypes> {
    #[error("{0:?}")]
    IncomingCommunication(FeedError),

    #[error("Impossible to send out outgoing messages")]
    OutgoingCommunication(Vec<Types::Out>),

    #[error("Internal state error: {error:?}")]
    State {
        error: Types::Err,
        state: BoxedState<Types>,
        feed: Feed<Types::In>,
    },

    #[error("State machine ran longer than permitted: {time_budget:?}")]
    Timeout {
        time_budget: Duration,
        state: BoxedState<Types>,
        feed: Feed<Types::In>,
    },
}

enum StateMachineDriverError<Types: StateTypes> {
    IncomingCommunication(FeedError),
    OutgoingCommunication(Vec<Types::Out>),
    StateError(Types::Err),
}

#[cfg(test)]
mod test {
    use crate::state::{DeliveryStatus, State, StateTypes, Transition};
    use crate::state_machine::{
        Either, StateMachineError, TimeBoundStateMachineResult, TimeBoundStateMachineRunner,
    };
    use pretty_env_logger;
    use std::collections::VecDeque;
    use std::time::Duration;
    use tokio::sync::mpsc;
    use tokio::{select, time};

    /// This module tests an order state machine.
    /// Item's lifecycle
    /// OnDisplay -(PlaceOrder)-> Placed -(Verify)-> Verified -(FinalConfirmation)-> Shipped
    ///    ↑           ↳(Cancel)-> Canceled             ↳(Cancel)-> Canceled
    ///    └-------------------------┘---------------------------------┘

    struct Types;
    impl StateTypes for Types {
        type In = Message;
        type Out = ();
        type Err = String;
    }

    #[derive(Debug)]
    enum Message {
        PlaceOrder,
        Damage,
        Verify(Verify),
        FinalConfirmation,
        Cancel,
    }

    #[derive(Debug)]
    enum Verify {
        Address,
        PaymentDetails,
    }

    // `OnDisplay` state ===========
    #[derive(Debug)]
    struct OnDisplay {
        is_ordered: bool,
        is_broken: bool,
    }

    impl OnDisplay {
        fn new() -> Self {
            Self {
                is_ordered: false,
                is_broken: false,
            }
        }
    }

    impl State<Types> for OnDisplay {
        fn desc(&self) -> String {
            "Item is on display for selling".to_string()
        }

        fn deliver(
            &mut self,
            message: Message,
        ) -> DeliveryStatus<Message, <Types as StateTypes>::Err> {
            match message {
                Message::PlaceOrder => self.is_ordered = true,
                Message::Damage => self.is_broken = true,
                _ => return DeliveryStatus::Unexpected(message),
            }
            DeliveryStatus::Delivered
        }

        fn advance(&self) -> Result<Transition<Types>, <Types as StateTypes>::Err> {
            if self.is_broken {
                return Err("Item is damaged".to_string());
            }

            Ok(if self.is_ordered {
                Transition::Next(Box::new(Placed::new()))
            } else {
                Transition::Same
            })
        }
    }
    // ========================

    // `Placed` state ===========
    #[derive(Debug)]
    struct Placed {
        is_address_present: bool,
        is_payment_ok: bool,
        is_canceled: bool,
    }

    impl Placed {
        fn new() -> Self {
            Self {
                is_address_present: false,
                is_payment_ok: false,
                is_canceled: false,
            }
        }
    }

    impl State<Types> for Placed {
        fn desc(&self) -> String {
            "Order has been placed, awaiting verification.".to_string()
        }

        fn deliver(
            &mut self,
            message: Message,
        ) -> DeliveryStatus<Message, <Types as StateTypes>::Err> {
            match message {
                Message::Cancel => self.is_canceled = true,
                Message::Verify(Verify::Address) => self.is_address_present = true,
                Message::Verify(Verify::PaymentDetails) => self.is_payment_ok = true,
                _ => return DeliveryStatus::Unexpected(message),
            }

            DeliveryStatus::Delivered
        }

        fn advance(&self) -> Result<Transition<Types>, <Types as StateTypes>::Err> {
            if self.is_canceled {
                return Ok(Transition::Next(Box::new(Canceled)));
            }

            Ok(if self.is_payment_ok && self.is_address_present {
                Transition::Next(Box::new(Verified::new()))
            } else {
                Transition::Same
            })
        }
    }
    // ========================

    // `Verified` state ===========
    #[derive(Debug)]
    struct Verified {
        is_canceled: bool,
        is_confirmed: bool,
    }

    impl Verified {
        fn new() -> Self {
            Self {
                is_canceled: false,
                is_confirmed: false,
            }
        }
    }

    impl State<Types> for Verified {
        fn desc(&self) -> String {
            "Order has all required details, awaiting the final confirmation".to_string()
        }

        fn deliver(
            &mut self,
            message: Message,
        ) -> DeliveryStatus<Message, <Types as StateTypes>::Err> {
            match message {
                Message::Cancel => self.is_canceled = true,
                Message::FinalConfirmation => self.is_confirmed = true,
                _ => return DeliveryStatus::Unexpected(message),
            }

            DeliveryStatus::Delivered
        }

        fn advance(&self) -> Result<Transition<Types>, <Types as StateTypes>::Err> {
            if self.is_canceled {
                return Ok(Transition::Next(Box::new(Canceled)));
            }

            Ok(if self.is_confirmed {
                Transition::Next(Box::new(Shipped))
            } else {
                Transition::Same
            })
        }
    }
    // ========================

    // `Shipped` state ===========
    #[derive(Debug)]
    struct Shipped;

    impl State<Types> for Shipped {
        fn desc(&self) -> String {
            "Order has been shipped".to_string()
        }

        fn advance(&self) -> Result<Transition<Types>, <Types as StateTypes>::Err> {
            Ok(Transition::Terminal)
        }
    }
    // ========================

    // `Shipped` state ===========
    #[derive(Debug)]
    struct Canceled;

    impl State<Types> for Canceled {
        fn desc(&self) -> String {
            "Order has been canceled".to_string()
        }

        fn advance(&self) -> Result<Transition<Types>, <Types as StateTypes>::Err> {
            Ok(Transition::Next(Box::new(OnDisplay::new())))
        }
    }
    // ========================

    #[tokio::test]
    async fn order_goes_to_shipping() {
        let _ = pretty_env_logger::try_init();

        // initial state.
        let on_display = OnDisplay::new();

        // feed of un-ordered messages.
        let mut feed = VecDeque::from(vec![
            Message::Verify(Verify::Address),
            Message::PlaceOrder,
            Message::FinalConfirmation,
            Message::Verify(Verify::PaymentDetails),
        ]);
        let mut feeding_interval = time::interval(Duration::from_millis(100));
        feeding_interval.tick().await;

        let mut state_machine_runner = TimeBoundStateMachineRunner::new(
            "Order".into(),
            Box::new(on_display),
            Duration::from_secs(5),
        );

        let (state_machine_tx, mut state_machine_rx) = mpsc::unbounded_channel();
        state_machine_runner.run(state_machine_tx);

        let res: TimeBoundStateMachineResult<Types> = loop {
            select! {
                Some(Either::Result{ result, ..} ) = state_machine_rx.recv() => {
                    break result;
                }
                _ = feeding_interval.tick() => {
                    // feed a message if present.
                    if let Some(msg) = feed.pop_front() {
                        let _ = state_machine_runner.deliver(msg);
                    }
                }
            }
        };

        let order = res.unwrap_or_else(|_| panic!("State machine did not complete in time"));
        assert!(order.is::<Shipped>());

        let _shipped = order
            .downcast::<Shipped>()
            .unwrap_or_else(|_| panic!("must work"));
    }

    #[tokio::test]
    async fn order_canceled_state_machine_timeouts() {
        let _ = pretty_env_logger::try_init();

        // initial state.
        let on_display = OnDisplay::new();

        // feed of un-ordered messages.
        let mut feed = VecDeque::from(vec![
            Message::Verify(Verify::Address),
            Message::PlaceOrder,
            Message::Cancel,
        ]);
        let mut feeding_interval = time::interval(Duration::from_millis(100));
        feeding_interval.tick().await;

        let mut state_machine_runner = TimeBoundStateMachineRunner::new(
            "Order".into(),
            Box::new(on_display),
            Duration::from_secs(5),
        );

        let (state_machine_tx, mut state_machine_rx) = mpsc::unbounded_channel();
        state_machine_runner.run(state_machine_tx);

        let res: TimeBoundStateMachineResult<Types> = loop {
            select! {
                Some(Either::Result{result, ..}) =  state_machine_rx.recv() => {
                    break result;
                }
                _ = feeding_interval.tick() => {
                    // feed a message if present.
                    if let Some(msg) = feed.pop_front() {
                        let _ = state_machine_runner.deliver(msg);
                    }
                }
            }
        };

        match res {
            Ok(_) => panic!("TimeMachine should not have completed"),
            Err(StateMachineError::Timeout { state: order, .. }) => {
                assert!(order.is::<OnDisplay>())
            }
            Err(_) => {
                panic!("unexpected error")
            }
        }
    }

    // TODO abstract away the setup with the artificial feed.

    #[tokio::test]
    async fn faulty_state_leads_to_state_machine_termination() {
        let _ = pretty_env_logger::try_init();

        // initial state.
        let on_display = OnDisplay::new();

        // feed of un-ordered messages.
        let mut feed = VecDeque::from(vec![Message::Verify(Verify::Address), Message::Damage]);
        let mut feeding_interval = time::interval(Duration::from_millis(100));
        feeding_interval.tick().await;

        let mut state_machine_runner = TimeBoundStateMachineRunner::new(
            "Order".into(),
            Box::new(on_display),
            Duration::from_secs(5),
        );

        let (state_machine_tx, mut state_machine_rx) = mpsc::unbounded_channel();
        state_machine_runner.run(state_machine_tx);

        let res: TimeBoundStateMachineResult<Types> = loop {
            select! {
                Some(Either::Result{result, ..}) = state_machine_rx.recv() => {
                    break result;
                }
                _ = feeding_interval.tick() => {
                    // feed a message if present.
                    if let Some(msg) = feed.pop_front() {
                        let _ = state_machine_runner.deliver(msg);
                    }
                }
            }
        };

        match res {
            Ok(_) => panic!("TimeMachine should not have completed"),
            Err(StateMachineError::Timeout { state: order, .. }) => {
                assert!(order.is::<OnDisplay>())
            }
            Err(err) => {
                if let StateMachineError::State { error, state, .. } = err {
                    assert_eq!(error, "Item is damaged".to_string());
                    assert!(state.is::<OnDisplay>());
                    log::debug!("{:?}", error);
                } else {
                    panic!("Unexpected error")
                }
            }
        }
    }
}
