# API Overview

The full API reference lives on
[docs.rs/zunesha](https://docs.rs/zunesha). The load-bearing types:

| Type | Role |
|---|---|
| [`Device`] | The substrate trait: queues, buffers, quiescence. |
| [`Queues`] | Capability view: `compute` always; `graphics`/`transfer` optional. |
| [`Buffer`] | The shared memory primitive both consumers wrap. |
| [`GpuEpochTracker`] | Dispatch counting; `prove_quiescent()` for GC safety. |
| [`QuiescenceProof`] | Certificate that all dispatches were observed complete (past instant). |
| [`InitRequest`] | Selection policy for `init_with`. |
| `NoDeviceStub` | Safe fallback backend (no hardware required). |
