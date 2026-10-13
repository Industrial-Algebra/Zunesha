// Copyright (C) 2026 Industrial Algebra
// SPDX-License-Identifier: Apache-2.0

//! GPU dispatch epoch tracking for GC safety.
//!
//! When a WASM runtime (e.g. Baedeker) runs a moving garbage collector, it
//! must know whether any GPU dispatches are in-flight before compacting host
//! memory. A zero-copy GPU buffer backed by GC-managed memory would dangle if
//! the GC moves the underlying allocation.
//!
//! [`GpuEpochTracker`] provides an atomic counter that is incremented on
//! dispatch and decremented on completion. The runtime checks
//! [`is_quiescent`](GpuEpochTracker::is_quiescent) before compaction — if
//! false, it defers compaction until the GPU quiesces.
//!
//! # Why Zunesha owns this
//!
//! In Borsalino the epoch tracker lived in the compute backend and tracked
//! only compute dispatches. By moving it to Zunesha — the shared device
//! substrate — a single tracker observes **every** dispatch through the
//! device: compute (Borsalino) **and** graphics (Goldenweek). One quiescence
//! query therefore certifies that *no* GPU work of any kind is touching host
//! memory before GC compaction. This is strictly stronger than the per-library
//! tracking it replaces.
//!
//! # Performance
//!
//! Each dispatch costs one `AtomicU64::fetch_add` and each completion costs
//! one `AtomicU64::fetch_sub`. A GC check is one `AtomicU64::load`. Negligible
//! against the microseconds of GPU dispatch overhead.

use std::sync::atomic::{AtomicU64, Ordering};

/// Tracks outstanding GPU dispatches for GC safety.
///
/// Embedded in each device backend struct. Call
/// [`begin_dispatch`](Self::begin_dispatch) before submitting work to the GPU
/// and [`end_dispatch`](Self::end_dispatch) when the GPU completes.
///
/// # Examples
///
/// ```
/// use zunesha::epoch::GpuEpochTracker;
///
/// let tracker = GpuEpochTracker::new();
/// assert!(tracker.is_quiescent());
///
/// tracker.begin_dispatch();
/// assert!(!tracker.is_quiescent()); // GPU work in-flight
///
/// tracker.end_dispatch();
/// assert!(tracker.is_quiescent()); // safe to GC compact
/// ```
#[derive(Debug)]
pub struct GpuEpochTracker {
    in_flight: AtomicU64,
}

impl Default for GpuEpochTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl GpuEpochTracker {
    /// Create a new epoch tracker with zero in-flight dispatches.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            in_flight: AtomicU64::new(0),
        }
    }

    /// Record the start of a GPU dispatch.
    ///
    /// Increments the in-flight counter. The counter must be balanced by a
    /// later call to [`end_dispatch`](Self::end_dispatch).
    pub fn begin_dispatch(&self) {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
    }

    /// Record the completion of a GPU dispatch.
    ///
    /// Decrements the in-flight counter. Must be paired with a prior
    /// [`begin_dispatch`](Self::begin_dispatch).
    ///
    /// # Panics
    ///
    /// Panics in debug builds if the counter would go negative, which
    /// indicates an unmatched `end_dispatch` (a readback without a dispatch).
    pub fn end_dispatch(&self) {
        let prev = self.in_flight.fetch_sub(1, Ordering::SeqCst);
        debug_assert!(
            prev > 0,
            "end_dispatch called without matching begin_dispatch (counter underflow)"
        );
    }

    /// Number of dispatches that have begun but not yet completed.
    #[must_use]
    pub fn in_flight(&self) -> u64 {
        self.in_flight.load(Ordering::SeqCst)
    }

    /// True when no GPU operations are outstanding.
    ///
    /// The WASM runtime (Baedeker) calls this before GC compaction.
    /// If `false`, compaction must be deferred until the GPU quiesces.
    #[must_use]
    pub fn is_quiescent(&self) -> bool {
        self.in_flight() == 0
    }

    /// Construct a [`QuiescenceProof`](crate::QuiescenceProof) if the
    /// tracker is currently idle.
    ///
    /// The proof is a phantom — obtainable only through this method, and
    /// this method returns `Some` only at zero in-flight dispatches.
    /// Host runtimes hand it to compaction code that takes the proof as a
    /// parameter, so unverified compaction does not compile.
    /// [`Device::prove_quiescent`](crate::Device::prove_quiescent) mirrors
    /// this at the device layer.
    ///
    /// # Time-of-check caveat
    ///
    /// The proof certifies a **past** instant (the counter was zero at call
    /// time). Another thread may begin a dispatch between this call and the
    /// proof's use; ordering does not close that window. Single-dispatcher
    /// discipline or a shared lock does. See `QuiescenceProof` docs.
    #[must_use]
    pub fn prove_quiescent(&self) -> Option<crate::QuiescenceProof> {
        if self.is_quiescent() {
            Some(crate::QuiescenceProof { _private: () })
        } else {
            None
        }
    }

    /// Run `f` as one tracked dispatch epoch (ADR 0005 §2).
    ///
    /// Begins a dispatch on entry and ends it on scope exit — normal
    /// return **or panic unwind** (a private `Drop` guard retires the
    /// epoch; a panicking consumer cannot stick an in-flight count).
    /// Backends' [`with_dispatch`](crate::Device::with_dispatch) is built
    /// on this; nesting counts depth, as the counter is a depth not a
    /// boolean.
    pub fn scope<R>(&self, f: impl FnOnce() -> R) -> R {
        self.begin_dispatch();
        let _end = EndOnDrop(self);
        f()
    }
}

