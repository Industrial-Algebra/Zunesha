# ADR 0005 — Dispatch-scope guard and phased cross-queue ownership

- **Date:** 2026-10-13
- **Status:** Accepted

## Context

Zunesha owns device/queues/memory/buffer substrate for two consumers:
Borsalino (compute) and Goldenweek (graphics). Three deferred concerns
meet here:

1. **The epoch tracker is unwired.** `GpuEpochTracker` ships in both
   backends as a field with `in_flight()` defaulting to honest-zero
   accounting at the consumer boundary. ADR 0004 promised: *"when
   ADR-0005-style accounting lands, it lands for both backends at
   once."*
2. **Borsalino's migration is blocked on a consumer-callable
   dispatch-scope guard** (Borsalino handoff 2026-10-06, design question
   1). Borsalino brings a hard constraint from its own crash history:
   teardown must quiesce **epoch-gated**, never a blanket
   `device_wait_idle` (a stampede of driver locks took down its first
   attempt — do not regress).
3. **Cross-queue buffer ownership is unowned.** Vulkan buffers are
   `SharingMode::EXCLUSIVE`; a buffer crossing compute↔graphics needs
   queue-family release/acquire. Today that is safe only because every
   submission is host-synchronous through the Vulkan `SubmissionProtocol`
   (the 2026-09-02 rabbit hole's "who orders the queues?" — answered, so
   far, by "the host, one submission at a time").

A graphics consumer does not exist yet. Goldenweek-Metal is unblocked by
ADR 0004 but needs no cross-queue machinery (one `MTLCommandQueue`
orders everything on-device); Goldenweek-Vulkan would be the trigger.
Per ADR 0001's reversibility clause, transfer **primitives** should not
land before that consumer does. But the **guard** is load-bearing now —
the migration prerequisite — and its API shape depends on whether
transfer rides submission boundaries. Designing them separately risks a
guard that cannot extend.

## Decision

### 1. One ADR, phased landing

The total design covers guard + cross-queue ownership. **Phase 1
(lands now):** the dispatch-scope guard, epoch wiring on both backends,
and the epoch-gated teardown contract. **Phase 2 (designed, not
landed):** queue-family ownership transfer for `EXCLUSIVE` buffers. No
Phase-2 code enters the crate until a graphics consumer needs it; the
design below is the contract that consumer will find.

### 2. Phase 1 — the guard: `Device::with_dispatch`

```rust
fn with_dispatch<R>(&self, f: impl FnOnce() -> R) -> R;
```

A required trait method (not defaulted — the implementation must reach
the backend's embedded tracker, which the trait cannot). Semantics:

- **Epoch accounting:** `begin_dispatch()` on entry, `end_dispatch()`
  on scope exit — normal return **or panic** (the guard is structured
  around the closure, not an opt-in `Drop` token; a panicking consumer
  cannot leave a stuck epoch behind). `in_flight()` therefore becomes
  real on both backends for work routed through the guard.
- **Protocol membership:** on Vulkan the scope joins the same
  `SubmissionProtocol` as `with_compute_queue` — a scope and a raw
  compute-queue closure serialize with each other, so a quiesce cannot
  interleave between a consumer's epoch-begin and its submit. On Metal
  no protocol exists (documented thread-safe queue, ADR 0004 §4); the
  scope is purely epoch accounting.
- **Host-synchronous contract, stated honestly:** an epoch means "a
  consumer-acknowledged dispatch window" — the closure is expected to
  submit **and settle** (inline wait or fence-signal observed) before
  returning. A consumer that returns with GPU work still executing has
  an in-flight count of zero while work runs; that is the documented
  limit of host-synchronous accounting, identical to the limit the
  `SubmissionProtocol` already documents for `with_compute_queue`.
  Phase 2's timeline semaphores are what move `end_dispatch` to a GPU
  signal — until then, settle-before-return is the consumer's
  discipline.

`with_compute_queue` (Vulkan escape hatch) gains the same accounting
internally: entering it begins an epoch, exit ends it. Consumers do not
need to nest `with_dispatch(|| with_compute_queue(..))` — the raw
accessor is itself a tracked scope. Nesting either scope is legal and
counts epochs per level (the counter is a depth, not a boolean; this is
why nested `with_compute_queue` already works — reviews r5/r6 P1s
established per-thread, per-protocol reentrancy keys, and the epoch
counter inherits the same shape).

### 3. Phase 1 — epoch-gated teardown

`VulkanDevice::drop`'s quiesce path is already taint-aware
(load→wait→clear under the protocol lock; a failed wait sets `unconfirmed`
and a taint survives to teardown). This ADR makes that path **the**
teardown contract for both backends and records Borsalino's constraint
as normative: teardown may wait on the protocol/queue only when
accounting says work could exist — never an unconditional
`device_wait_idle`. Metal needs no analogue (ADR 0004 §6: Metal retains
command buffers; the consumer contract plus retention covers it).

### 4. Phase 2 (designed) — ownership transfer for EXCLUSIVE buffers

**Ownership ledger.** The device tracks, per buffer, the queue family
that owns it. Creation assigns the creating consumer's family (compute
by default). A buffer used on another family must be handed over
explicitly:

