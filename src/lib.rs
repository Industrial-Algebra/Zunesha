// Copyright (C) 2026 Industrial Algebra
// SPDX-License-Identifier: Apache-2.0

//! # Zunesha — Shared GPU Device Substrate
//!
//! > The immortal elephant that carries compute and graphics on its back.
//!
//! Zunesha is the device layer under [Borsalino](https://crates.io/crates/borsalino)
//! (compute) and [Goldenweek](https://github.com/Industrial-Algebra/Goldenweek)
//! (graphics). It owns the one thing both genuinely share — the **GPU device**:
//! physical-device selection, queues, memory strategy, and buffer allocation.
//! Pipelines, shaders, and dispatch stay in the consumer libraries; Zunesha is
//! strictly the substrate they stand on.
//!
//! ## Why a shared device
//!
//! Compute→render interop (particle systems, GPU-driven geometry, Miriami's
//! Grade-1 compute-then-draw fields) needs compute output bound directly as
//! render input — zero-copy. That only works when both sides share one device.
//! Zunesha is IA Principle 1 (Geometric Unification) applied honestly: the
//! GPU device is the unifying primitive, and compute and graphics are its two
//! faces.
//!
//! ## Design
//!
//! - **Capability-driven queues.** A `Queues` always exposes a `compute` queue
//!   (Zunesha's baseline contract) and *optionally* `graphics` and `transfer`.
//!   Zunesha never requires a graphics queue — Borsalino runs on compute-only
//!   hardware (NVIDIA Grace Blackwell GB10 / DGX Spark, headless datacenter
//!   GPUs). Goldenweek refuses to initialise where `queues().graphics` is `None`.
//! - **Buffer ownership.** `zunesha::Buffer` is the shared primitive;
//!   `Borsalino::GpuBuffer` and `Goldenweek::GpuBuffer` will both wrap it. A
//!   buffer allocated for compute *is* the memory a render pipeline binds.
//! - **Unified GC safety.** The epoch tracker (ported from Borsalino) observes
//!   *every* dispatch through the device — compute and graphics — so a single
//!   [`Device::is_quiescent`] query certifies no GPU work touches host memory
//!   before a moving GC compacts.
//! - **Surface-agnostic.** Zunesha does no windowing and compiles no shaders.
//!
//! ## Backends
//!
//! | Feature  | Platform       | Status (v0.1)        |
//! |----------|----------------|----------------------|
//! | `metal`  | macOS          | 🚧 Trait + stub only |
//! | `vulkan` | Linux, Windows | 🚧 Trait + stub only |
//!
//! v0.1 ships the [`Device`] trait, the [`Buffer`] / queue / quiescence types,
//! the ported [`epoch`] tracker, and a [`NoDeviceStub`]. The Vulkan backend is
//! the first target (mirroring Borsalino/Goldenweek's development order).
//!
//! ## Status
//!
//! Pre-release. The trait surface is stabilising; backends land after v0.1.

#![warn(missing_docs)]
#![warn(clippy::all)]

mod error;

#[cfg(all(feature = "vulkan", not(target_os = "macos")))]
pub mod vulkan;

/// GPU dispatch epoch tracking for GC safety.
///
/// Ported from Borsalino. Backend-agnostic — real, tested code from v0.1,
/// not a stub. See the module docs for why owning this at the device layer is
/// strictly stronger than per-library tracking.
pub mod epoch;

pub use epoch::GpuEpochTracker;
pub use error::{DeviceError, Result};

use std::ffi::c_void;

// ── Memory strategy ───────────────────────────────────────────────

/// Memory allocation strategy for GPU buffers.
///
/// Controls whether buffer data lives in unified memory (shared with CPU) or
/// dedicated GPU memory (VRAM), with automatic staging transfers. Ported from
/// Borsalino — it is a device-level property, not compute- or graphics-specific.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MemoryStrategy {
    /// Let the backend choose based on hardware capabilities.
    /// Discrete GPUs get device-local memory; integrated/unified get host-visible.
    #[default]
    Auto,
    /// Force host-visible, host-coherent memory (unified memory systems).
    /// Best for Apple Silicon, AMD APUs, and GB10.
    Unified,
    /// Force device-local memory with staging transfers (discrete GPUs).
    /// Best for NVIDIA RTX, AMD RDNA, Intel Arc.
    DeviceLocal,
}

// ── Capability-driven init ────────────────────────────────────────

