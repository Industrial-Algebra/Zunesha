# Zunesha Roadmap

> **Snapshot: v0.1.0 (2026-09).** Items above the line are done and shipped
> in this release; items below the line are speculative — sequenced by
> current intent, not commitment. The substrate moves with its consumers.

## Shipped in v0.1.0

- [x] `Device` trait — init (incl. `device_hint` pinning), queues, limits,
      buffer create/read/write, quiescence protocol
- [x] Capability-driven queues (ADR 0002) — compute always present,
      graphics/transfer `Option`; policy-A family selection
- [x] `MemoryStrategy` (Auto / Unified / DeviceLocal) + staging transfers
- [x] **`MemoryPlacement` + `buffer_placement()`** — effective-placement
      observability (what `Auto` resolved to)
- [x] `GpuEpochTracker` + `QuiescenceProof` phantom (ported from Borsalino)
- [x] Raw-handle seams for trusted consumers (ADR 0001): `raw_instance`,
      `raw_device`, `physical_device`, `memory_properties`, `entry`,
      `raw_buffer` — the zero-copy compute→render interop path
- [x] Complete Vulkan backend (`ash`, Linux/Windows) — verified on
      NVIDIA RTX 5080, Intel ARL/Mesa, AMD Phoenix1/RADV, llvmpipe
- [x] Graphics-readiness: WSI extension negotiation under
      `InitRequest::prefer_graphics`
- [x] ADRs 0001–0003 (substrate, queues, cross-crate proof agreement)

## Pending — committed sequence

- [ ] **Borsalino migration** — Borsalino consumes Zunesha, deletes its own
      epoch tracker + device layer. The substrate's reason to exist; makes
      ADR 0001 fully true. Requires exposing a consumer-callable
      dispatch-scope guard (see `RABBIT_HOLE_2026-09-02` Thread B).
- [ ] **`verify` feature** — karpal-verify obligation bundles for buffer
      alignment + quiescence (ADR 0003's Zunesha row becomes load-bearing).

## Speculative — bannered

> Not scheduled. Each lands only with a concrete consumer need, per ADR
> 0001's reversibility clause.

- [ ] **Metal backend** (`objc`, macOS) — substrate-first sequencing applies:
      nothing consumes it until Goldenweek-Metal exists.
- [ ] **Cross-queue synchronization** (candidate ADR 0004) — queue-family
      release/acquire (or timeline semaphores) for `EXCLUSIVE` buffers
      crossing compute↔graphics families. Unowned today; safe only while
      everything is host-synchronous (the 09-02 rabbit hole's "who orders
      the queues?"). Becomes load-bearing the moment dispatch goes async.
- [ ] **Multi-device topologies** — explicit render-iGPU / compute-dGPU
      pairing beyond `device_hint` substring matching.
