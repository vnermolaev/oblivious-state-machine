/// A single-state state machine.
use oblivious_state_machine::state::{State, StateTypes, Transition};

#[derive(Debug)]
pub struct Types;
impl StateTypes for Types {
    type In = ();
    type Out = u32;
    type Err = String;
}

/// Struct to do a (dummy) computation.
#[derive(Debug)]
pub struct Compute {
    pub value: u32,
}

impl State<Types> for Compute {
    fn desc(&self) -> String {
        "Computing".to_string()
    }

    /// Dummy implementation.
    fn initialize(&self) -> Vec<u32> {
        vec![self.value]
    }

    /// Transition if 5 u32-values have been collected.
    fn advance(&self) -> Result<Transition<Types>, <Types as StateTypes>::Err> {
        Ok(Transition::Terminal)
    }
}
