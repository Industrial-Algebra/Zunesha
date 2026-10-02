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
| **Borsalino** | Intended compute consumer (migration planned, not yet landed). Will hold a `zunesha::Device` and dispatch compute on `queues().compute`; today it still owns its device internally. |
| **Goldenweek** | Intended graphics consumer (migration planned). Will require `queues().has_graphics()` and render against the surface. |
| **Baedeker** | WASM host (planned). Creates one Zunesha device, hands it to both the compute and graphics host modules. |
| **Miriami** | Trans-graphical framework (future). Lowers its geometric projection onto Goldenweek, which sits on Zunesha. |

Until the consumer migrations land, Zunesha's consumers are examples and
tests; the ecosystem diagram above is the *target* architecture. The zero-copy
compute→render interop story becomes real when Borsalino and Goldenweek both
stand on the same device — that work is on the roadmap, not in this release.

## Buffer ownership

`zunesha::Buffer` is the shared primitive. `Borsalino::GpuBuffer` and
`Goldenweek::GpuBuffer` both wrap it — a buffer allocated for compute *is* the
memory a render pipeline binds. No staging copies, no format negotiation
between siblings.
