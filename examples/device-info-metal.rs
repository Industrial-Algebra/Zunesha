// Copyright (C) 2026 Industrial Algebra
// SPDX-License-Identifier: Apache-2.0

// The metal backend is macOS-only. Gate the body — but keep a
// compiling `main` on every target, so `--features metal --all-targets`
// builds everywhere (review r1 P2: a crate-level cfg removed `main`
// entirely on Linux and broke the build).
#![cfg_attr(
    not(all(feature = "metal", target_os = "macos")),
    allow(unused_imports)
)]

//! Print what the Metal substrate resolved on this Mac: the ADR-0004
//! queue shape, limits, memory strategy, and effective buffer placement.
//!
//! ```sh
//! cargo run --features metal --example device-info-metal
//! ```

use std::process::ExitCode;

use zunesha::{Device, InitRequest};

#[cfg(all(feature = "metal", target_os = "macos"))]
fn main() -> ExitCode {
    let mut request = InitRequest::prefer_graphics();
    if let Ok(hint) = std::env::var("ZUNESHA_TEST_DEVICE") {
        if !hint.is_empty() {
            request = request.with_device_hint(hint);
        }
    }
    let device = match zunesha::init_with(request) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("device-info-metal: no Metal device ({e}) — nothing to report on this host");
            return ExitCode::SUCCESS;
        }
    };

    let queues = device.queues();
    println!("queues (ADR 0004 reinterpretation):");
    println!("  compute : the owned MTLCommandQueue handle");
    if queues.graphics.is_some() {
        println!("  graphics: Some — the same MTLCommandQueue (every Metal device renders)");
    } else {
        println!("  graphics: absent");
    }
    if queues.transfer.is_some() {
        println!("  transfer: Some");
    } else {
        println!("  transfer: None (blits ride the same queue)");
    }

    println!(
        "limits: min_storage_buffer_offset_alignment = {}",
        device.limits().min_storage_buffer_offset_alignment
    );
    println!("memory strategy requested: {:?}", device.memory_strategy());
    println!(
        "effective buffer placement: {:?}",
        device.buffer_placement()
    );
    ExitCode::SUCCESS
}

#[cfg(not(all(feature = "metal", target_os = "macos")))]
fn main() {
    eprintln!(
        "device-info-metal: the metal backend is macOS-only; this is the non-macOS stub build."
    );
}
