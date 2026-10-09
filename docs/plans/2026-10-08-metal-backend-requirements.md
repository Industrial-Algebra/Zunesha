# Zunesha Metal Backend — Requirements for the Implementing Session

- **Date:** 2026-10-08
- **Status:** requirements handoff — no code written; for the session that
  builds `zunesha`'s Metal backend in parallel with Borsalino's staged
  Vulkan migration
- **Context:** Borsalino's migration onto `zunesha::Device` was approved
  **staged (Vulkan-first)** on 2026-10-08 — see
  [Borsalino `docs/plans/2026-10-08-zunesha-migration.md`](https://github.com/Industrial-Algebra/Borsalino/blob/develop/docs/plans/2026-10-08-zunesha-migration.md).
  Borsalino's Metal backend keeps its internal device until **this** backend
  exists. ROADMAP sequencing is substrate-first: Zunesha-Metal precedes
  Goldenweek-Metal.
- **Reference implementation:** Borsalino `src/metal.rs` (v0.7.0, 1072 lines)
  — the Metal device/buffer/dispatch code this backend absorbs. Read it
  before writing code; its comments are the scar tissue of real crashes.

## 1. Mission

Implement `zunesha::Device` for a `MetalDevice` on macOS, mirroring the
structure of `src/vulkan.rs` (build/init, queues, limits, buffers, escape
hatches). Zunesha owns the **MTLDevice, MTLCommandQueue, and buffers**.
Consumers (Borsalino first, Goldenweek later) keep pipelines/shaders/frames —
Zunesha never compiles shaders (architecture: no naga dependency).

## 2. Scope and non-goals

**In scope:** `metal` feature (`dep:objc`), `MetalDevice`, buffers with
storage-mode mapping, queue exposure, limits, escape hatches, `NoDeviceStub`
parity on non-macOS, tests + CI, backend ADR, docs.

**Non-goals:**
- Pipelines/compute dispatch (Borsalino's; it will use the escape hatches),
- swapchains / `CAMetalLayer` / frame lifecycle (Goldenweek-Metal's),
- WGSL→MSL translation (stays in consumers),
- cross-queue synchronization semantics (Metal serialises per command
  queue — note in the ADR, but no ADR-0004-style machinery here).

## 3. Requirements

### R1 — Construction and `InitRequest` semantics

- `MTLCreateSystemDefaultDevice()`; null → clean `InitFailed` error
  ("no Metal-capable GPU").
- Own an `MTLCommandQueue` via `newCommandQueue` (Zunesha owns the queue;
  consumers receive it via `Queues`, never create their own). Null queue →
  release device + clean error (mirror `MetalBackend::init` failure path).
- `InitRequest::prefer_graphics` — on Metal every device renders (there are
  no compute-only Metal devices in practice); note this in the ADR rather
  than special-casing selection. Discrete Macs (`MTLCopyAllDevices`) may be
  used for `device_hint` matching; single-GPU headless Macs have exactly one
  device, so selection is trivially the system default.
- `MemoryStrategy` **must not be silently ignored** (Borsalino-Metal today
  ignores it — Zunesha is the improvement):
  - `Auto` / `Unified` → `MTLStorageModeShared` (unified by construction on
    Apple Silicon; correct default everywhere),
  - `DeviceLocal` → `MTLStorageModePrivate` + Shared staging buffer +
    `MTLBlitCommandEncoder` copies (mirror Zunesha-Vulkan's staging design).
    If that proves disproportionate for a first cut, return an explicit
    `UnsupportedStrategy`-style error instead of silently degrading.

### R2 — Capability-driven queues on Metal (needs an ADR)

Metal has no queue families. Mapping decided for this backend:

| `Queues` field | Metal reality | Value |
|---|---|---|
| `compute` | the owned `MTLCommandQueue` | always present |
| `graphics` | same queue services rendering; every Metal device renders | `Some` (same handle) |
| `transfer` | no distinct transfer queue; blits ride the same queue | `None` |

Document in a backend ADR (0004 or 0005 per numbering when you start): the
capability-driven contract from ADR 0002 is reinterpreted, not violated —
`has_graphics()` is `true` on every Metal device because that is the truth of
the hardware. Consumers must not assume distinct families (Borsalino doesn't;
Goldenweek must be told).

### R3 — Buffers and reads

- `create_buffer` / `create_buffer_uninit`: `newBufferWithBytes:length:options:`
  / `newBufferWithLength:options:` with `options` per R1's strategy mapping.
  `options: 0` **is** `MTLStorageModeShared` (Borsalino's
  `STORAGE_MODE_SHARED = 0`).
- Zero-length buffers: Metal permits `length: 0`; keep Zunesha's
  alignment-floor semantics consistent with the Vulkan backend.
- `read_buffer`: `contents()` pointer read for Shared; staging copy
  (Private → Shared via blit) for Private — mirror `VulkanDevice::read_buffer`.
- `create_device_buffer(_uninit)`: **override** to force Private+staging even
  under `Unified` strategy (same contract as the Vulkan 0.1.1 override — see
  Borsalino migration plan §5.1; do not ship this backend without the
  override).
- `create_buffer_pinned`: Shared storage is already zero-copy on unified
  memory; the pin handle remains a lifetime marker (trait default is fine).

### R4 — Escape hatches (Borsalino Phase 3 depends on these)

Parity with `VulkanDevice`'s accessors, adapted:

| Vulkan accessor | Metal equivalent | Contract |
|---|---|---|
| `raw_device() -> ash::Device` | `raw_device() -> *mut c_void` (MTLDevice, retained borrow) | Zunesha owns; consumers must not release |
| `raw_buffer(&Buffer) -> vk::Buffer` | `raw_buffer(&Buffer) -> *mut c_void` (MTLBuffer) | same |
| `queues()` (`Queue::raw`) | the MTLCommandQueue handle | consumers dispatch on it |
| `physical_device()` | not applicable — omit (or device name/registry-id accessor if needed later) | — |
| `memory_properties()` | not applicable — omit | — |
| `entry()` / `raw_instance()` | not applicable — omit | — |

Borsalino-Metal needs exactly three raw surfaces: MTLDevice (compile),
MTLCommandQueue (dispatch), MTLBuffer (binding). Do not expose more than the
trait + these require.

### R5 — objc discipline (binding, with regression tests)

These rules are lifted verbatim from Borsalino's battle history — violating
any of them produced a real crash once:

1. **Every path that mints objc objects runs inside
   `objc::rc::autoreleasepool`** — including buffer creation and any
   consumer-facing path. On plain Rust worker threads autoreleased objects
   are never reclaimed otherwise.
2. **Release only `new`/`copy`/`retain` products.** Borrowed returns
   (`commandBuffer`, `computeCommandEncoder`, `contents()`) are autoreleased
   — releasing them is an over-release SIGSEGV (Borsalino's dispatch path
   carried exactly this bug).
3. **Objects that must escape a pool get an explicit `retain`** (Borsalino's
   async `Pulse` command buffers). Regression test required: a retained
   command buffer survives `autoreleasepool` drain and still completes.
4. **Failure paths release what they own** — e.g. if queue creation fails
   after device creation, release the device before returning the error.

### R6 — Lifetime, drop, quiescence

- `Buffer` drop: `release` the MTLBuffer (Zunesha's opaque-handle pattern;
  keep `unsafe impl Send/Sync` justification comment from `lib.rs`).
- `MetalDevice` drop: release queue, then device (reverse creation order).
  Metal retains command buffers on a queue, so there is no
  `device_wait_idle` equivalent to argue about — **do not invent one**
  (e.g. no blanket `waitUntilCompleted` sweeps).
- Epoch tracker: same honest state as Vulkan 0.1 — tracker field present,
  consumer dispatch accounting unwired, `in_flight()` reads the tracker.
  When ADR-0004-style accounting lands it lands for both backends at once.

### R7 — Feature gating and stubs

- `[features] metal = ["dep:objc"]`; `objc = "0.2"` under
  `[target.'cfg(target_os = "macos")'.dependencies]` (mirror Borsalino).
- `NoDeviceStub` unchanged (hardware-free tests must keep passing on Linux
  CI with default features).
- Docs: `#![warn(missing_docs)]` applies — every public item documented.

### R8 — Tests and CI

- GPU tests are `#[serial]` (`serial_test` dev-dep already present) — Metal
  driver state is not safe under concurrent device creation.
- Port Borsalino's Metal test set as the acceptance bar:
  `device_init`, `add_one_kernel`, `vector_scale_1024`,
  `buffer_roundtrip` (Shared and, if implemented, Private+staging),
  `async` retained-buffer pool-drain regression (R5.3), placement reports.
- Hardware jobs **assert they touched a device** rather than silently
  passing zero tests: `BORSALINO_REQUIRE_METAL=1` pattern becomes
  `ZUNESHA_REQUIRE_METAL=1` (fail hard if unreachable).
- macOS CI on the self-hosted hantaro runner, mirroring Borsalino's macOS
  job (Holmberg `github-runner` state is the first suspect if it goes quiet).
- `examples/device-info` and `examples/buffer-roundtrip` grow Metal variants
  (feature-gated) — an example that prints queues/limits on a real Mac is
  the fastest manual smoke test.

## 4. Suggested TDD increments

1. `MetalDevice::init` + `queues()` + `limits()` (RED: device name/queue
   non-null on CI metal runner) — no buffers yet.
2. Shared buffer create/read round-trip (port `buffer_roundtrip_unified`).
3. `create_buffer_uninit`, zero-length, alignment-floor semantics.
4. Escape hatches + a consumer-style test that compiles nothing but binds a
   raw buffer handle (proves R4 contract).
5. Private storage + staging path (`DeviceLocal` strategy) if in scope for
   the first cut, else the explicit error (R1).
6. Pool-drain retention regression (R5.3).
7. ADR, ROADMAP check-off, book/architecture table update, examples.

## 5. Acceptance checklist

- [ ] `cargo test --features metal` green on Apple Silicon (hantaro)
- [ ] `ZUNESHA_REQUIRE_METAL=1` job fails hard when Metal is unreachable
- [ ] Linux CI green with default features (stub path intact)
- [ ] objc discipline rules R5.1–R5.4 each pinned by a passing test
- [ ] `create_device_buffer` override present (R3) or explicitly deferred
      with a tracking note
- [ ] Escape hatches R4 present with ownership-contract docs
- [ ] Backend ADR accepted (queue reinterpretation R2, storage-mode mapping)
- [ ] ROADMAP Metal item checked off; architecture doc backend table updated

## 6. Known unknowns for that session

- Whether `min_storage_buffer_offset_alignment` should report 16 (Metal's
  compute buffer alignment guarantee — Borsalino's Kani harnesses assume 16)
  or a queried value; decide in the ADR and keep `DeviceLimits` semantics
  documented.
- `MTLHeap` allocation is **not** required for parity with Vulkan 0.1 —
  plain `newBufferWithLength` suffices; note as future work if fragmentation
  ever matters.
- Borsalino-Metal's `naga_msl_fixup` post-processing stays in Borsalino
  (it is shader-output fixing, not device substrate).
