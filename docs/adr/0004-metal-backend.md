# ADR 0004 — Metal backend: queue reinterpretation and storage-mode mapping

- **Date:** 2026-10-10
- **Status:** Accepted

## Context

Zunesha v0.1 shipped Vulkan-only. Borsalino's staged migration onto the
substrate (approved 2026-10-08) therefore landed Vulkan-first — Borsalino's
Metal backend keeps its internal device until Zunesha ships one. ROADMAP
sequencing is substrate-first: Zunesha-Metal precedes Goldenweek-Metal, so
graphics consumers never build against a hypothetical backend.

The reference implementation is Borsalino `src/metal.rs` (v0.7.0, 1072
lines) — its comments record the crashes that taught each rule. This backend
absorbs its device/buffer layer and improves on two known gaps:
Borsalino-Metal ignores `MemoryStrategy` (always `Shared`), and its
autoreleasepool discipline covers only the dispatch paths (init, compile,
and buffer creation mint objc objects un-pooled).

Metal differs from Vulkan in three ways that shape this ADR:

1. **No queue families.** One `MTLCommandQueue` executes everything;
   there is no per-capability family topology to select.
2. **No queryable storage-buffer-offset alignment.** Vulkan reports
   `minStorageBufferOffsetAlignment` per device; Metal's headers document
   no equivalent property (the 16-byte note in `MTL4ComputeCommandEncoder.h`
   concerns threadgroup memory, not storage buffers).
3. **Documented thread-safe command queue.** `MTLCommandQueue` may be used
   from multiple host threads concurrently — Vulkan queues are externally
   synchronized objects, which is why the Vulkan backend needs a submission
   protocol.

## Decision

### 1. Queues: reinterpret the capability contract, never violate it

ADR 0002's contract — `compute` always present; `graphics`/`transfer`
optional per hardware truth — maps onto Metal as:

| `Queues` field | Metal reality | Value |
|---|---|---|
| `compute` | the owned `MTLCommandQueue` | always present |
| `graphics` | the same queue services rendering; every Metal device renders | `Some` (same handle) |
| `transfer` | no distinct transfer queue; blits ride the same queue | `None` |

`has_graphics()` is `true` on every Metal device because that is the truth
of the hardware — the contract's meaning ("present iff the hardware can")
is preserved; only its expression differs. Consumers must not assume
`compute` and `graphics` are distinct handles or families (Borsalino does
not; Goldenweek-Metal must be told). `Queue::family_index` carries `0` as a
documented placeholder — Metal has no families to index.

