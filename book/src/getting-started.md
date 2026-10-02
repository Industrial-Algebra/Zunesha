# Getting Started

Add Zunesha with the backend feature for your platform:

```toml
[dependencies]
zunesha = { version = "0.1", features = ["vulkan"] }   # Linux / Windows
# zunesha = { version = "0.1", features = ["metal"] }  # macOS (post-v0.1)
```

## Baseline use

```rust
use zunesha::{Device, InitRequest};

// Baseline: best compute device (Borsalino-safe on compute-only hardware).
let device = zunesha::init()?;

// Or prefer a graphics-capable device (Goldenweek on a multi-GPU box).
let device = zunesha::init_with(InitRequest::prefer_graphics())?;

// Capability-driven: compute always present, graphics optional.
if device.queues().has_graphics() {
    // Goldenweek can initialise against this device.
}

// Buffers are the shared primitive — compute writes, graphics reads.
let buf = device.create_buffer(&[1.0f32, 2.0, 3.0, 4.0])?;

// GC safety protocol: once consumers wire dispatch accounting into the
// device tracker, one quiescence query certifies no GPU work touches host
// memory. v0.1 caveat: no production path increments the tracker yet —
// see Concepts → Epoch Tracking before relying on this.
if device.is_quiescent() {
    gc_compact();
}
```

## Running the examples

```bash
# Pure-CPU: works everywhere, exercises the epoch tracker + stub.
cargo run --example quiescence

# GPU-backed: require the vulkan feature and a real device.
cargo run --features vulkan --example device-info
cargo run --features vulkan --example buffer-roundtrip
```

See the [Examples](./examples/quiescence.md) section for walkthroughs.
