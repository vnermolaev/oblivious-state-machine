use crate::feed::{Feed, FeedError};
use crate::state::{BoxedState, DeliveryStatus, StateTypes, Transition};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
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

    pub fn run(
        &mut self,
    ) -> (
        mpsc::UnboundedReceiver<Vec<Types::Out>>,
        oneshot::Receiver<TimeBoundStateMachineResult<Types>>,
    ) {
        // Prepare state machine.
        let initial_state = self
            .initial_state
            .take()
            .expect("An initial state is expected");

        let feed = self
            .feed
            .take()
            .expect("A channel for state machine communication is expected");

        let (on_initialization, rx_on_initialization) = mpsc::unbounded_channel();

        // State machine is ready.
        let state_machine =
            StateMachine::new(self.id.clone(), initial_state, feed, on_initialization);

        // Create a channel to report state machine execution result.
        let (tx_result, rx_result) = oneshot::channel();

        let time_budget = self.time_budget;

        tokio::spawn(async move {
            state_machine.run_with_timeout(time_budget, tx_result).await;
        });

        (rx_on_initialization, rx_result)
    }

    /// Attempts a message delivery, if unsuccessful, returns message back as Err(message).
    pub fn deliver(&self, message: Types::In) -> Result<(), Types::In> {
        self.feeder.send(message).map_err(|err| err.0)
    }
}

pub type TimeBoundStateMachineResult<T> = Result<BoxedState<T>, StateMachineError<T>>;

/// State machine that takes an initial states and attempts to advance it until the terminal state.
pub struct StateMachine<Types: StateTypes> {
    /// id for this state machine.
    id: StateMachineId,

    /// State is wrapped into [InnerState] that also maintains a flag whether the state has been initialized.
    state: InnerState<Types>,

    /// [Feed] combining delayed messages and the external feed.
    feed: Feed<Types::In>,
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
        on_initialization: mpsc::UnboundedSender<Vec<Types::Out>>,
    ) -> Self {
        log::debug!(
            "[{id:?}] State machine has been initialized at <{}>",
            initial_state.desc()
        );

        Self {
            id,
            state: InnerState::new(initial_state, on_initialization),
            feed: Feed::new(feed),
        }
    }

    pub async fn run_with_timeout(
        mut self,
        time_budget: Duration,
        responder: oneshot::Sender<TimeBoundStateMachineResult<Types>>,
    ) {
        let result = match time::timeout(time_budget, self.run()).await {
            Ok(Ok(())) => Ok(self.state.inner),
            Ok(Err(StateMachineDriverError::StateError(error))) => Err(StateMachineError::State {
                error,
                state: self.state.inner,
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
                state: self.state.inner,
                feed: self.feed,
            }),
        };

        responder
            .send(result)
            .unwrap_or_else(|_| panic!("[{:?}] State machine result receiver dropped", self.id));
    }

    async fn run(&mut self) -> Result<(), StateMachineDriverError<Types>> {
        let Self {
            id,
            ref mut state,
            ref mut feed,
        } = self;

        log::debug!("[{id:?}] State machine is running");

        loop {
            // Try to Initialize the state.
            state
                .try_initialize(id)
                .map_err(StateMachineDriverError::OutgoingCommunication)?;

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
                    state.advance_to(next);

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

/// Convenience structure holding a state and its initialization status.
struct InnerState<Types: StateTypes> {
    is_initialized: bool,
    on_initialization: mpsc::UnboundedSender<Vec<Types::Out>>,
    inner: BoxedState<Types>,
}

impl<Types: StateTypes + 'static> InnerState<Types> {
    fn new(
        inner: BoxedState<Types>,
        on_initialization: mpsc::UnboundedSender<Vec<Types::Out>>,
    ) -> Self {
        Self {
            is_initialized: false,
            on_initialization,
            inner,
        }
    }

    /// If the inner state is not initialized,
    /// initializes it and attempts to send out the messages produced on initialization.
    /// If successful returns OK(()), other wise Err(messages).
    fn try_initialize(&mut self, id: &StateMachineId) -> Result<(), Vec<Types::Out>> {
        if !self.is_initialized {
            log::debug!("[{id:?}] Initializing <{}>", self.inner.desc());

            let messages = self.inner.initialize();

            self.on_initialization.send(messages).map_err(|err| err.0)?;
            self.is_initialized = true;
        }

        Ok(())
    }

    fn deliver(&mut self, message: Types::In) -> DeliveryStatus<Types::In, Types::Err> {
        self.inner.deliver(message)
    }

    fn advance(&self) -> Result<Transition<Types>, Types::Err> {
        self.inner.advance()
    }

    fn advance_to(&mut self, inner: BoxedState<Types>) {
        self.is_initialized = false;
        self.inner = inner;
    }
}

#[derive(Debug, Clone)]
pub struct StateMachineId(String);

impl From<&str> for StateMachineId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
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
        StateMachineError, TimeBoundStateMachineResult, TimeBoundStateMachineRunner,
    };
    use pretty_env_logger;
    use std::collections::VecDeque;
    use std::time::Duration;
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

        // _outgoing needs to be in scope otherwise,
        // the state machine will produce an error not being able to send out messages.
        let (_outgoing, mut result) = state_machine_runner.run();

        let res: TimeBoundStateMachineResult<Types> = loop {
            select! {
                res = &mut result => {
                    break res.expect("Result from State Machine must be communicated");
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

        // _outgoing needs to be in scope otherwise,
        // the state machine will produce an error not being able to send out messages.
        let (_outgoing, mut result) = state_machine_runner.run();

        let res: TimeBoundStateMachineResult<Types> = loop {
            select! {
                res = &mut result => {
                    break res.expect("Result from State Machine must be communicated");
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

        // _outgoing needs to be in scope otherwise,
        // the state machine will produce an error not being able to send out messages.
        let (_outgoing, mut result) = state_machine_runner.run();

        let res: TimeBoundStateMachineResult<Types> = loop {
            select! {
                res = &mut result => {
                    break res.expect("Result from State Machine must be communicated");
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