`InitRequest::prefer_graphics` is a no-op at selection: there are no
compute-only Metal devices in practice, so no bias can change the outcome.
`device_hint` matching uses `MTLCopyAllDevices` (name substring,
case-insensitive; the returned array is `NS_RETURNS_RETAINED` — the
matched device gets an explicit `retain` to outlive the array's release);
single-GPU Macs trivially resolve to the system default.

### 2. Storage modes: the strategy is honoured, not ignored

| `MemoryStrategy` | Metal storage | Placement reported |
|---|---|---|
| `Auto`, `Unified` | `MTLStorageModeShared` (`options: 0`) | `HostVisible` |
| `DeviceLocal` | `MTLStorageModePrivate` + Shared staging + `MTLBlitCommandEncoder` | `DeviceLocal` |

`Auto` resolves to Shared everywhere on Metal — including discrete-GPU
Intel Macs, where Shared is system-memory-backed and slower but always
correct. The dominant fleet is Apple Silicon (unified by construction),
Borsalino-Metal ran Shared exclusively in production, and the explicit
`DeviceLocal` request exists for consumers that want the staging profile.
This diverges from the Vulkan backend's `Auto` heuristic (discrete → VRAM)
by being deliberately simpler; the *contract* — `Auto` resolves to a
concrete, reported placement — is identical.

`create_device_buffer(_uninit)` is the mandatory override (post-0.1.1
contract): it forces Private + staging regardless of the negotiated
strategy. `MTLStorageModePrivate` exists on every Metal device, so there is
no hard-error branch — the R1 explicit-error allowance for the
`DeviceLocal` strategy path was never needed and never applies to the
override.

Buffer allocation floors lengths at the reported alignment (16), matching
the Vulkan backend: zero-length requests allocate the floor. Uploads copy
`byte_len` bytes into the floored allocation via `contents()` rather than
`newBufferWithBytes:` (whose `length` is also its copy length — using the
floored length would over-read the source slice).

### 3. Alignment floor: 16, documented as chosen, not queried

Metal exposes no queryable minimum storage-buffer-offset alignment. The
backend reports **16** as a conservative floor on three legs of evidence:
Borsalino's Kani harnesses assume 16 for Metal (assumption, now adopted as
the contract), MoltenVK reports `minStorageBufferOffsetAlignment = 16` for
Metal devices (an independent implementation's precedent), and Apple's
best-practices guidance is to align buffer offsets to 16 bytes. It is
honest about being a *chosen* value: `DeviceLimits`'s doc names the field's
meaning and the backend docs point here.

### 4. No submission protocol — but a readback lock

`MTLCommandQueue` is documented thread-safe, so consumers dispatching their
own command buffers on `queues().compute.raw` need no
`with_compute_queue`-style protocol (a deliberate difference from the
Vulkan backend; consumers read the queue handle and go).

Private buffers keep one persistent Shared staging buffer each. A
concurrent `read_buffer` of the same buffer would rewrite that shared
mapping between the device→staging blit and the host copy — the race the
Vulkan backend's review round 2 (P1) closed with its submission lock. The
Metal backend closes the same race with `readback_lock`, scoped to the
Private readback path only (creation staging buffers are freshly allocated
per buffer and never shared).

### 5. objc discipline (absorbed from Borsalino's crash history)

1. **Every objc-minting path runs inside `objc::rc::autoreleasepool`** —
   init and buffer creation included, exceeding Borsalino (whose pools
   cover only dispatch). On plain Rust worker threads, un-pooled
   autoreleased objects are never reclaimed.
2. **Release only `new`/`copy`/`retain` products, exactly once.**
   Autoreleased (+0) returns (`commandBuffer`, encoders) are never
   released — that over-release was Borsalino's dispatch SIGSEGV.
3. **Escape accounting follows what escapes:** +0 objects needing to
   outlive their pool get one explicit `retain`; +1 products transferred
   into Rust structures need no further retain (their creation reference
   IS the ownership; released in drop).
4. **`contents()` is a borrowed pointer, never an objc object** — never
   released; lifetime governed by the owning MTLBuffer.
5. **Failure paths release what they own** (queue-fail → release device;
   staging-fail → release the private buffer; blit-fail → release both).

### 6. Teardown: no invented idle wait

`MetalDevice::drop` releases the queue, then the device (reverse creation
order). Metal retains command buffers on a queue, so there is deliberately
no `device_wait_idle` analogue and no blanket `waitUntilCompleted` sweep —
the consumer contract (all submissions retired before the last handle
drops) plus Metal's own retention make one unnecessary. The epoch tracker
sits at the same honest state as the Vulkan backend: field present,
consumer dispatch accounting unwired — when ADR-0005-style accounting
lands, it lands for both backends at once.

## Consequences

**Positive**

- Borsalino-Metal can complete its staged migration (Phase 3) onto the
  substrate — the escape hatches (`raw_device`, `raw_buffer`, the queue
  handle) are exactly its three raw surfaces, and `ZUNESHA_REQUIRE_METAL`
  gives its CI the same fail-hard guarantee.
- Goldenweek-Metal inherits a device that already reports
  `has_graphics() == true` honestly.
- `MemoryStrategy` is honoured on both backends — an improvement over
  Borsalino-Metal's silent Shared-everything.
- The pool-everything discipline removes the class of leaks Borsalino's
  init/compile/create paths still carry.

**Negative**

- `graphics == compute` (same handle) means a consumer cannot use the
  `Queues` shape alone to infer queue isolation on Metal — a Goldenweek
  assumption of distinct families would be wrong there and must be checked
  at its migration time.
- The 16-byte alignment floor is adopted, not queried; if Metal ever
  documents a different minimum, the value changes by editing this ADR and
  the constant, not by unifying with a query.
- Private readback serializes on one device-wide lock (correct, slightly
  conservative — per-buffer locks would allow unrelated Private buffers to
  read back concurrently; unnecessary until a consumer demonstrates the
  need).

## Verification

Verified on an Apple M5 Max (this machine), 31 tests green under
`ZUNESHA_REQUIRE_METAL=1`:

- `metal_device_inits_and_names_itself`, `queues_shape_matches_adr_0004`,
  `limits_report_documented_alignment_floor` — the ADR-0004 shape itself.
- `buffer_roundtrip_shared`, `buffer_roundtrip_device_local` — both
  storage modes round-trip exactly (Private exercises staging in both
  directions).
- `device_buffer_forces_private_under_unified_strategy` — the mandatory
  override (observable via internals: forced allocations carry staging,
  strategy-respecting ones do not).
- `consumer_style_blit_via_raw_handles` — the R4 contract doing real GPU
  work via raw handles with zero shader compilation.
- `pool_drain_retained_command_buffer_completes` — the +0-escape
  regression (retained command buffer survives the pool drain and
  completes).
- `concurrent_private_buffer_readback_is_serialized` — four threads
  creating + reading Private buffers round-trip exactly under
  `readback_lock`.
- `zero_length_buffer_follows_alignment_floor`,
  `uninit_buffer_allocates_with_logical_shape`,
  `device_hint_matches_or_falls_back`, `fresh_device_is_quiescent`.

## Alternatives considered

- **Wait for Goldenweek-Metal and build both at once** — rejected:
  substrate-first sequencing exists precisely so consumers never depend on
  a hypothetical backend; Borsalino-Metal's migration is blocked on this
  backend existing.
- **`MTLHeap` sub-allocation** — not required for parity with Vulkan 0.1
  (plain `newBufferWithLength` suffices); noted as future work if
  fragmentation ever matters.
- **Per-buffer readback locks** — deferred (see Consequences): a
  device-wide lock is correct and simpler until a consumer demonstrates
  concurrent-Private-readback contention.
