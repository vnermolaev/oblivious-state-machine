//! This crate provides an abstraction level allowing to conveniently describe state machines
//! by describing their states and not the transitions between the states.
//! A comprehensive overview and elaborated example can be found in [this article](https://medium.com/p/b1c9c7a84e76).
mod primitives;
pub use primitives::*;
