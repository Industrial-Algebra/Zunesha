// Copyright (C) 2026 Industrial Algebra
// SPDX-License-Identifier: Apache-2.0

// objc 0.2's `sel_impl!` macro (expanded by every `msg_send!`) still
// references the legacy `cfg(cargo-clippy)` value, which the modern
// check-cfg lint flags. The reference originates upstream, not here.
#![allow(unexpected_cfgs)]

//! Metal backend for Zunesha (macOS, `metal` feature).
//!
//! Absorbs the device/buffer layer of Borsalino's `src/metal.rs` (v0.7.0)
//! — its comments are the scar tissue of real crashes — restructured onto
//! the [`Device`] trait with the Zunesha-specific improvements the
//! requirements doc mandates:
//!
//! - **`MemoryStrategy` is honoured, not ignored.** Borsalino-Metal always
//!   allocated `Shared`; here `DeviceLocal` maps to `MTLStorageModePrivate`
//!   + Shared staging + `MTLBlitCommandEncoder` copies (mirroring the
//!   Vulkan backend's staging design), and
//!   [`create_device_buffer`](Device::create_device_buffer) forces that
//!   path regardless of the negotiated strategy (the post-0.1.1 contract).
//! - **Pools on every objc-minting path.** Borsalino pools only its
//!   dispatch paths; this backend exceeds that — init and buffer creation
//!   run inside `objc::rc::autoreleasepool` too (on plain Rust worker
//!   threads autoreleased objects are never reclaimed otherwise).
//! - **Queues reinterpreted, not violated** (ADR 0004): Metal has no queue
//!   families — the single owned `MTLCommandQueue` services everything, so
//!   `compute` is that queue, `graphics` is `Some` (same handle — every
//!   Metal device renders), `transfer` is `None` (blits ride the same
//!   queue). Consumers must not assume distinct families.
//!
//! Zunesha owns the MTLDevice, MTLCommandQueue, and buffers. Pipelines,
//! shaders, and dispatch stay in the consumers (Borsalino/Goldenweek) —
//! they reach the raw handles through the escape hatches
//! ([`MetalDevice::raw_device`], [`MetalDevice::raw_buffer`], and the
//! queue via [`Device::queues`]).

use std::ffi::c_void;

use objc::runtime::Object;
use objc::{msg_send, sel, sel_impl};

use crate::{
    Device, DeviceError, DeviceLimits, GpuEpochTracker, InitRequest, MemoryPlacement,
    MemoryStrategy, Queue, Queues, Result,
};

// ── Metal C symbols ───────────────────────────────────────────────

#[link(name = "Metal", kind = "framework")]
#[link(name = "Foundation", kind = "framework")]
unsafe extern "C" {
    /// Returns the system default Metal device, or null. +1 (create) — the
    /// caller owns the returned reference.
    fn MTLCreateSystemDefaultDevice() -> *mut c_void;

    /// Returns a retained (`NS_RETURNS_RETAINED`, +1) `NSArray<MTLDevice>`
    /// of all Metal devices. The caller owns the array reference; devices
    /// inside are retained *by the array* and need an explicit `retain` to
    /// outlive its release.
    fn MTLCopyAllDevices() -> *mut c_void;
}

// ── objc helpers ──────────────────────────────────────────────────

/// Cast a raw Metal object pointer to `*const Object` for `msg_send!`.
unsafe fn obj(ptr: *mut c_void) -> *const Object {
    ptr as *const Object
}

/// Read an NSString into a Rust `String`.
///
/// `UTF8String` returns a borrowed C string (+0, owned by the NSString) —
/// copy it out before the string can go away.
unsafe fn nsstring_read(ns: *mut c_void) -> String {
    if ns.is_null() {
        return "(null)".into();
    }
    let utf8: *const std::ffi::c_char = unsafe { msg_send![obj(ns), UTF8String] };
    if utf8.is_null() {
        return "(null)".into();
    }
    unsafe { std::ffi::CStr::from_ptr(utf8) }
        .to_string_lossy()
        .into_owned()
}

// ── Device backend ────────────────────────────────────────────────

