# ADR 0001 — Shared device substrate

- **Date:** 2026-08-12
- **Status:** Accepted

## Context

The Industrial Algebra stack has two GPU-consuming crates:

- **Borsalino** — compute (dispatches kernels on a compute queue).
- **Goldenweek** — graphics (renders to a surface on a graphics queue).

Both need a GPU device. A GPU device is a *singular* resource: one physical
adapter, a small fixed set of queue families, one memory heap. Before Zunesha,
each crate opened and owned its own logical device. That produced three problems:

1. **Waste.** Two logical devices on one adapter duplicate device state, command
   pools, and — most painfully — memory.
2. **No interop.** A buffer created by Borsalino cannot be read by Goldenweek
   without a host round-trip, defeating zero-copy compute→render pipelines.
3. **Queue contention.** Each device independently selects queue families, with
   no way to coordinate (e.g. give Borsalino the dedicated async-compute family
   and Goldenweek the graphics family).

Three options were considered:

- **Option 1 — independent device per crate.** Borsalino and Goldenweek each own
  a device. Simple per-crate, but suffers all three problems above.
- **Option 2 — dual-mode (Goldenweek accepts either).** Goldenweek can either own
  a device *or* receive an external one. Flexible, but doubles Goldenweek's test
  surface and spreads the "how do we share a device?" question across crates.
- **Option 3 — a shared device substrate.** A new crate (Zunesha) owns the single
  device; both consumers receive a view of it.

## Decision

**Adopt Option 3: a shared device substrate (Zunesha).**

Zunesha owns the physical device, the logical device, the queues, and the memory.
Borsalino and Goldenweek each hold a `zunesha::Device` and operate against its
queues and buffers. This follows Industrial Algebra Design Principle 1: **the
device is the unifying primitive.**

A buffer created through Zunesha is owned by Zunesha; both consumers' buffer
handles wrap the same `zunesha::Buffer`, enabling zero-copy compute→render
interchange.

## Consequences

**Positive**

- One device, one set of queues, one memory heap, coherently managed.
- Zero-copy compute→render pipelines are structurally possible (shared buffers).
- Queue selection is made once, centrally, with the full set of families visible
  (see [ADR 0002](0002-capability-driven-queues.md)).
- The epoch/GC tracker observes *all* GPU traffic — strictly stronger than when
  each crate tracked only its own (see
  [ADR 0003](0003-cross-crate-proof-agreement.md)).

**Negative**

- Borsalino must be refactored to consume Zunesha (a separate, dedicated
  session; handoff recorded in agent memory).
- Goldenweek must be wired to depend on Zunesha (the "Step C" reshape, pending).
- A bug in Zunesha's device layer now affects both consumers — mitigated by the
  verification split in ADR 0003.
- One more crate to version and publish.

**Neutral**

- Zunesha refuses to own a window or a surface — presentation stays in
  Goldenweek. The device is a pure resource; surfaces are consumer concerns.
