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
| **Borsalino** | Compute consumer. Holds a `zunesha::Device`, dispatches compute on `queues().compute`. |
| **Goldenweek** | Graphics consumer. Requires `queues().has_graphics()`, renders against the surface. |
| **Baedeker** | WASM host. Creates one Zunesha device, hands it to both the compute and graphics host modules. |
| **Miriami** | Trans-graphical framework (future). Lowers its geometric projection onto Goldenweek, which sits on Zunesha. |

## Buffer ownership

`zunesha::Buffer` is the shared primitive. `Borsalino::GpuBuffer` and
`Goldenweek::GpuBuffer` both wrap it — a buffer allocated for compute *is* the
memory a render pipeline binds. No staging copies, no format negotiation
between siblings.
