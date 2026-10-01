# Introduction

**Zunesha** is the shared GPU **device substrate** for the Industrial Algebra
ecosystem — physical-device selection, queues, memory strategy, buffer
allocation, and unified quiescence tracking.

> The immortal elephant that carries compute and graphics on its back.

## What It Does

Zunesha owns the one thing [Borsalino](https://github.com/Industrial-Algebra/Borsalino)
(compute) and [Goldenweek](https://github.com/Industrial-Algebra/Goldenweek)
(graphics) genuinely share: **the GPU device**. Pipelines, shaders, and dispatch
stay in the consumer libraries; Zunesha is the substrate they stand on.

- **Capability-driven queues** — a `Queues` value always exposes a compute
  queue and *optionally* graphics/transfer queues. Compute-only hardware
  (NVIDIA GB10, headless datacenter GPUs) is a first-class citizen.
- **Shared buffers** — a buffer allocated for compute *is* the memory a render
  pipeline binds. Zero-copy compute→render interop by construction.
- **Unified GC safety** — the epoch tracker observes *every* dispatch through
  the device, compute and graphics alike; one quiescence query certifies no GPU
  work touches host memory before a moving GC compacts.
- **Surface-agnostic** — no windowing, no presentation, no shader compilation.

## Why It Exists

Compute→render interop (particle systems, GPU-driven geometry, Miriami's
Grade-1 compute-then-draw fields) needs compute output bound directly as render
input. That only works when both sides share one device. Zunesha is IA Design
Principle 1 (Geometric Unification) applied honestly: **the GPU device is the
unifying primitive, and compute and graphics are its two faces.**

## Naming

Named for *Zunesha*, the giant immortal elephant that carries the island of Zou
on its back (One Piece) — a device substrate carrying an entire compute +
graphics ecosystem.
