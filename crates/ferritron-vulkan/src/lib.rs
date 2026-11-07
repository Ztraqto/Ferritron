//! Native Vulkan backend of Ferritron (design doc 9.3, 9.8): what wgpu
//! cannot do.
//!
//! - **Persistent kernels** ([`PersistentModalBank`]): a dispatch that stays
//!   on the GPU between audio blocks and is handed each block through
//!   memory, instead of one submission and readback per block. Small blocks
//!   (64 samples) become possible on the GPU in real time, without latency.
//! - **Tensor cores** ([`TensorMixer`]): cooperative matrices
//!   (VK_KHR_cooperative_matrix) for matrix products such as mixing
//!   hundreds of channels to hundreds of channels.
//!
//! Everything here is optional: [`Gpu::get`] fails without a Vulkan driver
//! or a suitable GPU, and callers keep their wgpu or CPU paths.
//!
//! **One device per process.** Work on two Vulkan devices does not run at
//! the same time: the GPU switches between them, and a persistent kernel
//! would make the other device's work wait. So `ferritron-gpu` creates the
//! device that wgpu uses with a second queue and hands it over with
//! [`Gpu::adopt`]; persistent kernels then run on that queue, alongside
//! wgpu's work. [`Gpu::is_shared`] tells which case applies. The Vulkan
//! loader is opened at run time, so building needs no Vulkan SDK. The
//! shaders are Ferritron's own, written in Slang (`shaders/`).

mod mixer;
mod modal;

/// The Vulkan bindings this crate uses (for [`Gpu::adopt`]).
pub use ash;

pub use mixer::TensorMixer;
pub use modal::{ModalLane, PersistentModalBank};

use ash::vk;
use std::ffi::CStr;
use std::sync::{Arc, Mutex, OnceLock};

/// The process's Vulkan device, shared by every component (design doc 2.3).
pub struct Gpu {
    /// Our own loader and instance, or none for an adopted device.
    _entry: Option<ash::Entry>,
    /// Destroy the device and instance when dropped (not when adopted).
    owned: bool,
    instance: ash::Instance,
    physical: vk::PhysicalDevice,
    pub(crate) device: ash::Device,
    pub(crate) queue: Mutex<vk::Queue>,
    pub(crate) queue_family: u32,
    memory: vk::PhysicalDeviceMemoryProperties,
    name: String,
    /// Optional features this device has.
    pub features: Features,
}

/// What the device offers beyond plain compute.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Features {
    /// VK_KHR_cooperative_matrix: Tensor cores.
    pub cooperative_matrix: bool,
    /// VK_KHR_ray_query with acceleration structures: RT cores.
    pub ray_query: bool,
}

static GPU: OnceLock<Result<Arc<Gpu>, String>> = OnceLock::new();

