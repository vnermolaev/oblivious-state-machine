use crate::state::BoxedState;
use std::fmt::Debug;

pub mod state;
pub mod state_machine;

/// State machine types.
pub trait StateMachineTypes: 'static {
    /// State machine accepts messages of type `I`.
    type I: Debug;
    /// State machine produces messages of type `O`.
    type O: Debug;
    /// Error type.
    type E: Debug;
}

pub enum Event<T: StateMachineTypes> {
    /// Record produced on successfully achieving the final state.
    Completion {
        /// Final state.
        state: BoxedState<T>,
        /// Left-over unconsumed messages.
        buffer: Vec<T::I>,
    },
    Output(Vec<T::O>),
    Error(Vec<T::E>),
}

#[cfg(test)]
mod test {
    use crate::state::{BoxedState, Delivery, State, Transition};
    use crate::state_machine::{StateMachine, StateMachineId};
    use crate::{Event, StateMachineTypes};
    use futures::StreamExt;
    use std::collections::VecDeque;
    use tokio::sync::mpsc;

    /// This module tests an order state machine.
    /// Item's lifecycle
    /// OnDisplay -(PlaceOrder)-> Placed -(Verify)-> Verified -(FinalConfirmation)-> Shipped
    ///                ↳(Cancel)-> Canceled             ↳(Cancel)-> Canceled

    struct Types;

    impl StateMachineTypes for Types {
        type I = Message;
        type O = ();
        type E = String;
    }

    #[derive(Debug)]
    #[allow(dead_code)]
    enum Message {
        SetOnDisplay,
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
        fn deliver(
            &mut self,
            message: Message,
        ) -> Delivery<Message, <Types as StateMachineTypes>::E> {
            match message {
                Message::PlaceOrder => self.is_ordered = true,
                Message::Damage => self.is_broken = true,
                _ => return Delivery::Unexpected(message),
            }
            Delivery::Delivered
        }

        fn prepare_advance(&mut self) -> Result<bool, Vec<<Types as StateMachineTypes>::E>> {
            if self.is_broken {
                return Err(vec!["Item is damaged".to_string()]);
            }

            Ok(self.is_ordered)
        }

        fn advance(self: Box<Self>) -> Transition<Types> {
            Transition::Next(Box::new(Placed::new()))
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
        fn deliver(
            &mut self,
            message: Message,
        ) -> Delivery<Message, <Types as StateMachineTypes>::E> {
            match message {
                Message::Cancel => self.is_canceled = true,
                Message::Verify(Verify::Address) => self.is_address_present = true,
                Message::Verify(Verify::PaymentDetails) => self.is_payment_ok = true,
                _ => return Delivery::Unexpected(message),
            }

            Delivery::Delivered
        }

        fn prepare_advance(&mut self) -> Result<bool, Vec<<Types as StateMachineTypes>::E>> {
            if self.is_payment_ok && self.is_address_present {
                return Ok(true);
            }

            if self.is_canceled {
                return Ok(true);
            }

            Ok(false)
        }

        fn advance(self: Box<Self>) -> Transition<Types> {
            if self.is_canceled {
                Transition::Final(Box::new(Canceled))
            } else {
                Transition::Next(Box::new(Verified::new()))
            }
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
        fn deliver(
            &mut self,
            message: Message,
        ) -> Delivery<Message, <Types as StateMachineTypes>::E> {
            match message {
                Message::Cancel => self.is_canceled = true,
                Message::FinalConfirmation => self.is_confirmed = true,
                _ => return Delivery::Unexpected(message),
            }

            Delivery::Delivered
        }

        fn prepare_advance(&mut self) -> Result<bool, Vec<<Types as StateMachineTypes>::E>> {
            if self.is_canceled {
                return Ok(true);
            }

            Ok(self.is_confirmed)
        }

        fn advance(self: Box<Self>) -> Transition<Types> {
            if self.is_canceled {
                Transition::Next(Box::new(Canceled))
            } else {
                Transition::Next(Box::new(Shipped))
            }
        }
    }
    // ========================

    // `Shipped` state ===========
    #[derive(Debug)]
    struct Shipped;

    impl State<Types> for Shipped {
        fn prepare_advance(&mut self) -> Result<bool, Vec<<Types as StateMachineTypes>::E>> {
            Ok(true)
        }

        fn advance(self: Box<Self>) -> Transition<Types> {
            Transition::Final(self)
        }
    }
    // ========================

    // `Canceled` state ===========
    #[derive(Debug)]
    struct Canceled;

    impl State<Types> for Canceled {
        fn prepare_advance(&mut self) -> Result<bool, Vec<<Types as StateMachineTypes>::E>> {
            Ok(true)
        }

        fn advance(self: Box<Self>) -> Transition<Types> {
            Transition::Final(self)
        }
    }
    // ========================

    #[tokio::test]
    async fn future_use() -> anyhow::Result<()> {
        let _ = pretty_env_logger::try_init();

        let (mut messages, mut state_machine) = get_test_setup();

        let (message_tx, mut message_rx) = mpsc::channel(10);

        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(event) = state_machine.next() => match event {
                        Event::Completion { state, buffer } => {
                            log::trace!("Terminal state reached");

                            // No new event must be produced by the state machine.
                            assert!(matches!(state_machine.next().await, None));

                            return Ok((state, buffer));
                        }
                        Event::Output(_output) => anyhow::bail!("No output is expected"),
                        Event::Error(errors) => {
                            anyhow::bail!("Inner state of the state machine is inconsistent: {errors:?}")
                        }
                    },

                    Some(m) = message_rx.recv() => {
                        state_machine.deliver(m);
                    },
                }
            }
        });

        // Emulate a message sender.
        tokio::spawn(async move {
            while let Some(m) = messages.pop_front() {
                log::trace!("Sending a message over the channel: {m:?}");
                message_tx.send(m).await.expect("Message must be sent");
            }
        });

        let (terminal, buffer): (BoxedState<Types>, _) = handle.await??;
        assert!(buffer.is_empty());
        assert!(terminal.is::<Shipped>());

        Ok(())
    }

    fn get_test_setup() -> (VecDeque<Message>, StateMachine<Types>) {
        // Messages in random order.
        let mut messages = VecDeque::from([
            Message::Verify(Verify::Address),
            Message::FinalConfirmation,
            Message::SetOnDisplay,
            Message::PlaceOrder,
            Message::Verify(Verify::PaymentDetails),
        ]);

        let state_machine = StateMachine::new_with_message(
            StateMachineId("Order".to_string()),
            messages.pop_front().unwrap(),
            Box::new(|id, messages| {
                if let Some(pos) = messages
                    .iter()
                    .position(|m| matches!(m, Message::SetOnDisplay))
                {
                    log::trace!(
                        "{id} Message required to construct the initial state IS found: {:?}",
                        Message::SetOnDisplay
                    );

                    let _ = messages.remove(pos);
                    return Some(Box::new(OnDisplay::new()));
                }

                log::trace!("{id} Message required to construct the initial state IS NOT found");
                None
            }),
        );
        (messages, state_machine)
    }
}
