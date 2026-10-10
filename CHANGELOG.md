# Changelog

All notable changes to Zunesha are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed — allocation-failure cleanup (2026-10-10)

Found by the Borsalino Phase-2 review (PR #60 round 1): the buffer
creation paths leaked partial state on allocation failure. Every failure
step now frees what was created before it propagates:

- `buffer_new` (device-local branch): staging-allocation failure frees
  the device buffer+memory; upload-transfer failure frees both
  allocations **only when nothing reached the GPU** and propagates the
  original transfer error (r1: the detail was initially discarded).
  After a successful submit with a failed wait, the GPU's access to the
  buffers is indeterminate — that case deliberately leaks rather than
  destroy in-flight resources (r2 P1), and sets a device-level
  `unconfirmed_submission` flag (r3 P1s) that every destruction path
  now honors: buffer drop and device teardown quiesce best-effort first
  (`quiesce_for_teardown` / inline `device_wait_idle`) and LEAK —
  with a stderr note — rather than destroy, if the device will not
  quiesce (sound for persistent host-OOM and device loss alike).
- `allocate_buffer`: memory-type lookup, `vkAllocateMemory`,
  `vkBindBufferMemory`, and `vkMapMemory` failures destroy the buffer
  (and free the memory once allocated).
- `allocate_device_local_buffer`: same for memory-type lookup,
  `vkAllocateMemory`, and `vkBindBufferMemory`.

Closed in this patch (was r1 P3): `one_shot_transfer` frees its
command buffer on every pre-submit failure path and documents why the
pending command buffer must NOT be freed after a failed wait.

Runtime fault injection is not feasible in a unit test (ash exposes no
mock layer and a test-only allocator seam is out of patch scope); the
cleanup is verified by construction and code review, matching the
discipline Borsalino adopted for the same shapes.

## [0.1.1] — 2026-10-10

### Fixed — review round 2 (2026-10-10)

- `read_buffer` holds the submission lock through **both** the
  device→staging transfer and the host copy of the shared per-buffer
  staging mapping — previously a concurrent read of the same buffer
  could rewrite the mapping mid-copy.

### Fixed — review round 1 (2026-10-09)

- **P1 (soundness):** `VulkanDevice: Send + Sync` made the unsynchronized
  staging paths reachable from safe code via cloned `Arc`s — the compute
  queue and transfer command pool are externally synchronized Vulkan
  objects. All submissions are now serialized through a `submit_lock`:
  internally for the substrate's staging transfers, and for consumers via
  the new `VulkanDevice::with_compute_queue` accessor (the protocol
  consumers must join to submit on the shared queue). Regression:
  `concurrent_buffer_creation_is_serialized` (4 threads × create+read).
- Teardown: the blanket `device_wait_idle` in `Drop` is now gated on the
  epoch tracker (a no-op today until consumer dispatch accounting is
  wired; activates exactly when work is outstanding) — removes the
  parallel-teardown driver-lock stampede regressed from Borsalino's prior
  discipline (review finding on both repos).

### Added

- `Queue` and `VulkanDevice` are `Send + Sync`, with the soundness
  contract documented on the type: safe sharing is backed by the
  `with_compute_queue` submission protocol (see below), not an appeal
  to caller discipline.
- `VulkanDevice` now overrides `create_device_buffer` /
  `create_device_buffer_uninit` to **force device-local + staging
  allocation regardless of the negotiated strategy** — a consumer that
  forces `MemoryStrategy::Unified` on discrete hardware still gets
  VRAM-resident data for GPU-resident weights. Found by the Borsalino
  migration survey: the trait documented device-local placement but the
  inherited default delegated to the strategy-respecting `create_buffer`
  (Borsalino's own implementation diverges from its trait docs the other
  way — host-visible under forced `Unified`; this override implements the
  documented contract instead). The trait docs now state the override
  contract and the `create_buffer`-vs-`create_device_buffer` behavioral
  distinction. Behavior-preserving refactor:
  `create_buffer`/`create_buffer_uninit` delegate to a shared
  `buffer_new` path (also removes their internal duplication). Pinned by
  `device_buffer_forces_device_local_under_unified_strategy` (discrete-GPU
  test, verified on RTX 5080).

## [0.1.0] — 2026-10-03

### Fixed — review findings, round 3 (2026-10-03)

- README Design bullets aligned with the current-vs-planned wording:
  Goldenweek wraps `zunesha::Buffer` today, Borsalino not yet;
  unified GC safety labeled the intended guarantee with the pending
  consumer-wiring caveat (device counter alone ≠ compaction evidence).

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
