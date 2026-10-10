// Copyright (C) 2026 Industrial Algebra
// SPDX-License-Identifier: Apache-2.0

//! Vulkan backend for Zunesha.
//!
//! Available on Linux and Windows with the `vulkan` feature enabled. Ported
//! from Borsalino's `vulkan.rs` device/memory/buffer layer, with two
//! Zunesha-specific changes:
//!
//! - **Capability-driven queue selection.** `select_queue_families` prefers a
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
    Device, DeviceError, DeviceLimits, GpuEpochTracker, InitRequest, MemoryPlacement,
    MemoryStrategy, Queue, Queues, Result,
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
/// When `device_hint` is set, a device whose name contains the substring
/// (case-insensitive) wins outright over scoring — a matched device still must
/// expose a compute queue; no match falls back to scoring.
///
/// # Safety
///
/// Vulkan FFI.
unsafe fn pick_physical_device(
    instance: &ash::Instance,
    prefer_graphics: bool,
    device_hint: Option<&str>,
) -> Result<(vk::PhysicalDevice, QueueFamilyChoice)> {
    let devices = unsafe {
        instance
            .enumerate_physical_devices()
            .map_err(|e| DeviceError::InitFailed(format!("vkEnumeratePhysicalDevices: {e}")))?
    };

    let mut best_score: i32 = -1;
    let mut best: Option<(vk::PhysicalDevice, QueueFamilyChoice)> = None;
    let mut hinted: Option<(vk::PhysicalDevice, QueueFamilyChoice)> = None;

    for &pd in &devices {
        let props = unsafe { instance.get_physical_device_properties(pd) };
        let families = unsafe { instance.get_physical_device_queue_family_properties(pd) };
        let Some(choice) = select_queue_families(&families) else {
            continue;
        };

        let has_graphics = choice.graphics.is_some();
        if let Some(hint) = device_hint {
            // Safety: `device_name` is a NUL-terminated array populated by the
            // driver.
            let name = unsafe { CStr::from_ptr(props.device_name.as_ptr()) }.to_string_lossy();
            if name.to_lowercase().contains(&hint.to_lowercase()) && hinted.is_none() {
                hinted = Some((pd, choice));
                continue;
            }
        }
        let mut score = device_type_score(props.device_type);
        if prefer_graphics && has_graphics {
            score += 100;
        }
        if score > best_score {
            best_score = score;
            best = Some((pd, choice));
        }
    }

    let chosen = hinted.or(best);
    chosen.ok_or_else(|| {
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
    let mem_type_index = {
        let preferred = find_memory_type_index(memory_properties, mem_reqs.memory_type_bits, flags);
        match preferred {
            Ok(idx) => Ok(idx),
            Err(e) => {
                flags =
                    vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
                find_memory_type_index(memory_properties, mem_reqs.memory_type_bits, flags)
                    .inspect_err(|_| {
                        // Free partial state: the buffer created above.
                        unsafe { device.destroy_buffer(buffer, None) };
                    })
                    .map_err(|_| e)
            }
        }
    }?;

    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(mem_type_index);

    let memory = match unsafe { device.allocate_memory(&alloc_info, None) } {
        Ok(memory) => memory,
        Err(e) => {
            unsafe { device.destroy_buffer(buffer, None) };
            return Err(DeviceError::BufferCreationFailed {
                message: format!("vkAllocateMemory: {e}"),
            });
        }
    };

    if let Err(e) = unsafe { device.bind_buffer_memory(buffer, memory, 0) } {
        unsafe {
            device.free_memory(memory, None);
            device.destroy_buffer(buffer, None);
        }
        return Err(DeviceError::BufferCreationFailed {
            message: format!("vkBindBufferMemory: {e}"),
        });
    }

    let mapped = match unsafe { device.map_memory(memory, 0, size, vk::MemoryMapFlags::empty()) } {
        Ok(mapped) => mapped,
        Err(e) => {
            unsafe {
                device.free_memory(memory, None);
                device.destroy_buffer(buffer, None);
            }
            return Err(DeviceError::BufferCreationFailed {
                message: format!("vkMapMemory: {e}"),
            });
        }
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
    )
    .inspect_err(|_| {
        // Free partial state: the buffer created above.
        unsafe { device.destroy_buffer(buffer, None) };
    })?;

    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(mem_type_index);

    let memory = match unsafe { device.allocate_memory(&alloc_info, None) } {
        Ok(memory) => memory,
        Err(e) => {
            unsafe { device.destroy_buffer(buffer, None) };
            return Err(DeviceError::BufferCreationFailed {
                message: format!("vkAllocateMemory(device-local): {e}"),
            });
        }
    };

    if let Err(e) = unsafe { device.bind_buffer_memory(buffer, memory, 0) } {
        unsafe {
            device.free_memory(memory, None);
            device.destroy_buffer(buffer, None);
        }
        return Err(DeviceError::BufferCreationFailed {
            message: format!("vkBindBufferMemory(device-local): {e}"),
        });
    }

    Ok((buffer, memory))
}

/// Record and submit a one-time transfer command buffer, then wait for it.
///
/// # Safety
///
/// Vulkan FFI.
/// One-shot transfer. The error's `bool` is **"submission reached the
/// GPU"**: `false` means nothing was submitted (allocate/begin/end/submit
/// failed — the recorded resources are untouched and safe to reclaim);
/// `true` means submission succeeded but the wait failed — the GPU's
/// access to the recorded resources is **indeterminate** (the spec
/// permits e.g. host-OOM from the wait; it is neither proof of device
/// loss nor of completion), so reclaiming them would be a driver-level
/// use-after-free. The sound side is to leak (review r2 P1).
impl VulkanDevice {
    /// One-shot transfer. The error's `bool` is **"submission reached the
    /// GPU"**: `false` means nothing was submitted (allocate/begin/end/
    /// submit failed — the recorded resources are untouched and safe to
    /// reclaim); `true` means submission succeeded but the wait failed —
    /// the GPU's access to the recorded resources is **indeterminate**
    /// (the spec permits e.g. host-OOM from the wait; it is neither proof
    /// of device loss nor of completion), so reclaiming them would be a
    /// driver-level use-after-free. In that case the
    /// [`TransferProtocol::unconfirmed`] flag is
    /// set: every destruction path (buffer drop, device teardown) then
    /// quiesces before destroying — or leaks. (Reviews r2+r3 P1.)
    fn one_shot_transfer(
        &self,
        queue: vk::Queue,
        record: impl FnOnce(vk::CommandBuffer),
    ) -> std::result::Result<(), (DeviceError, bool)> {
        unsafe {
            let alloc_info = vk::CommandBufferAllocateInfo::default()
                .command_pool(self.command_pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1);
            let cmd = match self.device.allocate_command_buffers(&alloc_info) {
                Ok(cmds) => cmds[0],
                Err(e) => {
                    return Err((
                        DeviceError::BufferCreationFailed {
                            message: format!("transfer allocate: {e}"),
                        },
                        false,
                    ));
                }
            };

            let begin_info = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            if let Err(e) = self.device.begin_command_buffer(cmd, &begin_info) {
                // Never submitted: the command buffer is not pending and may
                // be freed (closes the r1 P3 cb-retention follow-up for the
                // pre-submit failure paths).
                self.device.free_command_buffers(self.command_pool, &[cmd]);
                return Err((
                    DeviceError::BufferCreationFailed {
                        message: format!("transfer begin: {e}"),
                    },
                    false,
                ));
            }

            record(cmd);

            if let Err(e) = self.device.end_command_buffer(cmd) {
                self.device.free_command_buffers(self.command_pool, &[cmd]);
                return Err((
                    DeviceError::BufferCreationFailed {
                        message: format!("transfer end: {e}"),
                    },
                    false,
                ));
            }

            let submit_info = vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&cmd));
            if let Err(e) = self.device.queue_submit(
                queue,
                std::slice::from_ref(&submit_info),
                vk::Fence::null(),
            ) {
                self.device.free_command_buffers(self.command_pool, &[cmd]);
                return Err((
                    DeviceError::BufferCreationFailed {
                        message: format!("transfer submit: {e}"),
                    },
                    false,
                ));
            }

            // From here the command buffer is pending: it must NOT be freed
            // on the failure path below — only the pool's destruction (or a
            // later successful wait) can release it.
            if let Err(e) = self.device.queue_wait_idle(queue) {
                // Submission reached the GPU; its completion is unconfirmed.
                // Destruction paths must now quiesce-or-leak (r3 P1s).
                self.protocol
                    .unconfirmed
                    .store(true, std::sync::atomic::Ordering::Release);
                return Err((
                    DeviceError::BufferCreationFailed {
                        message: format!("transfer wait: {e}"),
                    },
                    true,
                ));
            }

            self.device.free_command_buffers(self.command_pool, &[cmd]);
            Ok(())
        }
    }

    /// Best-effort quiesce before destroying resources a possibly-pending
    /// submission may still touch — SERIALIZED with submissions through
    /// the shared protocol lock (a lock-free quiesce could clear a taint
    /// a concurrent submitter had just set, or overlap `device_wait_idle`
    /// with an in-flight submit; reviews r4 P1s). Returns `true` when
    /// destruction is safe; `false` means the caller MUST leak rather
    /// than destroy.
    ///
    /// **Scope:** this resolves only the substrate's TRANSFER taint —
    /// a submission Zunesha itself recorded whose wait failed. It does
    /// NOT retire consumer submissions (with_compute_queue closures are
    /// expected to complete their waits before returning) and does NOT
    /// exclude queues the consumer may have acquired outside the
    /// substrate (raw-handle unsafe use). Callers destroying resources
    /// touched by such work must handle that retirement themselves
    /// (review r5 P3).
    pub fn quiesce_for_teardown(&self) -> bool {
        self.protocol.quiesce(&self.device)
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
/// The submission/teardown protocol shared by the device and every
/// buffer inner: the compute-queue/transfer-pool `submit_lock` (all
/// submissions AND all quiesces serialize through it — a quiesce's
/// load→wait→clear must be atomic with respect to new submissions, or a
/// lost-taint race lets a later destroy race a pending transfer) and the
/// `unconfirmed` flag (a submission reached the GPU but its wait failed).
/// (Reviews r2–r4 P1s.)
struct TransferProtocol {
    unconfirmed: std::sync::atomic::AtomicBool,
    submit_lock: std::sync::Mutex<()>,
}

thread_local! {
    /// Per-thread nesting depth inside [`VulkanDevice::with_compute_queue`]
    /// (or the substrate's internal submission paths). `> 0` ⇒ this thread
    /// holds `submit_lock`, so BOTH a nested protocol entry (the
    /// substrate's own device-local staging transfer) and a quiesce
    /// reached re-entrantly (a buffer dropping at closure end) skip
    /// re-acquiring the non-reentrant mutex — the cross-thread mutual
    /// exclusion it exists to provide is already held by this thread
    /// (review r5 P1; nested-entry deadlock pre-existing since 0.1.1).
    static PROTOCOL_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

impl TransferProtocol {
    /// Quiesce-or-leak, SERIALIZED with submissions via `submit_lock`.
    /// Returns `true` when destruction is safe (no unconfirmed
    /// submission, or `device_wait_idle` succeeded — which also clears
    /// the flag under the lock, so a concurrent taint cannot be lost);
    /// `false` when the wait failed again — the caller MUST leak rather
    /// than destroy (persistent host-OOM or device loss are
    /// indistinguishable here; leaking is sound for both).
    ///
    /// **Scope:** this resolves only the substrate's TRANSFER taint.
    /// Consumer retirement (all non-Zunesha submissions completed before
    /// the last handle drops) and exclusion of other queues remain
    /// prerequisites of the caller (review r5 P3).
    fn quiesce(&self, device: &ash::Device) -> bool {
        use std::sync::atomic::Ordering;
        // Reentrancy: if this thread is inside with_compute_queue it
        // already holds submit_lock — acquiring it again would deadlock
        // (std's Mutex is not reentrant), and no other thread can submit
        // meanwhile, so the load→wait→clear atomicity holds without the
        // re-acquisition.
        let reentrant = PROTOCOL_DEPTH.get() > 0;
        // Poison-steal: teardown must proceed after a submitter panicked.
        let _guard = if reentrant {
            None
        } else {
            Some(
                self.submit_lock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            )
        };
        if !self.unconfirmed.load(Ordering::Acquire) {
            return true;
        }
        match unsafe { device.device_wait_idle() } {
            Ok(()) => {
                // Holding submit_lock: no submission can have started
                // since the load, so the wait retired everything the flag
                // could refer to — the clear cannot lose a taint.
                self.unconfirmed.store(false, Ordering::Release);
                true
            }
            Err(_) => false,
        }
    }
}

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
    /// Shared with the device: carries the submission lock and the
    /// unconfirmed-submission flag — destruction quiesces through it or
    /// leaks (reviews r3+r4 P1s).
    protocol: std::sync::Arc<TransferProtocol>,
}

unsafe impl Send for VulkanBufferInner {}
unsafe impl Sync for VulkanBufferInner {}

impl Drop for VulkanBufferInner {
    fn drop(&mut self) {
        // Reviews r3+r4 P1s (readback path): if a submission touching this
        // buffer is UNCONFIRMED (submit succeeded, wait failed), the GPU's
        // access is indeterminate — destroying now would be a driver-level
        // use-after-free. Quiesce SERIALIZED with submissions (the shared
        // protocol lock); on a second wait failure leak rather than
        // destroy (sound for both persistent host-OOM and device loss,
        // which cannot be distinguished here).
        if !self.protocol.quiesce(&self.device) {
            eprintln!(
                "zunesha: leaking buffer — a submission is unconfirmed and the device will not quiesce"
            );
            return;
        }
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
    /// Serializes every submission to the shared compute queue and every
    /// use of `command_pool` — both are *externally synchronized* Vulkan
    /// objects (one host thread at a time, spec §7). The substrate's own
    /// staging transfers, consumer submissions
    /// ([`with_compute_queue`](VulkanDevice::with_compute_queue)), AND
    /// teardown quiesces serialize through this lock (shared with buffer
    /// inners via [`TransferProtocol`]), which is what makes
    /// `Send + Sync` sound (reviews r1+r4 P1s).
    protocol: std::sync::Arc<TransferProtocol>,
    memory_properties: vk::PhysicalDeviceMemoryProperties,
    memory_strategy: MemoryStrategy,
    /// Effective memory placement: device-local (VRAM) vs host-visible.
    uses_device_local: bool,
    limits: DeviceLimits,
    epoch: GpuEpochTracker,
}

impl Drop for VulkanDevice {
    fn drop(&mut self) {
        // Defensive idle is GATED on the epoch tracker, not blanket:
        // Zunesha's own one-shot transfers retire synchronously, and the
        // consumer contract is that all submissions are retired before the
        // last handle drops (Borsalino pulses wait their fence *before*
        // releasing their `Arc`). A blanket `device_wait_idle` on every
        // last-release teardown stampedes driver-internal locks under
        // parallel test load — the failure mode Borsalino removed from its
        // own teardown and review round 1 flagged as regressed here. When
        // consumer dispatch accounting is wired into the tracker (ADR 0003
        // end state), this backstop fires exactly when work is outstanding.
        if self.epoch.in_flight() > 0 {
            unsafe {
                self.device.device_wait_idle().ok();
            }
        }
        // Review r3 P1 (teardown path): a retained one-shot transfer whose
        // wait failed leaves a PENDING command buffer in this pool —
        // destroying the pool would be UB. Quiesce best-effort; if the
        // device will not quiesce, leak everything (sound for persistent
        // host-OOM and device loss alike, which cannot be distinguished).
        if !self.quiesce_for_teardown() {
            eprintln!(
                "zunesha: leaking device teardown — a submission is unconfirmed and the device will not quiesce"
            );
            return;
        }
        // Safety: command pool before device before instance (Vulkan order).
        // Buffers must already have been dropped by the caller (documented
        // invariant) — they hold a cloned device handle.
        unsafe {
            self.device.destroy_command_pool(self.command_pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

// Compile-time proof that the device is shareable behind an `Arc` across
// threads — Borsalino's consumer contract (its buffers/pulses may outlive
// the backend and are moved between threads). Sound because: every field is
// either an ash handle (Send+Sync by ash's own impls), the loader `Entry`
// (fn tables + `Arc<Library>`), or the atomic epoch tracker; and every
// mutation of the externally-synchronized shared objects (the compute queue
// and the transfer command pool) is serialized through `submit_lock` —
// internally by the staging paths, and by consumers via
// `with_compute_queue` (review round 1, P1: without that protocol, safe
// code could race `vkQueueSubmit`/command-pool use from cloned `Arc`s).
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<VulkanDevice>();
};

impl VulkanDevice {
    /// Build a device from a full [`InitRequest`].
    fn build(request: InitRequest) -> Result<Self> {
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
        let instance_extensions: Vec<*const c_char> = if request.prefer_graphics {
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

        let (physical_device, choice) = unsafe {
            pick_physical_device(
                &instance,
                request.prefer_graphics,
                request.device_hint.as_deref(),
            )
        }?;

        let props = unsafe { instance.get_physical_device_properties(physical_device) };
        let memory_properties =
            unsafe { instance.get_physical_device_memory_properties(physical_device) };
        let limits = DeviceLimits {
            min_storage_buffer_offset_alignment: props.limits.min_storage_buffer_offset_alignment,
        };

        // Enable the swapchain device extension when graphics is requested and
        // the chosen physical device supports it (compute-only HW will not).
        let device_extensions: Vec<*const c_char> = if request.prefer_graphics {
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

        let uses_device_local = match request.memory {
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
            memory_strategy: request.memory,
            uses_device_local,
            limits,
            epoch: GpuEpochTracker::new(),
            protocol: std::sync::Arc::new(TransferProtocol {
                unconfirmed: std::sync::atomic::AtomicBool::new(false),
                submit_lock: std::sync::Mutex::new(()),
            }),
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

    /// Shared buffer allocation path.
    ///
    /// `data` uploads `byte_len` bytes when present (`None` = uninitialised).
    /// `force_device_local` takes the device-local + staging path even when
    /// the negotiated strategy is host-visible — the
    /// [`Device::create_device_buffer`](crate::Device::create_device_buffer)
    /// contract: GPU-resident data under a forced-`Unified` device. Found by
    /// the Borsalino migration survey — note Borsalino's own override
    /// diverges from its trait docs the *other* way (host-visible under
    /// forced `Unified`); this implements the documented contract, not
    /// Borsalino's behavior.
    fn buffer_new(
        &self,
        byte_len: vk::DeviceSize,
        data: Option<*const c_void>,
        force_device_local: bool,
    ) -> Result<crate::Buffer> {
        let aligned = if byte_len == 0 {
            self.limits.min_storage_buffer_offset_alignment
        } else {
            align_up(byte_len, self.limits.min_storage_buffer_offset_alignment)
        };
        let usage = Self::buffer_usage();

        let (buffer, memory, mapped, staging_buffer, staging_memory) = if self.uses_device_local
            || force_device_local
        {
            // Device-local buffer + host-visible staging; copy data →
            // staging, then staging → device.
            let (dev_buf, dev_mem) = unsafe {
                allocate_device_local_buffer(&self.device, &self.memory_properties, aligned, usage)?
            };
            // Review (Borsalino PR #60 r1, substrate half): if the staging
            // allocation fails, free the device allocation before
            // propagating — the owning inner does not exist yet.
            let (stg_buf, stg_mem, stg_mapped) = match unsafe {
                allocate_buffer(
                    &self.device,
                    &self.memory_properties,
                    aligned,
                    vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST,
                )
            } {
                Ok(alloc) => alloc,
                Err(e) => {
                    unsafe {
                        self.device.destroy_buffer(dev_buf, None);
                        self.device.free_memory(dev_mem, None);
                    }
                    return Err(e);
                }
            };
            if let Some(src) = data {
                if byte_len > 0 {
                    unsafe { ptr::copy_nonoverlapping(src, stg_mapped, byte_len as usize) };
                    // Failure discipline: reclaim BOTH allocations only
                    // when nothing reached the GPU (review r1: propagate
                    // the original error; review r2 P1: a failed wait
                    // after a successful submit leaves the GPU's access
                    // indeterminate — destroying would be a driver-level
                    // use-after-free, so that case deliberately leaks).
                    if let Err((e, submitted)) = self.with_compute_queue(|queue| unsafe {
                        self.one_shot_transfer(queue, |cmd| {
                            let copy = vk::BufferCopy::default().size(aligned);
                            self.device.cmd_copy_buffer(
                                cmd,
                                stg_buf,
                                dev_buf,
                                std::slice::from_ref(&copy),
                            );
                        })
                    }) {
                        if !submitted {
                            unsafe {
                                self.device.destroy_buffer(stg_buf, None);
                                self.device.free_memory(stg_mem, None);
                                self.device.destroy_buffer(dev_buf, None);
                                self.device.free_memory(dev_mem, None);
                            }
                        }
                        return Err(e);
                    }
                }
            }
            (dev_buf, dev_mem, stg_mapped, Some(stg_buf), Some(stg_mem))
        } else {
            // Unified memory: single host-visible buffer.
            let (buf, mem, mapped) =
                unsafe { allocate_buffer(&self.device, &self.memory_properties, aligned, usage)? };
            if let Some(src) = data {
                if byte_len > 0 {
                    unsafe { ptr::copy_nonoverlapping(src, mapped, byte_len as usize) };
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
            protocol: std::sync::Arc::clone(&self.protocol),
        });
        Ok(crate::Buffer {
            raw: Box::into_raw(inner) as *mut c_void,
            len: byte_len as usize,
            drop_fn: drop_vulkan_buffer,
        })
    }

    /// The Vulkan entry (loader handle) this device was created from.
    ///
    /// Exposed so graphics consumers can load instance-extension function
    /// tables (e.g. `VK_KHR_surface`, `VK_EXT_headless_surface`) against the
    /// shared instance.
    #[must_use]
    pub fn entry(&self) -> &Entry {
        &self._entry
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

    /// Raw `VkBuffer` handle for a buffer created by this device.
    ///
    /// Exposed so graphics consumers (Goldenweek) can bind Zunesha buffers as
    /// vertex buffers — the zero-copy compute→render interop path (ADR 0001).
    /// The handle is valid while the buffer lives.
    #[must_use]
    pub fn raw_buffer(&self, buffer: &crate::Buffer) -> vk::Buffer {
        // Safety: `raw` was produced by `Box::into_raw::<VulkanBufferInner>` in
        // `create_buffer` and remains valid while `buffer` is alive.
        unsafe { (*(buffer.raw as *const VulkanBufferInner)).buffer }
    }

    /// Run `f` with the shared compute queue, serialized against every
    /// other submission (the substrate's own staging transfers and other
    /// consumers' dispatch work).
    ///
    /// Vulkan queues and command pools are *externally synchronized*
    /// objects — at most one host thread may touch a given queue/pool at a
    /// time (spec host-sync rules). Since [`VulkanDevice`] is `Send + Sync`
    /// and shareable behind an `Arc`, every submitter must join this
    /// protocol: **consumers that submit work on the compute queue must do
    /// so inside this accessor** (submit AND any immediate wait —
    /// `queue_wait_idle` counts as queue access). Fence-based waits after
    /// the submit returns do not touch the queue and need no lock.
    ///
    /// The lock is held only for the closure's duration; it is never held
    /// across a long-running GPU wait started elsewhere.
    pub fn with_compute_queue<R>(&self, f: impl FnOnce(vk::Queue) -> R) -> R {
        // The lock's purpose is CROSS-THREAD exclusion of the shared
        // queue/pool; this thread re-entering the protocol must NOT
        // re-acquire the non-reentrant mutex. Re-entry is real and
        // pre-existing since 0.1.1: the substrate's own buffer creation
        // (device-local staging transfer) and a buffer's teardown quiesce
        // both run inside a consumer's closure (found via review r5 P1's
        // reproduction). PROTOCOL_DEPTH > 0 ⇒ this thread already holds
        // submit_lock.
        let _guard = if PROTOCOL_DEPTH.get() > 0 {
            None
        } else {
            // Poisoning only means a submitter panicked mid-closure; the
            // queue state is still usable (Vulkan object state is not
            // corrupted by a host panic between calls), so steal the lock
            // and continue.
            Some(
                self.protocol
                    .submit_lock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            )
        };
        // Depth resets on unwind via Drop, BEFORE any held lock releases,
        // keeping the invariant "depth > 0 ⇒ this thread holds the lock".
        struct DepthReset;
        impl Drop for DepthReset {
            fn drop(&mut self) {
                PROTOCOL_DEPTH.set(PROTOCOL_DEPTH.get().saturating_sub(1));
            }
        }
        PROTOCOL_DEPTH.set(PROTOCOL_DEPTH.get() + 1);
        let _depth = DepthReset;
        f(self.compute_queue)
    }
}

impl Device for VulkanDevice {
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
        if self.uses_device_local {
            MemoryPlacement::DeviceLocal
        } else {
            MemoryPlacement::HostVisible
        }
    }

    fn create_buffer<T: bytemuck::Pod>(&self, data: &[T]) -> Result<crate::Buffer> {
        self.buffer_new(
            mem::size_of_val(data) as vk::DeviceSize,
            Some(data.as_ptr() as *const c_void),
            false,
        )
    }

    fn create_buffer_uninit<T: bytemuck::Pod>(&self, len: usize) -> Result<crate::Buffer> {
        self.buffer_new((len * mem::size_of::<T>()) as vk::DeviceSize, None, false)
    }

    /// Forces the device-local + staging path regardless of the negotiated
    /// strategy (Borsalino's GPU-resident-weights contract; see the shared
    /// `buffer_new` allocation path). On hardware with no distinct
    /// device-local heap the `DEVICE_LOCAL`-flagged memory type is used
    /// wherever the driver exposes one.
    fn create_device_buffer<T: bytemuck::Pod>(&self, data: &[T]) -> Result<crate::Buffer> {
        self.buffer_new(
            mem::size_of_val(data) as vk::DeviceSize,
            Some(data.as_ptr() as *const c_void),
            true,
        )
    }

    /// Uninitialised variant of [`create_device_buffer`](Self::create_device_buffer).
    fn create_device_buffer_uninit<T: bytemuck::Pod>(&self, len: usize) -> Result<crate::Buffer> {
        self.buffer_new((len * mem::size_of::<T>()) as vk::DeviceSize, None, true)
    }

    fn read_buffer<T: bytemuck::Pod>(&self, buffer: &crate::Buffer) -> Result<Vec<T>> {
        // Safety: `raw` was produced by `Box::into_raw::<VulkanBufferInner>` and
        // is still valid (buffer not dropped).
        let inner = unsafe { &*(buffer.raw as *const VulkanBufferInner) };

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

        // Device-local buffers: copy device → staging, then read the staging
        // mapping — BOTH under the submission lock. The staging mapping is
        // shared per-buffer: releasing the lock between the transfer and the
        // host copy would let a concurrent `read_buffer` of the same buffer
        // rewrite the mapping mid-copy (review round 2, P1). Unified buffers
        // (no staging) read their own mapping directly — no transfer, no
        // staging race — and still take the lock for uniformity.
        self.with_compute_queue(|queue| unsafe {
            if let Some(stg_buf) = inner.staging_buffer {
                self.one_shot_transfer(queue, |cmd| {
                    let copy = vk::BufferCopy::default().size(inner.size);
                    self.device.cmd_copy_buffer(
                        cmd,
                        inner.buffer,
                        stg_buf,
                        std::slice::from_ref(&copy),
                    );
                })
                .map_err(|(e, _)| e)?;
            }
            let src = inner.mapped as *const T;
            let slice = std::slice::from_raw_parts(src, count);
            Ok(slice.to_vec())
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
    use crate::Device;
    use serial_test::serial;

    /// A graphics-requested init selects a device exposing a graphics queue and
    /// supporting `VK_KHR_swapchain` (the rendering prerequisite). On a
    /// compute-only host neither is guaranteed — the test then reports the
    /// degradation instead of asserting.
    /// Apply the `ZUNESHA_TEST_DEVICE` pin convention (mirrors Goldenweek's
    /// `GOLDENWEEK_TEST_DEVICE`, Goldenweek ADR 0002): when set, the value is
    /// a case-insensitive device-name substring that wins over capability
    /// scoring. Unset → the request is passed through unchanged.
    fn pinned(request: InitRequest) -> InitRequest {
        match std::env::var("ZUNESHA_TEST_DEVICE") {
            Ok(hint) if !hint.is_empty() => request.with_device_hint(hint),
            _ => request,
        }
    }

    /// `buffer_placement` reports the *effective* placement — what the
    /// requested strategy resolved to — not the request. Explicit strategies
    /// are deterministic; `Auto` resolves per hardware and must always be
    /// concrete (never panic, always one of the two variants).
    #[test]
    #[serial]
    fn placement_reports_effective_not_requested() {
        let unified = match VulkanDevice::init_with_strategy(MemoryStrategy::Unified) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("skipping: no Vulkan device ({e})");
                return;
            }
        };
        assert_eq!(unified.buffer_placement(), MemoryPlacement::HostVisible);
        assert_eq!(unified.memory_strategy(), MemoryStrategy::Unified);

        let discrete = match VulkanDevice::init_with_strategy(MemoryStrategy::DeviceLocal) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("skipping: no Vulkan device ({e})");
                return;
            }
        };
        assert_eq!(discrete.buffer_placement(), MemoryPlacement::DeviceLocal);
        assert_eq!(discrete.memory_strategy(), MemoryStrategy::DeviceLocal);

        let auto = match VulkanDevice::init() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("skipping: no Vulkan device ({e})");
                return;
            }
        };
        assert_eq!(auto.memory_strategy(), MemoryStrategy::Auto);
        assert!(
            matches!(
                auto.buffer_placement(),
                MemoryPlacement::HostVisible | MemoryPlacement::DeviceLocal
            ),
            "Auto must resolve to a concrete placement on any hardware"
        );
    }

    #[test]
    #[serial]
    fn graphics_init_enables_swapchain_and_graphics_queue() {
        let device = match VulkanDevice::init_with(pinned(InitRequest::prefer_graphics())) {
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
        let device = match VulkanDevice::init_with(pinned(InitRequest::prefer_graphics())) {
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

    /// Safe code can share one `Arc<VulkanDevice>` across threads; the
    /// staging transfers those threads trigger must therefore be
    /// internally serialized (the `submit_lock` protocol — review round
    /// 1's P1, with the readback copy inside the lock per round 2's).
    /// Regression: concurrent device-local buffer creation + readback from
    /// four threads round-trips exactly and does not deadlock.
    /// Review r5 P1 regression: a buffer dropped INSIDE a
    /// with_compute_queue closure must not deadlock the non-reentrant
    /// submit lock (its Drop quiesce skips the re-acquisition because
    /// this thread already holds the protocol). Timeout-guarded so a
    /// regression FAILS rather than hangs.
    #[test]
    #[serial]
    fn buffer_drop_inside_with_compute_queue_does_not_deadlock() {
        let device = match VulkanDevice::init() {
            Ok(d) => std::sync::Arc::new(d),
            Err(e) => {
                eprintln!("skipping: no Vulkan device ({e})");
                return;
            }
        };
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let d = std::sync::Arc::clone(&device);
        let handle = std::thread::spawn(move || {
            d.with_compute_queue(|_| {
                let buf = d.create_buffer(&[1.0f32, 2.0, 3.0, 4.0]).unwrap();
                drop(buf); // re-entrant quiesce — used to deadlock here
            });
            let _ = tx.send(());
        });
        rx.recv_timeout(std::time::Duration::from_secs(10))
            .expect("closure deadlocked dropping a buffer inside with_compute_queue");
        handle.join().unwrap();
    }

    #[test]
    #[serial]
    fn concurrent_buffer_creation_is_serialized() {
        let device = std::sync::Arc::new(match VulkanDevice::init() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("skipping: no Vulkan device ({e})");
                return;
            }
        });
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
            h.join().expect("no thread panicked under the submit lock");
        }
    }

    /// `create_device_buffer` must allocate device-local even when the
    /// device strategy is forced `Unified` — the documented trait contract
    /// for GPU-resident data (Borsalino's own implementation takes the
    /// host-visible path there instead, diverging from its trait docs;
    /// see the Borsalino migration plan §5.1 for the full relationship).
    ///
    /// Observable via internals: a device-local allocation carries a staging
    /// buffer; the strategy-respecting `create_buffer` under `Unified` does
    /// not. Both must still upload and read back exactly.
    #[test]
    #[serial]
    fn device_buffer_forces_device_local_under_unified_strategy() {
        let device = match VulkanDevice::init_with_strategy(MemoryStrategy::Unified) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("skipping: no Vulkan device ({e})");
                return;
            }
        };

        // The forced path needs a real device-local heap: only meaningful
        // on discrete GPUs (the RTX 5080 seat). Integrated/CPU devices have
        // no distinct local heap to force.
        let props = unsafe {
            device
                .instance
                .get_physical_device_properties(device.physical_device)
        };
        if props.device_type != vk::PhysicalDeviceType::DISCRETE_GPU {
            eprintln!(
                "skipping: {} is not discrete — no device-local heap to force",
                unsafe { CStr::from_ptr(props.device_name.as_ptr()) }.to_string_lossy()
            );
            return;
        }

        let unified = device.create_buffer(&[1.0f32, 2.0, 3.0, 4.0]).unwrap();
        let forced = device
            .create_device_buffer(&[1.0f32, 2.0, 3.0, 4.0])
            .unwrap();

        // Safety: both handles were produced by this device's create paths.
        let u = unsafe { &*(unified.raw as *const VulkanBufferInner) };
        let f = unsafe { &*(forced.raw as *const VulkanBufferInner) };
        assert!(
            u.staging_buffer.is_none(),
            "Unified-strategy create_buffer must stay host-visible"
        );
        assert!(
            f.staging_buffer.is_some(),
            "create_device_buffer must force the device-local + staging path"
        );

        // Both placements upload and read back exactly.
        let r1: Vec<f32> = device.read_buffer(&unified).unwrap();
        let r2: Vec<f32> = device.read_buffer(&forced).unwrap();
        assert_eq!(r1, vec![1.0, 2.0, 3.0, 4.0]);
        assert_eq!(r2, vec![1.0, 2.0, 3.0, 4.0]);
    }
}
