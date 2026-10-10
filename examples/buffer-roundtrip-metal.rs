// Copyright (C) 2026 Industrial Algebra
// SPDX-License-Identifier: Apache-2.0

// Mirrors the cfg of `zunesha::metal` (macOS + metal feature).
#![cfg(all(feature = "metal", target_os = "macos"))]

//! Round-trip data through buffers on both storage modes — the Metal
//! analogue of `examples/buffer-roundtrip.rs`: Shared (unified) writes
//! and reads the direct mapping; Private + staging exercises the blit
//! path in both directions. Transparent to the consumer.
//!
//! ```sh
//! cargo run --features metal --example buffer-roundtrip-metal
//! ```

use std::process::ExitCode;

use zunesha::{Device, MemoryStrategy};

fn main() -> ExitCode {
    // A pattern whose corruption is easy to spot by eye.
    let data: Vec<u8> = (0..64_u8)
        .map(|i| i.wrapping_mul(7).wrapping_add(3))
        .collect();

    for strategy in [MemoryStrategy::Unified, MemoryStrategy::DeviceLocal] {
        let device = match zunesha::init_with_strategy(strategy) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("buffer-roundtrip-metal: no Metal device ({e}) — skipping {strategy:?}");
                return ExitCode::SUCCESS;
            }
        };
        let buffer = match device.create_buffer(&data) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("buffer-roundtrip-metal: create_buffer({strategy:?}) failed: {e}");
                return ExitCode::FAILURE;
            }
        };
        let read: Vec<u8> = match device.read_buffer(&buffer) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("buffer-roundtrip-metal: read_buffer({strategy:?}) failed: {e}");
                return ExitCode::FAILURE;
            }
        };
        // The read may be padded to the buffer's aligned size; the prefix
        // must match exactly.
        if read.len() < data.len() || &read[..data.len()] != data.as_slice() {
            eprintln!(
                "buffer-roundtrip-metal: {strategy:?} verification FAILED — first bytes {:?}",
                &read[..data.len().min(read.len())]
            );
            return ExitCode::FAILURE;
        }
        println!(
            "buffer-roundtrip-metal: {:?} ✓ (placement {:?}, {} bytes verified)",
            strategy,
            device.buffer_placement(),
            data.len()
        );
    }
    ExitCode::SUCCESS
}