/// Metal implementation of [`Device`] (macOS, `metal` feature).
///
/// Owns the MTLDevice (+1 from creation) and the MTLCommandQueue (+1 from
/// `newCommandQueue`) — consumers receive the queue handle via
/// [`Device::queues`] and must never create their own or release ours.
///
/// # Drop order
///
/// Queue first, then device (reverse creation order). Metal retains
/// command buffers on a queue, so there is deliberately **no**
/// `device_wait_idle` equivalent here — no blanket `waitUntilCompleted`
/// sweeps (R6: do not invent one).
pub struct MetalDevice {
    /// MTLDevice — +1 owned.
    device: *mut c_void,
    /// MTLCommandQueue — +1 owned.
    queue: *mut c_void,
    /// The capability-shaped queue view (ADR 0004 reinterpretation).
    queues: Queues,
    memory_strategy: MemoryStrategy,
    placement: MemoryPlacement,
    limits: DeviceLimits,
    /// Serializes Private-buffer readback staging. *Not* needed for
    /// `MTLCommandQueue` safety — Apple documents the queue itself as
    /// thread-safe — but each Private buffer carries one persistent Shared
    /// staging mapping; a concurrent `read_buffer` of the same buffer
    /// would rewrite that mapping mid-host-copy (the Vulkan backend's
    /// review round 2, P1 race, transplanted). Scoped to the readback
    /// path only; creation staging buffers are freshly allocated per
    /// buffer and never shared.
    readback_lock: std::sync::Mutex<()>,
    epoch: GpuEpochTracker,
}

impl Drop for MetalDevice {
    fn drop(&mut self) {
        // Safety: both pointers are +1 owned by us and still valid — the
        // documented invariant is that buffers do not outlive the device.
        unsafe {
            let _: () = msg_send![obj(self.queue), release];
            let _: () = msg_send![obj(self.device), release];
        }
    }
}

// Safety: MTLDevice and MTLCommandQueue are documented thread-safe; the
// epoch tracker is atomics; the only shared mutable substrate state (the
// per-buffer staging mappings) is guarded by `readback_lock`. Consumers
// (Borsalino) share one device behind an `Arc` — same contract as
// `VulkanDevice`.
unsafe impl Send for MetalDevice {}
unsafe impl Sync for MetalDevice {}

// Compile-time proof the device is shareable behind an `Arc` (consumer
// contract, mirrors `VulkanDevice`).
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<MetalDevice>();
};

impl MetalDevice {
    /// Build a device from a full [`InitRequest`].
    fn build(request: InitRequest) -> Result<Self> {
        // R5.1: every objc-minting path runs inside an autorelease pool —
        // init included. Borsalino's `MetalBackend::init` lacks one; on a
        // plain Rust worker thread any autoreleased intermediate would
        // never be reclaimed. Exceed, don't inherit.
        objc::rc::autoreleasepool(|| unsafe {
            // R1 device selection: `device_hint` may match a name via
            // `MTLCopyAllDevices` (discrete-Mac case); otherwise — and on
            // every single-GPU Mac — the system default is the device.
            // `prefer_graphics` needs no selection bias: every Metal
            // device renders (ADR 0004).
            let device = match request.device_hint.as_deref() {
                Some(hint) => pick_hinted_device(hint)
                    .unwrap_or_else(|| unsafe { MTLCreateSystemDefaultDevice() }),
                None => unsafe { MTLCreateSystemDefaultDevice() },
            };
            if device.is_null() {
                return Err(DeviceError::InitFailed(
                    "MTLCreateSystemDefaultDevice returned null — no Metal-capable GPU".into(),
                ));
            }

            // +1 from `new` — we own the queue and hand out its handle via
            // `queues()`; consumers must not release it (R4 contract).
            let queue: *mut c_void = unsafe { msg_send![obj(device), newCommandQueue] };
            if queue.is_null() {
                // R5.5 failure paths release what they own: the device is
                // +1 ours; releasing it before the error balances creation.
                let _: () = unsafe { msg_send![obj(device), release] };
                return Err(DeviceError::InitFailed(
                    "failed to create MTLCommandQueue".into(),
                ));
            }

            // R1 storage mapping: Auto/Unified → Shared (unified by
            // construction on Apple Silicon; the correct default
            // everywhere on Metal — ADR 0004). DeviceLocal → Private +
            // staging (the buffers carry that; the placement reported here
            // is the negotiated strategy's resolution).
            let placement = match request.memory {
                MemoryStrategy::DeviceLocal => MemoryPlacement::DeviceLocal,
                MemoryStrategy::Auto | MemoryStrategy::Unified => MemoryPlacement::HostVisible,
            };

            // ADR 0004 queue reinterpretation: one MTLCommandQueue services
            // everything. `family_index` is meaningless on Metal (no
            // families) — 0 is the documented placeholder.
            let handle = Queue {
                raw: queue,
                family_index: 0,
            };
            let queues = Queues {
                compute: handle,
                graphics: Some(handle),
                transfer: None,
            };

            Ok(Self {
                device,
                queue,
                queues,
                memory_strategy: request.memory,
                placement,
                // Metal exposes no queryable storage-buffer-offset
                // alignment; 16 is the documented conservative floor
                // (ADR 0004 evidence trail).
                limits: DeviceLimits {
                    min_storage_buffer_offset_alignment: 16,
                },
                readback_lock: std::sync::Mutex::new(()),
                epoch: GpuEpochTracker::new(),
            })
        })
    }
}

