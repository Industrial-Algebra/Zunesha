// Copyright (C) 2026 Industrial Algebra
// SPDX-License-Identifier: Apache-2.0

//! Vulkan backend for Zunesha.
//!
//! Available on Linux and Windows with the `vulkan` feature enabled. Ported
//! from Borsalino's `vulkan.rs` device/memory/buffer layer, with two
//! Zunesha-specific changes:
//!
//! - **Capability-driven queue selection.** [`select_queue_families`] prefers a
//!   dedicated `COMPUTE`-without-`GRAPHICS` family for `compute` (async-compute
//!   isolation), with a graceful fallback to the first compute-capable family on
//!   unified hardware. `graphics` and `transfer` are `Option`, present only
//!   when distinct families exist.
//! - **No compute pipeline / dispatch.** Those stay in Borsalino. This backend
//!   owns only device + queues + memory + buffers.

use std::collections::HashSet;
use std::ffi::{CStr, CString, c_char, c_void};
use std::mem;
use std::ptr;

use ash::vk::Handle;
use ash::{Entry, vk};

use crate::{
    Device, DeviceError, DeviceLimits, GpuEpochTracker, InitRequest, MemoryStrategy, Queue, Queues,
    Result,
};

// ── Queue family selection (capability-driven, policy A) ──────────

/// Which queue family index serves each capability, chosen per policy A.
struct QueueFamilyChoice {
    /// Always present — a compute-capable family (preferred: dedicated).
    compute: u32,
    /// First graphics-capable family, if any.
    graphics: Option<u32>,
    /// First dedicated transfer-only family, if any.
    transfer: Option<u32>,
}

impl QueueFamilyChoice {
    /// The distinct family indices we must request at logical-device creation.
    fn distinct_families(&self) -> Vec<u32> {
        let mut families = vec![self.compute];
        if let Some(g) = self.graphics {
            if !families.contains(&g) {
                families.push(g);
            }
        }
        if let Some(t) = self.transfer {
            if !families.contains(&t) {
                families.push(t);
            }
        }
        families
    }
}

/// Select queue families per policy A.
///
/// - `compute`: first `COMPUTE`-without-`GRAPHICS` family (dedicated async
///   compute); falls back to the first `COMPUTE`-capable family when no
///   dedicated one exists (unified hardware, e.g. Intel iGPU, GB10).
/// - `graphics`: first `GRAPHICS`-capable family, else `None`.
/// - `transfer`: first `TRANSFER`-only family (no compute, no graphics), else
///   `None` (consumers reuse `compute`).
fn select_queue_families(props: &[vk::QueueFamilyProperties]) -> Option<QueueFamilyChoice> {
    let compute = props
        .iter()
        .position(|q| {
            q.queue_flags.contains(vk::QueueFlags::COMPUTE)
                && !q.queue_flags.contains(vk::QueueFlags::GRAPHICS)
        })
        .or_else(|| {
            props
                .iter()
                .position(|q| q.queue_flags.contains(vk::QueueFlags::COMPUTE))
        })?;
    let graphics = props
        .iter()
        .position(|q| q.queue_flags.contains(vk::QueueFlags::GRAPHICS))
        .map(|i| i as u32);
    let transfer = props
        .iter()
        .position(|q| {
            q.queue_flags.contains(vk::QueueFlags::TRANSFER)
                && !q.queue_flags.contains(vk::QueueFlags::COMPUTE)
                && !q.queue_flags.contains(vk::QueueFlags::GRAPHICS)
        })
        .map(|i| i as u32);
    Some(QueueFamilyChoice {
        compute: compute as u32,
        graphics,
        transfer,
    })
}

/// Physical-device preference score (discrete > integrated > virtual > cpu).
fn device_type_score(dt: vk::PhysicalDeviceType) -> i32 {
    match dt {
        vk::PhysicalDeviceType::DISCRETE_GPU => 4,
        vk::PhysicalDeviceType::INTEGRATED_GPU => 3,
        vk::PhysicalDeviceType::VIRTUAL_GPU => 2,
        vk::PhysicalDeviceType::CPU => 1,
        _ => 0,
    }
}

