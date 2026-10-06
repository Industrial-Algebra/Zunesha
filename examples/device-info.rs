// Copyright (C) 2026 Industrial Algebra
// SPDX-License-Identifier: Apache-2.0

// Mirrors the cfg of `zunesha::vulkan` (not macOS).
#![cfg(all(feature = "vulkan", not(target_os = "macos")))]

//! Print what the substrate resolved on this host: the queue shape
//! (capability-driven, ADR 0002), limits, memory strategy, and the
//! effective buffer placement.
//!
//! ```sh
//! cargo run --features vulkan --example device-info
//! # pin a device (see README §Testing):
//! ZUNESHA_TEST_DEVICE=intel cargo run --features vulkan --example device-info
//! ```

use std::process::ExitCode;

use zunesha::{Device, InitRequest};

fn main() -> ExitCode {
    let hint = std::env::var("ZUNESHA_TEST_DEVICE").ok();
    let mut request = InitRequest::prefer_graphics();
    if let Some(h) = hint.filter(|h| !h.is_empty()) {
        request = request.with_device_hint(h);
    }
    let device = match zunesha::init_with(request) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("device-info: no Vulkan device ({e}) — nothing to report on this host");
            return ExitCode::SUCCESS;
        }
    };

    let queues = device.queues();
    println!("queues (capability-driven, policy A):");
    println!("  compute : family {}", queues.compute.family_index,);
    match queues.graphics {
        Some(q) => println!(
            "  graphics: family {} (present — rendering consumers can init)",
            q.family_index
        ),
        None => println!("  graphics: absent (compute-only hardware — Borsalino's GB10 case)"),
    }
    match queues.transfer {
        Some(q) => println!(
            "  transfer: family {} (dedicated DMA family)",
            q.family_index
        ),
        None => println!("  transfer: None (unified family carries it)"),
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