/// Capabilities and preferences requested at device initialisation.
///
/// Passed to [`Device::init_with`]. The baseline [`Device::init`] never
/// requires a graphics queue (Borsalino-safe on compute-only hardware); this
/// struct lets a caller *prefer* a graphics-capable device when one is needed
/// (e.g. Goldenweek on a multi-GPU box with a compute GPU and a display GPU).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InitRequest {
    /// Memory allocation strategy.
    pub memory: MemoryStrategy,
    /// Request graphics readiness: prefer a graphics-capable physical device
    /// *and* enable presentation extensions.
    ///
    /// Under the Vulkan backend this both biases device selection toward a
    /// graphics-capable adapter and enables surface-related instance/device
    /// extensions (`VK_KHR_surface`, `VK_KHR_swapchain`, platform surface
    /// extensions, and `VK_EXT_headless_surface` for testing) — each only if
    /// the driver supports it.
    ///
    /// A *preference*, not a requirement: if no graphics-capable device exists,
    /// Zunesha still returns the best compute device and
    /// [`Queues::graphics`] is `None`. Goldenweek then refuses to initialise.
    pub prefer_graphics: bool,
}

impl InitRequest {
    /// Convenience: a compute-only request (Borsalino's baseline).
    #[must_use]
    pub fn compute_only() -> Self {
        Self {
            memory: MemoryStrategy::Auto,
            prefer_graphics: false,
        }
    }

    /// Convenience: prefer a graphics-capable device (Goldenweek's baseline).
    #[must_use]
    pub fn prefer_graphics() -> Self {
        Self {
            memory: MemoryStrategy::Auto,
            prefer_graphics: true,
        }
    }
}

// ── Queues ────────────────────────────────────────────────────────

/// Opaque handle to a single device queue.
///
/// Backends populate this at device creation. Consumers read it from
/// [`Queues`] to bind work; they never construct it. Queue resources are owned
/// by the device backend, so [`Queue`] is `Copy` and has no `Drop`.
#[derive(Debug, Clone, Copy)]
pub struct Queue {
    /// Backend-specific raw queue handle (e.g. a Vulkan `VkQueue`).
    pub raw: *mut c_void,
    /// Queue family index the queue belongs to.
    pub family_index: u32,
}

impl Queue {
    /// Sentinel null queue. Used only by [`NoDeviceStub`]; never observed by
    /// real consumers because the stub's `init` always fails.
    #[must_use]
    pub(crate) const fn null() -> Self {
        Self {
            raw: core::ptr::null_mut(),
            family_index: u32::MAX,
        }
    }
}

/// The queue families a [`Device`] exposes.
///
/// Capability-driven: `compute` is always present (Zunesha's baseline
/// contract); `graphics` and `transfer` are present only when the device
/// actually has distinct families for them. On compute-only hardware
/// (NVIDIA Grace Blackwell GB10, headless datacenter GPUs) `graphics` is
/// `None`, and Goldenweek refuses to initialise.
#[derive(Debug, Clone, Copy)]
pub struct Queues {
    /// The compute queue. Always present.
    pub compute: Queue,
    /// The graphics queue, if the device exposes one.
    pub graphics: Option<Queue>,
    /// A dedicated transfer queue, if distinct from `compute`.
    ///
    /// `None` does not mean transfers are impossible — it means there is no
    /// *separate* transfer family, and consumers reuse `compute`.
    pub transfer: Option<Queue>,
}

impl Queues {
    /// Sentinel queues for [`NoDeviceStub`]. Never observed by real consumers.
    #[must_use]
    pub(crate) const fn stub() -> Self {
        Self {
            compute: Queue::null(),
            graphics: None,
            transfer: None,
        }
    }

    /// True iff this device can drive a graphics/render pipeline.
    ///
    /// Goldenweek checks this before initialising.
    #[must_use]
    pub fn has_graphics(&self) -> bool {
        self.graphics.is_some()
    }
}

// ── Device limits ─────────────────────────────────────────────────

/// Device-level limits exposed to consumers.
///
/// Consumers query these to build verification proofs (e.g. Borsalino's
/// `verify_with_limits` reads `max_compute_work_group_count`). Kept minimal at
/// v0.1 — more limits land as consumers need them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DeviceLimits {
    /// Minimum alignment (bytes) for storage buffer offsets. Affects how
    /// consumers bind buffer slices.
    pub min_storage_buffer_offset_alignment: u64,
}

// ── Opaque buffer handle ──────────────────────────────────────────

