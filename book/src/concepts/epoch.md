# Epoch Tracking & Quiescence

The epoch tracker (ported from Borsalino, strengthened) observes **every**
dispatch made through the device — compute and graphics alike. A single
quiescence query then certifies that no GPU work is touching host memory:

```rust
if device.is_quiescent() {
    // Safe for a moving GC to compact host memory.
}
```

This is strictly stronger than the per-library tracking it replaced: one
tracker, both faces of the device, no cross-library accounting.

## The proof certifies a past instant

`prove_quiescent()` returns a `QuiescenceProof` that a dispatch was *observed
complete* — a fact about the instant it was taken, not a guarantee about the
future. Between taking the proof and acting on it, a dispatch can begin on
another thread (TOCTOU window). Closing that window is the consumer's
discipline: single-dispatcher structures, or a shared lock around
dispatch-and-compact, or holding the proof in the same critical section that
would issue new dispatches.

The tracker-level `GpuEpochTracker::prove_quiescent()` is the same logic
without a `Device` at hand — useful for embedding in larger proofs
(e.g. Borsalino's verification pipeline).