/// Retires one epoch on drop — including during panic unwind.
struct EndOnDrop<'a>(&'a GpuEpochTracker);

impl Drop for EndOnDrop<'_> {
    fn drop(&mut self) {
        self.0.end_dispatch();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_tracker_is_quiescent() {
        let tracker = GpuEpochTracker::new();
        assert_eq!(tracker.in_flight(), 0);
        assert!(tracker.is_quiescent());
    }

    /// ADR 0005 §2: the scope retires its epoch on unwind — a panicking
    /// consumer cannot stick an in-flight count (the `Drop` guard is the
    /// whole point of `scope` over manual begin/end).
    #[test]
    fn scope_panic_retires_the_epoch() {
        let tracker = GpuEpochTracker::new();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tracker.scope(|| panic!("consumer panicked inside the scope"));
        }));
        assert!(r.is_err());
        assert_eq!(tracker.in_flight(), 0, "unwind must retire the epoch");
    }

    /// ADR 0005 §2: nesting counts depth, not a boolean.
    #[test]
    fn scope_nesting_counts_depth() {
        let tracker = GpuEpochTracker::new();
        let inner = tracker.scope(|| tracker.scope(|| tracker.in_flight()));
        assert_eq!(inner, 2);
        assert_eq!(tracker.in_flight(), 0);
    }

    #[test]
    fn proof_exists_only_at_zero_in_flight() {
        let tracker = GpuEpochTracker::new();
        assert!(
            tracker.prove_quiescent().is_some(),
            "fresh tracker yields a proof"
        );
        tracker.begin_dispatch();
        assert!(
            tracker.prove_quiescent().is_none(),
            "no proof while in flight"
        );
        tracker.end_dispatch();
        assert!(tracker.prove_quiescent().is_some(), "proof returns at zero");
    }

    #[test]
    fn begin_dispatch_increments_counter() {
        let tracker = GpuEpochTracker::new();
        tracker.begin_dispatch();
        assert_eq!(tracker.in_flight(), 1);
        assert!(!tracker.is_quiescent());
    }

    #[test]
    fn end_dispatch_decrements_counter() {
        let tracker = GpuEpochTracker::new();
        tracker.begin_dispatch();
        tracker.end_dispatch();
        assert_eq!(tracker.in_flight(), 0);
        assert!(tracker.is_quiescent());
    }

    #[test]
    fn multiple_concurrent_dispatches_track_correctly() {
        let tracker = GpuEpochTracker::new();
        tracker.begin_dispatch();
        tracker.begin_dispatch();
        tracker.begin_dispatch();
        assert_eq!(tracker.in_flight(), 3);

        tracker.end_dispatch();
        assert_eq!(tracker.in_flight(), 2);
        assert!(!tracker.is_quiescent());

        tracker.end_dispatch();
        tracker.end_dispatch();
        assert_eq!(tracker.in_flight(), 0);
        assert!(tracker.is_quiescent());
    }

    #[test]
    fn interleaved_begin_end_tracks_correctly() {
        let tracker = GpuEpochTracker::new();

        tracker.begin_dispatch();
        assert_eq!(tracker.in_flight(), 1);

        tracker.begin_dispatch();
        assert_eq!(tracker.in_flight(), 2);

        tracker.end_dispatch();
        assert_eq!(tracker.in_flight(), 1);

        tracker.end_dispatch();
        assert!(tracker.is_quiescent());
    }

    #[test]
    fn default_trait_impl_matches_new() {
        let default = GpuEpochTracker::default();
        assert_eq!(default.in_flight(), 0);
        assert!(default.is_quiescent());
    }
}