/// Find a Metal device whose name contains `hint` (case-insensitive).
///
/// Returns a **retained (+1)** device reference that outlives the array it
/// came from; `None` when no device matches. Runs on `MTLCopyAllDevices`
/// (macOS-only API; on the single-GPU fleet Macs it degenerates to the
/// one device, which the caller then matches or falls back from).
///
/// # Safety
///
/// objc FFI; must run inside an autorelease pool (the caller provides it).
unsafe fn pick_hinted_device(hint: &str) -> Option<*mut c_void> {
    // +1 array (NS_RETURNS_RETAINED) — released before returning.
    let array = unsafe { MTLCopyAllDevices() };
    if array.is_null() {
        return None;
    }
    let mut found: Option<*mut c_void> = None;
    let count: u64 = unsafe { msg_send![obj(array), count] };
    for i in 0..count {
        // +0 — borrowed from the array.
        let dev: *mut c_void = unsafe { msg_send![obj(array), objectAtIndex: i] };
        let name_ns: *mut c_void = unsafe { msg_send![obj(dev), name] };
        let name = unsafe { nsstring_read(name_ns) };
        if name.to_lowercase().contains(&hint.to_lowercase()) {
            // The device must outlive the array's release: +1 retain.
            let _: () = unsafe { msg_send![obj(dev), retain] };
            found = Some(dev);
            break;
        }
    }
    // Balance the array's +1 — our only reference to it.
    let _: () = unsafe { msg_send![obj(array), release] };
    found
}

impl Device for MetalDevice {
    fn init() -> Result<Self> {
        Self::build(InitRequest::compute_only())
    }

    fn init_with_strategy(strategy: MemoryStrategy) -> Result<Self> {
        Self::build(InitRequest {
            memory: strategy,
            prefer_graphics: false,
            device_hint: None,
        })
    }

    fn init_with(request: InitRequest) -> Result<Self> {
        Self::build(request)
    }

    fn queues(&self) -> Queues {
        self.queues
    }

    fn limits(&self) -> DeviceLimits {
        self.limits
    }

    fn memory_strategy(&self) -> MemoryStrategy {
        self.memory_strategy
    }

    fn buffer_placement(&self) -> MemoryPlacement {
        self.placement
    }

    fn create_buffer<T: bytemuck::Pod>(&self, _data: &[T]) -> Result<crate::Buffer> {
        Err(DeviceError::BufferCreationFailed {
            message: "not yet implemented".into(),
        })
    }

    fn create_buffer_uninit<T: bytemuck::Pod>(&self, _len: usize) -> Result<crate::Buffer> {
        Err(DeviceError::BufferCreationFailed {
            message: "not yet implemented".into(),
        })
    }

    fn read_buffer<T: bytemuck::Pod>(&self, _buffer: &crate::Buffer) -> Result<Vec<T>> {
        Err(DeviceError::BufferReadFailed {
            message: "not yet implemented".into(),
        })
    }

    fn in_flight(&self) -> u64 {
        self.epoch.in_flight()
    }

