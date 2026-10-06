# Buffer Roundtrip

`examples/buffer-roundtrip.rs` is the substrate's smoke test on real
hardware — create a buffer from host data, read it back, verify contents:

```bash
cargo run --features vulkan --example buffer-roundtrip
```

It demonstrates the round-trip that both consumers build on: Borsalino binds
the same buffer as a kernel argument; Goldenweek binds it as render input.
If the roundtrip is correct, the zero-copy interop story holds.
