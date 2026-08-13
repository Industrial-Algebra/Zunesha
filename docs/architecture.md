# Zunesha Architecture

> The shared GPU **device substrate** for the Industrial Algebra graphics/compute
> stack. Named for Zunesha, the immortal elephant that carries the island of Zou —
> a single foundation both compute and graphics stand upon.

Zunesha owns the one thing that both
[Borsalino](https://github.com/Industrial-Algebra/Borsalino) (compute) and
[Goldenweek](https://github.com/Industrial-Algebra/Goldenweek) (graphics) need:
**the GPU device and its memory**. Everything above it is a consumer; everything
below it is a backend (today: Vulkan via raw `ash`).

```
        ┌──────────────────────────────────────────┐
        │                 Miriami                  │   trans-graphical framework (future)
        │     (geometric projection → rendering)   │
        └────────────────────┬─────────────────────┘
                ┌────────────┴────────────┐
        ┌───────▼────────┐      ┌─────────▼─────────┐
        │   Borsalino    │      │     Goldenweek    │
        │    (compute)   │      │     (graphics)    │
        │   dispatch on  │      │  render on        │
        │  compute queue │      │  graphics queue   │
        └───────┬────────┘      └─────────┬─────────┘
                └────────────┬────────────┘
                      ┌──────▼──────┐
                      │   Zunesha   │   device + queues + memory + buffers
                      │   (substrate)│
                      └──────┬──────┘
                             │
                ┌────────────┴────────────┐
                │   Vulkan (ash, raw FFI) │   ← backends (Metal planned)
                └─────────────────────────┘
```

## Why a shared substrate exists

Before Zunesha, every GPU-owning crate opened its own device. Two consumers
(Borsalino + Goldenweek) on the same machine meant **two logical devices, two
copies of every buffer, no compute→render interop**. The deeper problem is that a
GPU device is a *singular* resource: a single physical device, a small fixed set
of queue families, and a shared memory heap. Two crates independently claiming it
is structurally wasteful and prevents zero-copy pipelines.

Zunesha exists to own that singular resource once and hand capability-scoped
views of it to its consumers. See
[ADR 0001 — Shared device substrate](adr/0001-shared-device-substrate.md).

## The three tensions that shaped this design

Zunesha (and Goldenweek beside it) was designed against three tensions surfaced
during the architectural assessment:

1. **The irreducible graphics surface.** Graphics is *necessarily* stockier than
   compute: presentation requires a swapchain, drawing requires a render pass and
   a graphics pipeline (vertex + fragment), and frames have acquire/draw/present
   lifecycle that compute simply does not. This cannot be hidden behind the
   compute abstraction — it has to be a first-class, larger surface
   (`GraphicsBackend` lives in Goldenweek for exactly this reason).
2. **Raw-FFI scope.** Both crates hand-roll their backends (Vulkan via `ash`,
   Metal via `objc`, shaders via `naga`) — deliberately **no `wgpu`**. This is
   the Borsalino lineage: maximum auditability, no hidden translation layer. The
   cost is a larger, more mechanical FFI surface, accepted as the price of
   transparency.
3. **Device sharing.** Compute and graphics must coexist on one device (see
   above). This tension produced Zunesha itself.

## Key decisions

| Decision | ADR |
|---|---|
| Zunesha owns the single device; consumers receive a view of it (not their own device) | [0001 — Shared device substrate](adr/0001-shared-device-substrate.md) |
| Queues are capability-driven: `compute` always present, `graphics`/`transfer` optional | [0002 — Capability-driven queues](adr/0002-capability-driven-queues.md) |
| Each crate owns a distinct, non-overlapping verification responsibility | [0003 — Cross-crate proof agreement](adr/0003-cross-crate-proof-agreement.md) |

## Verification posture

Zunesha does **not** try to prove everything. Per
[ADR 0003](adr/0003-cross-crate-proof-agreement.md), responsibilities split:

| Crate | Proves |
|---|---|
| **Zunesha** | Structural device & buffer safety — buffers don't outlive the device, the epoch/GC tracker sees *all* work, queues match the hardware's capabilities |
| **Borsalino** | Numerical exactness of compute results |
| **Goldenweek** | Structural correctness of graphics state — pipelines are valid, frames are acquired-before-drawn-before-presented |

A notable consequence: moving the epoch tracker into Zunesha makes it
**strictly stronger** than it was in Borsalino, because one tracker now observes
both compute and graphics traffic rather than only compute.

## API surface

The [`Device`](../src/lib.rs) trait is the contract. Its essentials:

- **Construction** — `init()`, `init_with_strategy(MemoryStrategy)`,
  `init_with(InitRequest)`. `init()` is Borsalino-safe: it never requires a
  graphics queue, so it runs on compute-only hardware (e.g. NVIDIA Grace
  Blackwell GB10 / DGX Spark).
- **Introspection** — `queues()`, `limits()`, `memory_strategy()`.
- **Buffers** — `create_buffer`, `create_buffer_uninit`,
  `create_device_buffer*`, `create_buffer_pinned`, `read_buffer`. A `Buffer` owns
  its memory and self-destructs on drop (documented invariant: it must not
  outlive the device).
- **Quiescence** — `in_flight()`, `is_quiescent()`, `prove_quiescence()` (the
  last yields a `QuiescenceProof` a caller can hold as evidence that all
  in-flight work is done).

### Memory placement

`MemoryStrategy` (`Auto` | `Unified` | `DeviceLocal`) controls buffer placement.
Under `Auto`, Zunesha selects device-local (VRAM) for discrete GPUs and any
device with a >1 GB `DEVICE_LOCAL` heap, and unified (host-visible) otherwise —
matching Borsalino's heuristic. `Unified` and `DeviceLocal` force a placement
and are primarily used to exercise both paths in tests.

## Current state

- ✅ Vulkan backend (`src/vulkan.rs`): capability-driven queue selection
  (policy A), host-visible *and* device-local buffer paths, staging transfers,
  epoch tracking. Verified on an NVIDIA RTX 5080 Laptop (`compute=2
  graphics=0 transfer=Some(1)` — three distinct families, async-compute-capable).
- ✅ `NoDeviceStub` for compiling and testing without a GPU.
- ⏳ Metal backend (raw `objc`) — planned, mirroring Borsalino's backend split.
- ⏳ Borsalino refactor — a separate session consumes Zunesha as its device
  layer (handoff recorded in agent memory).

## Relationship to Borsalino

Zunesha is a deliberate extraction: Borsalino previously owned its own
device/buffer/epoch layer. That layer is being lifted into Zunesha so that
Borsalino (compute) and Goldenweek (graphics) share one device rather than each
claiming one. The refactor is out of scope for this crate; it lives in Borsalino.
