# Epoch Tracking & Quiescence

## Current status: the tracker is wired, the dispatches are not

`is_quiescent()` / `prove_quiescent()` read a `GpuEpochTracker` embedded in
the device — and as of v0.1, **no production path increments it yet**:
`begin_dispatch`/`end_dispatch` are called only by tests. Until consumers
wire their fence-completion callbacks into the device's tracker,
`prove_quiescent()` proves only that *nothing has been counted*, which is not
evidence of GPU quiescence. This is the first entry in the
[critique](./../design/critique.md), and the intended guarantee below is
what the migration will deliver — do **not** compact host memory today on
the strength of the device counter alone.

## The intended guarantee

Once consumers report dispatches through the device's tracker (the planned
Borsalino migration wires fence completion into it), one quiescence query
certifies that no GPU work is touching host memory:

```rust
match device.prove_quiescent() {
    Some(_proof) => { /* all counted dispatches observed complete */ }
    None => { /* in-flight work: not now */ }
}
```

That is strictly stronger than per-library tracking: one tracker, both faces
of the device, no cross-library accounting.

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