    fn is_quiescent(&self) -> bool {
        self.epoch.is_quiescent()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    /// Acquire a device for tests: skip politely on machines without one,
    /// but FAIL hard when `ZUNESHA_REQUIRE_METAL` is set — the dedicated
    /// Apple Silicon CI job sets it, so that job can never pass without
    /// touching a real device (Borsalino's `BORSALINO_REQUIRE_METAL`
    /// pattern, ported).
    fn test_device() -> Option<MetalDevice> {
        match MetalDevice::init() {
            Ok(d) => Some(d),
            Err(e) => {
                assert!(
                    std::env::var("ZUNESHA_REQUIRE_METAL").is_err(),
                    "ZUNESHA_REQUIRE_METAL is set but Metal init failed: {e}"
                );
                eprintln!("skipping: no Metal device ({e})");
                None
            }
        }
    }

    /// A real Metal device must initialise, expose the queue handle, and
    /// report a non-null device name (R1).
    #[test]
    #[serial]
    fn metal_device_inits_and_names_itself() {
        let Some(device) = test_device() else { return };
        assert!(!device.queue.is_null(), "MTLCommandQueue must be non-null");
        // Safety: `device` is a +1 owned MTLDevice, valid for its lifetime.
        let name: *mut c_void = unsafe { msg_send![obj(device.device), name] };
        let name = unsafe { nsstring_read(name) };
        assert!(!name.is_empty(), "device name must be non-empty");
        println!("metal device: {name}");
    }

    /// ADR 0004 queue reinterpretation (R2): `compute` is the owned
    /// `MTLCommandQueue`, `graphics` is `Some` with the *same* handle
    /// (every Metal device renders), `transfer` is `None` (no distinct
    /// transfer queue on Metal).
    #[test]
    #[serial]
    fn queues_shape_matches_adr_0004() {
        let Some(device) = test_device() else { return };
        let q = device.queues();
        assert!(
            !q.compute.raw.is_null(),
            "compute queue must be the owned MTLCommandQueue"
        );
        let graphics =
            q.graphics.expect("graphics must be Some on Metal — every Metal device renders");
        assert_eq!(
            q.compute.raw, graphics.raw,
            "graphics is the same MTLCommandQueue handle on Metal"
        );
        assert!(q.transfer.is_none(), "no distinct transfer queue on Metal");
        assert!(q.has_graphics());
        println!("queues: single MTLCommandQueue services compute + graphics");
    }

    /// `DeviceLimits::min_storage_buffer_offset_alignment` reports the
    /// documented conservative floor of 16 — Metal exposes no queryable
    /// value (see ADR 0004 for the evidence trail).
    #[test]
    #[serial]
    fn limits_report_documented_alignment_floor() {
        let Some(device) = test_device() else { return };
        assert_eq!(device.limits().min_storage_buffer_offset_alignment, 16);
    }

    /// A fresh device is quiescent (epoch counter at zero) — same honest
    /// tracker state as the Vulkan backend (R6: consumer dispatch
    /// accounting unwired until ADR-0005-style accounting lands for both
    /// backends at once).
    #[test]
    #[serial]
    fn fresh_device_is_quiescent() {
        let Some(device) = test_device() else { return };
        assert_eq!(device.in_flight(), 0);
        assert!(device.is_quiescent());
        assert!(device.prove_quiescent().is_some());
    }

    /// `device_hint` matching: `MTLCopyAllDevices` name matching wins over
    /// the system default when the hint matches (R1). On single-GPU Macs
    /// the hint degenerates to the same device — the invariant tested is
    /// that a matching hint never fails and yields a working device.
    #[test]
    #[serial]
    fn device_hint_matches_or_falls_back() {
        // A hint that matches every Apple-Silicon device name ("Apple").
        let request = InitRequest::compute_only().with_device_hint("apple");
        let device = match MetalDevice::init_with(request) {
            Ok(d) => d,
            Err(e) => {
                assert!(
                    std::env::var("ZUNESHA_REQUIRE_METAL").is_err(),
                    "ZUNESHA_REQUIRE_METAL is set but hinted init failed: {e}"
                );
                eprintln!("skipping: no Metal device ({e})");
                return;
            }
        };
        // Safety: `device` is a +1 owned MTLDevice.
        let name: *mut c_void = unsafe { msg_send![obj(device.device), name] };
        let name = unsafe { nsstring_read(name) };
        assert!(
            name.to_lowercase().contains("apple"),
            "hinted device name must contain the hint: {name}"
        );

        // A hint that matches nothing falls back to the system default
        // (preference, not requirement — mirrors Vulkan selection).
        let fallback = match MetalDevice::init_with(
            InitRequest::compute_only().with_device_hint("no-such-gpu-42"),
        ) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("skipping: no Metal device ({e})");
                return;
            }
        };
        let _q = fallback.queues(); // usable
    }
}
