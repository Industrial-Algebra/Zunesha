# Quiescence (pure CPU)

`examples/quiescence.rs` runs everywhere — no GPU, no backend feature. It
exercises the epoch tracker and the stub device to show the GC-safety
contract:

```bash
cargo run --example quiescence
```

The example:

1. Creates the stub device.
2. Simulates dispatch begin/end pairs through the tracker.
3. Takes a `QuiescenceProof` while idle — and shows it is *denied* while a
   dispatch is in flight.

The point: the quiescence machinery is verifiable with zero hardware, so its
semantics (including the TOCTOU honesty of the proof) are CI-enforced on every
push.
