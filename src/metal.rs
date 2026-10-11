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
//!   with Shared staging and `MTLBlitCommandEncoder` copies (mirroring the
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

/// `MTLResourceOptions` storage-mode bits (from `MTLResource.h`:
/// storage mode shifted left by `MTLResourceStorageModeShift` = 4).
/// `options: 0` **is** Shared — the same fact Borsalino's
/// `STORAGE_MODE_SHARED = 0` encodes.
const STORAGE_MODE_SHARED: u64 = 0;
/// `MTLStorageModePrivate` (2) << 4 — CPU-inaccessible; reached through
/// a Shared staging buffer + blit copies (mirrors the Vulkan backend's
/// device-local + staging design).
const STORAGE_MODE_PRIVATE: u64 = 32;

/// Round `value` up to a multiple of `alignment` (power of two), or
/// `None` on overflow. Checked wherever allocation sizes are computed
/// (review r1 P1-3: the unchecked variant panicked in debug builds and
/// silently wrapped in release).
fn checked_align_up(value: usize, alignment: usize) -> Option<usize> {
    if alignment == 0 {
        Some(value)
    } else {
        Some((value.checked_add(alignment - 1)?) & !(alignment - 1))
    }
}

/// Copy `byte_len` logical bytes out of a Metal `contents()` mapping into
/// a properly aligned, owned `Vec<T>` (review r1 P1-1).
///
/// `Pod` types may require more alignment than Metal guarantees for its
/// mappings, so a typed slice over the mapping is unsound — this reads the
/// mapping as raw bytes (align 1, always valid) and copy-constructs each
/// element into `MaybeUninit` slots that are aligned by construction.
/// Preserves floor semantics: `byte_len / size_of::<T>()` elements.
fn collect_aligned<T: bytemuck::Pod>(mapped: *const c_void, byte_len: usize) -> Vec<T> {
    let elem = std::mem::size_of::<T>();
    if elem == 0 {
        return Vec::new();
    }
    let count = byte_len / elem;
    let mut out = Vec::with_capacity(count);
    let src = mapped as *const u8;
    for i in 0..count {
        // Safety: element i's source bytes are in bounds (byte_len ≥
        // (i+1)·elem by the floor division, and the allocation is
        // alignment-floored ≥ byte_len); `Pod` implies `AnyBitPattern`,
        // so any bit pattern is a valid `T` and `assume_init` is sound.
        let mut slot = std::mem::MaybeUninit::<T>::uninit();
        unsafe {
            std::ptr::copy_nonoverlapping(src.add(i * elem), slot.as_mut_ptr() as *mut u8, elem);
            out.push(slot.assume_init());
        }
    }
    out
}

// ── Buffer inner type ─────────────────────────────────────────────

/// Internal state for a Metal buffer, stored behind the opaque
/// `Buffer.raw` pointer (the same shape as `VulkanBufferInner`).
struct MetalBufferInner {
    /// MTLBuffer — +1 owned (`new…` product; R5.2: released exactly once
    /// in [`drop_metal_buffer`]).
    buffer: *mut c_void,
    /// Shared staging MTLBuffer (+1 owned) for Private-storage buffers;
    /// `None` for Shared buffers.
    staging: Option<*mut c_void>,
    /// `contents()` of the Shared buffer (or of the staging buffer when
    /// Private) — a **borrowed raw pointer, not an objc object** (R5.4):
    /// never released; its lifetime is governed by the owning MTLBuffer.
    mapped: *mut c_void,
    /// Alignment-floored allocation size (staging copies move this many
    /// bytes, mirroring the Vulkan backend's `size` field).
    size: usize,
    /// The MTLDevice that created this buffer (review r1 P1-2). Metal
    /// resources are per-device; `read_buffer` rejects buffers created
    /// by a different `MetalDevice` before touching the staging mapping.
    device: *mut c_void,
    /// Serializes Private readback for **this buffer** (review r1 P1-2):
    /// the device→staging blit and the host copy run under this per-buffer
    /// lock, so every reader of the shared staging mapping contends on
    /// the lock that lives with that mapping — not on a per-device lock a
    /// second device instance would bypass.
    readback_lock: std::sync::Mutex<()>,
}

// Safety: opaque backend handles; all GPU access is serialized through
// the device command stream and the readback lock (see `MetalDevice`).
unsafe impl Send for MetalBufferInner {}
unsafe impl Sync for MetalBufferInner {}