```rust
fn transfer_ownership(&self, buffer: &Buffer, to: QueueKind) -> Result<()>;
```

performing, under the submission protocol, a submit-side **release
barrier** on the owning family followed by an **acquire barrier** on the
target family (ownership handoff with `srcAccess`/`dstAccess` chosen by
the buffer's usage). The ledger updates only when both halves commit.
Metal: the call exists, validates nothing, and succeeds — one queue
owns everything, always (the ADR-0004 reinterpretation applied to
ownership; `QueueKind` is advisory there).

**Alternatives considered for Phase 2:**

- **`SHARING_MODE_CONCURRENT` at creation** — rejected as the default:
  requires the family list upfront (before a graphics consumer exists),
  hides ownership instead of making it explicit, and degrades to
  implicit sync on some drivers. It remains the documented escape for
  consumers that truly share chaotically; the ledger does not prevent a
  consumer from creating its own concurrent buffers via raw handles.
- **Timeline semaphores as the Phase-2 mechanism** — deferred, not
  rejected. Release/acquire barriers under the host-synchronous
  protocol are the correct mechanism *for the submission model that
  exists*; timeline semaphores become correct when dispatch goes async
  and are the natural bridge to GPU-signalled `end_dispatch`. Phase 2's
  landed form, when a consumer arrives, re-evaluates this choice in
  light of that consumer's dispatch model.
- **Phantom-typed buffer families (`Buffer<F>`)** — rejected: it touches
  every `Buffer` signature in the crate and both consumers for a
  problem a runtime ledger answers reversibly. IA phantom-type style is
  reserved here for proofs (`QuiescenceProof`), not ownership bookkeeping.

## Consequences

**Positive**

- Borsalino's migration unblocks: the guard is its documented
  prerequisite, with the epoch-gated teardown constraint recorded as
  normative in this ADR.
- `in_flight()`/`is_quiescent()`/`prove_quiescent()` become real
  observability on both backends — Baedeker's GC gate stops relying on
  the default-zero fiction.
- The cross-queue question has a written answer ("the ledger +
  release/acquire, under the protocol") instead of a rabbit hole; the
  next session that hits it reads instead of re-deriving.
- No speculative API lands: `transfer_ownership` exists only on paper.

**Negative**

- A misbehaving consumer (returns with work executing) under-counts
  in-flight dispatches — documented, not enforced, until Phase 2's
  GPU-signalled accounting.
- `with_dispatch` as a required trait method is a breaking change for
  any external `Device` implementor; acceptable at 0.x with a
  changelog note (there are none outside this workspace today).
- The Phase-2 design may be revised when its first consumer arrives —
  the ledger is contract-shaped, not contract-frozen.

## Verification

Phase 1 lands with tests on both backends under
`ZUNESHA_REQUIRE_METAL=1`:

- `with_dispatch` counts one in-flight epoch during the closure and
  zero after.
- Nested scopes count depth (2 during the inner closure).
- A panicking closure cannot leave a stuck epoch (guard ends on unwind;
  asserted via `catch_unwind` + `in_flight() == 0`).
- `with_compute_queue` (Vulkan) is itself a tracked scope: in-flight
  during, zero after.
- A fresh device is quiescent (pre-existing tests keep this honest).
- `prove_quiescent()` inside a scope returns `None`; outside, `Some`.

Phase 2 is verified by construction on paper only — no code, no tests,
by design.

## Alternatives considered (whole-ADR)

- **Guard-only ADR, transfer later as 0006+** — rejected: the guard's
  protocol-membership choice (scopes join the submission protocol)
  exists *because* transfer barriers ride submission boundaries;
  splitting the ADRs would hide that dependency and risk a guard frozen
  against the wrong shape.
- **Full implementation now (guard + transfer primitives)** — rejected:
  violates ADR 0001's reversibility clause (no consumer, no surface);
  the 2026-09-02 rabbit hole was expensive precisely because it
  designed ahead of consumers.
