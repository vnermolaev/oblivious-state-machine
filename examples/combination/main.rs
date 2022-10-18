use oblivious_state_machine::state::BoxedState;
use oblivious_state_machine::state_machine::Either;
use oblivious_state_machine::{combined2, ConversionError, FinalSpec, IntermediateSpec};
use std::any::type_name;
use std::collections::VecDeque;
use std::time::Duration;
use tokio::time;

mod collect;
mod compute;

#[tokio::main]
async fn main() {
    println!("- Does it even build?\n- YES!");

    let mut feed = VecDeque::from([1u32, 2, 3, 4, 5]);
    let mut feeding_interval = time::interval(Duration::from_millis(1));
    feeding_interval.tick().await;

    let start = collect::Collect { u32s: Vec::new() };
    let sm1_spec = IntermediateSpec::new(
        Duration::from_secs(60),
        Box::new(|t| {
            t.downcast::<collect::Collect>()
                .map(|t| {
                    Box::new(compute::Compute {
                        value: t.u32s.iter().sum(),
                    }) as BoxedState<compute::Types>
                })
                .map_err(|unexpected| ConversionError::UnexpectedFinalState {
                    terminal_desc: unexpected.desc(),
                    expected: type_name::<collect::Collect>().into(),
                })
        }),
    );

    let sm2_spec = FinalSpec {
        time_budget: Duration::from_secs(60),
    };

    let mut combined = combined2::Combined::new(Box::new(start), sm1_spec, sm2_spec);

    let res: combined2::CombinedResult<_, _> = loop {
        tokio::select! {
            Some(Either::Result { result, .. }) = combined.recv() => {
                break result;
            }
            _ = feeding_interval.tick() => {
                // feed a message if present.
                if let Some(msg) = feed.pop_front() {
                    let _ = combined.deliver(combined2::CombinedIn::SM0(msg));
                }
            }
        }
    };

    println!("{:?}", res);
}
