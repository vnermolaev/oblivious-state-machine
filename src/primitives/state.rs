use downcast_rs::Downcast;
use std::fmt::Debug;

/// `State` is an abstraction for any internal state of a protocol.
/// State may send out some messages at initialization and then only deliver messages in order
/// to advance to a next state.
pub trait State<Types: StateTypes>: Downcast + Debug {
    fn desc(&self) -> String;

    fn initialize(&self) -> Vec<Types::Out> {
        Vec::new()
    }

    /// Deliver a message to the state to make progress.
    /// Default implementation just returns the message marking it as Unexpected.
    fn deliver(&mut self, message: Types::In) -> DeliveryStatus<Types::In, Types::Err> {
        DeliveryStatus::Unexpected(message)
    }

    /// Attempts to advance the state forward.
    /// This function my accept `self: Box<Self>` and consume itself,
    /// which may seem more logical, on the other hand it shall present challenges treating
    /// self-consuming states:
    /// - all possible transitions must contain `self` even in the case when the state is not advanceable or terminal,
    /// - in case of errors: `Types::Err` must also contain `self` to enable traceability of which state errored.
    /// This conditions tip the scale in favor of `&self` (and thus some memory copying overhead).
    fn advance(&self) -> Result<Transition<Types>, Types::Err>;
}

/// A direct copy-past from `impl_downcast!(State<Types> where Types: StateTypes)`
/// but with an addition of `Send`.
#[allow(unused_qualifications)]
impl<Types> dyn State<Types> + Send
where
    Types: ::downcast_rs::__std::any::Any + 'static,
    Types: StateTypes,
{
    /// Returns true if the trait object wraps an object of type `__T`.
    #[inline]
    pub fn is<__T: State<Types>>(&self) -> bool {
        ::downcast_rs::Downcast::as_any(self).is::<__T>()
    }

    /// Returns a boxed object from a boxed trait object if the underlying object is of type
    /// `__T`. Returns the original boxed trait if it isn't.
    #[inline]
    pub fn downcast<__T: State<Types>>(
        self: std::boxed::Box<Self>,
    ) -> std::result::Result<std::boxed::Box<__T>, std::boxed::Box<Self>> {
        if self.is::<__T>() {
            Ok(::downcast_rs::Downcast::into_any(self)
                .downcast::<__T>()
                .unwrap())
        } else {
            Err(self)
        }
    }
}
// =====================

/// Type bounds to use for a specific state machine.
///
/// All [State]s in the same state machine are required to be of the same [StateTypes].
// It is not possible to move the `Debug + Send` type bounds of the associated types to `StateTypes`,
// because in that case the compiler will not know that these bounds apply to the associated types.
pub trait StateTypes: 'static {
    /// Type of incoming messages
    type In: Debug + Send;
    /// Type of outgoing messages
    type Out: Debug + Send;
    /// Type of errors emitted from the state machine
    type Err: Debug + Send;
}

#[derive(Debug)]
pub enum DeliveryStatus<U: Debug, E: Debug> {
    Delivered,
    Unexpected(U),
    Error(E),
}

pub type BoxedState<Types> = Box<dyn State<Types> + Send>;

pub enum Transition<Types: StateTypes> {
    Same,
    Next(BoxedState<Types>),
    Terminal,
}

impl<Types: StateTypes + 'static> Debug for Transition<Types> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        match self {
            Transition::Same => write!(formatter, "Same"),
            Transition::Next(state) => write!(formatter, "Next: <{}>", state.desc()),
            Transition::Terminal => write!(formatter, "Terminal"),
        }
    }
}

#[cfg(test)]
mod test {
    use crate::state::{DeliveryStatus, State, StateTypes, Transition};
    use std::collections::VecDeque;

    struct Types;
    impl StateTypes for Types {
        type In = Incoming;
        type Out = ();
        type Err = String;
    }

    #[allow(dead_code)]
    #[derive(Debug)]
    enum Incoming {
        P1,
        P2(()),
        P3,
    }

    #[derive(Debug)]
    pub(crate) struct Initialization;

    impl State<Types> for Initialization {
        fn desc(&self) -> String {
            "Initialization".to_string()
        }

        fn deliver(
            &mut self,
            message: Incoming,
        ) -> DeliveryStatus<Incoming, <Types as StateTypes>::Err> {
            match message {
                Incoming::P1 => DeliveryStatus::Delivered,
                _ => DeliveryStatus::Unexpected(message),
            }
        }

        fn advance(&self) -> Result<Transition<Types>, <Types as StateTypes>::Err> {
            Ok(Transition::Next(Box::new(Process::new())))
        }
    }

    #[derive(Debug, Eq, PartialEq)]
    pub(crate) struct Process {
        messages: Vec<()>,
    }

    impl Process {
        pub fn new() -> Self {
            Self {
                messages: Vec::new(),
            }
        }

        fn _deliver(&mut self, message: ()) -> Result<(), String> {
            if self.messages.len() < 2 {
                self.messages.push(message);
                Ok(())
            } else {
                Err("I only need 2 messages".to_string())
            }
        }
    }

    impl State<Types> for Process {
        fn desc(&self) -> String {
            "Processing".to_string()
        }

        fn deliver(
            &mut self,
            message: Incoming,
        ) -> DeliveryStatus<Incoming, <Types as StateTypes>::Err> {
            match message {
                Incoming::P2(m) => match self._deliver(m) {
                    Ok(_) => DeliveryStatus::Delivered,
                    Err(err) => DeliveryStatus::Error(err),
                },
                _ => DeliveryStatus::Unexpected(message),
            }
        }

        fn advance(&self) -> Result<Transition<Types>, <Types as StateTypes>::Err> {
            println!("Attempting to advance: #(msg): {}", self.messages.len());
            Ok(if self.messages.len() == 2 {
                println!("Advancing");
                Transition::Next(Box::new(Finish))
            } else {
                Transition::Same
            })
        }
    }

    #[derive(Debug)]
    pub(crate) struct Finish;

    impl State<Types> for Finish {
        fn desc(&self) -> String {
            "Finishing".to_string()
        }

        fn deliver(
            &mut self,
            _message: Incoming,
        ) -> DeliveryStatus<Incoming, <Types as StateTypes>::Err> {
            DeliveryStatus::Error("Not accepting messages".to_string())
        }

        fn advance(&self) -> Result<Transition<Types>, <Types as StateTypes>::Err> {
            Ok(Transition::Terminal)
        }
    }

    #[test]
    fn advance_through_3_states() -> Result<(), String> {
        let mut feed = VecDeque::from([Incoming::P2(()), Incoming::P2(())]);

        let state: Box<dyn State<Types>> = Box::new(Initialization);

        let mut prev = state;
        let mut is_initiated = false;
        let _next = loop {
            if !is_initiated {
                prev.initialize();
                is_initiated = true;
            }

            match prev
                .advance()
                .unwrap_or_else(|_| panic!("All states must be advancable"))
            {
                Transition::Same => {
                    let _ =
                        prev.deliver(feed.pop_front().expect("Queue must be sufficiently long"));
                }
                Transition::Next(next) => {
                    is_initiated = false;
                    prev = next;
                }
                Transition::Terminal => break prev,
            };
        };

        Ok(())
    }
}