/// Pick the best physical device exposing a compute queue.
///
/// When `prefer_graphics` is set, graphics-capable devices score +100 so they
/// win over compute-only devices on multi-GPU systems — but if no device has
/// graphics, the best compute device still wins (preference, not requirement).
///
/// # Safety
///
/// Vulkan FFI.
unsafe fn pick_physical_device(
    instance: &ash::Instance,
    prefer_graphics: bool,
) -> Result<(vk::PhysicalDevice, QueueFamilyChoice)> {
    let devices = unsafe {
        instance
            .enumerate_physical_devices()
            .map_err(|e| DeviceError::InitFailed(format!("vkEnumeratePhysicalDevices: {e}")))?
    };

    let mut best_score: i32 = -1;
    let mut best: Option<(vk::PhysicalDevice, QueueFamilyChoice)> = None;

    for &pd in &devices {
        let props = unsafe { instance.get_physical_device_properties(pd) };
        let families = unsafe { instance.get_physical_device_queue_family_properties(pd) };
        let Some(choice) = select_queue_families(&families) else {
            continue;
        };

        let mut score = device_type_score(props.device_type);
        if prefer_graphics && choice.graphics.is_some() {
            score += 100;
        }
        if score > best_score {
            best_score = score;
            best = Some((pd, choice));
        }
    }

    best.ok_or_else(|| {
        DeviceError::InitFailed("no Vulkan device with a compute queue found".into())
    })
}

// ── Extension negotiation (graphics readiness) ───────────────────

/// Instance extensions Zunesha requests when a caller wants graphics, so the
/// caller can create whichever surface it needs (platform or headless). Each is
/// enabled only if the loader reports it available — unsupported extensions are
/// silently skipped, so compute-only drivers never fail here.
const INSTANCE_SURFACE_EXTENSIONS: &[&CStr] = &[
    // Universal surface base extension.
    c"VK_KHR_surface",
    // Headless rendering (tests, CI, headless servers). Harmless when unused.
    c"VK_EXT_headless_surface",
    // Platform window-system surfaces.
    c"VK_KHR_xlib_surface",
    c"VK_KHR_xcb_surface",
    c"VK_KHR_wayland_surface",
    c"VK_KHR_win32_surface",
    c"VK_KHR_android_surface",
    c"VK_EXT_metal_surface",
];

/// The swapchain device extension, requested alongside a graphics queue so a
/// consumer can build a swapchain.
const SWAPCHAIN_EXTENSION: &CStr = c"VK_KHR_swapchain";

