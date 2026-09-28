# Zunesha — Critique (v0.1.0 snapshot)

> **Honest self-assessment, not marketing.** Written at v0.1.0; re-snapshot
> at each release. What follows is what we would tell a careful reviewer
> before they told us.

## What v0.1.0 actually is

A small, deliberately narrow device substrate: one trait, one live backend
(Vulkan), one consumer (Goldenweek). It does physical-device selection,
capability-driven queue resolution, buffer allocation across two memory
paths, and a quiescence protocol. It has never been consumed by the crate
it was extracted *for* (Borsalino) — only by the crate born beside it.

## Known weaknesses (all deliberate, all open)

1. **The epoch tracker observes nothing.** `GpuEpochTracker` is embedded in
   the device, but no production path in any crate increments it yet —
   `begin_dispatch`/`end_dispatch` are called only by tests. Until the
   Borsalino migration wires its fence-completion callbacks into the
   device's tracker, `prove_quiescent()` proves only that *nothing has been
   counted*, which is not the same as quiescence. The relocation argument
   ("strictly stronger at the chokepoint") is architecturally right and
   operationally unproven.

2. **Time is unowned.** Buffers are `SHARING_MODE::EXCLUSIVE` while the
   queue model deliberately splits compute and graphics across families.
   Nothing performs release/acquire ownership transfers; correctness rests
   entirely on host-synchronous discipline (`queue_wait_idle` everywhere,
   blocking present). This is sound today and silently wrong the day
   dispatch goes async. See ROADMAP's ADR-0004 candidate.

3. **One live consumer.** Goldenweek's integration validated the trait
   shape (zero post-hoc API churn through five increments — the best
   available evidence it is right), but a single consumer is a sample of
   one. Borsalino, the harder migration (it has habits to unlearn), has not
   started.

4. **`verify` is a promise.** ADR 0003 assigns Zunesha structural
   device/buffer proof obligations; the feature exists and the phantom
   `QuiescenceProof` is well-shaped, but no karpal-verify obligation
   consumes any of it. The crate's differentiation claim is currently
   architectural, not demonstrated.

5. **Single backend.** Linux/Windows only. macOS consumers (`MemoryStrategy::Unified`'s natural constituency — Apple Silicon, GB10) have no path.

## What went right

- The proof treaty (ADR 0003) held under real integration: Goldenweek
  delegated rather than shadowed, zero defensive re-checks crept in.
- Capability-driven queues absorbed four hardware shapes (three-family
  RTX 5080, unified ARL, Phoenix APU, llvmpipe) with no code changes.
- Raw-FFI scope survived contact with the driver matrix: NVIDIA's stubbed
  headless extension, Mesa's presented-image clobbering and broken
  full-surface fast-clears (found via Goldenweek, ADR Goldenweek 0002) were
  all diagnosed *because* every layer is auditable.

## Security & trust posture

- **No parsing, no network, no secret handling.** Attack surface is the
  Vulkan loader and drivers, addressed by the usual NixOS pinning.
- **Trust boundary = raw-handle accessors** (`raw_device`, `raw_buffer`,
  ...). They are inherent methods on the concrete `VulkanDevice`,
  deliberately *not* on the `Device` trait: consuming them is an explicit
  opt-in to unsafety and to "you own the invariants now." Documented at
  ADR 0001; not yet enforced by type-state (a possible future refinement).
- **`unsafe` inventory:** all FFI is confined to `src/vulkan.rs` with
  safety comments at each call; the trait surface is safe.

## Addressed from earlier reviews

- Placement observability (`MemoryPlacement`) — added after the 2026-09-02
  research dive flagged `uses_device_local` as unobservable.
- Test-pinning convention (`ZUNESHA_TEST_DEVICE`) — mirrors Goldenweek's,
  ends silent device-selection variance across hosts.
- The stale "trait + stub only" backend tables — corrected everywhere.
