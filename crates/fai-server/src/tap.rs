//! Cross-connection traffic broadcast for `fai daemon tap`.
//!
//! A [`TapRegistry`] holds the set of currently subscribed `tap` clients. Every
//! frame read or written on a normal connection is offered to all subscribers,
//! so a tap sees a live JSON decode of traffic flowing on other connections.
//!
//! Delivery is **best-effort**: each subscriber has a bounded buffer, and a
//! subscriber that falls behind drops the overflowing frames rather than stalling
//! the connection producing them — a passive debug observer must never throttle
//! real work. A disconnected subscriber is pruned on the next broadcast.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};

use crate::protocol::TapFrame;

/// Per-subscriber buffer depth. A tap that falls more than this many frames
/// behind drops the surplus (best-effort), bounding the daemon's memory.
pub const TAP_BUFFER: usize = 1024;

/// The set of live `tap` subscribers for one daemon.
#[derive(Default)]
pub struct TapRegistry {
    /// Senders, one per subscribed tap connection.
    subscribers: Mutex<Vec<SyncSender<TapFrame>>>,
    /// A fast-path count of subscribers, so a broadcast with no taps attached
    /// skips the lock (and the caller skips decoding the frame to JSON).
    count: AtomicUsize,
}

impl TapRegistry {
    /// Registers a new subscriber, returning the receiver it should drain. The
    /// channel is bounded ([`TAP_BUFFER`]); a full channel drops frames.
    pub fn subscribe(&self) -> Receiver<TapFrame> {
        let (tx, rx) = sync_channel(TAP_BUFFER);
        let mut subscribers = lock(&self.subscribers);
        subscribers.push(tx);
        self.count.store(subscribers.len(), Ordering::Relaxed);
        rx
    }

    /// Whether any tap is currently attached. Cheap (a relaxed atomic load), so
    /// callers can gate the cost of decoding a frame on there being a listener.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count.load(Ordering::Relaxed) == 0
    }

    /// Offers `frame` to every subscriber, dropping it for any whose buffer is
    /// full and pruning any whose receiver has been dropped.
    pub fn broadcast(&self, frame: &TapFrame) {
        let mut subscribers = lock(&self.subscribers);
        subscribers.retain(|tx| match tx.try_send(frame.clone()) {
            // Delivered, or the tap is behind (drop this frame but keep the tap).
            Ok(()) | Err(TrySendError::Full(_)) => true,
            // The receiver is gone (the tap client disconnected): prune it.
            Err(TrySendError::Disconnected(_)) => false,
        });
        self.count.store(subscribers.len(), Ordering::Relaxed);
    }

    /// The number of live subscribers (for tests).
    #[cfg(test)]
    fn len(&self) -> usize {
        lock(&self.subscribers).len()
    }
}

/// Locks `mutex`, recovering the guard if a previous holder panicked. The
/// protected data (a `Vec` of senders) is always left consistent.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::TapDirection;

    fn frame(conn: u64) -> TapFrame {
        TapFrame { conn, direction: TapDirection::Inbound, json: "{}".to_owned() }
    }

    #[test]
    fn is_empty_until_a_tap_subscribes() {
        let registry = TapRegistry::default();
        assert!(registry.is_empty());
        let _rx = registry.subscribe();
        assert!(!registry.is_empty());
    }

    #[test]
    fn broadcast_reaches_every_subscriber() {
        let registry = TapRegistry::default();
        let rx1 = registry.subscribe();
        let rx2 = registry.subscribe();
        registry.broadcast(&frame(1));
        assert_eq!(rx1.recv().unwrap(), frame(1));
        assert_eq!(rx2.recv().unwrap(), frame(1));
    }

    #[test]
    fn a_dropped_subscriber_is_pruned_on_the_next_broadcast() {
        let registry = TapRegistry::default();
        let rx1 = registry.subscribe();
        let rx2 = registry.subscribe();
        drop(rx2);
        registry.broadcast(&frame(1));
        assert_eq!(registry.len(), 1, "the dropped receiver's sender is pruned");
        assert_eq!(rx1.recv().unwrap(), frame(1), "the live subscriber still receives");
    }

    #[test]
    fn a_full_subscriber_drops_frames_but_survives() {
        let registry = TapRegistry::default();
        let rx = registry.subscribe();
        // Fill the buffer, then overflow it; the surplus is dropped, not the tap.
        for n in 0..(TAP_BUFFER as u64 + 5) {
            registry.broadcast(&frame(n));
        }
        assert_eq!(registry.len(), 1, "a slow tap is kept, not pruned");
        // The earliest frames are buffered; the overflow was dropped.
        assert_eq!(rx.recv().unwrap(), frame(0));
    }
}
