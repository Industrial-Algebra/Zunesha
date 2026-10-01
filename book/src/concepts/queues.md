# Capability-Driven Queues

A [`Queues`] value always exposes a **compute** queue — Zunesha's baseline
contract — and *optionally* **graphics** and **transfer** queues.

## Why capability-driven

Zunesha never requires a graphics queue. Borsalino must run on compute-only
hardware (NVIDIA Grace Blackwell GB10 / DGX Spark, headless datacenter GPUs,
cloud compute instances); making graphics mandatory would strand those targets.
Goldenweek, which does need graphics, refuses to initialise where
`queues().graphics` is `None` — the capability is discovered, not assumed.

```rust
if device.queues().has_graphics() {
    // Goldenweek path
} else {
    // Still a perfectly good Borsalino device
}
```

The full reasoning is in
[ADR 0002 — Capability-driven queues](./../design/adr/0002-capability-driven-queues.md).
