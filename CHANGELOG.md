# Changelog

All notable changes to Zunesha are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

### Fixed — review findings, round 2 (2026-10-01)

- Compaction call removed from current-release examples: the README
  and book quick starts now demonstrate the protocol with an
  explicitly controlled pure-tracker example (`GpuEpochTracker`
  begin/end/is_quiescent) and state outright that
  `device.is_quiescent()` must not drive compaction today. The
  architecture chapter's tracker claims (source and book copy)
  downgraded to the designed-to-be guarantee with the v0.1
  accounting caveat.
- Goldenweek restored as the **live** consumer (borrows
  `zunesha::VulkanDevice`, wraps `zunesha::Buffer`s — verified against
  Goldenweek develop); Borsalino remains the pending migration. The
  "examples/tests only" assertion is gone; buffer-ownership prose now
  says Goldenweek wraps today, Borsalino not yet. README ecosystem
  table matches.
- Architecture source's remaining `../src/lib.rs` link fixed at the
  source this time (the first fix edited the book copy only and a
  re-carry reintroduced it); rendered-link walk zero on both books.

### Fixed — review findings (2026-10-01)

- Book no longer recommends GC compaction on the device counter: the
  epoch chapter, introduction, and quick start now carry the v0.1
  accounting status (no production dispatch path increments the
  tracker yet) and describe the intended guarantee separately.
- ADRs included as chapters in SUMMARY.md — the Decision Records
  index/queue/architecture links now render to real pages; the
  architecture page's `../src/lib.rs` link replaced with docs.rs.
- Backends table corrected: **Vulkan ships in v0.1** (book and README);
  Metal remains post-v0.1. Ecosystem tables now frame consumer
  migrations (Borsalino/Goldenweek/Baedeker) as planned, not shipped.
- Rendered local-link check added to the verification pass (zero
  missing targets).

### Added
- mdBook documentation (IA Navy theme) — `book/` with Introduction,
  Getting Started, Concepts (device substrate, capability queues, epoch
  tracking), Guide, API overview, Design (architecture, refusals, ADR
  index, critique, roadmap), and example walkthroughs. Netlify deploy
  config (`netlify.toml`) and a `v*`-tag docs workflow; README badge.

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

### Added — Placement Observability
- **`MemoryPlacement`** (`HostVisible` / `DeviceLocal`) and
  `Device::buffer_placement()` — the *effective* placement of a device's
  buffers, distinct from the requested `MemoryStrategy`. `Auto` resolves at
  init; this reports the resolution so consumers and tests no longer have to
  re-derive the hardware heuristic (per RABBIT_HOLE_2026-09-02).
- **`ZUNESHA_TEST_DEVICE`** test-pinning convention, mirroring Goldenweek's
  `GOLDENWEEK_TEST_DEVICE`.

### Added — Release Polish (v0.1.0)
- **Examples** (`examples/`): `quiescence` (epoch tracker + proof — pure
  CPU, CI-safe), `device-info` and `buffer-roundtrip` (vulkan-gated,
  skip gracefully without a device).
- **`release.yml`** — tag-triggered crates.io publish (version-verified) +
  GitHub Release with changelog-extracted notes.
- **README** — crates.io/docs.rs badges, Examples section, docs.rs link.
- **`docs/ROADMAP.md`** + **`docs/critique.md`** — v0.1.0 snapshot (honest
  weaknesses: tracker vacuity, unowned cross-queue time, single consumer,
  `verify` unproven, single backend).
- **Quiescence-proof TOCTOU honesty** — `QuiescenceProof` docs now state
  the proof certifies a **past** instant (time-of-check/time-of-use window
  between proof and use; consumer discipline closes it). Found by the
  2026-09-29 Borsalino research dive; documented on the type, the tracker
  method, and in the critique before first publish.

### Added — Vulkan Backend (`vulkan` feature)
- Full device/queue/memory/buffer implementation via `ash`: policy-A queue
  resolution, host-visible and device-local buffer paths (verified on
  RTX 5080), graphics-ready WSI extension negotiation
  (`VK_KHR_surface`/`VK_KHR_swapchain`/platform/`VK_EXT_headless_surface`,
  queried and intersected).
