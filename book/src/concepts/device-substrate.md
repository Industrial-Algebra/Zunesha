# The Device Substrate

Zunesha exists because two IA libraries — Borsalino (compute) and Goldenweek
(graphics) — need the *same* GPU device for zero-copy interop, and neither
should own it.

```
                 ┌─────────────┐
                 │   Miriami   │
                 └──────┬──────┘
                        │
              ┌─────────┴─────────┐
              │                   │
       ┌──────┴──────┐     ┌──────┴──────┐
       │  Borsalino  │     │  Goldenweek │
       │  (compute)  │     │  (graphics) │
       └──────┬──────┘     └──────┬──────┘
              │                   │
              └─────────┬─────────┘
                        │
                 ┌──────┴──────┐
                 │   Zunesha   │  the device, queues, memory, buffers
                 └─────────────┘
```

| Project | Relationship |
|---|---|
| **Goldenweek** | **Live graphics consumer** — depends on Zunesha (`vulkan` feature), borrows a `zunesha::VulkanDevice`, and allocates/wraps `zunesha::Buffer`s. |
| **Borsalino** | Intended compute consumer — migration planned, not yet landed; Borsalino still owns its device internally. |
| **Baedeker** | WASM host (planned). Creates one Zunesha device, hands it to both the compute and graphics host modules. |
| **Miriami** | Trans-graphical framework (future). Lowers its geometric projection onto Goldenweek, which sits on Zunesha. |

Goldenweek's integration is implemented today; Borsalino's is the pending
migration. The zero-copy compute→render interop story — compute output bound
directly as render input on one shared device — becomes real when **Borsalino**
also stands on Zunesha: Goldenweek's half of the seam already exists.

## Buffer ownership

`zunesha::Buffer` is the shared primitive. **Goldenweek wraps it today** —
its buffers are `zunesha::Buffer`s allocated through the borrowed device.
Borsalino's `GpuBuffer` does **not** yet (pending migration): once it does,
a buffer allocated for compute *is* the memory a render pipeline binds —
no staging copies, no format negotiation between siblings.