/// Drop function stored in [`crate::Buffer`] — releases the owned
/// MTLBuffer(s) and frees the inner box. Staging released before the
/// device buffer (creation order reversed). Metal retains resources
/// referenced by already-encoded commands, so releasing while older
/// command buffers drain is safe.
pub(super) fn drop_metal_buffer(raw: *mut c_void) {
    if !raw.is_null() {
        // Safety: `raw` was produced by `Box::into_raw` in `buffer_new`.
        unsafe {
            let inner = Box::from_raw(raw as *mut MetalBufferInner);
            if let Some(stg) = inner.staging {
                let _: () = msg_send![obj(stg), release];
            }
            let _: () = msg_send![obj(inner.buffer), release];
        }
    }
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
// per-buffer staging mappings) is guarded by each buffer's own
// `readback_lock`, and foreign-device reads are rejected outright
// (review r1 P1-2). Consumers (Borsalino) share one device behind an
// `Arc` — same contract as `VulkanDevice`.
unsafe impl Send for MetalDevice {}
unsafe impl Sync for MetalDevice {}

// Compile-time proof the device is shareable behind an `Arc` (consumer
// contract, mirrors `VulkanDevice`).
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<MetalDevice>();
};

impl MetalDevice {
    /// Encode, commit, and synchronously wait a one-shot blit copy
    /// between two MTLBuffers (`size` bytes, offset 0).
    ///
    /// The Metal analogue of the Vulkan backend's `one_shot_transfer`:
    /// transient command buffer, synchronous completion. The command
    /// buffer and encoder are **autoreleased (+0)** objc products — they
    /// are never explicitly released (R5.2: that over-release was the
    /// SIGSEGV Borsalino's dispatch path carried once), and the scoped
    /// pool (R5.1) bounds them deterministically.
    ///
    /// # Safety
    ///
    /// objc FFI; `src`/`dst` must be valid MTLBuffers of at least `size`
    /// bytes that outlive this call's completion.
    unsafe fn blit_copy(&self, src: *mut c_void, dst: *mut c_void, size: usize) -> Result<()> {
        objc::rc::autoreleasepool(|| unsafe {
            // +0 autoreleased — owned by this pool's drain.
            let cmd: *mut c_void = msg_send![obj(self.queue), commandBuffer];
            if cmd.is_null() {
                return Err(DeviceError::BufferCreationFailed {
                    message: "failed to create MTLCommandBuffer for staging".into(),
                });
            }
            // +0 autoreleased.
            let enc: *mut c_void = msg_send![obj(cmd), blitCommandEncoder];
            if enc.is_null() {
                return Err(DeviceError::BufferCreationFailed {
                    message: "failed to create MTLBlitCommandEncoder".into(),
                });
            }
            let _: () = msg_send![obj(enc),
                copyFromBuffer: src
                sourceOffset: 0u64
                toBuffer: dst
                destinationOffset: 0u64
                size: size as u64];
            let _: () = msg_send![obj(enc), endEncoding];
            let _: () = msg_send![obj(cmd), commit];
            // Synchronous, mirroring `one_shot_transfer`'s queue_wait_idle —
            // the substrate's staging transfers retire before returning.
            let _: () = msg_send![obj(cmd), waitUntilCompleted];
            Ok(())
        })
    }

    /// Shared buffer allocation path.
    ///
    /// `data` uploads `byte_len` bytes when present (`None` =
    /// uninitialised). `force_private` takes the Private + staging path
    /// even when the negotiated placement is Shared — the
    /// [`Device::create_device_buffer`] contract (GPU-resident data
    /// regardless of strategy; the same post-0.1.1 contract the Vulkan
    /// backend implements).
    ///
    /// Allocation uses the alignment floor from
    /// [`DeviceLimits::min_storage_buffer_offset_alignment`] (16), kept
    /// consistent with the Vulkan backend: zero-length requests still
    /// allocate the floor. Data uploads copy `byte_len` bytes into the
    /// floored allocation via `contents()` (rather than
    /// `newBufferWithBytes:`, whose `length` parameter is also its copy
    /// length and would over-read a padded source slice).
    fn buffer_new(
        &self,
        byte_len: usize,
        data: Option<*const c_void>,
        force_private: bool,
    ) -> Result<crate::Buffer> {
        let alignment = self.limits.min_storage_buffer_offset_alignment as usize;
        let aligned = if byte_len == 0 {
            alignment
        } else {
            // Checked rounding — an overflowing request must error, not
            // panic (debug) or wrap (release); review r1 P1-3.
            checked_align_up(byte_len, alignment).ok_or_else(|| {
                DeviceError::BufferCreationFailed {
                    message: format!(
                        "allocation of {byte_len} bytes (aligned to {alignment}) overflows usize"
                    ),
                }
            })?
        };
        let use_private = force_private || self.placement == MemoryPlacement::DeviceLocal;

        // R5.1: every objc-minting path is pooled — buffer creation
        // included (Borsalino's is not; exceed it).
        objc::rc::autoreleasepool(|| unsafe {
            if use_private {
                self.buffer_new_private(byte_len, data, aligned)
            } else {
                self.buffer_new_shared(byte_len, data, aligned)
            }
        })
    }

