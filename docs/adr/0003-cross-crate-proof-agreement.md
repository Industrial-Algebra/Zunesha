# ADR 0003 — Cross-crate proof agreement

- **Date:** 2026-08-12
- **Status:** Accepted

## Context

With three crates sharing one device (Zunesha, Borsalino, Goldenweek), the
question arises: *what does each crate prove, and what does it leave to the
others?* Industrial Algebra Design Principle 3 demands that proofs agree across
crate boundaries — a property established in one crate must not be silently
re-litigated or contradicted in another.

Two failure modes must be avoided:

- **Gaps** — a property nobody proves (e.g. "buffers don't outlive the device"
  assumed everywhere, checked nowhere).
- **Overlaps** — two crates both proving the same thing with subtly different
  definitions, producing false confidence and conflicting diagnostics.

## Decision

Each crate owns one, non-overlapping verification responsibility:

| Crate | Responsibility | What it proves |
|---|---|---|
| **Zunesha** | **Structural device & buffer safety** | Buffers do not outlive the device; the epoch/GC tracker accounts for *all* in-flight GPU work (compute + graphics); queues match the hardware's actual capabilities. |
| **Borsalino** | **Numerical exactness** | Compute results are bit-for-bit correct for a given kernel + input. |
| **Goldenweek** | **Structural graphics correctness** | A render pipeline is valid before use; a frame is acquired before it is drawn into and drawn before it is presented; the frame lifecycle is not violated. |

Key points:

- **Zunesha owns the epoch tracker**, not the consumers. This is a strict
  improvement over Borsalino's prior self-contained tracker, because one tracker
  now observes both compute and graphics traffic. A consumer cannot "forget" to
  account for the other's work.
- **Numerical correctness is *not* Zunesha's job.** Zunesha guarantees the buffer
  is valid and the transfer happened; whether the bytes it ferries are the right
  bytes is Borsalino's proof.
- **Goldenweek does not re-prove buffer safety.** It trusts Zunesha's buffer
  handle and layers pipeline/frame *lifecycle* correctness on top.

## Consequences

**Positive**

- No proof is duplicated with conflicting definitions; each property has exactly
  one owner.
- No structural property is unowned — buffer lifetime, work accounting, queue
  capabilities, pipeline validity, and frame lifecycle are each claimed.
- The split maps cleanly onto each crate's natural boundary (device vs. compute
  vs. graphics), so proofs live next to the code they govern.

**Negative**

- Cross-crate trust is explicit: Goldenweek trusts Zunesha's buffer, Borsalino
  trusts Zunesha's device. A Zunesha regression can violate a property that a
  consumer *assumed* — mitigated by Zunesha's own tests pinning the structural
  invariants.
- Numerical regressions caused by a bad *transfer* (Zunesha's domain) could be
  mis-attributed to Borsalino. The boundary is "Zunesha proves the transfer
  completed correctly; Borsalino proves the result is numerically right" —
  diagnosed by reproducing with a known-good transfer path.

## Relationship to the Borsalino refactor

When Borsalino adopts Zunesha, Borsalino's own epoch tracker is **deleted** and
its in-flight-work proofs delegate to Zunesha's. Borsalino keeps only its
numerical proofs. This is the single largest behavioral change in the refactor
and is the reason this ADR exists.
