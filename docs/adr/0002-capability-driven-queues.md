# ADR 0002 — Capability-driven queues

- **Date:** 2026-08-12
- **Status:** Accepted

## Context

A Vulkan physical device exposes a fixed set of *queue families*, each with a
set of capability flags (`GRAPHICS`, `COMPUTE`, `TRANSFER`, ...). The shape of
these families varies wildly across hardware:

- **Discrete GPU (e.g. NVIDIA RTX 5080).** Many families, including a dedicated
  family that supports `COMPUTE` but *not* `GRAPHICS` (async-compute), and a
  dedicated `TRANSFER`-only family.
- **Integrated GPU (e.g. Intel ARL).** Usually one unified family that does
  everything.
- **Compute-only accelerator (e.g. NVIDIA Grace Blackwell GB10 / DGX Spark).**
  May expose `COMPUTE` but **no `GRAPHICS` family at all**.

Borsalino (compute) must run on all of these, including the compute-only one —
Justin operates two GB10-class machines that may lack a graphics queue.
Goldenweek (graphics), conversely, *requires* a graphics queue. A naive model
that assumes "the device has a graphics queue" would break Borsalino on GB10; a
model that assumes "there is one queue" would waste the async-compute family on
discrete GPUs.

The queue model must therefore be **capability-driven**: never require graphics,
but exploit distinct families when they exist.

## Decision

Expose queues as a capability-shaped struct, with **policy A** for family
selection:

```rust
pub struct Queues {
    pub compute: Queue,           // always present
    pub graphics: Option<Queue>,  // present iff a graphics-capable family exists
    pub transfer: Option<Queue>,  // present iff a dedicated transfer-only family exists
}
```

Selection rules (**policy A**):

1. **`compute`** — prefer a dedicated `COMPUTE`-without-`GRAPHICS` family (async
   compute isolation); fall back to the first `COMPUTE`-capable family when no
   dedicated one exists (unified hardware).
2. **`graphics`** — the first `GRAPHICS`-capable family, or `None`.
3. **`transfer`** — the first dedicated `TRANSFER`-only family (no compute, no
   graphics), or `None`.

`compute` is unconditionally present because every physical device Zunesha
selects must support compute. `has_graphics()` lets consumers
(Goldenweek) refuse to initialise where there is no graphics queue.

## Consequences

**Positive**

- Borsalino runs unchanged on compute-only hardware (GB10 / DGX Spark): no
  graphics queue is ever required.
- On discrete GPUs, Borsalino's compute lands on a dedicated async-compute
  family, isolated from Goldenweek's graphics traffic — a real performance win,
  not a cosmetic one.
- `Option<Queue>` for graphics/transfer makes the hardware's shape *legible* in
  the type system: a consumer can branch on capability rather than guessing.

**Negative**

- Staging transfers (Zunesha's own internal `one_shot_transfer`) always use the
  compute queue/family, not the dedicated transfer family. This is correct
  (compute always supports transfer) but leaves the dedicated transfer family
  for *consumers* to use directly rather than for Zunesha's own staging.
- `None`-handling for `graphics`/`transfer` is a small bit of consumer-side
  branching.

## Verification

On an NVIDIA RTX 5080 Laptop (this development machine), policy A selects
`compute=2 graphics=0 transfer=Some(1)` — three distinct families, confirming
both the dedicated async-compute preference and the dedicated transfer family.
On the Intel integrated family, the unified fallback path is exercised
(compute == graphics == the single family, transfer `None`). The
`vulkan_device_inits_and_exposes_compute`,
`dedicated_compute_family_is_selected_when_present`, and
`prefer_graphics_exposes_graphics_when_available` tests guard these behaviors.