impl Gpu {
    /// The shared device (opened on first use). Fails without Vulkan, or
    /// with only a software or integrated-less setup that is slower than
    /// the CPU.
    pub fn get() -> Result<Arc<Gpu>, String> {
        GPU.get_or_init(|| Gpu::open().map(Arc::new)).clone()
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Takes a device created elsewhere (wgpu's) and uses its queue
    /// `queue_index` of `queue_family`, then makes it the process's device.
    /// Never destroys it. Returns false if a device was already set up.
    ///
    /// # Safety
    /// `device` must have been created from `physical` of `instance` with at
    /// least `queue_index + 1` queues in `queue_family`, none of them used
    /// by anyone else, and must outlive the process's use of this crate.
    pub unsafe fn adopt(instance: ash::Instance, physical: vk::PhysicalDevice, device: ash::Device, queue_family: u32, queue_index: u32, name: &str, features: Features) -> bool {
        let queue = unsafe { device.get_device_queue(queue_family, queue_index) };
        let memory = unsafe { instance.get_physical_device_memory_properties(physical) };
        let gpu = Gpu {
            _entry: None,
            owned: false,
            instance,
            physical,
            device,
            queue: Mutex::new(queue),
            queue_family,
            memory,
            name: format!("{name} (Vulkan, shared device)"),
            features,
        };
        GPU.set(Ok(Arc::new(gpu))).is_ok()
    }

    /// Whether the device does 16×16×16 cooperative-matrix tiles of f16
    /// with f32 sums in a subgroup (what [`TensorMixer`] uses).
    pub fn supports_f16_tiles(&self) -> bool {
        let Ok(entry) = (unsafe { ash::Entry::load() }) else { return false };
        let coop = ash::khr::cooperative_matrix::Instance::new(&entry, &self.instance);
        let Ok(configs) = (unsafe { coop.get_physical_device_cooperative_matrix_properties(self.physical) }) else { return false };
        configs.iter().any(|c| {
            (c.m_size, c.n_size, c.k_size) == (16, 16, 16)
                && c.a_type == vk::ComponentTypeKHR::FLOAT16
                && c.b_type == vk::ComponentTypeKHR::FLOAT16
                && c.c_type == vk::ComponentTypeKHR::FLOAT32
                && c.result_type == vk::ComponentTypeKHR::FLOAT32
                && c.scope == vk::ScopeKHR::SUBGROUP
        })
    }

    /// True when this is wgpu's device (persistent kernels run alongside
    /// wgpu's work), false for a device of our own.
    pub fn is_shared(&self) -> bool {
        !self.owned
    }

    fn open() -> Result<Gpu, String> {
        let entry = unsafe { ash::Entry::load() }.map_err(|e| format!("no Vulkan loader: {e}"))?;
        let app = vk::ApplicationInfo::default().application_name(c"Ferritron").api_version(vk::API_VERSION_1_3);
        let instance = unsafe { entry.create_instance(&vk::InstanceCreateInfo::default().application_info(&app), None) }.map_err(|e| format!("cannot create a Vulkan instance: {e}"))?;
        let picked = Self::pick(&instance);
        let Some((physical, queue_family, name, extensions)) = picked else {
            unsafe { instance.destroy_instance(None) };
            return Err("no Vulkan GPU with compute (and not a software one)".into());
        };
        let has = |want: &CStr| extensions.iter().any(|e| e.as_c_str() == want);
        let mut features = Features {
            cooperative_matrix: has(ash::khr::cooperative_matrix::NAME),
            ray_query: has(ash::khr::ray_query::NAME) && has(ash::khr::acceleration_structure::NAME) && has(ash::khr::deferred_host_operations::NAME),
        };
        let mut enable: Vec<*const i8> = Vec::new();
        if features.cooperative_matrix {
            enable.push(ash::khr::cooperative_matrix::NAME.as_ptr());
        }
        if features.ray_query {
            enable.extend([ash::khr::ray_query::NAME.as_ptr(), ash::khr::acceleration_structure::NAME.as_ptr(), ash::khr::deferred_host_operations::NAME.as_ptr()]);
        }
        let priorities = [1.0f32];
        let queues = [vk::DeviceQueueCreateInfo::default().queue_family_index(queue_family).queue_priorities(&priorities)];
        let mut v12 = vk::PhysicalDeviceVulkan12Features::default()
            .buffer_device_address(features.ray_query)
            .shader_float16(true)
            .vulkan_memory_model(true)
            .vulkan_memory_model_device_scope(true)
            .storage_buffer8_bit_access(true);
        let mut v11 = vk::PhysicalDeviceVulkan11Features::default().storage_buffer16_bit_access(true);
        let mut v13 = vk::PhysicalDeviceVulkan13Features::default().synchronization2(true);
        let mut coop = vk::PhysicalDeviceCooperativeMatrixFeaturesKHR::default().cooperative_matrix(true);
        let mut rq = vk::PhysicalDeviceRayQueryFeaturesKHR::default().ray_query(true);
        let mut accel = vk::PhysicalDeviceAccelerationStructureFeaturesKHR::default().acceleration_structure(true);
        let mut info = vk::DeviceCreateInfo::default().queue_create_infos(&queues).enabled_extension_names(&enable).push_next(&mut v11).push_next(&mut v12).push_next(&mut v13);
        if features.cooperative_matrix {
            info = info.push_next(&mut coop);
        }
        if features.ray_query {
            info = info.push_next(&mut rq).push_next(&mut accel);
        }
        let device = match unsafe { instance.create_device(physical, &info, None) } {
            Ok(device) => device,
            Err(e) => {
                unsafe { instance.destroy_instance(None) };
                return Err(format!("cannot open the Vulkan device: {e}"));
            }
        };
        if !features.cooperative_matrix && !features.ray_query {
            features = Features::default();
        }
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        let memory = unsafe { instance.get_physical_device_memory_properties(physical) };
        Ok(Gpu { _entry: Some(entry), owned: true, instance, physical, device, queue: Mutex::new(queue), queue_family, memory, name, features })
    }

    /// The discrete GPU if there is one, else any hardware GPU, with a
    /// compute queue: (device, queue family, name, extensions).
    fn pick(instance: &ash::Instance) -> Option<(vk::PhysicalDevice, u32, String, Vec<std::ffi::CString>)> {
        let mut best: Option<(u32, vk::PhysicalDevice, u32, String)> = None;
        for physical in unsafe { instance.enumerate_physical_devices() }.ok()? {
            let props = unsafe { instance.get_physical_device_properties(physical) };
            let rank = match props.device_type {
                vk::PhysicalDeviceType::DISCRETE_GPU => 3,
                vk::PhysicalDeviceType::INTEGRATED_GPU => 2,
                vk::PhysicalDeviceType::VIRTUAL_GPU => 1,
                _ => continue, // software (CPU) devices are slower than Ferritron's CPU paths
            };
            if props.api_version < vk::API_VERSION_1_3 {
                continue;
            }
            let families = unsafe { instance.get_physical_device_queue_family_properties(physical) };
            let Some(family) = families.iter().position(|f| f.queue_flags.contains(vk::QueueFlags::COMPUTE)) else { continue };
            if best.as_ref().is_none_or(|b| rank > b.0) {
                let name = unsafe { CStr::from_ptr(props.device_name.as_ptr()) }.to_string_lossy().into_owned();
                best = Some((rank, physical, family as u32, name));
            }
        }
        let (_, physical, family, name) = best?;
        let extensions = unsafe { instance.enumerate_device_extension_properties(physical) }
            .ok()?
            .iter()
            .map(|e| unsafe { CStr::from_ptr(e.extension_name.as_ptr()) }.to_owned())
            .collect();
        Some((physical, family, format!("{name} (Vulkan, native)"), extensions))
    }

    /// The first memory type with all of `wanted`, trying each set in order.
    pub(crate) fn memory_type(&self, bits: u32, wanted: &[vk::MemoryPropertyFlags]) -> Option<u32> {
        let types = &self.memory.memory_types[..self.memory.memory_type_count as usize];
        wanted.iter().find_map(|flags| (0..types.len() as u32).find(|&i| bits & (1 << i) != 0 && types[i as usize].property_flags.contains(*flags)))
    }
}

/// GPU tests one at a time (a persistent kernel is one per process).
#[cfg(test)]
pub(crate) fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Drop for Gpu {
    fn drop(&mut self) {
        if !self.owned {
            return;
        }
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

/// Where a buffer lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Place {
    /// GPU memory the host can write directly (resizable BAR), else host
    /// memory: for data the host writes and the GPU reads every block.
    Upload,
    /// Host memory, cached: for data the GPU writes and the host reads.
    Readback,
    /// GPU memory only.
    Device,
}

/// A buffer with its memory, mapped when the host can see it.
pub(crate) struct Buffer {
    gpu: Arc<Gpu>,
    pub buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    pub ptr: *mut u8,
}

unsafe impl Send for Buffer {}

impl Buffer {
    pub fn new(gpu: &Arc<Gpu>, size: usize, usage: vk::BufferUsageFlags, place: Place) -> Result<Buffer, String> {
        let size = size.max(16);
        let device = &gpu.device;
        let buffer = unsafe { device.create_buffer(&vk::BufferCreateInfo::default().size(size as u64).usage(usage), None) }.map_err(|e| format!("cannot create a buffer: {e}"))?;
        let needs = unsafe { device.get_buffer_memory_requirements(buffer) };
        use vk::MemoryPropertyFlags as M;
        let wanted: &[M] = match place {
            Place::Upload => &[M::DEVICE_LOCAL | M::HOST_VISIBLE | M::HOST_COHERENT, M::HOST_VISIBLE | M::HOST_COHERENT],
            Place::Readback => &[M::HOST_VISIBLE | M::HOST_COHERENT | M::HOST_CACHED, M::HOST_VISIBLE | M::HOST_COHERENT],
            Place::Device => &[M::DEVICE_LOCAL],
        };
        let Some(kind) = gpu.memory_type(needs.memory_type_bits, wanted) else {
            unsafe { device.destroy_buffer(buffer, None) };
            return Err("no suitable GPU memory".into());
        };
        let mut flags = vk::MemoryAllocateFlagsInfo::default().flags(vk::MemoryAllocateFlags::DEVICE_ADDRESS);
        let mut alloc = vk::MemoryAllocateInfo::default().allocation_size(needs.size).memory_type_index(kind);
        if usage.contains(vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS) {
            alloc = alloc.push_next(&mut flags);
        }
        let memory = match unsafe { device.allocate_memory(&alloc, None) } {
            Ok(m) => m,
            Err(e) => {
                unsafe { device.destroy_buffer(buffer, None) };
                return Err(format!("cannot allocate GPU memory: {e}"));
            }
        };
        unsafe { device.bind_buffer_memory(buffer, memory, 0) }.map_err(|e| e.to_string())?;
        let ptr = if place == Place::Device {
            std::ptr::null_mut()
        } else {
            let p = unsafe { device.map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()) }.map_err(|e| format!("cannot map GPU memory: {e}"))?;
            unsafe { std::ptr::write_bytes(p as *mut u8, 0, size) };
            p as *mut u8
        };
        Ok(Buffer { gpu: gpu.clone(), buffer, memory, ptr })
    }

    /// The mapped memory as `T`s (host-visible buffers only).
    pub fn slice<T>(&self) -> *mut T {
        debug_assert!(!self.ptr.is_null());
        self.ptr as *mut T
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        unsafe {
            self.gpu.device.destroy_buffer(self.buffer, None);
            self.gpu.device.free_memory(self.memory, None);
        }
    }
}

/// A compute pipeline from SPIR-V with `bindings` storage buffers (set 0)
/// and `push` bytes of push constants.
pub(crate) struct Pipeline {
    gpu: Arc<Gpu>,
    pub pipeline: vk::Pipeline,
    pub layout: vk::PipelineLayout,
    pub set_layout: vk::DescriptorSetLayout,
    pub pool: vk::DescriptorPool,
    pub set: vk::DescriptorSet,
}

impl Pipeline {
    pub fn new(gpu: &Arc<Gpu>, spirv: &[u8], buffers: &[&Buffer], push: u32) -> Result<Pipeline, String> {
        let device = &gpu.device;
        let code = ash::util::read_spv(&mut std::io::Cursor::new(spirv)).map_err(|e| e.to_string())?;
        let module = unsafe { device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None) }.map_err(|e| format!("bad shader: {e}"))?;
        let bindings: Vec<_> = (0..buffers.len() as u32)
            .map(|b| vk::DescriptorSetLayoutBinding::default().binding(b).descriptor_type(vk::DescriptorType::STORAGE_BUFFER).descriptor_count(1).stage_flags(vk::ShaderStageFlags::COMPUTE))
            .collect();
        let set_layout = unsafe { device.create_descriptor_set_layout(&vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings), None) }.map_err(|e| e.to_string())?;
        let ranges = [vk::PushConstantRange::default().stage_flags(vk::ShaderStageFlags::COMPUTE).size(push)];
        let set_layouts = [set_layout];
        let mut layout_info = vk::PipelineLayoutCreateInfo::default().set_layouts(&set_layouts);
        if push > 0 {
            layout_info = layout_info.push_constant_ranges(&ranges);
        }
        let layout = unsafe { device.create_pipeline_layout(&layout_info, None) }.map_err(|e| e.to_string())?;
        let stage = vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::COMPUTE).module(module).name(c"main");
        let pipeline = unsafe { device.create_compute_pipelines(vk::PipelineCache::null(), &[vk::ComputePipelineCreateInfo::default().stage(stage).layout(layout)], None) }
            .map_err(|(_, e)| format!("cannot build the pipeline: {e}"))?[0];
        unsafe { device.destroy_shader_module(module, None) };
        let sizes = [vk::DescriptorPoolSize::default().ty(vk::DescriptorType::STORAGE_BUFFER).descriptor_count(buffers.len().max(1) as u32)];
        let pool = unsafe { device.create_descriptor_pool(&vk::DescriptorPoolCreateInfo::default().max_sets(1).pool_sizes(&sizes), None) }.map_err(|e| e.to_string())?;
        let set = unsafe { device.allocate_descriptor_sets(&vk::DescriptorSetAllocateInfo::default().descriptor_pool(pool).set_layouts(&set_layouts)) }.map_err(|e| e.to_string())?[0];
        let infos: Vec<[vk::DescriptorBufferInfo; 1]> = buffers.iter().map(|b| [vk::DescriptorBufferInfo::default().buffer(b.buffer).range(vk::WHOLE_SIZE)]).collect();
        let writes: Vec<_> = infos
            .iter()
            .enumerate()
            .map(|(i, info)| vk::WriteDescriptorSet::default().dst_set(set).dst_binding(i as u32).descriptor_type(vk::DescriptorType::STORAGE_BUFFER).buffer_info(info))
            .collect();
        unsafe { device.update_descriptor_sets(&writes, &[]) };
        Ok(Pipeline { gpu: gpu.clone(), pipeline, layout, set_layout, pool, set })
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        unsafe {
            let device = &self.gpu.device;
            device.destroy_pipeline(self.pipeline, None);
            device.destroy_pipeline_layout(self.layout, None);
            device.destroy_descriptor_pool(self.pool, None);
            device.destroy_descriptor_set_layout(self.set_layout, None);
        }
    }
}

