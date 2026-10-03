# Device Initialisation

Two constructors cover the two consumption patterns:

```rust
// Baseline: best compute device. Borsalino-safe on compute-only hardware.
let device = zunesha::init()?;

// Prefer graphics: picks a graphics-capable device when one exists,
// falling back to the baseline. Goldenweek's entry point.
let device = zunesha::init_with(InitRequest::prefer_graphics())?;
```

## Test-device pinning

CI and test suites can pin a specific device via environment variables
(e.g. `ZUNESHA_TEST_DEVICE`) so GPU tests run against a deterministic target
instead of whatever enumeration order happens to produce.

## Verification backends

Tests run against the stub by default (pure CPU, CI-safe) and against real
hardware on GPU runners. The `prove_quiescent` path has a first-class
fake in the test harness so epoch semantics can be verified without a GPU.
