# Decision Records

The architecture decision records live in the repository under
`docs/adr/` and are included here verbatim:

- [ADR 0001 — Shared device substrate](./adr/0001-shared-device-substrate.md)
  — why the device moves down into a shared library, and what stays in the
  consumers.
- [ADR 0002 — Capability-driven queues](./adr/0002-capability-driven-queues.md)
  — compute always, graphics optional; the GB10 story.
- [ADR 0003 — Cross-crate proof agreement](./adr/0003-cross-crate-proof-agreement.md)
  — how quiescence proofs stay meaningful across crate boundaries.
- [ADR 0004 — Metal backend](./adr/0004-metal-backend.md)
  — queue reinterpretation, storage-mode mapping, the 16-byte alignment
  floor, and the objc discipline absorbed from Borsalino's crash history.
- [ADR 0005 — Dispatch-scope guard and phased cross-queue
  ownership](./adr/0005-dispatch-guard-and-cross-queue.md)
  — the consumer dispatch epoch lands on both backends; cross-queue
  ownership transfer is designed (ledger + release/acquire) but
  deliberately not landed until a graphics consumer exists.
