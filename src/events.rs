//! The one virtual-time event queue.
//!
//! A min-ordered queue of future events keyed by **vtsc** (see [`crate::vtsc`]).
//! Every guest timer is an entry here: the userspace LAPIC timer (and anything
//! else that must fire at a guest-time deadline) pushes its next deadline, and
//! the vCPU loop drains everything due before each `KVM_RUN`:
//!
//! ```ignore
//! // vCPU loop boundary (see service_timers in main.rs):
//! while let Some(ev) = queue.pop_due(clock.vtsc_now()) {
//!     ev.payload.fire();          // e.g. raise the LAPIC timer vector into IRR
//! }
//! let next = queue.peek_deadline();   // the park's wait-or-jump target
//! ```
//!
//! The LAPIC deadline is mirrored here each loop boundary, `pop_due` fires
//! everything due, and `peek_deadline` is what the park either waits for or
//! JUMPs to.
//!
//! Ordering is deterministic: strictly by `deadline`, and for equal deadlines
//! by insertion order (FIFO). Stable, reproducible ordering keeps virtual-time
//! semantics well-defined and runs debuggable, so ties must never resolve by
//! address or hash iteration order.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

/// One scheduled event: fire `payload` when the virtual clock reaches
/// `deadline` (a vtsc value, in TSC cycles).
#[derive(Clone, Copy, Debug)]
pub struct Event<T> {
    pub deadline: u64,
    pub payload: T,
    /// Insertion sequence, for FIFO tie-breaking among equal deadlines.
    seq: u64,
}

// Ordering is by (deadline, seq). We store events in a max-heap but want the
// *earliest* deadline on top, so all comparisons are reversed here — the
// smallest (deadline, seq) compares as "greatest" and lands at the heap root.
impl<T> PartialEq for Event<T> {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline && self.seq == other.seq
    }
}
impl<T> Eq for Event<T> {}
impl<T> Ord for Event<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse: earlier deadline (then earlier seq) is "greater".
        other
            .deadline
            .cmp(&self.deadline)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}
impl<T> PartialOrd for Event<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A min-by-vtsc priority queue of [`Event`]s.
#[derive(Debug, Default)]
pub struct EventQueue<T> {
    heap: BinaryHeap<Event<T>>,
    next_seq: u64,
}

impl<T> EventQueue<T> {
    pub fn new() -> Self {
        Self {
            heap: BinaryHeap::new(),
            next_seq: 0,
        }
    }

    /// Number of pending events. Introspection for tests / future callers.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Schedule `payload` to fire at vtsc `deadline`. Equal deadlines fire in
    /// insertion order.
    pub fn push(&mut self, deadline: u64, payload: T) {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.heap.push(Event {
            deadline,
            payload,
            seq,
        });
    }

    /// The earliest pending deadline (vtsc), or `None` if empty. This arms the
    /// park timeout so the vCPU wakes in time to service it.
    pub fn peek_deadline(&self) -> Option<u64> {
        self.heap.peek().map(|e| e.deadline)
    }

    /// Drop all pending events. Used by the vCPU loop to reconcile the queue
    /// with the LAPIC's single armed timer each boundary (the LAPIC is the timer
    /// authority; the queue mirrors its current deadline so `peek_deadline`
    /// drives the idle park). Cheap: there is at most one timer entry.
    pub fn clear(&mut self) {
        self.heap.clear();
    }

    /// Pop and return the earliest event iff it is due at `now` (deadline
    /// <= now). Call in a loop to drain everything due at a loop boundary.
    pub fn pop_due(&mut self, now: u64) -> Option<Event<T>> {
        match self.heap.peek() {
            Some(e) if e.deadline <= now => self.heap.pop(),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_queue() {
        let mut q: EventQueue<u32> = EventQueue::new();
        assert!(q.is_empty());
        assert_eq!(q.len(), 0);
        assert_eq!(q.peek_deadline(), None);
        assert_eq!(q.pop_due(u64::MAX), None);
    }

    #[test]
    fn orders_by_deadline_not_insertion() {
        let mut q: EventQueue<&str> = EventQueue::new();
        q.push(300, "c");
        q.push(100, "a");
        q.push(200, "b");
        assert_eq!(q.len(), 3);
        assert_eq!(q.peek_deadline(), Some(100));
        assert_eq!(q.pop_due(u64::MAX).unwrap().payload, "a");
        assert_eq!(q.pop_due(u64::MAX).unwrap().payload, "b");
        assert_eq!(q.pop_due(u64::MAX).unwrap().payload, "c");
        assert!(q.is_empty());
    }

    #[test]
    fn equal_deadlines_are_fifo() {
        let mut q: EventQueue<u32> = EventQueue::new();
        q.push(50, 1);
        q.push(50, 2);
        q.push(50, 3);
        assert_eq!(q.pop_due(50).unwrap().payload, 1);
        assert_eq!(q.pop_due(50).unwrap().payload, 2);
        assert_eq!(q.pop_due(50).unwrap().payload, 3);
    }

    #[test]
    fn pop_due_respects_now() {
        let mut q: EventQueue<&str> = EventQueue::new();
        q.push(100, "soon");
        q.push(500, "later");
        // Nothing due before the earliest deadline.
        assert_eq!(q.pop_due(99), None);
        // Boundary: deadline == now is due.
        assert_eq!(q.pop_due(100).unwrap().payload, "soon");
        // "later" still not due.
        assert_eq!(q.pop_due(100), None);
        assert_eq!(q.pop_due(499), None);
        assert_eq!(q.pop_due(500).unwrap().payload, "later");
        assert!(q.is_empty());
    }

    #[test]
    fn drain_all_due_in_order() {
        let mut q: EventQueue<u64> = EventQueue::new();
        for d in [40u64, 10, 30, 20, 60, 50] {
            q.push(d, d);
        }
        let now = 45;
        let mut drained = Vec::new();
        while let Some(ev) = q.pop_due(now) {
            drained.push(ev.deadline);
        }
        assert_eq!(drained, vec![10, 20, 30, 40]); // all <= 45, ascending
        assert_eq!(q.peek_deadline(), Some(50)); // 50, 60 remain
    }
}
