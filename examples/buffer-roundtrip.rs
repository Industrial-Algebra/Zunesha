// Copyright (C) 2026 Industrial Algebra
// SPDX-License-Identifier: Apache-2.0

// Mirrors the cfg of `zunesha::vulkan` (not macOS).
#![cfg(all(feature = "vulkan", not(target_os = "macos")))]

//! Round-trip data through buffers on both memory strategies: write a
//! recognizable pattern, read it back, verify byte-for-byte. Demonstrates
//! the two placement paths a Zunesha device offers (host-visible direct
//! mapping vs device-local + staging transfers) and that they are
//! transparent to the consumer.
//!
//! ```sh
//! cargo run --features vulkan --example buffer-roundtrip
//! ```

use std::process::ExitCode;

use zunesha::{Device, MemoryStrategy};

fn main() -> ExitCode {
    // A pattern whose corruption is easy to spot by eye: i, i+1, i+2, ...
    let data: Vec<u8> = (0..64_u8)
        .map(|i| i.wrapping_mul(7).wrapping_add(3))
        .collect();

    for strategy in [MemoryStrategy::Unified, MemoryStrategy::DeviceLocal] {
        let device = match zunesha::init_with_strategy(strategy) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("buffer-roundtrip: no Vulkan device ({e}) — skipping {strategy:?}");
                return ExitCode::SUCCESS;
            }
        };
        let buffer = match device.create_buffer(&data) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("buffer-roundtrip: create_buffer({strategy:?}) failed: {e}");
                return ExitCode::FAILURE;
            }
        };
        let read: Vec<u8> = match device.read_buffer(&buffer) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("buffer-roundtrip: read_buffer({strategy:?}) failed: {e}");
                return ExitCode::FAILURE;
            }
        };
        // The read may be padded to the buffer's aligned size; the prefix
        // must match exactly.
        if read.len() < data.len() || &read[..data.len()] != data.as_slice() {
            eprintln!(
                "buffer-roundtrip: {strategy:?} verification FAILED — first bytes {:?}",
                &read[..data.len().min(read.len())]
            );
            return ExitCode::FAILURE;
        }
        println!(
            "buffer-roundtrip: {:?} ✓ (placement {:?}, {} bytes verified)",
            strategy,
            device.buffer_placement(),
            data.len()
        );
    }
    ExitCode::SUCCESS
}
