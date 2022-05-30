use crate::feed::Feed;
use crate::state::{BoxedState, DeliveryStatus, StateTypes, Transition};
use anyhow::anyhow;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time;

// TODO [EXCEPTIONS] Proper error handling with `thiserror` instead of `anyhow`

/// A time-bound state machine runner.
/// Runs a state machine for at most as long as `[time_budget_ms]` milliseconds.
/// It is a convenient interface to deliver and receive messages from the inner state machine.
pub struct TimeBoundStateMachineRunner<Types: StateTypes> {
    id: String,
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
    pub fn new(id: String, initial_state: BoxedState<Types>, time_budget: Duration) -> Self {
        log::debug!(
            "[{}] Time-bound state machine runner for state machine [{}] is initializing with budget {:?}",
            id, id, time_budget
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

    pub fn deliver(&self, message: Types::In) -> anyhow::Result<()> {
        self.feeder
            .send(message)
            .map_err(|_| anyhow!("Can't deliver"))
    }
}

pub type TimeBoundStateMachineResult<T> = Result<BoxedState<T>, StateMachineError<T>>;

/// State machine that takes an initial states and attempts to advance it until the terminal state.
pub struct StateMachine<Types: StateTypes> {
    /// id for this state machine.
    id: String,

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
        id: String,
        initial_state: BoxedState<Types>,
        feed: mpsc::UnboundedReceiver<Types::In>,
        on_initialization: mpsc::UnboundedSender<Vec<Types::Out>>,
    ) -> Self {
        log::debug!("[{}] State machine has been initialized", id);

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
            Ok(Ok(_)) => Ok(self.state.inner),
            Ok(Err(error)) => Err(StateMachineError::State {
                error,
                state: self.state.inner,
                feed: self.feed,
            }),
            Err(_) => Err(StateMachineError::Timeout {
                state: self.state.inner,
                feed: self.feed,
                time_budget,
            }),
        };

        let _ = responder.send(result);
    }

    pub async fn run(&mut self) -> Result<(), Types::Err> {
        let Self {
            id,
            ref mut state,
            ref mut feed,
        } = self;

        log::debug!("[{}] State machine is running", id);

        loop {
            // Try to Initialize the state.
            log::debug!("[{}] Initialization attempt", id);
            state.try_initialize();

            // Attempt to advance.
            log::debug!("[{}] State advance attempt", id);
            let advanced = state.advance()?;

            match advanced {
                Transition::Same => {
                    log::debug!("[{}]\t\tState requires more input", id);

                    // No advancement. Try to deliver a message.
                    let message = feed.next().await;

                    match state.deliver(message) {
                        DeliveryStatus::Delivered => {
                            log::debug!("[{}]\t\tMessage has been delivered", id);
                        }
                        DeliveryStatus::Unexpected(message) => {
                            log::debug!(
                                "[{}]\t\tUnexpected message. Storing for future attempts",
                                id
                            );
                            feed.delay(message);
                        }
                        DeliveryStatus::Error(_err) => {
                            todo!("Message processing error")
                        }
                    }
                }
                Transition::Next(next) => {
                    log::debug!("[{}]\t\tState has been advanced to <{}>", id, next.desc());

                    // Update the current state.
                    state.advance_to(next);

                    // Refresh the feed.
                    feed.refresh();
                }
                Transition::Terminal => {
                    log::debug!("[{}]\t\tState is terminal. Completing...", id);
                    break; // state;
                }
            };
        }

        log::debug!("[{}] State machine has completed", id);
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

    fn try_initialize(&mut self) {
        if !self.is_initialized {
            let _ = self.on_initialization.send(self.inner.initialize());
            self.is_initialized = true;
        }
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

// TODO Derive Debug.
#[derive(Error)]
pub enum StateMachineError<Types: StateTypes> {
    // #[error("Internal state error: {error:?}")]
    State {
        error: Types::Err,
        state: BoxedState<Types>,
        feed: Feed<Types::In>,
    },

    // #[error("State machine ran longer than permitted: {time_budget_ms:?} ms.")]
    Timeout {
        state: BoxedState<Types>,
        feed: Feed<Types::In>,
        time_budget: Duration,
    },
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
        type Err = ();
    }

    enum Message {
        PlaceOrder,
        Verify(Verify),
        FinalConfirmation,
        Cancel,
    }

    enum Verify {
        Address,
        PaymentDetails,
    }

    // `OnDisplay` state ===========
    #[derive(Debug)]
    struct OnDisplay {
        is_ordered: bool,
    }

    impl OnDisplay {
        fn new() -> Self {
            Self { is_ordered: false }
        }
    }

    impl State<Types> for OnDisplay {
        fn desc(&self) -> &'static str {
            "Item is on display for selling"
        }

        fn deliver(
            &mut self,
            message: Message,
        ) -> DeliveryStatus<Message, <Types as StateTypes>::Err> {
            match message {
                Message::PlaceOrder => self.is_ordered = true,
                _ => return DeliveryStatus::Unexpected(message),
            }
            DeliveryStatus::Delivered
        }

        fn advance(&self) -> Result<Transition<Types>, <Types as StateTypes>::Err> {
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
        fn desc(&self) -> &'static str {
            "Order has been placed, awaiting verification."
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
        fn desc(&self) -> &'static str {
            "Order has all required details, awaiting the final confirmation"
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
        fn desc(&self) -> &'static str {
            "Order has been shipped"
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
        fn desc(&self) -> &'static str {
            "Order has been canceled"
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
            "Order".to_string(),
            Box::new(on_display),
            Duration::from_secs(5),
        );

        let (_, mut result) = state_machine_runner.run();

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
            "Order".to_string(),
            Box::new(on_display),
            Duration::from_secs(5),
        );

        let (_, mut result) = state_machine_runner.run();

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
            Err(_) => panic!("Unexpected error"),
        }
    }
}