/// Command buffers with their fences, for submissions the host waits on
/// (or not).
pub(crate) struct Commands {
    gpu: Arc<Gpu>,
    pool: vk::CommandPool,
    pub buffers: Vec<vk::CommandBuffer>,
    pub fences: Vec<vk::Fence>,
}

impl Commands {
    pub fn new(gpu: &Arc<Gpu>, count: usize) -> Result<Commands, String> {
        let device = &gpu.device;
        let pool = unsafe { device.create_command_pool(&vk::CommandPoolCreateInfo::default().queue_family_index(gpu.queue_family).flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER), None) }
            .map_err(|e| e.to_string())?;
        let buffers = unsafe { device.allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(pool).command_buffer_count(count as u32)) }.map_err(|e| e.to_string())?;
        let fences = (0..count)
            .map(|_| unsafe { device.create_fence(&vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED), None) }.map_err(|e| e.to_string()))
            .collect::<Result<_, _>>()?;
        Ok(Commands { gpu: gpu.clone(), pool, buffers, fences })
    }

    /// Submits command buffer `i`; its fence signals when it is done.
    pub fn submit(&self, i: usize) -> Result<(), String> {
        let device = &self.gpu.device;
        unsafe { device.reset_fences(&[self.fences[i]]) }.map_err(|e| e.to_string())?;
        let buffers = [self.buffers[i]];
        let submit = vk::SubmitInfo::default().command_buffers(&buffers);
        let queue = self.gpu.queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        unsafe { device.queue_submit(*queue, &[submit], self.fences[i]) }.map_err(|e| format!("cannot submit GPU work: {e}"))
    }

    pub fn finished(&self, i: usize) -> bool {
        unsafe { self.gpu.device.get_fence_status(self.fences[i]) }.unwrap_or(true)
    }

    pub fn wait(&self, i: usize) {
        let _ = unsafe { self.gpu.device.wait_for_fences(&[self.fences[i]], true, u64::MAX) };
    }
}

impl Drop for Commands {
    fn drop(&mut self) {
        unsafe {
            let device = &self.gpu.device;
            let _ = device.wait_for_fences(&self.fences, true, u64::MAX);
            for &fence in &self.fences {
                device.destroy_fence(fence, None);
            }
            device.destroy_command_pool(self.pool, None);
        }
    }
}
