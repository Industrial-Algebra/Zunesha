# Installation & Feature Flags

```toml
[dependencies]
zunesha = { version = "0.1", features = ["vulkan"] }
```

| Backend | Platform | Feature | Status |
|---|---|---|---|
| Metal | macOS (Apple Silicon) | `metal` | 🚧 post-v0.1 — raw `objc_msgSend` FFI |
| Vulkan | Linux, Windows | `vulkan` | ✅ ships in v0.1 — raw `ash` FFI |
| Stub | Any | (none) | ✅ `NoDeviceStub` — safe fallback |

v0.1 ships the `Device` trait, the buffer/queue/quiescence types, the ported
epoch tracker, the complete **Vulkan** backend, and a `NoDeviceStub`. Metal
follows post-v0.1. Both backends hand-roll their FFI (no
`wgpu`) to match the Borsalino/Goldenweek lineage — auditability over
convenience.

## Feature gates are additive

Every feature gate is additive-only per IA coding standards: the crate builds
and tests with no features (stub path), and each backend feature layers on
without touching the core types.