/// Handle to a GPU buffer of device-allocated memory.
///
/// Created by [`Device::create_buffer`] and friends. This is the **shared
/// primitive**: `Borsalino::GpuBuffer` and `Goldenweek::GpuBuffer` will both
/// wrap a `zunesha::Buffer`, so a buffer allocated for compute is the same
/// memory a render pipeline binds — zero-copy compute→render interop.
///
/// # Drop behaviour
///
/// When dropped, releases its device memory via the backend-specific drop
/// function stored at construction time.
pub struct Buffer {
    pub(crate) raw: *mut c_void,
    pub(crate) len: usize,
    pub(crate) drop_fn: fn(*mut c_void),
}

impl std::fmt::Debug for Buffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Buffer")
            .field("raw", &self.raw)
            .field("len", &self.len)
            .finish()
    }
}

// Safety: the raw pointer is an opaque backend handle. Backends guarantee
// thread-safe access to buffer memory (reads/writes are serialised through the
// device command stream).
unsafe impl Send for Buffer {}
unsafe impl Sync for Buffer {}

impl Drop for Buffer {
    fn drop(&mut self) {
        (self.drop_fn)(self.raw);
    }
}

/// RAII guard proving that host memory backing a zero-copy GPU buffer is
/// pinned and stable.
///
/// Created by [`Device::create_buffer_pinned`]. The lifetime `'a` is bound to
/// the input data slice — the borrow checker ensures the host data outlives
/// the handle (and thus the GPU buffer).
///
/// On discrete GPUs (device-local memory), this is a no-op — the buffer is
/// copied to VRAM and no host reference survives.
#[derive(Debug)]
pub struct BufferPinHandle<'a> {
    _lifetime: std::marker::PhantomData<&'a mut [u8]>,
}

impl<'a> BufferPinHandle<'a> {
    /// Create a new pin handle bound to the given lifetime.
    ///
    /// Called by [`Device::create_buffer_pinned`]; users do not construct it
    /// directly.
    #[must_use]
    pub fn new() -> Self {
        Self {
            _lifetime: std::marker::PhantomData,
        }
    }
}

impl<'a> Default for BufferPinHandle<'a> {
    fn default() -> Self {
        Self::new()
    }
}

/// Compile-time proof that no GPU operations are outstanding.
///
/// Required by GC-sensitive contexts. Constructed via
/// [`Device::prove_quiescent`] when [`Device::is_quiescent`] returns true.
///
/// This proof certifies that the epoch counter was zero at construction time —
/// no dispatches were in-flight. Combined with [`GpuEpochTracker`], this
/// ensures GC compaction is safe.
#[derive(Clone, Copy, Debug)]
pub struct QuiescenceProof {
    _private: (),
}

// ── Trait ─────────────────────────────────────────────────────────

/// Backend-agnostic GPU device interface.
///
/// Each backend (Metal on macOS, Vulkan elsewhere) implements this trait.
/// Callers obtain a device via [`init`] / [`init_with_strategy`] /
/// [`init_with`].
///
/// # Capability model
///
/// Every Zunesha device exposes a compute queue ([`Queues::compute`]).
/// Graphics and transfer queues are optional — see [`Queues`]. Use
/// [`init_with`](Self::init_with) with [`InitRequest::prefer_graphics`] to
/// prefer a graphics-capable physical device on multi-GPU systems.
pub trait Device: Sized {
    /// Initialise the best available device (baseline: best compute device).
    ///
    /// Never requires a graphics queue — Borsalino-safe on compute-only
    /// hardware (GB10, headless datacenter GPUs, cloud compute instances).
    fn init() -> Result<Self>;

    /// Initialise with an explicit memory strategy.
    fn init_with_strategy(_strategy: MemoryStrategy) -> Result<Self> {
        Self::init()
    }

    /// Initialise with explicit capability preferences.
    ///
    /// `request.prefer_graphics` biases physical-device selection toward a
    /// graphics-capable device; it does not *require* one.
    fn init_with(_request: InitRequest) -> Result<Self> {
        Self::init_with_strategy(MemoryStrategy::Auto)
    }

    /// The queue families this device exposes.
    fn queues(&self) -> Queues;

    /// Device-level limits for consumer verification proofs.
    fn limits(&self) -> DeviceLimits;

    /// The memory strategy this device was initialised with.
    fn memory_strategy(&self) -> MemoryStrategy;

    /// Allocate a buffer and upload initial data.
    fn create_buffer<T: bytemuck::Pod>(&self, data: &[T]) -> Result<Buffer>;

    /// Allocate an uninitialised buffer of `len` elements.
    fn create_buffer_uninit<T: bytemuck::Pod>(&self, len: usize) -> Result<Buffer>;

