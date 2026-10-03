# Device Info

`examples/device-info.rs` enumerates the devices a backend can see and prints
their capabilities:

```bash
cargo run --features vulkan --example device-info
```

Typical output on a multi-GPU box: each physical device with its queue
families (compute present, graphics present or absent) and memory properties.
Use it to sanity-check what `init()` vs `init_with(InitRequest::prefer_graphics())`
would select on a given machine.