    /// Shared-storage allocation: one MTLBuffer, direct `contents()`
    /// mapping.
    ///
    /// # Safety
    ///
    /// objc FFI; caller provides the autorelease pool.
    unsafe fn buffer_new_shared(
        &self,
        byte_len: usize,
        data: Option<*const c_void>,
        aligned: usize,
    ) -> Result<crate::Buffer> {
        // Safety: objc FFI on `self.device`; the caller holds an
        // autoreleasepool and `self` is a live MetalDevice.
        unsafe {
            // +1 from `new…` — owned, released in `drop_metal_buffer`.
            let buffer: *mut c_void = msg_send![obj(self.device), newBufferWithLength: aligned as u64 options: STORAGE_MODE_SHARED];
            if buffer.is_null() {
                return Err(DeviceError::BufferCreationFailed {
                    message: format!("failed to allocate {aligned} bytes (shared)"),
                });
            }
            // Borrowed pointer (R5.4) — valid while the MTLBuffer lives.
            let contents: *mut c_void = msg_send![obj(buffer), contents];
            if contents.is_null() && aligned > 0 {
                let _: () = msg_send![obj(buffer), release];
                return Err(DeviceError::BufferCreationFailed {
                    message: "contents() returned null for a shared buffer".into(),
                });
            }
            if let Some(src) = data {
                if byte_len > 0 {
                    std::ptr::copy_nonoverlapping(src, contents, byte_len);
                }
            }
            let inner = Box::new(MetalBufferInner {
                buffer,
                staging: None,
                mapped: contents,
                size: aligned,
                device: self.device,
                readback_lock: std::sync::Mutex::new(()),
            });
            Ok(crate::Buffer {
                raw: Box::into_raw(inner) as *mut c_void,
                len: byte_len,
                drop_fn: drop_metal_buffer,
            })
        }
    }

    /// Private-storage allocation: MTLBuffer(Private) + Shared staging,
    /// upload via staging + blit (mirrors the Vulkan device-local path).
    ///
    /// # Safety
    ///
    /// objc FFI; caller provides the autorelease pool.
    unsafe fn buffer_new_private(
        &self,
        byte_len: usize,
        data: Option<*const c_void>,
        aligned: usize,
    ) -> Result<crate::Buffer> {
        // Safety: objc FFI on `self.device`/`self.queue`; the caller holds
        // an autoreleasepool and `self` is a live MetalDevice.
        unsafe {
            // +1 each — owned; both released in `drop_metal_buffer` (staging
            // first). Failure paths release what they own before erroring
            // (R5.5 — the leak class Borsalino's review round 2 fixed).
            let dev_buf: *mut c_void = msg_send![obj(self.device), newBufferWithLength: aligned as u64 options: STORAGE_MODE_PRIVATE];
            if dev_buf.is_null() {
                return Err(DeviceError::BufferCreationFailed {
                    message: format!("failed to allocate {aligned} bytes (private)"),
                });
            }
            let staging: *mut c_void = msg_send![obj(self.device), newBufferWithLength: aligned as u64 options: STORAGE_MODE_SHARED];
            if staging.is_null() {
                let _: () = msg_send![obj(dev_buf), release];
                return Err(DeviceError::BufferCreationFailed {
                    message: format!("failed to allocate {aligned} bytes (staging)"),
                });
            }
            let stg_contents: *mut c_void = msg_send![obj(staging), contents];
            if stg_contents.is_null() && aligned > 0 {
                let _: () = msg_send![obj(staging), release];
                let _: () = msg_send![obj(dev_buf), release];
                return Err(DeviceError::BufferCreationFailed {
                    message: "contents() returned null for the staging buffer".into(),
                });
            }
            if let Some(src) = data {
                if byte_len > 0 {
                    std::ptr::copy_nonoverlapping(src, stg_contents, byte_len);
                    if let Err(e) = self.blit_copy(staging, dev_buf, aligned) {
                        let _: () = msg_send![obj(staging), release];
                        let _: () = msg_send![obj(dev_buf), release];
                        return Err(e);
                    }
                }
            }
            let inner = Box::new(MetalBufferInner {
                buffer: dev_buf,
                staging: Some(staging),
                mapped: stg_contents,
                size: aligned,
                device: self.device,
                readback_lock: std::sync::Mutex::new(()),
            });
            Ok(crate::Buffer {
                raw: Box::into_raw(inner) as *mut c_void,
                len: byte_len,
                drop_fn: drop_metal_buffer,
            })
        }
    }

