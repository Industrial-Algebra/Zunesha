# Changelog

All notable changes to Zunesha are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] — Unreleased

### Added — Device Abstraction
- **`Device` trait** — the shared GPU device substrate beneath both
  Borsalino (compute) and Goldenweek (graphics): enumerate devices, resolve
  queues, create/copy/read/write buffers, query limits.
- **Opaque-handle isolation** — `Buffer`, `Queue` expose no backend types.
  Consumers stay FFI-free.
- **`InitRequest`** — named backend, optional `device_hint` (substring match
  against device names) for deterministic device pinning in tests.
- **`NoDevice`** — stub backend for trait-only consumers.

### Added — Capability-Driven Queues (ADR 0002)
- Queue families resolved by capability, not index: dedicated
  compute-without-graphics preferred for compute (async isolation),
  `graphics`/`transfer` queues are `Option` — present only when distinct
  families exist. Never graphics-required: compute-only hardware
  (GB10-class) boots the compute path.
- `prefer_graphics` request enables graphics-readiness (WSI extensions)
  without making it a hard requirement.

### Added — Memory Management
- **`MemoryStrategy`** — `HostVisible` (direct map) or `DeviceLocal`
  (staging + `one_shot_transfer`); detected default per device.

### Added — GPU Epoch Tracker
- `GpuEpochTracker` ported from Borsalino: in-flight dispatch counting and
  quiescence queries for GC coordination (Baedeker).

### Added — Raw-Handle Seams (ADR 0001 / ADR 0003)
- Trusted-consumer accessors on the concrete `VulkanDevice`:
  `raw_instance`, `raw_device`, `physical_device`, `memory_properties`,
  `entry`, and `raw_buffer(&Buffer)` — the zero-copy compute→render interop
  seam Goldenweek's renderer consumes.

### Added — Vulkan Backend (`vulkan` feature)
- Full device/queue/memory/buffer implementation via `ash`: policy-A queue
  resolution, host-visible and device-local buffer paths (verified on
  RTX 5080), graphics-ready WSI extension negotiation
  (`VK_KHR_surface`/`VK_KHR_swapchain`/platform/`VK_EXT_headless_surface`,
  queried and intersected).
