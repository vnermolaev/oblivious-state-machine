use crate::StateMachineTypes;
use downcast_rs::Downcast;
use std::any::type_name;

pub trait State<T: StateMachineTypes>: Downcast {
    fn describe(&self) -> String {
        type_name::<Self>().to_string()
    }

    fn initialize(&self) -> Vec<T::O> {
        Vec::new()
    }

    fn deliver(&mut self, message: T::I) -> Delivery<T::I, T::E> {
        Delivery::Unexpected(message)
    }

    /// Complete all actions required for a successful [Transition] to the next state, e.g.,
    /// verify all conditions, compute extra data.
    fn prepare_advance(&mut self) -> Result<bool, Vec<T::E>>;

    /// Produce a [Transition] to the next state.
    fn advance(self: Box<Self>) -> Transition<T>;
}

// downcast_rs::impl_downcast!(State<T> where T: StateMachineTypes);

/// A direct copy-past from `impl_downcast!(State<Types> where Types: StateTypes)`
/// but with an addition of `Send`.
#[allow(unused_qualifications)]
impl<T> dyn State<T> + Send
where
    T: ::downcast_rs::__std::any::Any + 'static,
    T: StateMachineTypes,
{
    /// Returns true if the trait object wraps an object of type `__T`.
    #[inline]
    pub fn is<__T: State<T>>(&self) -> bool {
        ::downcast_rs::Downcast::as_any(self).is::<__T>()
    }

    /// Returns a boxed object from a boxed trait object if the underlying object is of type
    /// `__T`. Returns the original boxed trait if it isn't.
    #[inline]
    pub fn downcast<__T: State<T>>(
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

pub type BoxedState<T> = Box<dyn State<T> + Send>;

pub enum Delivery<I, E> {
    Delivered,
    Unexpected(I),
    Error(E),
}

pub enum Transition<T: StateMachineTypes> {
    Next(BoxedState<T>),
    Final(BoxedState<T>),
}
