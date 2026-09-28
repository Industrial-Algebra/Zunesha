// Copyright (C) 2026 Industrial Algebra
// SPDX-License-Identifier: Apache-2.0

//! The quiescence protocol: how a host runtime (Baedeker's moving GC, two
//! layers up) knows the GPU is not touching host memory before compacting.
//!
//! The tracker counts in-flight dispatches; `prove_quiescent()` hands back
//! a `QuiescenceProof` **phantom type** only when the count is zero —
//! compaction code takes the proof as a parameter, and the compiler
//! refuses the call without it. Parse, don't validate.
//!
//! This example runs on the tracker directly (pure CPU, no GPU required —
//! works in CI and on compute-only hosts).
//!
//! ```sh
//! cargo run --example quiescence
//! ```

use zunesha::GpuEpochTracker;

fn main() {
    let tracker = GpuEpochTracker::new();

    assert_eq!(tracker.in_flight(), 0, "fresh tracker: nothing in flight");
    assert!(tracker.is_quiescent(), "fresh tracker is quiescent");

    // A dispatch begins: the tracker now sees GPU work in flight.
    tracker.begin_dispatch();
    tracker.begin_dispatch();
    assert_eq!(tracker.in_flight(), 2);
    assert!(!tracker.is_quiescent());
    assert!(
        tracker.prove_quiescent().is_none(),
        "no proof while work is in flight — the GC must wait"
    );

    // Work completes.
    tracker.end_dispatch();
    tracker.end_dispatch();
    assert!(tracker.is_quiescent());

    // The proof: obtainable only at zero, unforgeable by construction.
    let proof = tracker
        .prove_quiescent()
        .expect("quiescent tracker yields a proof");
    compact_host_memory(&proof);

    println!("quiescence: dispatch lifecycle verified, proof obtained, compaction safe");
}

/// Stand-in for a moving GC's compaction step: it will not compile unless
/// the caller can produce a `QuiescenceProof` — which only the tracker
/// issues at zero in-flight dispatches.
fn compact_host_memory(_proof: &zunesha::QuiescenceProof) {
    // ... relocate buffers, shuffle host memory ...
}