    /// Allocate a device-local buffer and upload initial data.
    ///
    /// Persists across dispatches without CPU readback overhead. On unified
    /// memory (Apple Silicon, GB10) identical to [`create_buffer`](Self::create_buffer).
    fn create_device_buffer<T: bytemuck::Pod>(&self, data: &[T]) -> Result<Buffer> {
        self.create_buffer(data)
    }

    /// Allocate an uninitialised device-local buffer.
    fn create_device_buffer_uninit<T: bytemuck::Pod>(&self, len: usize) -> Result<Buffer> {
        self.create_buffer_uninit::<T>(len)
    }

    /// Create a zero-copy GPU buffer backed by the host slice.
    ///
    /// The returned [`BufferPinHandle`] borrows the input data — the borrow
    /// checker ensures the host data outlives the handle. On discrete GPUs
    /// this falls back to a copy; the pin handle is a no-op.
    ///
    /// # GC Safety
    ///
    /// For WASM runtimes with a moving GC, combine with
    /// [`is_quiescent`](Self::is_quiescent).
    fn create_buffer_pinned<'a, T: bytemuck::Pod>(
        &self,
        data: &'a [T],
    ) -> Result<(Buffer, BufferPinHandle<'a>)> {
        let buf = self.create_buffer(data)?;
        Ok((buf, BufferPinHandle::new()))
    }

    /// Read a buffer's contents back to host memory.
    fn read_buffer<T: bytemuck::Pod>(&self, buffer: &Buffer) -> Result<Vec<T>>;

    /// Number of dispatches that have begun but not yet completed.
    ///
    /// Zero means the GPU is idle — safe for a WASM runtime to compact memory.
    /// The default returns `0` (no tracking); backends embed a
    /// [`GpuEpochTracker`] and override this.
    fn in_flight(&self) -> u64 {
        0
    }

    /// True when no GPU operations are outstanding.
    ///
    /// The runtime (e.g. Baedeker) calls this before GC compaction. Because
    /// the epoch tracker lives at the device layer, this certifies that
    /// **neither** compute **nor** graphics dispatches are in-flight.
    fn is_quiescent(&self) -> bool {
        self.in_flight() == 0
    }

    /// Construct a [`QuiescenceProof`] if the device is currently idle.
    fn prove_quiescent(&self) -> Option<QuiescenceProof> {
        if self.is_quiescent() {
            Some(QuiescenceProof { _private: () })
        } else {
            None
        }
    }
}

// ── Stub backend (compile-time sentinel) ──────────────────────────

/// Stub device — no device backend compiled for this target.
///
/// Exists so a backend-less build still type-checks and every [`Device`]
/// allocation method returns [`DeviceError::NoDevice`]. Replaced by cfg-gated
/// `metal::MetalDevice` / `vulkan::VulkanDevice` when the backends land.
///
/// Although `init` always fails (so this is never returned to callers), the
/// type is constructible for unit-testing its quiescence defaults.
pub struct NoDeviceStub;

impl Device for NoDeviceStub {
    fn init() -> Result<Self> {
        Err(DeviceError::NoDevice)
    }
    fn queues(&self) -> Queues {
        Queues::stub()
    }
    fn limits(&self) -> DeviceLimits {
        DeviceLimits::default()
    }
    fn memory_strategy(&self) -> MemoryStrategy {
        MemoryStrategy::Auto
    }
    fn create_buffer<T: bytemuck::Pod>(&self, _data: &[T]) -> Result<Buffer> {
        Err(DeviceError::NoDevice)
    }
    fn create_buffer_uninit<T: bytemuck::Pod>(&self, _len: usize) -> Result<Buffer> {
        Err(DeviceError::NoDevice)
    }
    fn read_buffer<T: bytemuck::Pod>(&self, _buffer: &Buffer) -> Result<Vec<T>> {
        Err(DeviceError::NoDevice)
    }
}

// ── Top-level initialisers ────────────────────────────────────────

/// Initialise the best available device (baseline: best compute device).
///
/// Never requires a graphics queue — Borsalino-safe on compute-only hardware.
///
/// - Linux / Windows: `vulkan::VulkanDevice` (requires `vulkan` feature)
/// - macOS: `metal::MetalDevice` (requires `metal` feature) — not yet implemented
/// - Otherwise: [`DeviceError::NoDevice`]
#[cfg(all(feature = "vulkan", not(target_os = "macos")))]
pub fn init() -> Result<vulkan::VulkanDevice> {
    vulkan::VulkanDevice::init()
}