/// Set of instance extension names the loader reports as available.
fn available_instance_extensions(entry: &Entry) -> HashSet<CString> {
    unsafe { entry.enumerate_instance_extension_properties(None) }
        .map(|props| {
            props
                .iter()
                .map(|p| {
                    // Safety: `extension_name` is a NUL-terminated array
                    // populated by the loader.
                    unsafe { CStr::from_ptr(p.extension_name.as_ptr()) }.to_owned()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Set of device extension names the physical device reports as available.
///
/// # Safety
///
/// Vulkan FFI.
unsafe fn available_device_extensions(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
) -> HashSet<CString> {
    unsafe { instance.enumerate_device_extension_properties(physical_device) }
        .map(|props| {
            props
                .iter()
                .map(|p| unsafe { CStr::from_ptr(p.extension_name.as_ptr()) }.to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Create the logical device requesting every distinct family the choice uses,
/// then retrieve each queue handle. Returns the device, the public [`Queues`],
/// and the compute queue handle (used for staging transfers). When
/// `enabled_device_extensions` is non-empty (graphics requested), they are
/// passed to `vkCreateDevice`.
///
/// # Safety
///
/// Vulkan FFI.
unsafe fn create_logical_device(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    choice: &QueueFamilyChoice,
    enabled_device_extensions: &[*const c_char],
) -> Result<(ash::Device, Queues, vk::Queue)> {
    let queue_priority = 1.0f32;
    let families = choice.distinct_families();
    let queue_cis: Vec<vk::DeviceQueueCreateInfo> = families
        .iter()
        .map(|&fi| {
            vk::DeviceQueueCreateInfo::default()
                .queue_family_index(fi)
                .queue_priorities(std::slice::from_ref(&queue_priority))
        })
        .collect();

    let create_info = vk::DeviceCreateInfo::default()
        .queue_create_infos(&queue_cis)
        .enabled_extension_names(enabled_device_extensions);
    let device = unsafe {
        instance
            .create_device(physical_device, &create_info, None)
            .map_err(|e| DeviceError::InitFailed(format!("vkCreateDevice: {e}")))?
    };

    let compute_q = unsafe { device.get_device_queue(choice.compute, 0) };
    let graphics_q = choice
        .graphics
        .map(|fi| (fi, unsafe { device.get_device_queue(fi, 0) }));
    let transfer_q = choice
        .transfer
        .map(|fi| (fi, unsafe { device.get_device_queue(fi, 0) }));

    let queues = Queues {
        compute: Queue {
            raw: compute_q.as_raw() as *mut c_void,
            family_index: choice.compute,
        },
        graphics: graphics_q.map(|(fi, q)| Queue {
            raw: q.as_raw() as *mut c_void,
            family_index: fi,
        }),
        transfer: transfer_q.map(|(fi, q)| Queue {
            raw: q.as_raw() as *mut c_void,
            family_index: fi,
        }),
    };

    Ok((device, queues, compute_q))
}

// ── Memory allocation ─────────────────────────────────────────────

/// Find a memory type matching `filter` (memory type bits) and `required` flags.
fn find_memory_type_index(
    memory_properties: &vk::PhysicalDeviceMemoryProperties,
    type_filter: u32,
    required: vk::MemoryPropertyFlags,
) -> Result<u32> {
    for i in 0..memory_properties.memory_type_count {
        if (type_filter & (1 << i)) != 0
            && memory_properties.memory_types[i as usize]
                .property_flags
                .contains(required)
        {
            return Ok(i);
        }
    }
    Err(DeviceError::BufferCreationFailed {
        message: "no suitable memory type found".into(),
    })
}

/// Allocate a host-visible, host-coherent buffer of `size` bytes with `usage`.
///
/// Returns `(buffer, memory, mapped_ptr)`. Prefer cached memory on discrete
/// GPUs (avoids PCIe round-trips); fall back to uncached coherent.
///
/// # Safety
///
/// Vulkan FFI.
unsafe fn allocate_buffer(
    device: &ash::Device,
    memory_properties: &vk::PhysicalDeviceMemoryProperties,
    size: vk::DeviceSize,
    usage: vk::BufferUsageFlags,
) -> Result<(vk::Buffer, vk::DeviceMemory, *mut c_void)> {
    let buffer_info = vk::BufferCreateInfo::default()
        .size(size)
        .usage(usage)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);

    let buffer = unsafe {
        device
            .create_buffer(&buffer_info, None)
            .map_err(|e| DeviceError::BufferCreationFailed {
                message: format!("vkCreateBuffer: {e}"),
            })?
    };

    let mem_reqs = unsafe { device.get_buffer_memory_requirements(buffer) };

    let mut flags = vk::MemoryPropertyFlags::HOST_VISIBLE
        | vk::MemoryPropertyFlags::HOST_COHERENT
        | vk::MemoryPropertyFlags::HOST_CACHED;
    let mem_type_index =
        find_memory_type_index(memory_properties, mem_reqs.memory_type_bits, flags).or_else(
            |_| {
                flags =
                    vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
                find_memory_type_index(memory_properties, mem_reqs.memory_type_bits, flags)
            },
        )?;

    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(mem_type_index);

    let memory = unsafe {
        device.allocate_memory(&alloc_info, None).map_err(|e| {
            DeviceError::BufferCreationFailed {
                message: format!("vkAllocateMemory: {e}"),
            }
        })?
    };

    unsafe {
        device.bind_buffer_memory(buffer, memory, 0).map_err(|e| {
            DeviceError::BufferCreationFailed {
                message: format!("vkBindBufferMemory: {e}"),
            }
        })?;
    }

    let mapped = unsafe {
        device
            .map_memory(memory, 0, size, vk::MemoryMapFlags::empty())
            .map_err(|e| DeviceError::BufferCreationFailed {
                message: format!("vkMapMemory: {e}"),
            })?
    };

    Ok((buffer, memory, mapped))
}

/// Allocate a device-local buffer of `size` bytes with `usage` (not mapped —
/// transfers require staging).
///
/// # Safety
///
/// Vulkan FFI.
unsafe fn allocate_device_local_buffer(
    device: &ash::Device,
    memory_properties: &vk::PhysicalDeviceMemoryProperties,
    size: vk::DeviceSize,
    usage: vk::BufferUsageFlags,
) -> Result<(vk::Buffer, vk::DeviceMemory)> {
    let buffer_info = vk::BufferCreateInfo::default()
        .size(size)
        .usage(usage)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);

    let buffer = unsafe {
        device
            .create_buffer(&buffer_info, None)
            .map_err(|e| DeviceError::BufferCreationFailed {
                message: format!("vkCreateBuffer(device-local): {e}"),
            })?
    };

    let mem_reqs = unsafe { device.get_buffer_memory_requirements(buffer) };
    let mem_type_index = find_memory_type_index(
        memory_properties,
        mem_reqs.memory_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
    )?;

    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(mem_type_index);

    let memory = unsafe {
        device.allocate_memory(&alloc_info, None).map_err(|e| {
            DeviceError::BufferCreationFailed {
                message: format!("vkAllocateMemory(device-local): {e}"),
            }
        })?
    };

    unsafe {
        device.bind_buffer_memory(buffer, memory, 0).map_err(|e| {
            DeviceError::BufferCreationFailed {
                message: format!("vkBindBufferMemory(device-local): {e}"),
            }
        })?;
    }

    Ok((buffer, memory))
}

/// Record and submit a one-time transfer command buffer, then wait for it.
///
/// # Safety
///
/// Vulkan FFI.
unsafe fn one_shot_transfer(
    device: &ash::Device,
    command_pool: vk::CommandPool,
    queue: vk::Queue,
    record: impl FnOnce(vk::CommandBuffer),
) -> Result<()> {
    unsafe {
        let alloc_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let cmd = device.allocate_command_buffers(&alloc_info).map_err(|e| {
            DeviceError::BufferCreationFailed {
                message: format!("transfer allocate: {e}"),
            }
        })?[0];

        let begin_info = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        device.begin_command_buffer(cmd, &begin_info).map_err(|e| {
            DeviceError::BufferCreationFailed {
                message: format!("transfer begin: {e}"),
            }
        })?;

        record(cmd);

        device
            .end_command_buffer(cmd)
            .map_err(|e| DeviceError::BufferCreationFailed {
                message: format!("transfer end: {e}"),
            })?;

        let submit_info = vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&cmd));
        device
            .queue_submit(queue, std::slice::from_ref(&submit_info), vk::Fence::null())
            .map_err(|e| DeviceError::BufferCreationFailed {
                message: format!("transfer submit: {e}"),
            })?;
        device
            .queue_wait_idle(queue)
            .map_err(|e| DeviceError::BufferCreationFailed {
                message: format!("transfer wait: {e}"),
            })?;

        device.free_command_buffers(command_pool, std::slice::from_ref(&cmd));
        Ok(())
    }
}

/// Detect whether device-local memory should be used under `Auto` strategy:
/// discrete GPUs, or any device with a >1 GB DEVICE_LOCAL heap, get VRAM.
fn detect_device_local(
    device_type: vk::PhysicalDeviceType,
    memory_properties: &vk::PhysicalDeviceMemoryProperties,
) -> bool {
    if device_type == vk::PhysicalDeviceType::DISCRETE_GPU {
        return true;
    }
    for i in 0..memory_properties.memory_heap_count {
        if memory_properties.memory_heaps[i as usize]
            .flags
            .contains(vk::MemoryHeapFlags::DEVICE_LOCAL)
            && memory_properties.memory_heaps[i as usize].size > 1024 * 1024 * 1024
        {
            return true;
        }
    }
    false
}

/// Round `value` up to a multiple of `alignment`.
fn align_up(value: vk::DeviceSize, alignment: vk::DeviceSize) -> vk::DeviceSize {
    if alignment == 0 {
        value
    } else {
        (value + alignment - 1) & !(alignment - 1)
    }
}

// ── Buffer inner type ─────────────────────────────────────────────

/// Internal state for a Vulkan buffer, stored behind the opaque `Buffer.raw`
/// pointer. Self-contained: clones the device handle so it can destroy itself
/// on drop. (ash `Device` does not auto-destroy on Drop, so the clone is safe.)
struct VulkanBufferInner {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    /// Allocation size — used by the device-local readback copy.
    size: vk::DeviceSize,
    /// Persistently mapped host pointer. For unified memory, the buffer's own
    /// mapping; for device-local, the staging buffer's mapping.
    mapped: *mut c_void,
    /// Staging buffer for device-local memory (`None` if unified).
    staging_buffer: Option<vk::Buffer>,
    /// Staging buffer memory (`None` if unified).
    staging_memory: Option<vk::DeviceMemory>,
    /// Clone of the logical device, used for destroy in drop.
    device: ash::Device,
}

unsafe impl Send for VulkanBufferInner {}
unsafe impl Sync for VulkanBufferInner {}

impl Drop for VulkanBufferInner {
    fn drop(&mut self) {
        // Safety: the buffer owns its resources exclusively; `free_memory`
        // implicitly unmaps any mapped memory. The device handle is valid
        // because buffers must not outlive the device (documented invariant).
        unsafe {
            if let Some(sb) = self.staging_buffer {
                self.device.destroy_buffer(sb, None);
            }
            if let Some(sm) = self.staging_memory {
                self.device.free_memory(sm, None);
            }
            self.device.destroy_buffer(self.buffer, None);
            self.device.free_memory(self.memory, None);
        }
    }
}

/// Drop function stored in [`crate::Buffer`] — drops the `Box<VulkanBufferInner>`.
pub(super) fn drop_vulkan_buffer(raw: *mut c_void) {
    if !raw.is_null() {
        // Safety: `raw` was produced by `Box::into_raw` in `create_buffer`.
        unsafe {
            drop(Box::from_raw(raw as *mut VulkanBufferInner));
        }
    }
}

// ── Device backend ────────────────────────────────────────────────

/// Vulkan implementation of [`Device`].
///
/// Owns the Vulkan instance, physical device, logical device, queues, transfer
/// command pool, and memory properties. Buffers carry a device clone and
/// self-destruct; the device itself is destroyed in [`Drop`].
///
/// # Drop order
///
/// `destroy_command_pool` → `destroy_device` → `destroy_instance` (the
/// Vulkan-mandated order) in the manual [`Drop`] impl.
pub struct VulkanDevice {
    device: ash::Device,
    instance: ash::Instance,
    _entry: Entry,
    /// Physical device — exposed via [`VulkanDevice::physical_device`] for
    /// consumers (Goldenweek) that query surface capabilities.
    physical_device: vk::PhysicalDevice,
    queues: Queues,
    /// Compute queue handle, used for staging transfers.
    compute_queue: vk::Queue,
    /// Transient command pool (on the compute family) for one-shot transfers.
    command_pool: vk::CommandPool,
    memory_properties: vk::PhysicalDeviceMemoryProperties,
    memory_strategy: MemoryStrategy,
    /// Effective memory placement: device-local (VRAM) vs host-visible.
    uses_device_local: bool,
    limits: DeviceLimits,
    epoch: GpuEpochTracker,
}

impl Drop for VulkanDevice {
    fn drop(&mut self) {
        // Safety: command pool before device before instance (Vulkan order).
        // Buffers must already have been dropped by the caller (documented
        // invariant) — they hold a cloned device handle.
        unsafe {
            self.device.device_wait_idle().ok();
            self.device.destroy_command_pool(self.command_pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

impl VulkanDevice {
    /// Build a device with an explicit memory strategy and graphics preference.
    fn build(strategy: MemoryStrategy, prefer_graphics: bool) -> Result<Self> {
        let entry = unsafe { Entry::load().map_err(|e| DeviceError::InitFailed(format!("{e}")))? };

        // Query the available instance version before requesting one. Requesting
        // an unsupported version can crash drivers (Borsalino issue #34).
        let available_version = unsafe { entry.try_enumerate_instance_version() }
            .ok()
            .flatten();
        let api_version = available_version.unwrap_or(vk::API_VERSION_1_1);

        let app_name = CString::new("zunesha").unwrap();
        let engine_name = CString::new("zunesha").unwrap();
        let app_info = vk::ApplicationInfo::default()
            .application_name(&app_name)
            .engine_name(&engine_name)
            .api_version(api_version);
        // When graphics is requested, enable every available surface-related
        // instance extension so the caller can create whichever surface it needs
        // (platform or headless). Unsupported extensions are skipped, so a
        // compute-only loader never fails here.
        let instance_extensions: Vec<*const c_char> = if prefer_graphics {
            let available = available_instance_extensions(&entry);
            INSTANCE_SURFACE_EXTENSIONS
                .iter()
                .filter(|ext| available.contains(**ext))
                .map(|ext| ext.as_ptr())
                .collect()
        } else {
            Vec::new()
        };
        let instance_create_info = vk::InstanceCreateInfo::default()
            .application_info(&app_info)
            .enabled_extension_names(&instance_extensions);

        let instance = unsafe {
            entry
                .create_instance(&instance_create_info, None)
                .map_err(|e| DeviceError::InitFailed(format!("vkCreateInstance: {e}")))?
        };

        let (physical_device, choice) =
            unsafe { pick_physical_device(&instance, prefer_graphics) }?;

        let props = unsafe { instance.get_physical_device_properties(physical_device) };
        let memory_properties =
            unsafe { instance.get_physical_device_memory_properties(physical_device) };
        let limits = DeviceLimits {
            min_storage_buffer_offset_alignment: props.limits.min_storage_buffer_offset_alignment,
        };

        // Enable the swapchain device extension when graphics is requested and
        // the chosen physical device supports it (compute-only HW will not).
        let device_extensions: Vec<*const c_char> = if prefer_graphics {
            let available = unsafe { available_device_extensions(&instance, physical_device) };
            if available.contains(SWAPCHAIN_EXTENSION) {
                vec![SWAPCHAIN_EXTENSION.as_ptr()]
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

        let (device, queues, compute_queue) = unsafe {
            create_logical_device(&instance, physical_device, &choice, &device_extensions)
        }?;

        // Transient transfer command pool on the compute family (compute always
        // supports transfer; dedicated transfer family is exposed to consumers
        // but Zunesha's own staging uses the compute queue).
        let pool_ci = vk::CommandPoolCreateInfo::default()
            .flags(vk::CommandPoolCreateFlags::TRANSIENT)
            .queue_family_index(choice.compute);
        let command_pool = unsafe {
            device
                .create_command_pool(&pool_ci, None)
                .map_err(|e| DeviceError::InitFailed(format!("vkCreateCommandPool: {e}")))?
        };

        let uses_device_local = match strategy {
            MemoryStrategy::DeviceLocal => true,
            MemoryStrategy::Unified => false,
            MemoryStrategy::Auto => detect_device_local(props.device_type, &memory_properties),
        };

        Ok(Self {
            device,
            instance,
            _entry: entry,
            physical_device,
            queues,
            compute_queue,
            command_pool,
            memory_properties,
            memory_strategy: strategy,
            uses_device_local,
            limits,
            epoch: GpuEpochTracker::new(),
        })
    }

    /// Buffer usage flags broad enough for both compute (storage) and graphics
    /// (vertex) consumers — a Zunesha buffer serves either.
    fn buffer_usage() -> vk::BufferUsageFlags {
        vk::BufferUsageFlags::STORAGE_BUFFER
            | vk::BufferUsageFlags::VERTEX_BUFFER
            | vk::BufferUsageFlags::TRANSFER_SRC
            | vk::BufferUsageFlags::TRANSFER_DST
    }

    /// Raw Vulkan instance handle.
    ///
    /// Exposed so graphics consumers (Goldenweek) can create a surface against
    /// the instance that owns this device, and query surface capabilities.
    #[must_use]
    pub fn raw_instance(&self) -> ash::Instance {
        self.instance.clone()
    }

    /// Raw Vulkan logical-device handle (a cheap clone — `ash::Device` is a
    /// refcount-free handle wrapper; the original is destroyed in [`Drop`]).
    ///
    /// Graphics consumers use this to build swapchains, render passes, and
    /// pipelines against this device.
    #[must_use]
    pub fn raw_device(&self) -> ash::Device {
        self.device.clone()
    }

    /// Raw physical-device handle, for surface-format and capability queries.
    #[must_use]
    pub fn physical_device(&self) -> vk::PhysicalDevice {
        self.physical_device
    }

    /// Physical-device memory properties, for image/memory allocation by
    /// graphics consumers (e.g. offscreen render targets).
    pub fn memory_properties(&self) -> vk::PhysicalDeviceMemoryProperties {
        self.memory_properties
    }
}

impl Device for VulkanDevice {
    fn init() -> Result<Self> {
        Self::build(MemoryStrategy::Auto, false)
    }

    fn init_with_strategy(strategy: MemoryStrategy) -> Result<Self> {
        Self::build(strategy, false)
    }

    fn init_with(request: InitRequest) -> Result<Self> {
        Self::build(request.memory, request.prefer_graphics)
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

    fn create_buffer<T: bytemuck::Pod>(&self, data: &[T]) -> Result<crate::Buffer> {
        let byte_len = mem::size_of_val(data) as vk::DeviceSize;
        let aligned = if byte_len == 0 {
            self.limits.min_storage_buffer_offset_alignment
        } else {
            align_up(byte_len, self.limits.min_storage_buffer_offset_alignment)
        };
        let usage = Self::buffer_usage();

        let (buffer, memory, mapped, staging_buffer, staging_memory) = if self.uses_device_local {
            // Device-local buffer + host-visible staging; copy data → staging,
            // then staging → device.
            let (dev_buf, dev_mem) = unsafe {
                allocate_device_local_buffer(&self.device, &self.memory_properties, aligned, usage)?
            };
            let (stg_buf, stg_mem, stg_mapped) = unsafe {
                allocate_buffer(
                    &self.device,
                    &self.memory_properties,
                    aligned,
                    vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST,
                )?
            };
            if byte_len > 0 {
                unsafe {
                    ptr::copy_nonoverlapping(
                        data.as_ptr() as *const c_void,
                        stg_mapped,
                        byte_len as usize,
                    );
                }
                unsafe {
                    one_shot_transfer(
                        &self.device,
                        self.command_pool,
                        self.compute_queue,
                        |cmd| {
                            let copy = vk::BufferCopy::default().size(aligned);
                            self.device.cmd_copy_buffer(
                                cmd,
                                stg_buf,
                                dev_buf,
                                std::slice::from_ref(&copy),
                            );
                        },
                    )?;
                }
            }
            (dev_buf, dev_mem, stg_mapped, Some(stg_buf), Some(stg_mem))
        } else {
            // Unified memory: single host-visible buffer.
            let (buf, mem, mapped) =
                unsafe { allocate_buffer(&self.device, &self.memory_properties, aligned, usage)? };
            if byte_len > 0 {
                unsafe {
                    ptr::copy_nonoverlapping(
                        data.as_ptr() as *const c_void,
                        mapped,
                        byte_len as usize,
                    );
                }
            }
            (buf, mem, mapped, None, None)
        };

        let inner = Box::new(VulkanBufferInner {
            buffer,
            memory,
            size: aligned,
            mapped,
            staging_buffer,
            staging_memory,
            device: self.device.clone(),
        });
        Ok(crate::Buffer {
            raw: Box::into_raw(inner) as *mut c_void,
            len: byte_len as usize,
            drop_fn: drop_vulkan_buffer,
        })
    }

    fn create_buffer_uninit<T: bytemuck::Pod>(&self, len: usize) -> Result<crate::Buffer> {
        let byte_len = (len * mem::size_of::<T>()) as vk::DeviceSize;
        let aligned = if byte_len == 0 {
            self.limits.min_storage_buffer_offset_alignment
        } else {
            align_up(byte_len, self.limits.min_storage_buffer_offset_alignment)
        };
        let usage = Self::buffer_usage();

        let (buffer, memory, mapped, staging_buffer, staging_memory) = if self.uses_device_local {
            let (dev_buf, dev_mem) = unsafe {
                allocate_device_local_buffer(&self.device, &self.memory_properties, aligned, usage)?
            };
            let (stg_buf, stg_mem, stg_mapped) = unsafe {
                allocate_buffer(
                    &self.device,
                    &self.memory_properties,
                    aligned,
                    vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST,
                )?
            };
            (dev_buf, dev_mem, stg_mapped, Some(stg_buf), Some(stg_mem))
        } else {
            let (buf, mem, mapped) =
                unsafe { allocate_buffer(&self.device, &self.memory_properties, aligned, usage)? };
            (buf, mem, mapped, None, None)
        };

        let inner = Box::new(VulkanBufferInner {
            buffer,
            memory,
            size: aligned,
            mapped,
            staging_buffer,
            staging_memory,
            device: self.device.clone(),
        });
        Ok(crate::Buffer {
            raw: Box::into_raw(inner) as *mut c_void,
            len: byte_len as usize,
            drop_fn: drop_vulkan_buffer,
        })
    }

    fn read_buffer<T: bytemuck::Pod>(&self, buffer: &crate::Buffer) -> Result<Vec<T>> {
        // Safety: `raw` was produced by `Box::into_raw::<VulkanBufferInner>` and
        // is still valid (buffer not dropped).
        let inner = unsafe { &*(buffer.raw as *const VulkanBufferInner) };

        // Device-local buffers: copy device → staging, then read the staging
        // mapping. Unified buffers: read the buffer's own mapping directly.
        if let Some(stg_buf) = inner.staging_buffer {
            unsafe {
                one_shot_transfer(&self.device, self.command_pool, self.compute_queue, |cmd| {
                    let copy = vk::BufferCopy::default().size(inner.size);
                    self.device.cmd_copy_buffer(
                        cmd,
                        inner.buffer,
                        stg_buf,
                        std::slice::from_ref(&copy),
                    );
                })?;
            }
        }

        if inner.mapped.is_null() {
            return Err(DeviceError::BufferReadFailed {
                message: "buffer is not host-visible (no staging mapping)".into(),
            });
        }

        let count = if mem::size_of::<T>() == 0 {
            0
        } else {
            buffer.len / mem::size_of::<T>()
        };
        let src = inner.mapped as *const T;
        let slice = unsafe { std::slice::from_raw_parts(src, count) };
        Ok(slice.to_vec())
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
    use crate::Device;
    use serial_test::serial;

    /// A graphics-requested init selects a device exposing a graphics queue and
    /// supporting `VK_KHR_swapchain` (the rendering prerequisite). On a
    /// compute-only host neither is guaranteed — the test then reports the
    /// degradation instead of asserting.
    #[test]
    #[serial]
    fn graphics_init_enables_swapchain_and_graphics_queue() {
        let device = match VulkanDevice::init_with(InitRequest::prefer_graphics()) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("skipping: no Vulkan device ({e})");
                return;
            }
        };
        let q = device.queues();
        // Exercise the raw-handle accessors that Goldenweek will consume.
        let inst = device.raw_instance();
        let pd = device.physical_device();
        let available = unsafe { available_device_extensions(&inst, pd) };
        let swapchain_supported = available.contains(SWAPCHAIN_EXTENSION);
        if q.has_graphics() {
            assert!(
                swapchain_supported,
                "graphics queue present but VK_KHR_swapchain unsupported"
            );
            println!(
                "graphics-ready: graphics family = {:?}, VK_KHR_swapchain supported",
                q.graphics.map(|g| g.family_index)
            );
        } else {
            println!(
                "compute-only host (no graphics queue); swapchain_supported = {swapchain_supported}"
            );
        }
    }

    /// A real device must initialise on the host's Vulkan, and `compute` is
    /// always present (Zunesha's baseline contract).
    #[test]
    #[serial]
    fn vulkan_device_inits_and_exposes_compute() {
        let device = match VulkanDevice::init() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("skipping: no Vulkan device available ({e})");
                return;
            }
        };
        assert!(
            device.queues().compute.family_index != u32::MAX,
            "compute family must be a real index"
        );
        println!("compute family = {}", device.queues().compute.family_index);
    }

    /// On a device with a dedicated async-compute family (e.g. the RTX 5080),
    /// policy A must place `compute` on a family distinct from `graphics`.
    #[test]
    #[serial]
    fn dedicated_compute_family_is_selected_when_present() {
        let device = match VulkanDevice::init() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("skipping: no Vulkan device ({e})");
                return;
            }
        };
        let q = device.queues();
        let props = unsafe {
            device
                .instance
                .get_physical_device_queue_family_properties(device.physical_device)
        };
        let has_dedicated_compute = props.iter().any(|f| {
            f.queue_flags.contains(vk::QueueFlags::COMPUTE)
                && !f.queue_flags.contains(vk::QueueFlags::GRAPHICS)
        });
        if let Some(gfx) = q.graphics {
            if has_dedicated_compute {
                assert_ne!(
                    q.compute.family_index, gfx.family_index,
                    "compute should use a dedicated family distinct from graphics"
                );
                println!(
                    "async-compute verified: compute={} graphics={} transfer={:?}",
                    q.compute.family_index,
                    gfx.family_index,
                    q.transfer.map(|t| t.family_index)
                );
            } else {
                println!("no dedicated compute family on this device; unified path verified");
            }
        }
    }

    /// Host-visible (unified) buffer roundtrip.
    #[test]
    #[serial]
    fn buffer_roundtrip_unified() {
        let device = match VulkanDevice::init_with_strategy(MemoryStrategy::Unified) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("skipping: no Vulkan device ({e})");
                return;
            }
        };
        let input = [1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0];
        let buffer = device.create_buffer(&input).expect("create_buffer");
        let output: Vec<f32> = device.read_buffer(&buffer).expect("read_buffer");
        assert_eq!(output, input);
    }

    /// Device-local (VRAM) buffer roundtrip — exercises the staging path
    /// (data → staging → device, then device → staging → read).
    #[test]
    #[serial]
    fn buffer_roundtrip_device_local() {
        let device = match VulkanDevice::init_with_strategy(MemoryStrategy::DeviceLocal) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("skipping: no Vulkan device ({e})");
                return;
            }
        };
        let input: Vec<u32> = (0..1000).collect();
        let buffer = device
            .create_buffer(&input)
            .expect("create_buffer (device-local)");
        let output: Vec<u32> = device
            .read_buffer(&buffer)
            .expect("read_buffer (device-local)");
        assert_eq!(output, input);
        println!(
            "device-local roundtrip ok (placement={}, strategy={:?})",
            device.uses_device_local, device.memory_strategy
        );
    }

    /// A fresh device is quiescent (epoch counter at zero).
    #[test]
    #[serial]
    fn fresh_device_is_quiescent() {
        let device = match VulkanDevice::init() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("skipping: no Vulkan device ({e})");
                return;
            }
        };
        assert_eq!(device.in_flight(), 0);
        assert!(device.is_quiescent());
        assert!(device.prove_quiescent().is_some());
    }

    /// `init_with(prefer_graphics)` exposes graphics when the host has a
    /// graphics-capable device.
    #[test]
    #[serial]
    fn prefer_graphics_exposes_graphics_when_available() {
        let device = match VulkanDevice::init_with(InitRequest::prefer_graphics()) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("skipping: no Vulkan device ({e})");
                return;
            }
        };
        if device.queues().has_graphics() {
            println!("graphics queue present under prefer_graphics");
        } else {
            println!(
                "no graphics on this host (compute-only) — preference honoured, no requirement"
            );
        }
    }
}
