/// A single-state state machine.
use oblivious_state_machine::state::{DeliveryStatus, State, StateTypes, Transition};

#[derive(Debug)]
pub struct Types;
impl StateTypes for Types {
    type In = u32;
    type Out = ();
    type Err = u32;
}

/// Struct to collect 5 u32.
#[derive(Debug)]
pub struct Collect {
    pub u32s: Vec<u32>,
}

impl State<Types> for Collect {
    fn desc(&self) -> String {
        "Collecting numbers".to_string()
    }

    fn deliver(&mut self, message: u32) -> DeliveryStatus<u32, <Types as StateTypes>::Err> {
        self.u32s.push(message);
        DeliveryStatus::Delivered
    }

    /// Transition if 5 u32-values have been collected.
    fn advance(&self) -> Result<Transition<Types>, <Types as StateTypes>::Err> {
        Ok(if self.u32s.len() == 5 {
            Transition::Terminal
        } else {
            Transition::Same
        })
    }
}