/// Initialise with an explicit memory strategy (Vulkan backend).
#[cfg(all(feature = "vulkan", not(target_os = "macos")))]
pub fn init_with_strategy(strategy: MemoryStrategy) -> Result<vulkan::VulkanDevice> {
    vulkan::VulkanDevice::init_with_strategy(strategy)
}

/// Initialise with explicit capability preferences (Vulkan backend).
///
/// See [`InitRequest`] and [`Device::init_with`].
#[cfg(all(feature = "vulkan", not(target_os = "macos")))]
pub fn init_with(request: InitRequest) -> Result<vulkan::VulkanDevice> {
    vulkan::VulkanDevice::init_with(request)
}

/// Initialise the best available device — fallback when no backend is compiled.
#[cfg(not(all(feature = "vulkan", not(target_os = "macos"))))]
pub fn init() -> Result<NoDeviceStub> {
    Err(DeviceError::NoDevice)
}

/// Initialise with an explicit memory strategy — fallback (no backend).
#[cfg(not(all(feature = "vulkan", not(target_os = "macos"))))]
pub fn init_with_strategy(_strategy: MemoryStrategy) -> Result<NoDeviceStub> {
    Err(DeviceError::NoDevice)
}

/// Initialise with explicit capability preferences — fallback (no backend).
///
/// See [`InitRequest`] and [`Device::init_with`].
#[cfg(not(all(feature = "vulkan", not(target_os = "macos"))))]
pub fn init_with(_request: InitRequest) -> Result<NoDeviceStub> {
    Err(DeviceError::NoDevice)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn memory_strategy_default_is_auto() {
        assert_eq!(MemoryStrategy::default(), MemoryStrategy::Auto);
    }

    #[test]
    fn init_request_conveniences() {
        assert!(!InitRequest::compute_only().prefer_graphics);
        assert!(InitRequest::prefer_graphics().prefer_graphics);
        assert_eq!(InitRequest::default().memory, MemoryStrategy::Auto);
    }

    #[test]
    fn queues_stub_has_compute_but_no_graphics() {
        // The sentinel used by NoDeviceStub: compute present (null), graphics absent.
        let q = Queues::stub();
        // compute is always present, but the stub's is the null sentinel.
        assert_eq!(q.compute.family_index, u32::MAX);
        assert!(q.graphics.is_none());
        assert!(!q.has_graphics());
    }

    #[test]
    fn stub_reports_no_device_for_allocation() {
        let stub = NoDeviceStub;
        assert!(matches!(
            stub.create_buffer(&[1.0f32, 2.0, 3.0]),
            Err(DeviceError::NoDevice)
        ));
        assert!(matches!(
            stub.create_buffer_uninit::<u8>(16),
            Err(DeviceError::NoDevice)
        ));
    }

    #[test]
    fn stub_is_always_quiescent() {
        // The stub inherits the default in_flight() = 0, so it is trivially
        // quiescent and can always produce a proof.
        let stub = NoDeviceStub;
        assert_eq!(stub.in_flight(), 0);
        assert!(stub.is_quiescent());
        assert!(stub.prove_quiescent().is_some());
    }

    #[test]
    fn stub_exposes_no_graphics_capability() {
        let stub = NoDeviceStub;
        assert!(!stub.queues().has_graphics());
    }

    #[test]
    #[serial]
    fn top_level_init_resolves_to_a_backend() {
        // With a backend compiled, init() resolves to a real device on capable
        // hardware (Ok) or NoDevice if none. Without any backend, it always
        // refuses. Either way it must not panic.
        #[cfg(all(feature = "vulkan", not(target_os = "macos")))]
        {
            let _ = init();
            let _ = init_with_strategy(MemoryStrategy::Unified);
            let _ = init_with(InitRequest::prefer_graphics());
        }

        #[cfg(not(all(feature = "vulkan", not(target_os = "macos"))))]
        {
            assert!(matches!(init(), Err(DeviceError::NoDevice)));
            assert!(matches!(
                init_with_strategy(MemoryStrategy::Unified),
                Err(DeviceError::NoDevice)
            ));
            assert!(matches!(
                init_with(InitRequest::prefer_graphics()),
                Err(DeviceError::NoDevice)
            ));
        }
    }

    #[test]
    fn buffer_pin_handle_default_is_valid() {
        let _handle: BufferPinHandle<'static> = BufferPinHandle::default();
    }

    #[test]
    fn device_limits_default_is_zero() {
        let limits = DeviceLimits::default();
        assert_eq!(limits.min_storage_buffer_offset_alignment, 0);
    }
}
