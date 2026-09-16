# Zunesha

[![CI](https://github.com/Industrial-Algebra/Zunesha/actions/workflows/ci.yml/badge.svg)](https://github.com/Industrial-Algebra/Zunesha/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](./LICENSE)

Shared GPU **device substrate** for the Industrial Algebra ecosystem.

> The immortal elephant that carries compute and graphics on its back.

## Documentation

- **[docs/architecture.md](docs/architecture.md)** — purpose, the three-tensions assessment, and position in the stack.
- **Decision records** in [docs/adr/](docs/adr/):
  - [0001 — Shared device substrate](docs/adr/0001-shared-device-substrate.md)
  - [0002 — Capability-driven queues](docs/adr/0002-capability-driven-queues.md)
  - [0003 — Cross-crate proof agreement](docs/adr/0003-cross-crate-proof-agreement.md)

Zunesha owns the one thing [Borsalino](https://github.com/Industrial-Algebra/Borsalino)
(compute) and [Goldenweek](https://github.com/Industrial-Algebra/Goldenweek)
(graphics) genuinely share — the **GPU device**: physical-device selection,
queues, memory strategy, and buffer allocation. Pipelines, shaders, and
dispatch stay in the consumer libraries; Zunesha is the substrate they stand on.

Named for *Zunesha*, the giant immortal elephant that carries the island of Zou
on its back (One Piece) — a device substrate carrying an entire compute +
graphics ecosystem. Zunesha was briefly named *Rayleigh*, renamed when `rayleigh`
was found taken on crates.io.

## Why a shared device

Compute→render interop (particle systems, GPU-driven geometry, Miriami's
Grade-1 compute-then-draw fields) needs compute output bound directly as render
input — zero-copy. That only works when both sides share one device. Zunesha is
IA Design Principle 1 (Geometric Unification) applied honestly: **the GPU device
is the unifying primitive, and compute and graphics are its two faces.**

## Quick Start

```rust
use zunesha::{Device, InitRequest};

// Baseline: best compute device (Borsalino-safe on compute-only hardware).
let device = zunesha::init()?;

// Or prefer a graphics-capable device (Goldenweek on a multi-GPU box).
let device = zunesha::init_with(InitRequest::prefer_graphics())?;

// Capability-driven: compute always present, graphics optional.
if device.queues().has_graphics() {
    // Goldenweek can initialise against this device.
}

// Buffers are the shared primitive — compute writes, graphics reads.
let buf = device.create_buffer(&[1.0f32, 2.0, 3.0, 4.0])?;

// GC safety: one quiescence query certifies NO GPU work (compute or graphics)
// is touching host memory before a moving GC compacts.
if device.is_quiescent() {
    gc_compact();
}
```

## Relation to the ecosystem

| Project | Relationship |
|---|---|
| **Borsalino** | Compute consumer of Zunesha. Holds a `zunesha::Device`, dispatches compute on `queues().compute`. |
| **Goldenweek** | Graphics consumer of Zunesha. Requires `queues().has_graphics()`, renders against the surface. |
| **Baedeker** | WASM host. Creates one Zunesha device, hands it to both the compute and graphics host modules. |
| **Miriami** | Trans-graphical framework (future). Lowers its geometric projection onto Goldenweek, which sits on Zunesha. |

```
                 ┌─────────────┐
                 │   Miriami   │
                 └──────┬──────┘
                        │
              ┌─────────┴─────────┐
              │                   │
       ┌──────┴──────┐     ┌──────┴──────┐
       │  Borsalino  │     │  Goldenweek │
       │  (compute)  │     │  (graphics) │
       └──────┬──────┘     └──────┬──────┘
              │                   │
              └─────────┬─────────┘
                        │
                 ┌──────┴──────┐
                 │   Zunesha   │  the device, queues, memory, buffers
                 └─────────────┘
```

## Design

- **Capability-driven queues.** A [`Queues`] always exposes a `compute` queue
  (Zunesha's baseline contract) and *optionally* `graphics` and `transfer`.
  Zunesha never requires a graphics queue — Borsalino runs on compute-only
  hardware (NVIDIA Grace Blackwell GB10 / DGX Spark, headless datacenter GPUs,
  cloud compute instances). Goldenweek refuses to initialise where
  `queues().graphics` is `None`.
- **Buffer ownership.** `zunesha::Buffer` is the shared primitive;
  `Borsalino::GpuBuffer` and `Goldenweek::GpuBuffer` both wrap it. A buffer
  allocated for compute *is* the memory a render pipeline binds.
- **Unified GC safety.** The epoch tracker (ported from Borsalino) observes
  *every* dispatch through the device — compute and graphics — so a single
  quiescence query certifies no GPU work touches host memory before a moving GC
  compacts. Strictly stronger than the per-library tracking it replaces.
- **Surface-agnostic.** Zunesha does no windowing and compiles no shaders.

## Backends

| Backend | Platform | Feature | Status |
|---|---|---|---|
| Metal | macOS (Apple Silicon) | `metal` | 🚧 post-v0.1 — raw `objc_msgSend` FFI |
| Vulkan | Linux, Windows | `vulkan` | 🚧 post-v0.1 — raw `ash` FFI |
| Stub | Any | (none) | ✅ `NoDeviceStub` — safe fallback |

v0.1 ships the [`Device`] trait, the buffer/queue/quiescence types, the ported
epoch tracker, and a `NoDeviceStub`. Both backends hand-roll their FFI (no
`wgpu`) to match Borsalino/Goldenweek's auditability.

## Design refusals (v0.1)

Zunesha follows IA Design Principle 4 (Architectural Refusal):

| # | Refusal | Rationale |
|---|---|---|
| 1 | **No shaders, no pipelines.** | Shader compilation stays in Borsalino (compute) and Goldenweek (graphics). Zunesha never imports `naga`. |
| 2 | **No windowing, no presentation.** | Zunesha owns the device, not a surface. Presentation is Goldenweek's concern. |
| 3 | **No graphics queue requirement.** | Compute-only hardware (GB10, headless GPUs) must run Borsalino unimpeded. Queues are capability-driven. |
| 4 | **No `wgpu` dependency.** | Hand-roll Metal/Vulkan FFI — Borsalino/Goldenweek lineage. |

## Verification

Zunesha inherits the device/buffer-lifecycle layer of Borsalino's verification
pipeline — the parts that genuinely move with the code:

- **Epoch/GC tracking** (ported, always-on): [`epoch::GpuEpochTracker`].
- **Buffer alignment, pinned-buffer lifetimes, Miri buffer-lifecycle harnesses**
  (behind a future `verify` feature).

Compute-semantics verification (numerical exact-match, determinism, per-kernel
obligation bundles) stays in Borsalino. Cross-crate proof agreement (IA P3) —
where Borsalino/Goldenweek per-kernel bundles cite Zunesha's device/buffer
obligations by origin — is governed by an ADR.

## Testing

GPU tests run against the host's Vulkan ICDs and skip with a message when
no device (or loader) is present — e.g. on CI runners.

Pin a device for reproducibility with `ZUNESHA_TEST_DEVICE` (a
case-insensitive device-name substring, the same convention as Goldenweek's
`GOLDENWEEK_TEST_DEVICE`):

```sh
ZUNESHA_TEST_DEVICE=intel cargo test --features vulkan
```

GPU tests are serialized with `serial_test` — the Vulkan loader is not safe
under parallel instance creation.

## License

Apache-2.0. Copyright (C) 2026 Industrial Algebra.

Contributors must sign the [CLA](https://github.com/Industrial-Algebra/.github/blob/main/CLA.md).
