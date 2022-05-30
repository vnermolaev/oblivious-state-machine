use std::collections::VecDeque;
use tokio::sync::mpsc;

/// [Feed] combines polling from a queue of messages and a channel. Message can be delayed
/// and later placed in the queue.
pub struct Feed<T> {
    /// Messages from [queue] will be delivered first.
    queue: VecDeque<T>,

    /// Channel to receive message to deliver..
    feed: mpsc::UnboundedReceiver<T>,

    /// Any message drawn from [Feed] can be delayed and later placed in the [queue].
    delayed: Vec<T>,
}

impl<T> Feed<T> {
    pub(crate) fn new(feed: mpsc::UnboundedReceiver<T>) -> Self {
        Self {
            queue: VecDeque::new(),
            feed,
            delayed: Vec::new(),
        }
    }

    // TODO Probably return a result.
    /// Draw the next message either from [queue] or [feed].
    pub(crate) async fn next(&mut self) -> T {
        if !self.queue.is_empty() {
            return self.queue.pop_front().unwrap_or_else(|| {
                panic!("Popping a message from a non-empty queue must not fail")
            });
        }

        self.feed.recv().await.unwrap_or_else(|| {
            panic!("Channel has been closed prematurely; More messages are expected")
        })
    }

    pub(crate) fn delay(&mut self, message: T) {
        self.delayed.push(message);
    }

    /// Place [delayed] messages in the [queue].
    pub(crate) fn refresh(&mut self) {
        self.delayed
            .drain(..)
            .rev()
            .for_each(|message| self.queue.push_front(message));
    }
}