    /// Raw MTLDevice handle — a **retained borrow** (R4 contract).
    ///
    /// Zunesha owns the device; consumers (Borsalino-Metal: shader
    /// compilation; Goldenweek-Metal later: pipelines) must not release it.
    /// Valid for the lifetime of the [`MetalDevice`].
    #[must_use]
    pub fn raw_device(&self) -> *mut c_void {
        self.device
    }

    /// Raw MTLBuffer handle for a buffer created by this device (R4).
    ///
    /// Exposed so consumers can bind Zunesha buffers into their own
    /// command encoders (`setBuffer:offset:atIndex:`) — the zero-copy
    /// compute→render interop path (ADR 0001). The handle is valid while
    /// the [`crate::Buffer`] lives; consumers must not release it.
    #[must_use]
    pub fn raw_buffer(&self, buffer: &crate::Buffer) -> *mut c_void {
        // Safety: `raw` was produced by `Box::into_raw::<MetalBufferInner>`
        // in `buffer_new` and remains valid while `buffer` is alive.
        unsafe { (*(buffer.raw as *const MetalBufferInner)).buffer }
    }

    /// The MTLCommandQueue handle Zunesha owns (same value as
    /// `queues().compute.raw` and `queues().graphics`).
    ///
    /// Convenience for consumers that prefer an explicit accessor over
    /// the [`Queues`] view; the ownership contract is identical — dispatch
    /// on it, never release it. `MTLCommandQueue` is documented
    /// thread-safe, so unlike the Vulkan backend there is no
    /// submission-lock protocol to join (ADR 0004).
    #[must_use]
    pub fn command_queue(&self) -> *mut c_void {
        self.queue
    }

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
                Some(hint) => {
                    pick_hinted_device(hint).unwrap_or_else(|| MTLCreateSystemDefaultDevice())
                }
                None => MTLCreateSystemDefaultDevice(),
            };
            if device.is_null() {
                return Err(DeviceError::InitFailed(
                    "MTLCreateSystemDefaultDevice returned null — no Metal-capable GPU".into(),
                ));
            }

            // +1 from `new` — we own the queue and hand out its handle via
            // `queues()`; consumers must not release it (R4 contract).
            let queue: *mut c_void = msg_send![obj(device), newCommandQueue];
            if queue.is_null() {
                // R5.5 failure paths release what they own: the device is
                // +1 ours; releasing it before the error balances creation.
                let _: () = msg_send![obj(device), release];
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

    fn create_buffer<T: bytemuck::Pod>(&self, data: &[T]) -> Result<crate::Buffer> {
        self.buffer_new(
            std::mem::size_of_val(data),
            Some(data.as_ptr() as *const c_void),
            false,
        )
    }

    fn create_buffer_uninit<T: bytemuck::Pod>(&self, len: usize) -> Result<crate::Buffer> {
        // Checked multiply — an overflowing request must error in every
        // build profile (review r1 P1-3), not panic or wrap.
        let byte_len = len.checked_mul(std::mem::size_of::<T>()).ok_or_else(|| {
            DeviceError::BufferCreationFailed {
                message: format!(
                    "allocation of {len} × {} elements overflows usize",
                    std::mem::size_of::<T>()
                ),
            }
        })?;
        self.buffer_new(byte_len, None, false)
    }

    /// Forces the Private + staging path regardless of the negotiated
    /// strategy — the mandatory override (R3, post-0.1.1 contract, no
    /// deferral and no silent Shared fallback). On Metal,
    /// `MTLStorageModePrivate` exists on every device, so there is no
    /// hard-error branch here (the R1 explicit-error allowance never
    /// applies to this override).
    fn create_device_buffer<T: bytemuck::Pod>(&self, data: &[T]) -> Result<crate::Buffer> {
        self.buffer_new(
            std::mem::size_of_val(data),
            Some(data.as_ptr() as *const c_void),
            true,
        )
    }

    /// Uninitialised variant of
    /// [`create_device_buffer`](Self::create_device_buffer).
    fn create_device_buffer_uninit<T: bytemuck::Pod>(&self, len: usize) -> Result<crate::Buffer> {
        // Checked multiply — review r1 P1-3, same as `create_buffer_uninit`.
        let byte_len = len.checked_mul(std::mem::size_of::<T>()).ok_or_else(|| {
            DeviceError::BufferCreationFailed {
                message: format!(
                    "allocation of {len} × {} elements overflows usize",
                    std::mem::size_of::<T>()
                ),
            }
        })?;
        self.buffer_new(byte_len, None, true)
    }

    fn read_buffer<T: bytemuck::Pod>(&self, buffer: &crate::Buffer) -> Result<Vec<T>> {
        // Safety: `raw` was produced by `Box::into_raw::<MetalBufferInner>`
        // in `buffer_new` and remains valid while `buffer` is alive.
        let inner = unsafe { &*(buffer.raw as *const MetalBufferInner) };
        // Ownership rule (review r1 P1-2): Metal resources are per-device.
        // A buffer created by a different MTLDevice (reachable on
        // multi-GPU Macs via `device_hint`) must not be read through this
        // one — its blit would target foreign resources. Note
        // `MTLCreateSystemDefaultDevice` returns a per-GPU singleton, so
        // two `MetalDevice` instances on one GPU share the pointer and
        // pass this check — their readers are serialized instead by the
        // buffer's own `readback_lock` below (the lock lives with the
        // mapping it guards).
        if !core::ptr::eq(self.device, inner.device) {
            return Err(DeviceError::BufferReadFailed {
                message: "buffer was not created by this device (Metal resources are per-device)"
                    .into(),
            });
        }
        if std::mem::size_of::<T>() == 0 {
            return Ok(Vec::new());
        }
        if inner.mapped.is_null() {
            return Err(DeviceError::BufferReadFailed {
                message: "buffer has no CPU mapping (no staging)".into(),
            });
        }

        if inner.staging.is_none() {
            // Shared: copy the mapping out as raw bytes into aligned
            // storage — a typed slice over the mapping would be unsound
            // for over-aligned `Pod` types (review r1 P1-1). `contents()`
            // stays borrowed (R5.4) — read-only, never released.
            return Ok(collect_aligned(inner.mapped, buffer.len));
        }

        // Private: device → staging blit, wait, then host copy — BOTH
        // under the buffer's own lock. The staging mapping is shared by
        // every reader of this buffer; the lock lives with the mapping,
        // so same-device concurrency and any legitimate re-entrant path
        // serialize correctly (review r1 P1-2 — the Vulkan backend's
        // review round 2, P1 race, transplanted and then some).
        let guard = inner
            .readback_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Safety: `staging` is a valid MTLBuffer of `inner.size` bytes,
        // and the blit completes synchronously before the copy below.
        unsafe { self.blit_copy(inner.buffer, inner.staging.unwrap(), inner.size)? };
        // Safety: `mapped` is the staging `contents()` — borrowed (R5.4),
        // valid for `buffer.len` logical bytes while `inner` lives.
        let out = collect_aligned(inner.mapped, buffer.len);
        drop(guard);
        Ok(out)
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
        let graphics = q
            .graphics
            .expect("graphics must be Some on Metal — every Metal device renders");
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

    /// A hint that matches nothing falls back to the system default
    /// (preference, not requirement — mirrors Vulkan selection).
    #[test]
    #[serial]
    fn buffer_roundtrip_shared() {
        let Some(device) = test_device() else { return };
        assert_eq!(device.buffer_placement(), MemoryPlacement::HostVisible);

        // f32 round-trip through Shared storage (port of the Vulkan
        // backend's `buffer_roundtrip_unified` / Borsalino's Metal test).
        let input = [1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0];
        let buffer = device
            .create_buffer(&input)
            .expect("create_buffer (shared)");
        let output: Vec<f32> = device.read_buffer(&buffer).expect("read_buffer (shared)");
        assert_eq!(output, input);

        // Byte-pattern round-trip (a different Pod shape).
        let bytes: Vec<u8> = (0..64_u8)
            .map(|i| i.wrapping_mul(7).wrapping_add(3))
            .collect();
        let buffer = device.create_buffer(&bytes).expect("create_buffer (u8)");
        let output: Vec<u8> = device.read_buffer(&buffer).expect("read_buffer (u8)");
        assert_eq!(output, bytes);
    }

    /// `create_buffer_uninit` allocates without uploading; the shape
    /// (logical length) is observable, contents are not asserted.
    #[test]
    #[serial]
    fn uninit_buffer_allocates_with_logical_shape() {
        let Some(device) = test_device() else { return };
        let buffer = device
            .create_buffer_uninit::<f32>(1024)
            .expect("create_buffer_uninit");
        let read: Vec<f32> = device.read_buffer(&buffer).expect("read uninit");
        assert_eq!(read.len(), 1024, "logical length must be element count");
        // Contents undefined; dropping must still be clean (release path).
    }

    /// Zero-length buffers: Metal permits `length: 0`, but Zunesha's
    /// alignment-floor semantics (consistent with the Vulkan backend)
    /// allocate the floor; reads observe the logical zero length.
    #[test]
    #[serial]
    fn zero_length_buffer_follows_alignment_floor() {
        let Some(device) = test_device() else { return };
        let empty: [u8; 0] = [];
        let buffer = device
            .create_buffer(&empty)
            .expect("create_buffer (zero-length)");
        let read: Vec<u8> = device.read_buffer(&buffer).expect("read zero-length");
        assert!(read.is_empty());

        let uninit = device
            .create_buffer_uninit::<u8>(0)
            .expect("create_buffer_uninit (zero-length)");
        let read: Vec<u8> = device.read_buffer(&uninit).expect("read uninit zero");
        assert!(read.is_empty());
    }

    /// Escape hatches (R4): the three raw surfaces Borsalino-Metal needs —
    /// MTLDevice (compile), MTLCommandQueue (dispatch), MTLBuffer (binding).
    /// Nothing more is exposed.
    #[test]
    #[serial]
    fn escape_hatches_expose_raw_handles() {
        let Some(device) = test_device() else { return };
        assert!(
            !device.raw_device().is_null(),
            "raw_device must be the MTLDevice"
        );
        assert_eq!(
            device.command_queue(),
            device.queues().compute.raw,
            "command_queue is the same handle the Queues expose"
        );
        let buffer = device.create_buffer(&[1u32, 2, 3]).expect("create_buffer");
        assert!(
            !device.raw_buffer(&buffer).is_null(),
            "raw_buffer must be the MTLBuffer"
        );
    }

    /// The R4 consumer contract, exercised consumer-style: raw handles
    /// only — real GPU work (a blit copy between two Zunesha buffers)
    /// encoded on the queue from `queues()`, no shader compilation, no
    /// substrate assistance beyond the handles.
    #[test]
    #[serial]
    fn consumer_style_blit_via_raw_handles() {
        let Some(device) = test_device() else { return };
        let src = device.create_buffer(&[10u32, 20, 30, 40]).expect("src");
        let dst = device.create_buffer_uninit::<u32>(4).expect("dst");

        let queue = device.queues().compute.raw;
        let src_raw = device.raw_buffer(&src);
        let dst_raw = device.raw_buffer(&dst);
        assert!(!queue.is_null(), "queue handle must be real");
        assert!(!src_raw.is_null(), "raw_buffer(src) must be real");
        assert!(!dst_raw.is_null(), "raw_buffer(dst) must be real");

        objc::rc::autoreleasepool(|| unsafe {
            let cmd: *mut c_void = msg_send![obj(queue), commandBuffer];
            assert!(!cmd.is_null());
            let enc: *mut c_void = msg_send![obj(cmd), blitCommandEncoder];
            assert!(!enc.is_null());
            let _: () = msg_send![obj(enc),
                copyFromBuffer: src_raw
                sourceOffset: 0u64
                toBuffer: dst_raw
                destinationOffset: 0u64
                size: 16u64];
            let _: () = msg_send![obj(enc), endEncoding];
            let _: () = msg_send![obj(cmd), commit];
            let _: () = msg_send![obj(cmd), waitUntilCompleted];
            // cmd/enc are +0 autoreleased — never released here (R5.2).
        });

        let out: Vec<u32> = device.read_buffer(&dst).expect("read dst");
        assert_eq!(out, vec![10, 20, 30, 40]);
    }

    /// DeviceLocal strategy: Private storage + staging in both directions
    /// (port of the Vulkan backend's `buffer_roundtrip_device_local`).
    #[test]
    #[serial]
    fn buffer_roundtrip_device_local() {
        let device = match MetalDevice::init_with_strategy(MemoryStrategy::DeviceLocal) {
            Ok(d) => d,
            Err(e) => {
                assert!(
                    std::env::var("ZUNESHA_REQUIRE_METAL").is_err(),
                    "ZUNESHA_REQUIRE_METAL is set but DeviceLocal init failed: {e}"
                );
                eprintln!("skipping: no Metal device ({e})");
                return;
            }
        };
        assert_eq!(device.buffer_placement(), MemoryPlacement::DeviceLocal);
        let input: Vec<u32> = (0..1000).collect();
        let buffer = device
            .create_buffer(&input)
            .expect("create_buffer (private)");
        let output: Vec<u32> = device.read_buffer(&buffer).expect("read_buffer (private)");
        assert_eq!(output, input);
    }

    /// `create_device_buffer` must force the Private + staging path even
    /// under a forced-`Unified` device — the mandatory override (R3),
    /// mirroring the Vulkan backend's post-0.1.1 contract. Observable via
    /// internals: a Private allocation carries a staging buffer;
    /// strategy-respecting `create_buffer` under `Unified` does not.
    #[test]
    #[serial]
    fn device_buffer_forces_private_under_unified_strategy() {
        let device = match MetalDevice::init_with_strategy(MemoryStrategy::Unified) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("skipping: no Metal device ({e})");
                return;
            }
        };
        let unified = device.create_buffer(&[1.0f32, 2.0, 3.0, 4.0]).unwrap();
        let forced = device
            .create_device_buffer(&[1.0f32, 2.0, 3.0, 4.0])
            .unwrap();

        // Safety: both handles were produced by this device's create paths.
        let u = unsafe { &*(unified.raw as *const MetalBufferInner) };
        let f = unsafe { &*(forced.raw as *const MetalBufferInner) };
        assert!(
            u.staging.is_none(),
            "Unified-strategy create_buffer must stay Shared"
        );
        assert!(
            f.staging.is_some(),
            "create_device_buffer must force the Private + staging path"
        );

        let r1: Vec<f32> = device.read_buffer(&unified).unwrap();
        let r2: Vec<f32> = device.read_buffer(&forced).unwrap();
        assert_eq!(r1, vec![1.0, 2.0, 3.0, 4.0]);
        assert_eq!(r2, vec![1.0, 2.0, 3.0, 4.0]);
    }

    /// R5.6 pool-escape regression: a **retained (+0-escaped)** command
    /// buffer survives an autoreleasepool drain and still completes. This
    /// is the accounting that keeps async consumer work (Borsalino's
    /// `Pulse` shape) sound: explicit `retain` owns the escape, the pool's
    /// drain must not invalidate it, and the balancing `release` follows
    /// completion. Without the retain this test is the over-release
    /// SIGSEGV / use-after-drain class.
    #[test]
    #[serial]
    fn pool_drain_retained_command_buffer_completes() {
        let Some(device) = test_device() else { return };
        let src = device.create_buffer(&[7u32, 8, 9]).expect("src");
        let dst = device.create_buffer_uninit::<u32>(3).expect("dst");

        let (cmd, src_raw, dst_raw) = objc::rc::autoreleasepool(|| unsafe {
            let cmd: *mut c_void = msg_send![obj(device.queue), commandBuffer];
            assert!(!cmd.is_null());
            let enc: *mut c_void = msg_send![obj(cmd), blitCommandEncoder];
            assert!(!enc.is_null());
            let src_raw = device.raw_buffer(&src);
            let dst_raw = device.raw_buffer(&dst);
            assert!(
                !src_raw.is_null(),
                "raw handles must be real before encoding"
            );
            assert!(
                !dst_raw.is_null(),
                "raw handles must be real before encoding"
            );
            let _: () = msg_send![obj(enc),
                copyFromBuffer: src_raw
                sourceOffset: 0u64
                toBuffer: dst_raw
                destinationOffset: 0u64
                size: 16u64];
            let _: () = msg_send![obj(enc), endEncoding];
            let _: () = msg_send![obj(cmd), commit];
            // +0 escape: the command buffer must outlive this pool — the
            // explicit retain is the ownership (Borsalino's async Pulse
            // pattern, R5.3 first bullet).
            let _: () = msg_send![obj(cmd), retain];
            (cmd, src_raw, dst_raw)
        });
        // Pool has drained. The retained command buffer must still be
        // valid: wait for completion, then read the result.
        unsafe {
            let _: () = msg_send![obj(cmd), waitUntilCompleted];
            let _: () = msg_send![obj(cmd), release];
        }
        let out: Vec<u32> = device.read_buffer(&dst).expect("read after drain");
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], 7);
        let _ = (src_raw, dst_raw); // handles stay valid across the drain
    }

    /// Safe code can share one `Arc<MetalDevice>` across threads (consumer
    /// contract, same as Vulkan). Regression port of the Vulkan backend's
    /// `concurrent_buffer_creation_is_serialized`: concurrent Private
    /// buffer creation + readback from four threads round-trips exactly —
    /// the `readback_lock` serializes the shared per-buffer staging
    /// mappings (host-copy race, review round 2 P1 class).
    #[test]
    #[serial]
    fn concurrent_private_buffer_readback_is_serialized() {
        let device = match MetalDevice::init_with_strategy(MemoryStrategy::DeviceLocal) {
            Ok(d) => std::sync::Arc::new(d),
            Err(e) => {
                eprintln!("skipping: no Metal device ({e})");
                return;
            }
        };
        let mut handles = Vec::new();
        for t in 0..4u32 {
            let device = std::sync::Arc::clone(&device);
            handles.push(std::thread::spawn(move || {
                for i in 0..8u32 {
                    let payload = [(t * 8 + i) as f32; 4];
                    let buf = device.create_device_buffer(&payload).unwrap();
                    let back: Vec<f32> = device.read_buffer(&buf).unwrap();
                    assert_eq!(back, payload);
                }
            }));
        }
        for h in handles {
            h.join()
                .expect("no thread panicked under the readback lock");
        }
    }

    /// Review r1 P1-3: overflowing allocation requests must error cleanly in
    /// EVERY build profile — debug must not panic on the multiply/align, and
    /// release must not wrap to a bogus `Ok` (observed: `usize::MAX` u8
    /// elements wrapped to a 0-byte buffer in release).
    #[test]
    #[serial]
    fn overflowing_allocation_requests_error_cleanly() {
        let Some(device) = test_device() else { return };
        for res in [
            device.create_buffer_uninit::<u64>(usize::MAX / 8 + 1),
            device.create_buffer_uninit::<u8>(usize::MAX),
            device.create_device_buffer_uninit::<u8>(usize::MAX),
        ] {
            match res {
                Err(DeviceError::BufferCreationFailed { .. }) => {}
                other => panic!("overflowing request must error, got {other:?}"),
            }
        }
    }

    /// A huge-but-non-overflowing allocation fails cleanly at the Metal
    /// layer (null buffer → error, no crash), and the device stays usable
    /// afterwards — failure paths release what they own (R5.5 evidence).
    #[test]
    #[serial]
    fn oversized_allocation_fails_cleanly() {
        let Some(device) = test_device() else { return };
        assert!(matches!(
            device.create_buffer_uninit::<u8>(1 << 48),
            Err(DeviceError::BufferCreationFailed { .. })
        ));
        // Device still functional after the failure.
        let ok = device
            .create_buffer(&[1u32, 2, 3])
            .expect("device usable after failure");
        let back: Vec<u32> = device.read_buffer(&ok).unwrap();
        assert_eq!(back, vec![1, 2, 3]);
    }

    /// Review r1 P1-2: the staging-mapping race across `MetalDevice`
    /// instances. `MTLCreateSystemDefaultDevice` returns a per-GPU
    /// singleton, so two independently initialized devices share one
    /// MTLDevice (but own different command queues) — the buffer's own
    /// `readback_lock` must serialize their reads of the single staging
    /// mapping: every read exact, no panic. This is the reviewer's exact
    /// repro shape, now pinned as the regression.
    ///
    /// A buffer read through a *genuinely different* MTLDevice (discrete
    /// multi-GPU Mac, reachable via `device_hint`) is rejected by the
    /// ownership check in `read_buffer` — untestable on single-GPU
    /// hardware, guarded by the device-pointer comparison.
    #[test]
    #[serial]
    fn cross_device_instance_reads_are_serialized() {
        let Some(a) = test_device() else { return };
        let Some(b) = test_device() else { return };
        let buffer = a
            .create_device_buffer(&vec![42u8; 64 * 1024])
            .expect("create");
        let expected = vec![42u8; 64 * 1024];
        let buffer_ref = &buffer;
        std::thread::scope(|s| {
            for device in [&a, &b] {
                let expected = expected.clone();
                s.spawn(move || {
                    for _ in 0..100 {
                        let back: Vec<u8> = device.read_buffer(buffer_ref).unwrap();
                        assert_eq!(back, expected);
                    }
                });
            }
        });
    }

    /// Review r1 P1-2 (same-device face): four threads reading the SAME
    /// Private buffer through one shared device must serialize on the
    /// buffer's own staging mapping — every read exact, no panic. Pins the
    /// per-buffer readback lock (replaces the prior per-thread-buffer
    /// shape, which never contended one staging mapping).
    #[test]
    #[serial]
    fn same_buffer_concurrent_reads_are_serialized() {
        let device = match MetalDevice::init_with_strategy(MemoryStrategy::DeviceLocal) {
            Ok(d) => std::sync::Arc::new(d),
            Err(e) => {
                eprintln!("skipping: no Metal device ({e})");
                return;
            }
        };
        let payload: Vec<u32> = (0..1024u32).map(|i| i.wrapping_mul(2654435761)).collect();
        let buffer = device.create_device_buffer(&payload).unwrap();
        let buffer_ref = &buffer;
        std::thread::scope(|s| {
            for _ in 0..4 {
                let device = std::sync::Arc::clone(&device);
                let expected = payload.clone();
                s.spawn(move || {
                    for _ in 0..25 {
                        let back: Vec<u32> = device.read_buffer(buffer_ref).unwrap();
                        assert_eq!(back, expected);
                    }
                });
            }
        });
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
