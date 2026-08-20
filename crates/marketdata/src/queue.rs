//! Per-subscriber drop-oldest queue (SPEC §8).
//!
//! Backpressure is per subscriber, not a shared funnel: each subscriber
//! connection gets its own bounded queue with its own drop-oldest policy,
//! so one slow subscriber can only ever affect its own feed. `push` never
//! blocks — the fan-out thread that calls it must never stall waiting on
//! any one subscriber's socket, which is the entire reason market data
//! has a separate thread from order entry (SPEC §4).

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};

use core::event::Event;

struct State {
    items: VecDeque<Event>,
    /// How many items this queue has silently dropped since the last
    /// successful `pop_blocking`. A subscriber's own `StreamSeq` jumps by
    /// `1 + dropped` on the next pop, so a gap in its sequence is exactly
    /// where its own backpressure lost something (SPEC §8: "detects the
    /// loss via a sequence-number gap").
    dropped_since_last_pop: u64,
}

pub struct DropOldestQueue {
    state: Mutex<State>,
    condvar: Condvar,
    capacity: usize,
}

impl DropOldestQueue {
    pub fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(State {
                items: VecDeque::with_capacity(capacity),
                dropped_since_last_pop: 0,
            }),
            condvar: Condvar::new(),
            capacity,
        }
    }

    /// Push an event. Never blocks: if the queue is at capacity, the
    /// oldest item is dropped to make room, and the drop is counted so
    /// the eventual gap is detectable.
    pub fn push(&self, event: Event) {
        let mut state = self.state.lock().expect("queue mutex poisoned");
        if state.items.len() >= self.capacity {
            state.items.pop_front();
            state.dropped_since_last_pop += 1;
        }
        state.items.push_back(event);
        self.condvar.notify_one();
    }

    /// Blocks until an item is available, then returns it along with how
    /// many items were dropped since the previous `pop_blocking` call.
    pub fn pop_blocking(&self) -> (Event, u64) {
        let mut state = self.state.lock().expect("queue mutex poisoned");
        while state.items.is_empty() {
            state = self.condvar.wait(state).expect("queue mutex poisoned");
        }
        let event = state.items.pop_front().expect("checked non-empty above");
        let dropped = state.dropped_since_last_pop;
        state.dropped_since_last_pop = 0;
        (event, dropped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::types::{Price, Qty, Side};

    fn trade(price: u64) -> Event {
        Event::Trade {
            price: Price(price),
            qty: Qty(1),
            taker_side: Side::Buy,
        }
    }

    #[test]
    fn pop_returns_items_in_order_with_no_drops_under_capacity() {
        let queue = DropOldestQueue::new(4);
        queue.push(trade(1));
        queue.push(trade(2));
        assert_eq!(queue.pop_blocking(), (trade(1), 0));
        assert_eq!(queue.pop_blocking(), (trade(2), 0));
    }

    #[test]
    fn push_past_capacity_drops_the_oldest_and_counts_it() {
        let queue = DropOldestQueue::new(2);
        queue.push(trade(1));
        queue.push(trade(2));
        queue.push(trade(3)); // drops 1, queue now holds [2, 3]

        // The oldest surviving item is 2; one item was dropped before it.
        assert_eq!(queue.pop_blocking(), (trade(2), 1));
        // No further drops since that pop.
        assert_eq!(queue.pop_blocking(), (trade(3), 0));
    }

    #[test]
    fn pop_blocking_waits_for_a_push_from_another_thread() {
        use std::sync::Arc;
        let queue = Arc::new(DropOldestQueue::new(4));
        let queue2 = Arc::clone(&queue);
        let handle = std::thread::spawn(move || queue2.pop_blocking());

        std::thread::sleep(std::time::Duration::from_millis(20));
        queue.push(trade(7));

        let (event, dropped) = handle.join().expect("writer thread panicked");
        assert_eq!(event, trade(7));
        assert_eq!(dropped, 0);
    }
}
