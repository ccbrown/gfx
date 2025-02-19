#[cfg(feature = "pipeline-cache")]
use crate::pipeline_cache;
use crate::{
    command, conversions as conv, internal::Channel, native as n, AsNative, Backend, FastHashMap,
    OnlineRecording, QueueFamily, ResourceIndex, Shared, VisibilityShared,
    MAX_BOUND_DESCRIPTOR_SETS, MAX_COLOR_ATTACHMENTS,
};

use arrayvec::ArrayVec;
use cocoa_foundation::foundation::NSUInteger;
use copyless::VecHelper;
use foreign_types::{ForeignType, ForeignTypeRef};
use hal::{
    adapter, buffer, device as d, display, format, image, memory,
    memory::Properties,
    pass,
    pool::CommandPoolCreateFlags,
    pso,
    pso::VertexInputRate,
    query,
    queue::{QueueFamilyId, QueueGroup, QueuePriority},
};
use metal::{
    CaptureManager, MTLCPUCacheMode, MTLLanguageVersion, MTLPrimitiveTopologyClass,
    MTLPrimitiveType, MTLResourceOptions, MTLSamplerMipFilter, MTLStorageMode, MTLTextureType,
    MTLVertexStepFunction, NSRange,
};
use objc::{
    rc::autoreleasepool,
    runtime::{Object, BOOL, NO},
};
use parking_lot::Mutex;

use std::collections::BTreeMap;
#[cfg(feature = "pipeline-cache")]
use std::io::Write;
use std::{
    cmp, iter, mem,
    ops::Range,
    ptr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread, time,
};

const STRIDE_GRANULARITY: pso::ElemStride = 4; //TODO: work around?
const SHADER_STAGE_COUNT: u32 = 3;

#[derive(Clone, Debug)]
enum FunctionError {
    InvalidEntryPoint,
    MissingRequiredSpecialization,
    BadSpecialization,
}

fn get_final_function(
    library: &metal::LibraryRef,
    entry: &str,
    specialization: &pso::Specialization,
    function_specialization: bool,
) -> Result<metal::Function, FunctionError> {
    type MTLFunctionConstant = Object;
    profiling::scope!("get_final_function");

    let mut mtl_function = library.get_function(entry, None).map_err(|e| {
        error!(
            "Function retrieval error {:?}. Known names: {:?}",
            e,
            library.function_names()
        );
        FunctionError::InvalidEntryPoint
    })?;

    if !function_specialization {
        if !specialization.data.is_empty() || !specialization.constants.is_empty() {
            error!("platform does not support specialization");
        }
        return Ok(mtl_function);
    }

    let dictionary = mtl_function.function_constants_dictionary();
    let count: NSUInteger = unsafe { msg_send![dictionary, count] };
    if count == 0 {
        return Ok(mtl_function);
    }

    let all_values: *mut Object = unsafe { msg_send![dictionary, allValues] };

    let constants = metal::FunctionConstantValues::new();
    for i in 0..count {
        let object: *mut MTLFunctionConstant = unsafe { msg_send![all_values, objectAtIndex: i] };
        let index: NSUInteger = unsafe { msg_send![object, index] };
        let required: BOOL = unsafe { msg_send![object, required] };
        match specialization
            .constants
            .iter()
            .find(|c| c.id as NSUInteger == index)
        {
            Some(c) => unsafe {
                let ptr = &specialization.data[c.range.start as usize] as *const u8 as *const _;
                let ty: metal::MTLDataType = msg_send![object, type];
                constants.set_constant_value_at_index(ptr, ty, c.id as NSUInteger);
            },
            None if required != NO => {
                //TODO: get name
                error!("Missing required specialization constant id {}", index);
                return Err(FunctionError::MissingRequiredSpecialization);
            }
            None => {}
        }
    }

    mtl_function = library.get_function(entry, Some(constants)).map_err(|e| {
        error!("Specialized function retrieval error {:?}", e);
        FunctionError::BadSpecialization
    })?;

    Ok(mtl_function)
}

impl VisibilityShared {
    fn are_available(&self, pool_base: query::Id, queries: &Range<query::Id>) -> bool {
        unsafe {
            let availability_ptr = ((self.buffer.contents() as *mut u8)
                .offset(self.availability_offset as isize)
                as *mut u32)
                .offset(pool_base as isize);
            queries
                .clone()
                .all(|id| *availability_ptr.offset(id as isize) != 0)
        }
    }
}

struct CompiledShader {
    library: metal::Library,
    function: metal::Function,
    wg_size: metal::MTLSize,
    rasterizing: bool,
    sized_bindings: Vec<naga::ResourceBinding>,
}

#[derive(Debug)]
pub struct Device {
    pub(crate) shared: Arc<Shared>,
    invalidation_queue: command::QueueInner,
    memory_types: Vec<adapter::MemoryType>,
    features: hal::Features,
    pub online_recording: OnlineRecording,
    #[cfg(any(feature = "pipeline-cache", feature = "cross"))]
    spv_options: naga::back::spv::Options,
}
unsafe impl Send for Device {}
unsafe impl Sync for Device {}

bitflags! {
    /// Memory type bits.
    struct MemoryTypes: u32 {
        // = `DEVICE_LOCAL`
        const PRIVATE = 1<<0;
        // = `CPU_VISIBLE | COHERENT`
        const SHARED = 1<<1;
        // = `DEVICE_LOCAL | CPU_VISIBLE`
        const MANAGED_UPLOAD = 1<<2;
        // = `DEVICE_LOCAL | CPU_VISIBLE | CACHED`
        // Memory range invalidation is implemented to stall the whole pipeline.
        // It's inefficient, therefore we aren't going to expose this type.
        //const MANAGED_DOWNLOAD = 1<<3;
    }
}

impl MemoryTypes {
    fn describe(index: usize) -> (MTLStorageMode, MTLCPUCacheMode) {
        match Self::from_bits(1 << index).unwrap() {
            Self::PRIVATE => (MTLStorageMode::Private, MTLCPUCacheMode::DefaultCache),
            Self::SHARED => (MTLStorageMode::Shared, MTLCPUCacheMode::DefaultCache),
            Self::MANAGED_UPLOAD => (MTLStorageMode::Managed, MTLCPUCacheMode::WriteCombined),
            //Self::MANAGED_DOWNLOAD => (MTLStorageMode::Managed, MTLCPUCacheMode::DefaultCache),
            _ => unreachable!(),
        }
    }
}

#[derive(Debug)]
pub struct PhysicalDevice {
    pub(crate) shared: Arc<Shared>,
    memory_types: Vec<adapter::MemoryType>,
}
unsafe impl Send for PhysicalDevice {}
unsafe impl Sync for PhysicalDevice {}

impl PhysicalDevice {
    pub(crate) fn new(shared: Arc<Shared>) -> Self {
        let memory_types = if shared.private_caps.os_is_mac {
            vec![
                adapter::MemoryType {
                    // PRIVATE
                    properties: Properties::DEVICE_LOCAL,
                    heap_index: 0,
                },
                adapter::MemoryType {
                    // SHARED
                    properties: Properties::CPU_VISIBLE | Properties::COHERENT,
                    heap_index: 1,
                },
                adapter::MemoryType {
                    // MANAGED_UPLOAD
                    properties: Properties::DEVICE_LOCAL | Properties::CPU_VISIBLE,
                    heap_index: 1,
                },
                // MANAGED_DOWNLOAD (removed)
            ]
        } else {
            vec![
                adapter::MemoryType {
                    // PRIVATE
                    properties: Properties::DEVICE_LOCAL,
                    heap_index: 0,
                },
                adapter::MemoryType {
                    // SHARED
                    properties: Properties::CPU_VISIBLE | Properties::COHERENT,
                    heap_index: 1,
                },
            ]
        };
        PhysicalDevice {
            shared: shared.clone(),
            memory_types,
        }
    }

    /// Return true if the specified format-swizzle pair is supported natively.
    pub fn supports_swizzle(&self, format: format::Format, swizzle: format::Swizzle) -> bool {
        self.shared
            .private_caps
            .map_format_with_swizzle(format, swizzle)
            .is_some()
    }
}

impl adapter::PhysicalDevice<Backend> for PhysicalDevice {
    unsafe fn open(
        &self,
        families: &[(&QueueFamily, &[QueuePriority])],
        requested_features: hal::Features,
    ) -> Result<adapter::Gpu<Backend>, d::CreationError> {
        use hal::queue::QueueFamily as _;

        // TODO: Query supported features by feature set rather than hard coding in the supported
        // features. https://developer.apple.com/metal/Metal-Feature-Set-Tables.pdf
        if !self.features().contains(requested_features) {
            warn!(
                "Features missing: {:?}",
                requested_features - self.features()
            );
            return Err(d::CreationError::MissingFeature);
        }

        let device = self.shared.device.lock();

        assert_eq!(families.len(), 1);
        assert_eq!(families[0].1.len(), 1);
        let mut queue_group = QueueGroup::new(families[0].0.id());
        for _ in 0..self.shared.private_caps.exposed_queues {
            queue_group.add_queue(command::Queue::new(self.shared.clone()));
        }

        #[cfg(any(feature = "pipeline-cache", feature = "cross"))]
        let spv_options = {
            use naga::back::spv;
            let mut flags = spv::WriterFlags::empty();
            flags.set(spv::WriterFlags::DEBUG, cfg!(debug_assertions));
            flags.set(
                spv::WriterFlags::ADJUST_COORDINATE_SPACE,
                !requested_features.contains(hal::Features::NDC_Y_UP),
            );
            spv::Options {
                lang_version: (1, 0),
                flags,
                // doesn't matter since we send it through SPIRV-Cross
                capabilities: None,
            }
        };

        let device = Device {
            shared: self.shared.clone(),
            invalidation_queue: command::QueueInner::new(&*device, Some(1)),
            memory_types: self.memory_types.clone(),
            features: requested_features,
            online_recording: OnlineRecording::default(),
            #[cfg(any(feature = "pipeline-cache", feature = "cross"))]
            spv_options,
        };

        Ok(adapter::Gpu {
            device,
            queue_groups: vec![queue_group],
        })
    }

    fn format_properties(&self, format: Option<format::Format>) -> format::Properties {
        match format {
            Some(format) => self.shared.private_caps.map_format_properties(format),
            None => format::Properties {
                linear_tiling: format::ImageFeature::empty(),
                optimal_tiling: format::ImageFeature::empty(),
                buffer_features: format::BufferFeature::empty(),
            },
        }
    }

    fn image_format_properties(
        &self,
        format: format::Format,
        dimensions: u8,
        tiling: image::Tiling,
        usage: image::Usage,
        view_caps: image::ViewCapabilities,
    ) -> Option<image::FormatProperties> {
        if let image::Tiling::Linear = tiling {
            let format_desc = format.surface_desc();
            let host_usage = image::Usage::TRANSFER_SRC | image::Usage::TRANSFER_DST;
            if dimensions != 2
                || !view_caps.is_empty()
                || !host_usage.contains(usage)
                || format_desc.aspects != format::Aspects::COLOR
                || format_desc.is_compressed()
            {
                return None;
            }
        }
        if dimensions == 1
            && usage
                .intersects(image::Usage::COLOR_ATTACHMENT | image::Usage::DEPTH_STENCIL_ATTACHMENT)
        {
            // MTLRenderPassDescriptor texture must not be MTLTextureType1D
            return None;
        }
        if dimensions == 3 && view_caps.contains(image::ViewCapabilities::KIND_2D_ARRAY) {
            // Can't create 2D/2DArray views of 3D textures
            return None;
        }
        let max_dimension = if dimensions == 3 {
            self.shared.private_caps.max_texture_3d_size as _
        } else {
            self.shared.private_caps.max_texture_size as _
        };

        let max_extent = image::Extent {
            width: max_dimension,
            height: if dimensions >= 2 { max_dimension } else { 1 },
            depth: if dimensions >= 3 { max_dimension } else { 1 },
        };

        self.shared
            .private_caps
            .map_format(format)
            .map(|_| image::FormatProperties {
                max_extent,
                max_levels: if dimensions == 1 { 1 } else { 12 },
                // 3D images enforce a single layer
                max_layers: if dimensions == 3 {
                    1
                } else {
                    self.shared.private_caps.max_texture_layers as _
                },
                sample_count_mask: self.shared.private_caps.sample_count_mask as _,
                //TODO: buffers and textures have separate limits
                // Max buffer size is determined by feature set
                // Max texture size does not appear to be documented publicly
                max_resource_size: self.shared.private_caps.max_buffer_size as _,
            })
    }

    fn memory_properties(&self) -> adapter::MemoryProperties {
        adapter::MemoryProperties {
            memory_heaps: vec![
                adapter::MemoryHeap {
                    size: !0, //TODO: private memory limits
                    flags: memory::HeapFlags::DEVICE_LOCAL,
                },
                adapter::MemoryHeap {
                    size: self.shared.private_caps.max_buffer_size,
                    flags: memory::HeapFlags::empty(),
                },
            ],
            memory_types: self.memory_types.to_vec(),
        }
    }

    fn features(&self) -> hal::Features {
        use hal::Features as F;
        let mut features = F::FULL_DRAW_INDEX_U32
            | F::INDEPENDENT_BLENDING
            | F::DRAW_INDIRECT_FIRST_INSTANCE
            | F::DEPTH_CLAMP
            | F::SAMPLER_ANISOTROPY
            | F::FORMAT_BC
            | F::PRECISE_OCCLUSION_QUERY
            | F::SHADER_STORAGE_BUFFER_ARRAY_DYNAMIC_INDEXING
            | F::VERTEX_STORES_AND_ATOMICS
            | F::FRAGMENT_STORES_AND_ATOMICS
            | F::INSTANCE_RATE
            | F::SEPARATE_STENCIL_REF_VALUES
            | F::SHADER_CLIP_DISTANCE
            | F::MUTABLE_UNNORMALIZED_SAMPLER
            | F::NDC_Y_UP;

        features.set(
            F::IMAGE_CUBE_ARRAY,
            self.shared.private_caps.texture_cube_array,
        );
        features.set(
            F::DUAL_SRC_BLENDING,
            self.shared.private_caps.dual_source_blending,
        );
        features.set(
            F::NON_FILL_POLYGON_MODE,
            self.shared.private_caps.expose_line_mode,
        );
        if self.shared.private_caps.msl_version >= MTLLanguageVersion::V2_0 {
            features |= F::TEXTURE_DESCRIPTOR_ARRAY
                | F::SHADER_SAMPLED_IMAGE_ARRAY_DYNAMIC_INDEXING
                | F::SAMPLED_TEXTURE_DESCRIPTOR_INDEXING
                | F::STORAGE_TEXTURE_DESCRIPTOR_INDEXING;
        }
        features.set(
            F::SAMPLER_BORDER_COLOR,
            self.shared.private_caps.sampler_clamp_to_border,
        );
        features.set(
            F::MUTABLE_COMPARISON_SAMPLER,
            self.shared.private_caps.mutable_comparison_samplers,
        );

        //TODO: F::DEPTH_BOUNDS
        //TODO: F::SAMPLER_MIRROR_CLAMP_EDGE
        features
    }

    fn properties(&self) -> hal::PhysicalDeviceProperties {
        let pc = &self.shared.private_caps;
        let device = self.shared.device.lock();

        let mut caveats = hal::PerformanceCaveats::empty();
        if !self.shared.private_caps.base_vertex_instance_drawing {
            caveats |= hal::PerformanceCaveats::BASE_VERTEX_INSTANCE_DRAWING;
        }
        hal::PhysicalDeviceProperties {
            limits: hal::Limits {
                max_image_1d_size: pc.max_texture_size as _,
                max_image_2d_size: pc.max_texture_size as _,
                max_image_3d_size: pc.max_texture_3d_size as _,
                max_image_cube_size: pc.max_texture_size as _,
                max_image_array_layers: pc.max_texture_layers as _,
                max_texel_elements: (pc.max_texture_size * pc.max_texture_size) as usize,
                max_uniform_buffer_range: pc.max_buffer_size,
                max_storage_buffer_range: pc.max_buffer_size,
                // "Maximum length of an inlined constant data buffer, per graphics or compute function"
                max_push_constants_size: 0x1000,
                max_sampler_allocation_count: !0,
                max_bound_descriptor_sets: MAX_BOUND_DESCRIPTOR_SETS as _,
                descriptor_limits: hal::DescriptorLimits {
                    max_per_stage_descriptor_samplers: pc.max_samplers_per_stage,
                    max_per_stage_descriptor_uniform_buffers: pc.max_buffers_per_stage,
                    max_per_stage_descriptor_storage_buffers: pc.max_buffers_per_stage,
                    max_per_stage_descriptor_sampled_images: pc
                        .max_textures_per_stage
                        .min(pc.max_samplers_per_stage)
                        as u32,
                    max_per_stage_descriptor_storage_images: pc.max_textures_per_stage,
                    max_per_stage_descriptor_input_attachments: pc.max_textures_per_stage, //TODO
                    max_per_stage_resources: 0x100,                                        //TODO
                    max_descriptor_set_samplers: pc.max_samplers_per_stage * SHADER_STAGE_COUNT,
                    max_descriptor_set_uniform_buffers: pc.max_buffers_per_stage
                        * SHADER_STAGE_COUNT,
                    max_descriptor_set_uniform_buffers_dynamic: 8 * SHADER_STAGE_COUNT,
                    max_descriptor_set_storage_buffers: pc.max_buffers_per_stage
                        * SHADER_STAGE_COUNT,
                    max_descriptor_set_storage_buffers_dynamic: 4 * SHADER_STAGE_COUNT,
                    max_descriptor_set_sampled_images: pc
                        .max_textures_per_stage
                        .min(pc.max_samplers_per_stage)
                        * SHADER_STAGE_COUNT,
                    max_descriptor_set_storage_images: pc.max_textures_per_stage
                        * SHADER_STAGE_COUNT,
                    max_descriptor_set_input_attachments: pc.max_textures_per_stage
                        * SHADER_STAGE_COUNT,
                },
                max_fragment_input_components: pc.max_fragment_input_components as usize,
                max_framebuffer_layers: 2048, // TODO: Determine is this is the correct value
                max_memory_allocation_count: 4096, // TODO: Determine is this is the correct value

                max_patch_size: 0, // No tessellation

                // Note: The maximum number of supported viewports and scissor rectangles varies by device.
                // TODO: read from Metal Feature Sets.
                max_viewports: 1,
                max_viewport_dimensions: [pc.max_texture_size as _; 2],
                max_framebuffer_extent: hal::image::Extent {
                    //TODO
                    width: pc.max_texture_size as _,
                    height: pc.max_texture_size as _,
                    depth: pc.max_texture_layers as _,
                },
                min_memory_map_alignment: 4,

                optimal_buffer_copy_offset_alignment: pc.buffer_alignment,
                optimal_buffer_copy_pitch_alignment: 4,
                min_texel_buffer_offset_alignment: pc.buffer_alignment,
                min_uniform_buffer_offset_alignment: pc.buffer_alignment,
                min_storage_buffer_offset_alignment: pc.buffer_alignment,

                max_compute_work_group_count: [!0; 3], // really undefined
                max_compute_work_group_size: {
                    let size = device.max_threads_per_threadgroup();
                    [size.width as u32, size.height as u32, size.depth as u32]
                },
                max_compute_shared_memory_size: pc.max_total_threadgroup_memory as usize,

                max_vertex_input_attributes: 31,
                max_vertex_input_bindings: 31,
                max_vertex_input_attribute_offset: 255, // TODO
                max_vertex_input_binding_stride: 256,   // TODO
                max_vertex_output_components: pc.max_fragment_input_components as usize,

                framebuffer_color_sample_counts: 0b101, // TODO
                framebuffer_depth_sample_counts: 0b101, // TODO
                framebuffer_stencil_sample_counts: 0b101, // TODO
                max_color_attachments: pc.max_color_render_targets as usize,

                buffer_image_granularity: 1,
                // Note: we issue Metal buffer-to-buffer copies on memory flush/invalidate,
                // and those need to operate on sizes being multiples of 4.
                non_coherent_atom_size: 4,
                max_sampler_anisotropy: 16.,
                min_vertex_input_binding_stride_alignment: STRIDE_GRANULARITY as u64,

                ..hal::Limits::default() // TODO!
            },
            downlevel: hal::DownlevelProperties::all_enabled(),
            performance_caveats: caveats,
            dynamic_pipeline_states: hal::DynamicStates::all(),

            ..hal::PhysicalDeviceProperties::default()
        }
    }

    unsafe fn enumerate_displays(
        &self,
    ) -> Vec<hal::display::Display<crate::Backend>> {
        unimplemented!();
    }

    unsafe fn enumerate_compatible_planes(
        &self,
        _display: &hal::display::Display<crate::Backend>,
    ) -> Vec<hal::display::Plane> {
        unimplemented!();
    }

    unsafe fn create_display_mode(
        &self,
        _display: &hal::display::Display<crate::Backend>,
        _resolution: (u32, u32),
        _refresh_rate: u32,
    ) -> Result<hal::display::DisplayMode<crate::Backend>, hal::display::DisplayModeError> {
        unimplemented!();
    }

    unsafe fn create_display_plane<'a>(
        &self,
        _display: &'a hal::display::DisplayMode<crate::Backend>,
        _plane: &'a hal::display::Plane,
    ) -> Result<hal::display::DisplayPlane<'a, crate::Backend>, d::OutOfMemory> {
        unimplemented!();
    }
}

pub struct LanguageVersion {
    pub major: u8,
    pub minor: u8,
}

impl LanguageVersion {
    pub fn new(major: u8, minor: u8) -> Self {
        LanguageVersion { major, minor }
    }
}

impl Device {
    fn _is_heap_coherent(&self, heap: &n::MemoryHeap) -> bool {
        match *heap {
            n::MemoryHeap::Private => false,
            n::MemoryHeap::Public(memory_type, _) => self.memory_types[memory_type.0]
                .properties
                .contains(Properties::COHERENT),
            n::MemoryHeap::Native(ref heap) => heap.storage_mode() == MTLStorageMode::Shared,
        }
    }

    #[cfg(feature = "cross")]
    fn compile_shader_library_cross(
        device: &Mutex<metal::Device>,
        raw_data: &[u32],
        compiler_options: &spirv_cross::msl::CompilerOptions,
        msl_version: MTLLanguageVersion,
        specialization: &pso::Specialization,
        stage: naga::ShaderStage,
    ) -> Result<n::ModuleInfo, String> {
        use spirv_cross::ErrorCode as Ec;
        profiling::scope!("compile_shader_library_cross");

        // now parse again using the new overrides
        let mut ast = {
            profiling::scope!("spvc::parse");
            let module = spirv_cross::spirv::Module::from_words(raw_data);
            spirv_cross::spirv::Ast::<spirv_cross::msl::Target>::parse(&module).map_err(|err| {
                match err {
                    Ec::CompilationError(msg) => msg,
                    Ec::Unhandled => "Unexpected parse error".into(),
                }
            })?
        };

        auxil::spirv_cross_specialize_ast(&mut ast, specialization)?;

        ast.set_compiler_options(compiler_options)
            .map_err(|err| match err {
                Ec::CompilationError(msg) => msg,
                Ec::Unhandled => "Unexpected error".into(),
            })?;

        let entry_points = ast.get_entry_points().map_err(|err| match err {
            Ec::CompilationError(msg) => msg,
            Ec::Unhandled => "Unexpected entry point error".into(),
        })?;

        let shader_code = {
            profiling::scope!("spvc::compile");
            ast.compile().map_err(|err| match err {
                Ec::CompilationError(msg) => msg,
                Ec::Unhandled => "Unknown compile error".into(),
            })?
        };

        let mut entry_point_map = n::EntryPointMap::default();
        for entry_point in entry_points {
            info!("Entry point {:?}", entry_point);
            let cleansed = ast
                .get_cleansed_entry_point_name(&entry_point.name, entry_point.execution_model)
                .map_err(|err| match err {
                    Ec::CompilationError(msg) => msg,
                    Ec::Unhandled => "Unknown compile error".into(),
                })?;
            entry_point_map.insert(
                (stage, entry_point.name),
                n::EntryPoint {
                    //TODO: should we try to do better?
                    internal_name: Ok(cleansed),
                    work_group_size: [
                        entry_point.work_group_size.x,
                        entry_point.work_group_size.y,
                        entry_point.work_group_size.z,
                    ],
                },
            );
        }

        let rasterization_enabled = ast
            .is_rasterization_enabled()
            .map_err(|_| "Unknown compile error".to_string())?;

        // done
        debug!("SPIRV-Cross generated shader:\n{}", shader_code);
        let options = metal::CompileOptions::new();
        options.set_language_version(msl_version);

        let library = {
            profiling::scope!("Metal::new_library_with_source");
            device
                .lock()
                .new_library_with_source(shader_code.as_ref(), &options)
                .map_err(|err| err.to_string())?
        };

        Ok(n::ModuleInfo {
            library,
            entry_point_map,
            rasterization_enabled,
        })
    }

    fn compile_shader_library_naga(
        device: &Mutex<metal::Device>,
        shader: &d::NagaShader,
        naga_options: &naga::back::msl::Options,
        pipeline_options: &naga::back::msl::PipelineOptions,
        #[cfg(feature = "pipeline-cache")] spv_hash: u64,
        #[cfg(feature = "pipeline-cache")] spv_to_msl_cache: Option<&pipeline_cache::SpvToMsl>,
    ) -> Result<n::ModuleInfo, String> {
        profiling::scope!("compile_shader_library_naga");

        let get_module_info = || {
            profiling::scope!("naga::msl::write_string");

            let (source, info) = match naga::back::msl::write_string(
                &shader.module,
                &shader.info,
                naga_options,
                pipeline_options,
            ) {
                Ok(pair) => pair,
                Err(e) => {
                    warn!("Naga: {:?}", e);
                    return Err(format!("MSL: {:?}", e));
                }
            };

            let mut entry_point_map = n::EntryPointMap::default();
            for (ep, internal_name) in shader
                .module
                .entry_points
                .iter()
                .zip(info.entry_point_names)
            {
                entry_point_map.insert(
                    (ep.stage, ep.name.clone()),
                    n::EntryPoint {
                        internal_name,
                        work_group_size: ep.workgroup_size,
                    },
                );
            }

            debug!("Naga generated shader:\n{}", source);

            Ok(n::SerializableModuleInfo {
                source,
                entry_point_map,
                rasterization_enabled: true, //TODO
            })
        };

        #[cfg(feature = "pipeline-cache")]
        let module_info = if let Some(spv_to_msl_cache) = spv_to_msl_cache {
            let key = pipeline_cache::SpvToMslKey {
                options: naga_options.clone(),
                pipeline_options: pipeline_options.clone(),
                spv_hash,
            };

            spv_to_msl_cache
                .get_or_create_with(&key, || get_module_info().unwrap())
                .clone()
        } else {
            get_module_info()?
        };

        #[cfg(not(feature = "pipeline-cache"))]
        let module_info = get_module_info()?;

        let options = metal::CompileOptions::new();
        let msl_version = match naga_options.lang_version {
            (1, 0) => MTLLanguageVersion::V1_0,
            (1, 1) => MTLLanguageVersion::V1_1,
            (1, 2) => MTLLanguageVersion::V1_2,
            (2, 0) => MTLLanguageVersion::V2_0,
            (2, 1) => MTLLanguageVersion::V2_1,
            (2, 2) => MTLLanguageVersion::V2_2,
            (2, 3) => MTLLanguageVersion::V2_3,
            other => panic!("Unexpected language version {:?}", other),
        };
        options.set_language_version(msl_version);

        let library = {
            profiling::scope!("Metal::new_library_with_source");
            device
                .lock()
                .new_library_with_source(module_info.source.as_ref(), &options)
                .map_err(|err| {
                    warn!("Naga generated shader:\n{}", module_info.source);
                    warn!("Failed to compile: {}", err);
                    format!("{:?}", err)
                })?
        };

        Ok(n::ModuleInfo {
            library,
            entry_point_map: module_info.entry_point_map,
            rasterization_enabled: module_info.rasterization_enabled,
        })
    }

    #[cfg_attr(not(feature = "pipeline-cache"), allow(unused_variables))]
    fn load_shader(
        &self,
        ep: &pso::EntryPoint<Backend>,
        layout: &n::PipelineLayout,
        primitive_class: MTLPrimitiveTopologyClass,
        pipeline_cache: Option<&n::PipelineCache>,
        stage: naga::ShaderStage,
    ) -> Result<CompiledShader, pso::CreationError> {
        let _profiling_tag = match stage {
            naga::ShaderStage::Vertex => "vertex",
            naga::ShaderStage::Fragment => "fragment",
            naga::ShaderStage::Compute => "compute",
        };
        profiling::scope!("load_shader", _profiling_tag);

        let device = &self.shared.device;

        #[cfg(feature = "cross")]
        let mut compiler_options = layout.spirv_cross_options.clone();
        #[cfg(feature = "cross")]
        {
            compiler_options.entry_point =
                Some((ep.entry.to_string(), conv::map_naga_stage_to_cross(stage)));
            compiler_options.enable_point_size_builtin =
                primitive_class == MTLPrimitiveTopologyClass::Point;
        }
        let pipeline_options = naga::back::msl::PipelineOptions {
            allow_point_size: match primitive_class {
                MTLPrimitiveTopologyClass::Point => true,
                _ => false,
            },
        };

        let info = {
            #[cfg_attr(not(feature = "cross"), allow(unused_mut))]
            let mut result = match ep.module.naga {
                Ok(ref shader) => Self::compile_shader_library_naga(
                    device,
                    shader,
                    &layout.naga_options,
                    &pipeline_options,
                    #[cfg(feature = "pipeline-cache")]
                    ep.module.spv_hash,
                    #[cfg(feature = "pipeline-cache")]
                    pipeline_cache.as_ref().map(|cache| &cache.spv_to_msl),
                ),
                Err(ref e) => Err(e.clone()),
            };

            #[cfg(feature = "cross")]
            if result.is_err() {
                result = Self::compile_shader_library_cross(
                    device,
                    &ep.module.spv,
                    &compiler_options,
                    self.shared.private_caps.msl_version,
                    &ep.specialization,
                    stage,
                );
            }
            result.map_err(|e| {
                let error = format!("Error compiling the shader {:?}", e);
                pso::CreationError::ShaderCreationError(stage.into(), error)
            })?
        };

        // collect sizes indices
        let mut sized_bindings = Vec::new();
        if let Ok(ref shader) = ep.module.naga {
            for (_handle, var) in shader.module.global_variables.iter() {
                if let naga::TypeInner::Struct { ref members, .. } =
                    shader.module.types[var.ty].inner
                {
                    if let Some(member) = members.last() {
                        if let naga::TypeInner::Array {
                            size: naga::ArraySize::Dynamic,
                            ..
                        } = shader.module.types[member.ty].inner
                        {
                            // Note: unwraps are fine, since the MSL is already generated
                            let br = var.binding.clone().unwrap();
                            sized_bindings.push(br);
                        }
                    }
                }
            }
        }

        let lib = info.library.clone();
        let entry_key = (stage, ep.entry.to_string());
        //TODO: avoid heap-allocating the string?
        let (name, wg_size) = match info.entry_point_map.get(&entry_key) {
            Some(p) => (
                match p.internal_name {
                    Ok(ref name) => name.as_str(),
                    Err(ref e) => {
                        return Err(pso::CreationError::ShaderCreationError(
                            stage.into(),
                            format!("{}", e),
                        ))
                    }
                },
                metal::MTLSize {
                    width: p.work_group_size[0] as _,
                    height: p.work_group_size[1] as _,
                    depth: p.work_group_size[2] as _,
                },
            ),
            // this can only happen if the shader came directly from the user
            None => (
                ep.entry,
                metal::MTLSize {
                    width: 0,
                    height: 0,
                    depth: 0,
                },
            ),
        };
        let mtl_function = get_final_function(
            &lib,
            name,
            &ep.specialization,
            self.shared.private_caps.function_specialization,
        )
        .map_err(|e| {
            let error = format!("Invalid shader entry point '{}': {:?}", name, e);
            pso::CreationError::ShaderCreationError(stage.into(), error)
        })?;

        Ok(CompiledShader {
            library: lib,
            function: mtl_function,
            wg_size,
            rasterizing: info.rasterization_enabled,
            sized_bindings,
        })
    }

    fn make_sampler_descriptor(
        &self,
        info: &image::SamplerDesc,
    ) -> Option<metal::SamplerDescriptor> {
        let caps = &self.shared.private_caps;
        let descriptor = metal::SamplerDescriptor::new();

        descriptor.set_normalized_coordinates(info.normalized);

        descriptor.set_min_filter(conv::map_filter(info.min_filter));
        descriptor.set_mag_filter(conv::map_filter(info.mag_filter));
        descriptor.set_mip_filter(match info.mip_filter {
            // Note: this shouldn't be required, but Metal appears to be confused when mipmaps
            // are provided even with trivial LOD bias.
            image::Filter::Nearest if info.lod_range.end.0 < 0.5 => {
                MTLSamplerMipFilter::NotMipmapped
            }
            image::Filter::Nearest => MTLSamplerMipFilter::Nearest,
            image::Filter::Linear => MTLSamplerMipFilter::Linear,
        });

        if let Some(aniso) = info.anisotropy_clamp {
            descriptor.set_max_anisotropy(aniso as _);
        }

        let (s, t, r) = info.wrap_mode;
        descriptor.set_address_mode_s(conv::map_wrap_mode(s));
        descriptor.set_address_mode_t(conv::map_wrap_mode(t));
        descriptor.set_address_mode_r(conv::map_wrap_mode(r));

        let lod_bias = info.lod_bias.0;
        if lod_bias != 0.0 {
            if self.features.contains(hal::Features::SAMPLER_MIP_LOD_BIAS) {
                unsafe {
                    descriptor.set_lod_bias(lod_bias);
                }
            } else {
                error!("Lod bias {:?} is not supported", info.lod_bias);
            }
        }
        descriptor.set_lod_min_clamp(info.lod_range.start.0);
        descriptor.set_lod_max_clamp(info.lod_range.end.0);

        // TODO: Clarify minimum macOS version with Apple (43707452)
        if (caps.os_is_mac && caps.has_version_at_least(10, 13))
            || (!caps.os_is_mac && caps.has_version_at_least(9, 0))
        {
            descriptor.set_lod_average(true); // optimization
        }

        if let Some(fun) = info.comparison {
            if !caps.mutable_comparison_samplers {
                return None;
            }
            descriptor.set_compare_function(conv::map_compare_function(fun));
        }
        if [r, s, t].iter().any(|&am| am == image::WrapMode::Border) {
            descriptor.set_border_color(conv::map_border_color(info.border));
        }

        if caps.argument_buffers {
            descriptor.set_support_argument_buffers(true);
        }

        Some(descriptor)
    }
}

impl hal::device::Device<Backend> for Device {
    unsafe fn create_command_pool(
        &self,
        _family: QueueFamilyId,
        _flags: CommandPoolCreateFlags,
    ) -> Result<command::CommandPool, d::OutOfMemory> {
        Ok(command::CommandPool::new(
            &self.shared,
            self.online_recording.clone(),
        ))
    }

    unsafe fn destroy_command_pool(&self, mut pool: command::CommandPool) {
        use hal::pool::CommandPool as _;
        pool.reset(false);
    }

    unsafe fn create_render_pass<'a, Ia, Is, Id>(
        &self,
        attachments: Ia,
        subpasses: Is,
        _dependencies: Id,
    ) -> Result<n::RenderPass, d::OutOfMemory>
    where
        Ia: Iterator<Item = pass::Attachment>,
        Is: Iterator<Item = pass::SubpassDesc<'a>>,
    {
        let attachments: Vec<pass::Attachment> = attachments.collect();

        let mut subpasses: Vec<n::Subpass> = subpasses
            .map(|sub| {
                let mut colors: ArrayVec<[_; MAX_COLOR_ATTACHMENTS]> = sub
                    .colors
                    .iter()
                    .map(|&(id, _)| {
                        let hal_format = attachments[id].format.expect("No format!");
                        n::AttachmentInfo {
                            id,
                            resolve_id: None,
                            ops: n::AttachmentOps::empty(),
                            format: self
                                .shared
                                .private_caps
                                .map_format(hal_format)
                                .expect("Unable to map color format!"),
                            channel: Channel::from(hal_format.base_format().1),
                        }
                    })
                    .collect();
                for (color, &(resolve_id, _)) in colors.iter_mut().zip(sub.resolves.iter()) {
                    if resolve_id != pass::ATTACHMENT_UNUSED {
                        color.resolve_id = Some(resolve_id);
                    }
                }
                let depth_stencil = sub.depth_stencil.map(|&(id, _)| {
                    let hal_format = attachments[id].format.expect("No format!");
                    n::AttachmentInfo {
                        id,
                        resolve_id: None,
                        ops: n::AttachmentOps::empty(),
                        format: self
                            .shared
                            .private_caps
                            .map_format(hal_format)
                            .expect("Unable to map depth-stencil format!"),
                        channel: Channel::Float,
                    }
                });

                let samples = colors
                    .iter()
                    .chain(depth_stencil.as_ref())
                    .map(|at_info| attachments[at_info.id].samples)
                    .max()
                    .unwrap_or(1);

                n::Subpass {
                    attachments: n::SubpassData {
                        colors,
                        depth_stencil,
                    },
                    inputs: sub.inputs.iter().map(|&(id, _)| id).collect(),
                    samples,
                }
            })
            .collect();

        // sprinkle load operations
        // an attachment receives LOAD flag on a subpass if it's the first sub-pass that uses it
        let mut use_mask = 0u64;
        for sub in subpasses.iter_mut() {
            for at in sub.attachments.colors.iter_mut() {
                if use_mask & 1 << at.id == 0 {
                    at.ops |= n::AttachmentOps::LOAD;
                    use_mask ^= 1 << at.id;
                }
            }
            if let Some(ref mut at) = sub.attachments.depth_stencil {
                if use_mask & 1 << at.id == 0 {
                    at.ops |= n::AttachmentOps::LOAD;
                    use_mask ^= 1 << at.id;
                }
            }
        }
        // sprinkle store operations
        // an attachment receives STORE flag on a subpass if it's the last sub-pass that uses it
        for sub in subpasses.iter_mut().rev() {
            for at in sub.attachments.colors.iter_mut() {
                if use_mask & 1 << at.id != 0 {
                    at.ops |= n::AttachmentOps::STORE;
                    use_mask ^= 1 << at.id;
                }
            }
            if let Some(ref mut at) = sub.attachments.depth_stencil {
                if use_mask & 1 << at.id != 0 {
                    at.ops |= n::AttachmentOps::STORE;
                    use_mask ^= 1 << at.id;
                }
            }
        }

        Ok(n::RenderPass {
            attachments,
            subpasses,
            name: String::new(),
        })
    }

    unsafe fn create_pipeline_layout<'a, Is, Ic>(
        &self,
        set_layouts: Is,
        push_constant_ranges: Ic,
    ) -> Result<n::PipelineLayout, d::OutOfMemory>
    where
        Is: Iterator<Item = &'a n::DescriptorSetLayout>,
        Ic: Iterator<Item = (pso::ShaderStageFlags, Range<u32>)>,
    {
        #[derive(Debug)]
        struct StageInfo {
            stage: naga::ShaderStage,
            counters: n::ResourceData<ResourceIndex>,
            push_constant_buffer: Option<ResourceIndex>,
            sizes_buffer: Option<ResourceIndex>,
            sizes_count: u8,
        }
        let mut stage_infos = [
            StageInfo {
                stage: naga::ShaderStage::Vertex,
                counters: n::ResourceData::new(),
                push_constant_buffer: None,
                sizes_buffer: None,
                sizes_count: 0,
            },
            StageInfo {
                stage: naga::ShaderStage::Fragment,
                counters: n::ResourceData::new(),
                push_constant_buffer: None,
                sizes_buffer: None,
                sizes_count: 0,
            },
            StageInfo {
                stage: naga::ShaderStage::Compute,
                counters: n::ResourceData::new(),
                push_constant_buffer: None,
                sizes_buffer: None,
                sizes_count: 0,
            },
        ];
        let mut binding_map = BTreeMap::default();
        let mut argument_buffer_bindings = FastHashMap::default();
        let mut inline_samplers = Vec::new();
        #[cfg(feature = "cross")]
        let mut cross_const_samplers = BTreeMap::new();
        let mut infos = Vec::new();

        // First, place the push constants
        let mut pc_limits = [0u32; 3];
        for (flags, range) in push_constant_ranges {
            for (limit, info) in pc_limits.iter_mut().zip(&stage_infos) {
                if flags.contains(info.stage.into()) {
                    debug_assert_eq!(range.end % 4, 0);
                    *limit = (range.end / 4).max(*limit);
                }
            }
        }

        const LIMIT_MASK: u32 = 3;
        // round up the limits alignment to 4, so that it matches MTL compiler logic
        //TODO: figure out what and how exactly does the alignment. Clearly, it's not
        // straightforward, given that value of 2 stays non-aligned.
        for limit in &mut pc_limits {
            if *limit > LIMIT_MASK {
                *limit = (*limit + LIMIT_MASK) & !LIMIT_MASK;
            }
        }

        for (limit, info) in pc_limits.iter().zip(stage_infos.iter_mut()) {
            // handle the push constant buffer assignment and shader overrides
            if *limit != 0 {
                info.push_constant_buffer = Some(info.counters.buffers);
                info.counters.buffers += 1;
            }
        }

        // Second, place the descripted resources
        for (set_index, set_layout) in set_layouts.enumerate() {
            // remember where the resources for this set start at each shader stage
            let mut dynamic_buffers = Vec::new();
            let mut sized_buffer_bindings = Vec::new();
            let offsets = n::MultiStageResourceCounters {
                vs: stage_infos[0].counters.clone(),
                ps: stage_infos[1].counters.clone(),
                cs: stage_infos[2].counters.clone(),
            };

            match *set_layout {
                n::DescriptorSetLayout::Emulated {
                    layouts: ref desc_layouts,
                    ref immutable_samplers,
                    ..
                } => {
                    #[cfg(feature = "cross")]
                    for (&binding, immutable_sampler) in immutable_samplers.iter() {
                        //TODO: array support?
                        cross_const_samplers.insert(
                            spirv_cross::msl::SamplerLocation {
                                desc_set: set_index as u32,
                                binding,
                            },
                            immutable_sampler.cross_data.clone(),
                        );
                    }
                    for layout in desc_layouts.iter() {
                        if layout.content.contains(n::DescriptorContent::SIZED_BUFFER) {
                            sized_buffer_bindings.push((layout.binding, layout.stages));
                            if layout.stages.contains(pso::ShaderStageFlags::VERTEX) {
                                stage_infos[0].sizes_count += 1;
                            }
                            if layout.stages.contains(pso::ShaderStageFlags::FRAGMENT) {
                                stage_infos[1].sizes_count += 1;
                            }
                            if layout.stages.contains(pso::ShaderStageFlags::COMPUTE) {
                                stage_infos[2].sizes_count += 1;
                            }
                        }

                        if layout
                            .content
                            .contains(n::DescriptorContent::DYNAMIC_BUFFER)
                        {
                            dynamic_buffers.alloc().init(n::MultiStageData {
                                vs: if layout.stages.contains(pso::ShaderStageFlags::VERTEX) {
                                    stage_infos[0].counters.buffers
                                } else {
                                    !0
                                },
                                ps: if layout.stages.contains(pso::ShaderStageFlags::FRAGMENT) {
                                    stage_infos[1].counters.buffers
                                } else {
                                    !0
                                },
                                cs: if layout.stages.contains(pso::ShaderStageFlags::COMPUTE) {
                                    stage_infos[2].counters.buffers
                                } else {
                                    !0
                                },
                            });
                        }

                        for info in stage_infos.iter_mut() {
                            if !layout.stages.contains(info.stage.into()) {
                                continue;
                            }
                            let target = naga::back::msl::BindTarget {
                                buffer: if layout.content.contains(n::DescriptorContent::BUFFER) {
                                    Some(info.counters.buffers as _)
                                } else {
                                    None
                                },
                                texture: if layout.content.contains(n::DescriptorContent::TEXTURE) {
                                    Some(info.counters.textures as _)
                                } else {
                                    None
                                },
                                sampler: if layout
                                    .content
                                    .contains(n::DescriptorContent::IMMUTABLE_SAMPLER)
                                {
                                    let immutable_sampler = &immutable_samplers[&layout.binding];
                                    let handle = inline_samplers.len()
                                        as naga::back::msl::InlineSamplerIndex;
                                    inline_samplers.push(immutable_sampler.data.clone());
                                    Some(naga::back::msl::BindSamplerTarget::Inline(handle))
                                } else if layout.content.contains(n::DescriptorContent::SAMPLER) {
                                    Some(naga::back::msl::BindSamplerTarget::Resource(
                                        info.counters.samplers as _,
                                    ))
                                } else {
                                    None
                                },
                                mutable: layout.content.contains(n::DescriptorContent::WRITABLE),
                            };
                            info.counters.add(layout.content);
                            if layout.array_index == 0 {
                                let source = naga::back::msl::BindSource {
                                    stage: info.stage,
                                    group: set_index as _,
                                    binding: layout.binding,
                                };
                                binding_map.insert(source, target);
                            }
                        }
                    }
                }
                n::DescriptorSetLayout::ArgumentBuffer {
                    bindings: _,
                    stage_flags,
                    ..
                } => {
                    for info in stage_infos.iter_mut() {
                        if !stage_flags.contains(info.stage.into()) {
                            continue;
                        }
                        //TODO: mark `bindings` as belonging to the argument buffer
                        argument_buffer_bindings
                            .insert((info.stage, set_index as u32), info.counters.buffers);
                        info.counters.buffers += 1;
                    }
                }
            }

            infos.alloc().init(n::DescriptorSetInfo {
                offsets,
                dynamic_buffers,
                sized_buffer_bindings,
            });
        }

        // Finally, make sure we fit the limits
        for info in stage_infos.iter_mut() {
            // handle the sizes buffer assignment and shader overrides
            if info.sizes_count != 0 {
                info.sizes_buffer = Some(info.counters.buffers);
                info.counters.buffers += 1;
            }
            if info.counters.buffers > self.shared.private_caps.max_buffers_per_stage
                || info.counters.textures > self.shared.private_caps.max_textures_per_stage
                || info.counters.samplers > self.shared.private_caps.max_samplers_per_stage
            {
                log::error!("Resource limit exceeded: {:?}", info);
                return Err(d::OutOfMemory::Host);
            }
        }

        #[cfg(feature = "cross")]
        let spirv_cross_options = {
            use spirv_cross::msl;
            const PUSH_CONSTANTS_DESC_SET: u32 = !0;
            const PUSH_CONSTANTS_DESC_BINDING: u32 = 0;

            let mut compiler_options = msl::CompilerOptions::default();
            compiler_options.version = match self.shared.private_caps.msl_version {
                MTLLanguageVersion::V1_0 => msl::Version::V1_0,
                MTLLanguageVersion::V1_1 => msl::Version::V1_1,
                MTLLanguageVersion::V1_2 => msl::Version::V1_2,
                MTLLanguageVersion::V2_0 => msl::Version::V2_0,
                MTLLanguageVersion::V2_1 => msl::Version::V2_1,
                MTLLanguageVersion::V2_2 => msl::Version::V2_2,
                MTLLanguageVersion::V2_3 => msl::Version::V2_3,
            };
            compiler_options.enable_point_size_builtin = false;
            compiler_options.vertex.invert_y = !self.features.contains(hal::Features::NDC_Y_UP);
            // populate resource overrides
            for (source, target) in binding_map.iter() {
                compiler_options.resource_binding_overrides.insert(
                    msl::ResourceBindingLocation {
                        stage: conv::map_naga_stage_to_cross(source.stage),
                        desc_set: source.group,
                        binding: source.binding,
                    },
                    msl::ResourceBinding {
                        buffer_id: target.buffer.map_or(!0, |id| id as u32),
                        texture_id: target.texture.map_or(!0, |id| id as u32),
                        sampler_id: match target.sampler {
                            Some(naga::back::msl::BindSamplerTarget::Resource(id)) => id as u32,
                            _ => !0,
                        },
                        count: 0,
                    },
                );
            }
            // argument buffers
            for ((stage, desc_set), buffer_id) in argument_buffer_bindings {
                compiler_options.resource_binding_overrides.insert(
                    msl::ResourceBindingLocation {
                        stage: conv::map_naga_stage_to_cross(stage),
                        desc_set,
                        binding: msl::ARGUMENT_BUFFER_BINDING,
                    },
                    msl::ResourceBinding {
                        buffer_id,
                        texture_id: !0,
                        sampler_id: !0,
                        count: 0,
                    },
                );
                //TODO: assign argument buffer locations
            }
            // push constants
            for info in stage_infos.iter() {
                let buffer_id = match info.push_constant_buffer {
                    Some(id) => id,
                    None => continue,
                };
                compiler_options.resource_binding_overrides.insert(
                    msl::ResourceBindingLocation {
                        stage: conv::map_naga_stage_to_cross(info.stage),
                        desc_set: PUSH_CONSTANTS_DESC_SET,
                        binding: PUSH_CONSTANTS_DESC_BINDING,
                    },
                    msl::ResourceBinding {
                        buffer_id,
                        texture_id: !0,
                        sampler_id: !0,
                        count: 0,
                    },
                );
            }
            // other properties
            compiler_options.const_samplers = cross_const_samplers;
            compiler_options.enable_argument_buffers = self.shared.private_caps.argument_buffers;
            compiler_options.force_zero_initialized_variables = true;
            compiler_options.force_native_arrays = true;

            let mut compiler_options_point = compiler_options.clone();
            compiler_options_point.enable_point_size_builtin = true;
            compiler_options
        };

        let naga_options = naga::back::msl::Options {
            lang_version: match self.shared.private_caps.msl_version {
                MTLLanguageVersion::V1_0 => (1, 0),
                MTLLanguageVersion::V1_1 => (1, 1),
                MTLLanguageVersion::V1_2 => (1, 2),
                MTLLanguageVersion::V2_0 => (2, 0),
                MTLLanguageVersion::V2_1 => (2, 1),
                MTLLanguageVersion::V2_2 => (2, 2),
                MTLLanguageVersion::V2_3 => (2, 3),
            },
            binding_map,
            inline_samplers,
            spirv_cross_compatibility: cfg!(feature = "cross"),
            fake_missing_bindings: false,
            per_stage_map: naga::back::msl::PerStageMap {
                vs: naga::back::msl::PerStageResources {
                    push_constant_buffer: stage_infos[0]
                        .push_constant_buffer
                        .map(|buffer_index| buffer_index as naga::back::msl::Slot),
                    sizes_buffer: stage_infos[0]
                        .sizes_buffer
                        .map(|buffer_index| buffer_index as naga::back::msl::Slot),
                },
                fs: naga::back::msl::PerStageResources {
                    push_constant_buffer: stage_infos[1]
                        .push_constant_buffer
                        .map(|buffer_index| buffer_index as naga::back::msl::Slot),
                    sizes_buffer: stage_infos[1]
                        .sizes_buffer
                        .map(|buffer_index| buffer_index as naga::back::msl::Slot),
                },
                cs: naga::back::msl::PerStageResources {
                    push_constant_buffer: stage_infos[2]
                        .push_constant_buffer
                        .map(|buffer_index| buffer_index as naga::back::msl::Slot),
                    sizes_buffer: stage_infos[2]
                        .sizes_buffer
                        .map(|buffer_index| buffer_index as naga::back::msl::Slot),
                },
            },
        };

        Ok(n::PipelineLayout {
            #[cfg(feature = "cross")]
            spirv_cross_options,
            naga_options,
            infos,
            total: n::MultiStageResourceCounters {
                vs: stage_infos[0].counters.clone(),
                ps: stage_infos[1].counters.clone(),
                cs: stage_infos[2].counters.clone(),
            },
            push_constants: n::MultiStageData {
                vs: stage_infos[0]
                    .push_constant_buffer
                    .map(|buffer_index| n::PushConstantInfo {
                        count: pc_limits[0],
                        buffer_index,
                    }),
                ps: stage_infos[1]
                    .push_constant_buffer
                    .map(|buffer_index| n::PushConstantInfo {
                        count: pc_limits[1],
                        buffer_index,
                    }),
                cs: stage_infos[2]
                    .push_constant_buffer
                    .map(|buffer_index| n::PushConstantInfo {
                        count: pc_limits[2],
                        buffer_index,
                    }),
            },
            total_push_constants: pc_limits[0].max(pc_limits[1]).max(pc_limits[2]),
        })
    }

    #[cfg(not(feature = "pipeline-cache"))]
    unsafe fn create_pipeline_cache(
        &self,
        _data: Option<&[u8]>,
    ) -> Result<n::PipelineCache, d::OutOfMemory> {
        Ok(())
    }

    #[cfg(feature = "pipeline-cache")]
    unsafe fn create_pipeline_cache(
        &self,
        data: Option<&[u8]>,
    ) -> Result<n::PipelineCache, d::OutOfMemory> {
        let device = self.shared.device.lock();

        let create_binary_archive = |data: &[u8]| {
            if self.shared.private_caps.supports_binary_archives {
                let descriptor = metal::BinaryArchiveDescriptor::new();

                // We need to keep the temp file alive so that it doesn't get deleted until after a
                // binary archive has been created.
                let _temp_file = if !data.is_empty() {
                    // It would be nice to use a `data:text/plain;base64` url here and just pass in a
                    // base64-encoded version of the data, but metal validation doesn't like that:
                    // -[MTLDebugDevice newBinaryArchiveWithDescriptor:error:]:1046: failed assertion `url, if not nil, must be a file URL.'

                    let temp_file = tempfile::NamedTempFile::new().unwrap();
                    temp_file.as_file().write_all(&data).unwrap();

                    let url = metal::URL::new_with_string(&format!(
                        "file://{}",
                        temp_file.path().display()
                    ));
                    descriptor.set_url(&url);

                    Some(temp_file)
                } else {
                    None
                };

                Ok(Some(pipeline_cache::BinaryArchive {
                    inner: device
                        .new_binary_archive_with_descriptor(&descriptor)
                        .map_err(|_| d::OutOfMemory::Device)?,
                    is_empty: AtomicBool::new(data.is_empty()),
                }))
            } else {
                Ok(None)
            }
        };

        if let Some(data) = data.filter(|data| !data.is_empty()) {
            let pipeline_cache: pipeline_cache::SerializablePipelineCache =
                bincode::deserialize(data).unwrap();

            Ok(n::PipelineCache {
                binary_archive: create_binary_archive(&pipeline_cache.binary_archive)?,
                spv_to_msl: pipeline_cache::load_spv_to_msl_cache(pipeline_cache.spv_to_msl),
            })
        } else {
            Ok(n::PipelineCache {
                binary_archive: create_binary_archive(&[])?,
                spv_to_msl: Default::default(),
            })
        }
    }

    #[cfg(not(feature = "pipeline-cache"))]
    unsafe fn get_pipeline_cache_data(
        &self,
        _cache: &n::PipelineCache,
    ) -> Result<Vec<u8>, d::OutOfMemory> {
        Ok(Vec::new())
    }

    #[cfg(feature = "pipeline-cache")]
    unsafe fn get_pipeline_cache_data(
        &self,
        cache: &n::PipelineCache,
    ) -> Result<Vec<u8>, d::OutOfMemory> {
        let binary_archive = || {
            let binary_archive = match cache.binary_archive {
                Some(ref binary_archive) => binary_archive,
                None => return Ok(Vec::new()),
            };

            // Without this, we get an extremely vague "Serialization of binaries to file failed"
            // error when serializing an empty binary archive.
            if binary_archive.is_empty.load(Ordering::Relaxed) {
                return Ok(Vec::new());
            }

            let temp_path = tempfile::NamedTempFile::new().unwrap().into_temp_path();
            let tmp_file_url =
                metal::URL::new_with_string(&format!("file://{}", temp_path.display()));

            binary_archive
                .inner
                .serialize_to_url(&tmp_file_url)
                .unwrap();

            let bytes = std::fs::read(&temp_path).unwrap();
            Ok(bytes)
        };

        Ok(
            bincode::serialize(&pipeline_cache::SerializablePipelineCache {
                binary_archive: &binary_archive()?,
                spv_to_msl: pipeline_cache::serialize_spv_to_msl_cache(&cache.spv_to_msl),
            })
            .unwrap(),
        )
    }

    unsafe fn destroy_pipeline_cache(&self, _cache: n::PipelineCache) {
        //drop
    }

    unsafe fn merge_pipeline_caches<'a, I>(
        &self,
        _target: &mut n::PipelineCache,
        _sources: I,
    ) -> Result<(), d::OutOfMemory>
    where
        I: Iterator<Item = &'a n::PipelineCache>,
    {
        warn!("`merge_pipeline_caches` is not currently implemented on the Metal backend.");
        Ok(())
    }

    unsafe fn create_graphics_pipeline<'a>(
        &self,
        pipeline_desc: &pso::GraphicsPipelineDesc<'a, Backend>,
        cache: Option<&n::PipelineCache>,
    ) -> Result<n::GraphicsPipeline, pso::CreationError> {
        profiling::scope!("create_graphics_pipeline");
        trace!("create_graphics_pipeline {:#?}", pipeline_desc);

        let pipeline = metal::RenderPipelineDescriptor::new();
        let pipeline_layout = &pipeline_desc.layout;
        let (rp_attachments, subpass) = {
            let pass::Subpass { main_pass, index } = pipeline_desc.subpass;
            (&main_pass.attachments, &main_pass.subpasses[index as usize])
        };

        let (desc_vertex_buffers, attributes, input_assembler, vs_ep) =
            match pipeline_desc.primitive_assembler {
                pso::PrimitiveAssemblerDesc::Vertex {
                    tessellation: Some(_),
                    ..
                } => {
                    error!("Tessellation is not supported");
                    return Err(pso::CreationError::UnsupportedPipeline);
                }
                pso::PrimitiveAssemblerDesc::Vertex {
                    geometry: Some(_), ..
                } => {
                    error!("Geometry shader is not supported");
                    return Err(pso::CreationError::UnsupportedPipeline);
                }
                pso::PrimitiveAssemblerDesc::Mesh { .. } => {
                    error!("Mesh shader is not supported");
                    return Err(pso::CreationError::UnsupportedPipeline);
                }
                pso::PrimitiveAssemblerDesc::Vertex {
                    buffers,
                    attributes,
                    ref input_assembler,
                    ref vertex,
                    tessellation: _,
                    geometry: _,
                } => (buffers, attributes, input_assembler, vertex),
            };

        let (primitive_class, primitive_type) = match input_assembler.primitive {
            pso::Primitive::PointList => {
                (MTLPrimitiveTopologyClass::Point, MTLPrimitiveType::Point)
            }
            pso::Primitive::LineList => (MTLPrimitiveTopologyClass::Line, MTLPrimitiveType::Line),
            pso::Primitive::LineStrip => {
                (MTLPrimitiveTopologyClass::Line, MTLPrimitiveType::LineStrip)
            }
            pso::Primitive::TriangleList => (
                MTLPrimitiveTopologyClass::Triangle,
                MTLPrimitiveType::Triangle,
            ),
            pso::Primitive::TriangleStrip => (
                MTLPrimitiveTopologyClass::Triangle,
                MTLPrimitiveType::TriangleStrip,
            ),
            pso::Primitive::PatchList(_) => (
                MTLPrimitiveTopologyClass::Unspecified,
                MTLPrimitiveType::Point,
            ),
        };
        if self.shared.private_caps.layered_rendering {
            pipeline.set_input_primitive_topology(primitive_class);
        }

        // Vertex shader
        let vs = self.load_shader(
            vs_ep,
            pipeline_layout,
            primitive_class,
            cache,
            naga::ShaderStage::Vertex,
        )?;

        pipeline.set_vertex_function(Some(&vs.function));

        // Fragment shader
        let fs = match pipeline_desc.fragment {
            Some(ref ep) => Some(self.load_shader(
                ep,
                pipeline_layout,
                primitive_class,
                cache,
                naga::ShaderStage::Fragment,
            )?),
            None => {
                // TODO: This is a workaround for what appears to be a Metal validation bug
                // A pixel format is required even though no attachments are provided
                if subpass.attachments.colors.is_empty()
                    && subpass.attachments.depth_stencil.is_none()
                {
                    pipeline.set_depth_attachment_pixel_format(metal::MTLPixelFormat::Depth32Float);
                }
                None
            }
        };

        if let Some(ref compiled) = fs {
            pipeline.set_fragment_function(Some(&compiled.function));
        }
        pipeline.set_rasterization_enabled(vs.rasterizing);

        // Assign target formats
        let blend_targets = pipeline_desc
            .blender
            .targets
            .iter()
            .chain(iter::repeat(&pso::ColorBlendDesc::EMPTY));
        for (i, (at, color_desc)) in subpass
            .attachments
            .colors
            .iter()
            .zip(blend_targets)
            .enumerate()
        {
            let desc = pipeline
                .color_attachments()
                .object_at(i as u64)
                .expect("too many color attachments");

            desc.set_pixel_format(at.format);
            desc.set_write_mask(conv::map_write_mask(color_desc.mask));

            if let Some(ref blend) = color_desc.blend {
                desc.set_blending_enabled(true);
                let (color_op, color_src, color_dst) = conv::map_blend_op(blend.color);
                let (alpha_op, alpha_src, alpha_dst) = conv::map_blend_op(blend.alpha);

                desc.set_rgb_blend_operation(color_op);
                desc.set_source_rgb_blend_factor(color_src);
                desc.set_destination_rgb_blend_factor(color_dst);

                desc.set_alpha_blend_operation(alpha_op);
                desc.set_source_alpha_blend_factor(alpha_src);
                desc.set_destination_alpha_blend_factor(alpha_dst);
            }
        }
        if let Some(ref at) = subpass.attachments.depth_stencil {
            let orig_format = rp_attachments[at.id].format.unwrap();
            if orig_format.is_depth() {
                pipeline.set_depth_attachment_pixel_format(at.format);
            }
            if orig_format.is_stencil() {
                pipeline.set_stencil_attachment_pixel_format(at.format);
            }
        }

        // Vertex buffers
        let vertex_descriptor = metal::VertexDescriptor::new();
        let mut vertex_buffers: n::VertexBufferVec = Vec::new();
        trace!("Vertex attribute remapping started");

        for &pso::AttributeDesc {
            location,
            binding,
            element,
        } in attributes
        {
            let original = desc_vertex_buffers
                .iter()
                .find(|vb| vb.binding == binding)
                .expect("no associated vertex buffer found");
            // handle wrapping offsets
            let elem_size = element.format.surface_desc().bits as pso::ElemOffset / 8;
            let (cut_offset, base_offset) =
                if original.stride == 0 || element.offset + elem_size <= original.stride {
                    (element.offset, 0)
                } else {
                    let remainder = element.offset % original.stride;
                    if remainder + elem_size <= original.stride {
                        (remainder, element.offset - remainder)
                    } else {
                        (0, element.offset)
                    }
                };
            let relative_index = vertex_buffers
                .iter()
                .position(|(ref vb, offset)| vb.binding == binding && base_offset == *offset)
                .unwrap_or_else(|| {
                    vertex_buffers.alloc().init((original.clone(), base_offset));
                    vertex_buffers.len() - 1
                });
            let mtl_buffer_index = self.shared.private_caps.max_buffers_per_stage
                - 1
                - (relative_index as ResourceIndex);
            if mtl_buffer_index < pipeline_layout.total.vs.buffers {
                error!("Attribute offset {} exceeds the stride {}, and there is no room for replacement.",
                    element.offset, original.stride);
                return Err(pso::CreationError::Other);
            }
            trace!("\tAttribute[{}] is mapped to vertex buffer[{}] with binding {} and offsets {} + {}",
                location, binding, mtl_buffer_index, base_offset, cut_offset);
            // pass the refined data to Metal
            let mtl_attribute_desc = vertex_descriptor
                .attributes()
                .object_at(location as u64)
                .expect("too many vertex attributes");
            let mtl_vertex_format =
                conv::map_vertex_format(element.format).expect("unsupported vertex format");
            mtl_attribute_desc.set_format(mtl_vertex_format);
            mtl_attribute_desc.set_buffer_index(mtl_buffer_index as _);
            mtl_attribute_desc.set_offset(cut_offset as _);
        }

        for (i, (vb, _)) in vertex_buffers.iter().enumerate() {
            let mtl_buffer_desc = vertex_descriptor
                .layouts()
                .object_at(self.shared.private_caps.max_buffers_per_stage as u64 - 1 - i as u64)
                .expect("too many vertex descriptor layouts");
            if vb.stride % STRIDE_GRANULARITY != 0 {
                error!(
                    "Stride ({}) must be a multiple of {}",
                    vb.stride, STRIDE_GRANULARITY
                );
                return Err(pso::CreationError::Other);
            }
            if vb.stride != 0 {
                mtl_buffer_desc.set_stride(vb.stride as u64);
                match vb.rate {
                    VertexInputRate::Vertex => {
                        mtl_buffer_desc.set_step_function(MTLVertexStepFunction::PerVertex);
                    }
                    VertexInputRate::Instance(divisor) => {
                        mtl_buffer_desc.set_step_function(MTLVertexStepFunction::PerInstance);
                        mtl_buffer_desc.set_step_rate(divisor as u64);
                    }
                }
            } else {
                mtl_buffer_desc.set_stride(256); // big enough to fit all the elements
                mtl_buffer_desc.set_step_function(MTLVertexStepFunction::PerInstance);
                mtl_buffer_desc.set_step_rate(!0);
            }
        }
        if !vertex_buffers.is_empty() {
            pipeline.set_vertex_descriptor(Some(&vertex_descriptor));
        }

        if let pso::State::Static(w) = pipeline_desc.rasterizer.line_width {
            if w != 1.0 {
                warn!("Unsupported line width: {:?}", w);
            }
        }

        let rasterizer_state = Some(n::RasterizerState {
            front_winding: conv::map_winding(pipeline_desc.rasterizer.front_face),
            fill_mode: conv::map_polygon_mode(pipeline_desc.rasterizer.polygon_mode),
            cull_mode: match conv::map_cull_face(pipeline_desc.rasterizer.cull_face) {
                Some(mode) => mode,
                None => {
                    //TODO - Metal validation fails with
                    // RasterizationEnabled is false but the vertex shader's return type is not void
                    error!("Culling both sides is not yet supported");
                    //pipeline.set_rasterization_enabled(false);
                    metal::MTLCullMode::None
                }
            },
            depth_clip: if self.shared.private_caps.depth_clip_mode {
                Some(if pipeline_desc.rasterizer.depth_clamping {
                    metal::MTLDepthClipMode::Clamp
                } else {
                    metal::MTLDepthClipMode::Clip
                })
            } else {
                None
            },
        });
        let depth_bias = pipeline_desc
            .rasterizer
            .depth_bias
            .unwrap_or(pso::State::Static(pso::DepthBias::default()));

        // prepare the depth-stencil state now
        let device = self.shared.device.lock();
        self.shared
            .service_pipes
            .depth_stencil_states
            .prepare(&pipeline_desc.depth_stencil, &*device);

        let samples = if let Some(multisampling) = &pipeline_desc.multisampling {
            pipeline.set_sample_count(multisampling.rasterization_samples as u64);
            pipeline.set_alpha_to_coverage_enabled(multisampling.alpha_coverage);
            pipeline.set_alpha_to_one_enabled(multisampling.alpha_to_one);
            // TODO: sample_mask
            // TODO: sample_shading
            multisampling.rasterization_samples
        } else {
            1
        };

        if let Some(name) = pipeline_desc.label {
            pipeline.set_label(name);
        }

        profiling::scope!("Metal::new_render_pipeline_state");

        #[cfg(feature = "pipeline-cache")]
        if let Some(binary_archive) = pipeline_cache::pipeline_cache_to_binary_archive(cache) {
            pipeline.set_binary_archives(&[&binary_archive.inner]);
        }

        let (fs_lib, ps_sized_bindings) = match fs {
            Some(compiled) => (Some(compiled.library), compiled.sized_bindings),
            None => (None, Vec::new()),
        };

        let pipeline_state = device
            // Replace this with `new_render_pipeline_state_with_fail_on_binary_archive_miss`
            // to debug that the cache is actually working.
            .new_render_pipeline_state(&pipeline)
            .map(|raw| n::GraphicsPipeline {
                vs_lib: vs.library,
                fs_lib,
                raw,
                primitive_type,
                vs_info: n::PipelineStageInfo {
                    push_constants: pipeline_desc.layout.push_constants.vs,
                    sizes_slot: pipeline_desc
                        .layout
                        .naga_options
                        .per_stage_map
                        .vs
                        .sizes_buffer,
                    sized_bindings: vs.sized_bindings,
                },
                ps_info: n::PipelineStageInfo {
                    push_constants: pipeline_desc.layout.push_constants.ps,
                    sizes_slot: pipeline_desc
                        .layout
                        .naga_options
                        .per_stage_map
                        .fs
                        .sizes_buffer,
                    sized_bindings: ps_sized_bindings,
                },
                rasterizer_state,
                depth_bias,
                depth_stencil_desc: pipeline_desc.depth_stencil.clone(),
                baked_states: pipeline_desc.baked_states.clone(),
                vertex_buffers,
                attachment_formats: subpass.attachments.map(|at| (at.format, at.channel)),
                samples,
            })
            .map_err(|err| {
                error!("PSO creation failed: {}", err);
                pso::CreationError::Other
            })?;

        // We need to add the pipline descriptor to the binary archive after creating the
        // pipeline, otherwise `new_render_pipeline_state_with_fail_on_binary_archive_miss`
        // succeeds when it shouldn't.
        #[cfg(feature = "pipeline-cache")]
        if let Some(binary_archive) = pipeline_cache::pipeline_cache_to_binary_archive(cache) {
            binary_archive
                .inner
                .add_render_pipeline_functions_with_descriptor(&pipeline)
                .unwrap();
            binary_archive.is_empty.store(false, Ordering::Relaxed);
        }

        Ok(pipeline_state)
    }

    unsafe fn create_compute_pipeline<'a>(
        &self,
        pipeline_desc: &pso::ComputePipelineDesc<'a, Backend>,
        cache: Option<&n::PipelineCache>,
    ) -> Result<n::ComputePipeline, pso::CreationError> {
        profiling::scope!("create_compute_pipeline");
        trace!("create_compute_pipeline {:?}", pipeline_desc);
        let pipeline = metal::ComputePipelineDescriptor::new();

        let cs = self.load_shader(
            &pipeline_desc.shader,
            &pipeline_desc.layout,
            MTLPrimitiveTopologyClass::Unspecified,
            cache,
            naga::ShaderStage::Compute,
        )?;
        pipeline.set_compute_function(Some(&cs.function));
        if let Some(name) = pipeline_desc.label {
            pipeline.set_label(name);
        }

        profiling::scope!("Metal::new_compute_pipeline_state");

        #[cfg(feature = "pipeline-cache")]
        if let Some(binary_archive) = pipeline_cache::pipeline_cache_to_binary_archive(cache) {
            pipeline.set_binary_archives(&[&binary_archive.inner]);
        }

        let pipeline_state = self
            .shared
            .device
            .lock()
            .new_compute_pipeline_state(&pipeline)
            .map(|raw| n::ComputePipeline {
                cs_lib: cs.library,
                raw,
                work_group_size: cs.wg_size,
                info: n::PipelineStageInfo {
                    push_constants: pipeline_desc.layout.push_constants.cs,
                    sizes_slot: pipeline_desc
                        .layout
                        .naga_options
                        .per_stage_map
                        .cs
                        .sizes_buffer,
                    sized_bindings: cs.sized_bindings,
                },
            })
            .map_err(|err| {
                error!("PSO creation failed: {}", err);
                pso::CreationError::Other
            })?;

        // We need to add the pipline descriptor to the binary archive after creating the
        // pipeline, see `create_graphics_pipeline`.
        #[cfg(feature = "pipeline-cache")]
        if let Some(binary_archive) = pipeline_cache::pipeline_cache_to_binary_archive(cache) {
            binary_archive
                .inner
                .add_compute_pipeline_functions_with_descriptor(&pipeline)
                .unwrap();
            binary_archive.is_empty.store(false, Ordering::Relaxed)
        }

        Ok(pipeline_state)
    }

    unsafe fn create_framebuffer<I>(
        &self,
        _render_pass: &n::RenderPass,
        _attachments: I,
        extent: image::Extent,
    ) -> Result<n::Framebuffer, d::OutOfMemory> {
        Ok(n::Framebuffer { extent })
    }

    unsafe fn create_shader_module(
        &self,
        raw_data: &[u32],
    ) -> Result<n::ShaderModule, d::ShaderError> {
        profiling::scope!("create_shader_module");
        Ok(n::ShaderModule {
            #[cfg(feature = "cross")]
            spv: raw_data.to_vec(),
            #[cfg(feature = "pipeline-cache")]
            spv_hash: fxhash::hash64(raw_data),
            naga: if cfg!(feature = "cross") {
                Err("Cross is enabled".into())
            } else {
                let options = naga::front::spv::Options {
                    adjust_coordinate_space: !self.features.contains(hal::Features::NDC_Y_UP),
                    strict_capabilities: true,
                    flow_graph_dump_prefix: None,
                };
                let parse_result = {
                    profiling::scope!("naga::spv::parse");
                    let parser = naga::front::spv::Parser::new(raw_data.iter().cloned(), &options);
                    parser.parse()
                };
                match parse_result {
                    Ok(module) => {
                        debug!("Naga module {:#?}", module);
                        match naga::valid::Validator::new(
                            naga::valid::ValidationFlags::empty(),
                            naga::valid::Capabilities::PUSH_CONSTANT,
                        )
                        .validate(&module)
                        {
                            Ok(info) => Ok(d::NagaShader { module, info }),
                            Err(e) => Err(format!("Naga validation: {}", e)),
                        }
                    }
                    Err(e) => Err(format!("Naga parsing: {:?}", e)),
                }
            },
        })
    }

    unsafe fn create_shader_module_from_naga(
        &self,
        shader: d::NagaShader,
    ) -> Result<n::ShaderModule, (d::ShaderError, d::NagaShader)> {
        profiling::scope!("create_shader_module_from_naga");

        #[cfg(any(feature = "pipeline-cache", feature = "cross"))]
        let spv = match naga::back::spv::write_vec(&shader.module, &shader.info, &self.spv_options)
        {
            Ok(spv) => spv,
            Err(e) => return Err((d::ShaderError::CompilationFailed(format!("{}", e)), shader)),
        };

        Ok(n::ShaderModule {
            #[cfg(feature = "pipeline-cache")]
            spv_hash: fxhash::hash64(&spv),
            #[cfg(feature = "cross")]
            spv,
            naga: Ok(shader),
        })
    }

    unsafe fn create_sampler(
        &self,
        info: &image::SamplerDesc,
    ) -> Result<n::Sampler, d::AllocationError> {
        Ok(n::Sampler {
            raw: match self.make_sampler_descriptor(info) {
                Some(ref descriptor) => Some(self.shared.device.lock().new_sampler(descriptor)),
                None => None,
            },
            data: conv::map_sampler_data_to_naga(info),
            #[cfg(feature = "cross")]
            cross_data: conv::map_sampler_data_to_cross(info),
        })
    }

    unsafe fn destroy_sampler(&self, _sampler: n::Sampler) {}

    unsafe fn map_memory(
        &self,
        memory: &mut n::Memory,
        segment: memory::Segment,
    ) -> Result<*mut u8, d::MapError> {
        let range = memory.resolve(&segment);
        debug!("map_memory of size {} at {:?}", memory.size, range);

        let base_ptr = match memory.heap {
            n::MemoryHeap::Public(_, ref cpu_buffer) => cpu_buffer.contents() as *mut u8,
            n::MemoryHeap::Native(_) | n::MemoryHeap::Private => panic!("Unable to map memory!"),
        };
        Ok(base_ptr.offset(range.start as _))
    }

    unsafe fn unmap_memory(&self, memory: &mut n::Memory) {
        debug!("unmap_memory of size {}", memory.size);
    }

    unsafe fn flush_mapped_memory_ranges<'a, I>(&self, iter: I) -> Result<(), d::OutOfMemory>
    where
        I: Iterator<Item = (&'a n::Memory, memory::Segment)>,
    {
        debug!("flush_mapped_memory_ranges");
        for (memory, ref segment) in iter {
            let range = memory.resolve(segment);
            debug!("\trange {:?}", range);

            match memory.heap {
                n::MemoryHeap::Native(_) => unimplemented!(),
                n::MemoryHeap::Public(mt, ref cpu_buffer)
                    if 1 << mt.0 != MemoryTypes::SHARED.bits() as usize =>
                {
                    cpu_buffer.did_modify_range(NSRange {
                        location: range.start as _,
                        length: (range.end - range.start) as _,
                    });
                }
                n::MemoryHeap::Public(..) => continue,
                n::MemoryHeap::Private => panic!("Can't map private memory!"),
            };
        }

        Ok(())
    }

    unsafe fn invalidate_mapped_memory_ranges<'a, I>(&self, iter: I) -> Result<(), d::OutOfMemory>
    where
        I: Iterator<Item = (&'a n::Memory, memory::Segment)>,
    {
        let mut num_syncs = 0;
        debug!("invalidate_mapped_memory_ranges");

        // temporary command buffer to copy the contents from
        // the given buffers into the allocated CPU-visible buffers
        // Note: using a separate internal queue in order to avoid a stall
        let cmd_buffer = self.invalidation_queue.spawn_temp();
        autoreleasepool(|| {
            let encoder = cmd_buffer.new_blit_command_encoder();

            for (memory, ref segment) in iter {
                let range = memory.resolve(segment);
                debug!("\trange {:?}", range);

                match memory.heap {
                    n::MemoryHeap::Native(_) => unimplemented!(),
                    n::MemoryHeap::Public(mt, ref cpu_buffer)
                        if 1 << mt.0 != MemoryTypes::SHARED.bits() as usize =>
                    {
                        num_syncs += 1;
                        encoder.synchronize_resource(cpu_buffer);
                    }
                    n::MemoryHeap::Public(..) => continue,
                    n::MemoryHeap::Private => panic!("Can't map private memory!"),
                };
            }
            encoder.end_encoding();
        });

        if num_syncs != 0 {
            debug!("\twaiting...");
            cmd_buffer.set_label("invalidate_mapped_memory_ranges");
            cmd_buffer.commit();
            cmd_buffer.wait_until_completed();
        }

        Ok(())
    }

    fn create_semaphore(&self) -> Result<n::Semaphore, d::OutOfMemory> {
        Ok(n::Semaphore {
            // Semaphore synchronization between command buffers of the same queue
            // is useless, don't bother even creating one.
            system: if self.shared.private_caps.exposed_queues > 1 {
                Some(n::SystemSemaphore::new())
            } else {
                None
            },
        })
    }

    unsafe fn create_descriptor_pool<I>(
        &self,
        max_sets: usize,
        descriptor_ranges: I,
        _flags: pso::DescriptorPoolCreateFlags,
    ) -> Result<n::DescriptorPool, d::OutOfMemory>
    where
        I: Iterator<Item = pso::DescriptorRangeDesc>,
    {
        if self.shared.private_caps.argument_buffers {
            let mut arguments = n::ArgumentArray::default();
            for dr in descriptor_ranges {
                let content = n::DescriptorContent::from(dr.ty);
                let usage = n::ArgumentArray::describe_usage(dr.ty);
                if content.contains(n::DescriptorContent::BUFFER) {
                    arguments.push(metal::MTLDataType::Pointer, dr.count, usage);
                }
                if content.contains(n::DescriptorContent::TEXTURE) {
                    arguments.push(metal::MTLDataType::Texture, dr.count, usage);
                }
                if content.contains(n::DescriptorContent::SAMPLER) {
                    arguments.push(metal::MTLDataType::Sampler, dr.count, usage);
                }
            }

            let device = self.shared.device.lock();
            let (array_ref, total_resources) = arguments.build();
            let encoder = device.new_argument_encoder(array_ref);

            let alignment = self.shared.private_caps.buffer_alignment;
            let total_size = encoder.encoded_length() + (max_sets as u64) * alignment;
            let raw = device.new_buffer(total_size, MTLResourceOptions::empty());

            Ok(n::DescriptorPool::new_argument(
                raw,
                total_size,
                alignment,
                total_resources,
            ))
        } else {
            let mut counters = n::ResourceData::<n::PoolResourceIndex>::new();
            for dr in descriptor_ranges {
                counters.add_many(
                    n::DescriptorContent::from(dr.ty),
                    dr.count as pso::DescriptorBinding,
                );
            }
            Ok(n::DescriptorPool::new_emulated(counters))
        }
    }

    unsafe fn create_descriptor_set_layout<'a, I, J>(
        &self,
        binding_iter: I,
        immutable_samplers: J,
    ) -> Result<n::DescriptorSetLayout, d::OutOfMemory>
    where
        I: Iterator<Item = pso::DescriptorSetLayoutBinding>,
        J: Iterator<Item = &'a n::Sampler>,
    {
        if self.shared.private_caps.argument_buffers {
            let mut stage_flags = pso::ShaderStageFlags::empty();
            let mut arguments = n::ArgumentArray::default();
            let mut bindings = FastHashMap::default();
            for desc in binding_iter {
                //TODO: have the API providing the dimensions and MSAA flag
                // for textures in an argument buffer
                match desc.ty {
                    pso::DescriptorType::Buffer {
                        format:
                            pso::BufferDescriptorFormat::Structured {
                                dynamic_offset: true,
                            },
                        ..
                    } => {
                        //TODO: apply the offsets somehow at the binding time
                        error!("Dynamic offsets are not yet supported in argument buffers!");
                    }
                    pso::DescriptorType::Image {
                        ty: pso::ImageDescriptorType::Storage { .. },
                    }
                    | pso::DescriptorType::Buffer {
                        ty: pso::BufferDescriptorType::Storage { .. },
                        format: pso::BufferDescriptorFormat::Texel,
                    } => {
                        //TODO: bind storage buffers and images separately
                        error!("Storage images are not yet supported in argument buffers!");
                    }
                    _ => {}
                }

                stage_flags |= desc.stage_flags;
                let content = n::DescriptorContent::from(desc.ty);
                let usage = n::ArgumentArray::describe_usage(desc.ty);
                let bind_target = naga::back::msl::BindTarget {
                    buffer: if content.contains(n::DescriptorContent::BUFFER) {
                        Some(
                            arguments.push(metal::MTLDataType::Pointer, desc.count, usage)
                                as naga::back::msl::Slot,
                        )
                    } else {
                        None
                    },
                    texture: if content.contains(n::DescriptorContent::TEXTURE) {
                        Some(
                            arguments.push(metal::MTLDataType::Texture, desc.count, usage)
                                as naga::back::msl::Slot,
                        )
                    } else {
                        None
                    },
                    sampler: if content.contains(n::DescriptorContent::SAMPLER) {
                        let slot = arguments.push(metal::MTLDataType::Sampler, desc.count, usage);
                        Some(naga::back::msl::BindSamplerTarget::Resource(
                            slot as naga::back::msl::Slot,
                        ))
                    } else {
                        None
                    },
                    mutable: content.contains(n::DescriptorContent::WRITABLE),
                };
                let res_offset = bind_target
                    .buffer
                    .or(bind_target.texture)
                    .or(bind_target.sampler.as_ref().and_then(|bst| match *bst {
                        naga::back::msl::BindSamplerTarget::Resource(slot) => Some(slot),
                        naga::back::msl::BindSamplerTarget::Inline(_) => None,
                    }))
                    .unwrap() as u32;
                bindings.insert(
                    desc.binding,
                    n::ArgumentLayout {
                        bind_target,
                        res_offset,
                        count: desc.count,
                        usage,
                        content,
                    },
                );
            }

            let (array_ref, arg_total) = arguments.build();
            let encoder = self.shared.device.lock().new_argument_encoder(array_ref);

            Ok(n::DescriptorSetLayout::ArgumentBuffer {
                encoder,
                stage_flags,
                bindings: Arc::new(bindings),
                total: arg_total as n::PoolResourceIndex,
            })
        } else {
            struct TempSampler {
                data: n::ImmutableSampler,
                binding: pso::DescriptorBinding,
                array_index: pso::DescriptorArrayIndex,
            }
            let mut immutable_sampler_iter = immutable_samplers;
            let mut tmp_samplers = Vec::new();
            let mut desc_layouts = Vec::new();
            let mut total = n::ResourceData::new();

            for slb in binding_iter {
                let mut content = n::DescriptorContent::from(slb.ty);
                total.add_many(content, slb.count as _);

                #[cfg_attr(not(feature = "cross"), allow(unused_variables))]
                if slb.immutable_samplers {
                    tmp_samplers.extend(
                        immutable_sampler_iter
                            .by_ref()
                            .take(slb.count)
                            .enumerate()
                            .map(|(array_index, sm)| TempSampler {
                                data: n::ImmutableSampler {
                                    data: sm.data.clone(),
                                    #[cfg(feature = "cross")]
                                    cross_data: sm.cross_data.clone(),
                                },
                                binding: slb.binding,
                                array_index,
                            }),
                    );
                    content |= n::DescriptorContent::IMMUTABLE_SAMPLER;
                }

                desc_layouts.extend((0..slb.count).map(|array_index| n::DescriptorLayout {
                    content,
                    stages: slb.stage_flags,
                    binding: slb.binding,
                    array_index,
                }));
            }

            desc_layouts.sort_by_key(|dl| (dl.binding, dl.array_index));
            tmp_samplers.sort_by_key(|ts| (ts.binding, ts.array_index));
            // From here on, we assume that `desc_layouts` has at most a single item for
            // a (binding, array_index) pair. To achieve that, we deduplicate the array now
            desc_layouts.dedup_by(|a, b| {
                if (a.binding, a.array_index) == (b.binding, b.array_index) {
                    debug_assert!(!b.stages.intersects(a.stages));
                    debug_assert_eq!(a.content, b.content); //TODO: double check if this can be demanded
                    b.stages |= a.stages; //`b` is here to stay
                    true
                } else {
                    false
                }
            });

            Ok(n::DescriptorSetLayout::Emulated {
                layouts: Arc::new(desc_layouts),
                total,
                immutable_samplers: tmp_samplers
                    .into_iter()
                    .map(|ts| (ts.binding, ts.data))
                    .collect(),
            })
        }
    }

    unsafe fn write_descriptor_set<'a, I>(&self, op: pso::DescriptorSetWrite<'a, Backend, I>)
    where
        I: Iterator<Item = pso::Descriptor<'a, Backend>>,
    {
        debug!("write_descriptor_set");
        match *op.set {
            n::DescriptorSet::Emulated {
                ref pool,
                ref layouts,
                ref resources,
            } => {
                let mut counters = resources.map(|r| r.start);
                let mut start = None; //TODO: can pre-compute this
                for (i, layout) in layouts.iter().enumerate() {
                    if layout.binding == op.binding && layout.array_index == op.array_offset {
                        start = Some(i);
                        break;
                    }
                    counters.add(layout.content);
                }
                let mut data = pool.write();

                for (layout, descriptor) in layouts[start.unwrap()..].iter().zip(op.descriptors) {
                    trace!("\t{:?}", layout);
                    match descriptor {
                        pso::Descriptor::Sampler(sam) => {
                            debug_assert!(!layout
                                .content
                                .contains(n::DescriptorContent::IMMUTABLE_SAMPLER));
                            data.samplers[counters.samplers as usize] = (
                                layout.stages,
                                Some(AsNative::from(sam.raw.as_ref().unwrap().as_ref())),
                            );
                        }
                        pso::Descriptor::Image(view, il) => {
                            data.textures[counters.textures as usize] = (
                                layout.stages,
                                Some(AsNative::from(view.texture.as_ref())),
                                il,
                            );
                        }
                        pso::Descriptor::CombinedImageSampler(view, il, sam) => {
                            if !layout
                                .content
                                .contains(n::DescriptorContent::IMMUTABLE_SAMPLER)
                            {
                                data.samplers[counters.samplers as usize] = (
                                    layout.stages,
                                    Some(AsNative::from(sam.raw.as_ref().unwrap().as_ref())),
                                );
                            }
                            data.textures[counters.textures as usize] = (
                                layout.stages,
                                Some(AsNative::from(view.texture.as_ref())),
                                il,
                            );
                        }
                        pso::Descriptor::TexelBuffer(view) => {
                            data.textures[counters.textures as usize] = (
                                layout.stages,
                                Some(AsNative::from(view.raw.as_ref())),
                                image::Layout::General,
                            );
                        }
                        pso::Descriptor::Buffer(buf, ref sub) => {
                            let (raw, range) = buf.as_bound();
                            debug_assert!(
                                range.start + sub.offset + sub.size.unwrap_or(0) <= range.end
                            );
                            let raw_binding_size = match sub.size {
                                Some(size) => size,
                                None => range.end - range.start - sub.offset,
                            };
                            data.buffers[counters.buffers as usize] = (
                                layout.stages,
                                Some(AsNative::from(raw)),
                                range.start + sub.offset,
                                layout.binding,
                                if layout.content.contains(n::DescriptorContent::SIZED_BUFFER) {
                                    raw_binding_size.min(u32::MAX as buffer::Offset - 1) as u32
                                } else {
                                    !0
                                },
                            );
                        }
                    }
                    counters.add(layout.content);
                }
            }
            n::DescriptorSet::ArgumentBuffer {
                ref raw,
                raw_offset,
                ref pool,
                ref range,
                ref encoder,
                ref bindings,
                ..
            } => {
                debug_assert!(self.shared.private_caps.argument_buffers);

                encoder.set_argument_buffer(raw, raw_offset);
                let mut arg_index = {
                    let binding = &bindings[&op.binding];
                    debug_assert!((op.array_offset as usize) < binding.count);
                    (binding.res_offset as NSUInteger) + (op.array_offset as NSUInteger)
                };

                for (data, descriptor) in pool.write().resources
                    [range.start as usize + arg_index as usize..range.end as usize]
                    .iter_mut()
                    .zip(op.descriptors)
                {
                    match descriptor {
                        pso::Descriptor::Sampler(sampler) => {
                            debug_assert!(!bindings[&op.binding]
                                .content
                                .contains(n::DescriptorContent::IMMUTABLE_SAMPLER));
                            encoder.set_sampler_state(arg_index, sampler.raw.as_ref().unwrap());
                            arg_index += 1;
                        }
                        pso::Descriptor::Image(image, _layout) => {
                            let tex_ref = image.texture.as_ref();
                            encoder.set_texture(arg_index, tex_ref);
                            data.ptr = (&**tex_ref).as_ptr();
                            arg_index += 1;
                        }
                        pso::Descriptor::CombinedImageSampler(image, _il, sampler) => {
                            let binding = &bindings[&op.binding];
                            if !binding
                                .content
                                .contains(n::DescriptorContent::IMMUTABLE_SAMPLER)
                            {
                                //TODO: supporting arrays of combined image-samplers can be tricky.
                                // We need to scan both sampler and image sections of the encoder
                                // at the same time.
                                assert!(
                                    arg_index
                                        < (binding.res_offset as NSUInteger)
                                            + (binding.count as NSUInteger)
                                );
                                encoder.set_sampler_state(
                                    arg_index + binding.count as NSUInteger,
                                    sampler.raw.as_ref().unwrap(),
                                );
                            }
                            let tex_ref = image.texture.as_ref();
                            encoder.set_texture(arg_index, tex_ref);
                            data.ptr = (&**tex_ref).as_ptr();
                        }
                        pso::Descriptor::TexelBuffer(view) => {
                            encoder.set_texture(arg_index, &view.raw);
                            data.ptr = (&**view.raw).as_ptr();
                            arg_index += 1;
                        }
                        pso::Descriptor::Buffer(buffer, ref sub) => {
                            let (buf_raw, buf_range) = buffer.as_bound();
                            encoder.set_buffer(arg_index, buf_raw, buf_range.start + sub.offset);
                            data.ptr = (&**buf_raw).as_ptr();
                            arg_index += 1;
                        }
                    }
                }
            }
        }
    }

    unsafe fn copy_descriptor_set<'a>(&self, _op: pso::DescriptorSetCopy<'a, Backend>) {
        unimplemented!()
    }

    unsafe fn destroy_descriptor_pool(&self, _pool: n::DescriptorPool) {}

    unsafe fn destroy_descriptor_set_layout(&self, _layout: n::DescriptorSetLayout) {}

    unsafe fn destroy_pipeline_layout(&self, _pipeline_layout: n::PipelineLayout) {}

    unsafe fn destroy_shader_module(&self, _module: n::ShaderModule) {}

    unsafe fn destroy_render_pass(&self, _pass: n::RenderPass) {}

    unsafe fn destroy_graphics_pipeline(&self, _pipeline: n::GraphicsPipeline) {}

    unsafe fn destroy_compute_pipeline(&self, _pipeline: n::ComputePipeline) {}

    unsafe fn destroy_framebuffer(&self, _framebuffer: n::Framebuffer) {}

    unsafe fn destroy_semaphore(&self, _semaphore: n::Semaphore) {}

    unsafe fn allocate_memory(
        &self,
        memory_type: hal::MemoryTypeId,
        size: u64,
    ) -> Result<n::Memory, d::AllocationError> {
        profiling::scope!("allocate_memory");
        let (storage, cache) = MemoryTypes::describe(memory_type.0);
        let device = self.shared.device.lock();
        debug!("allocate_memory type {:?} of size {}", memory_type, size);

        // Heaps cannot be used for CPU coherent resources
        //TEMP: MacOS supports Private only, iOS and tvOS can do private/shared
        let heap = if self.shared.private_caps.resource_heaps
            && storage != MTLStorageMode::Shared
            && false
        {
            let descriptor = metal::HeapDescriptor::new();
            descriptor.set_storage_mode(storage);
            descriptor.set_cpu_cache_mode(cache);
            descriptor.set_size(size);
            let heap_raw = device.new_heap(&descriptor);
            n::MemoryHeap::Native(heap_raw)
        } else if storage == MTLStorageMode::Private {
            n::MemoryHeap::Private
        } else {
            let options = conv::resource_options_from_storage_and_cache(storage, cache);
            let cpu_buffer = device.new_buffer(size, options);
            debug!("\tbacked by cpu buffer {:?}", cpu_buffer.as_ptr());
            n::MemoryHeap::Public(memory_type, cpu_buffer)
        };

        Ok(n::Memory::new(heap, size))
    }

    unsafe fn free_memory(&self, memory: n::Memory) {
        profiling::scope!("free_memory");
        debug!("free_memory of size {}", memory.size);
        if let n::MemoryHeap::Public(_, ref cpu_buffer) = memory.heap {
            debug!("\tbacked by cpu buffer {:?}", cpu_buffer.as_ptr());
        }
    }

    unsafe fn create_buffer(
        &self,
        size: u64,
        usage: buffer::Usage,
        _sparse: memory::SparseFlags,
    ) -> Result<n::Buffer, buffer::CreationError> {
        debug!("create_buffer of size {} and usage {:?}", size, usage);
        Ok(n::Buffer::Unbound {
            usage,
            size,
            name: String::new(),
        })
    }

    unsafe fn get_buffer_requirements(&self, buffer: &n::Buffer) -> memory::Requirements {
        let (size, usage) = match *buffer {
            n::Buffer::Unbound { size, usage, .. } => (size, usage),
            n::Buffer::Bound { .. } => panic!("Unexpected Buffer::Bound"),
        };
        let mut max_size = size;
        let mut max_alignment = self.shared.private_caps.buffer_alignment;

        if self.shared.private_caps.resource_heaps {
            // We don't know what memory type the user will try to allocate the buffer with, so we test them
            // all get the most stringent ones.
            for (i, _mt) in self.memory_types.iter().enumerate() {
                let (storage, cache) = MemoryTypes::describe(i);
                let options = conv::resource_options_from_storage_and_cache(storage, cache);
                let requirements = self
                    .shared
                    .device
                    .lock()
                    .heap_buffer_size_and_align(size, options);
                max_size = cmp::max(max_size, requirements.size);
                max_alignment = cmp::max(max_alignment, requirements.align);
            }
        }

        // based on Metal validation error for view creation:
        // failed assertion `BytesPerRow of a buffer-backed texture with pixelFormat(XXX) must be aligned to 256 bytes
        const SIZE_MASK: u64 = 0xFF;
        let supports_texel_view =
            usage.intersects(buffer::Usage::UNIFORM_TEXEL | buffer::Usage::STORAGE_TEXEL);

        memory::Requirements {
            size: (max_size + SIZE_MASK) & !SIZE_MASK,
            alignment: max_alignment,
            type_mask: if !supports_texel_view || self.shared.private_caps.shared_textures {
                MemoryTypes::all().bits()
            } else {
                (MemoryTypes::all() ^ MemoryTypes::SHARED).bits()
            },
        }
    }

    unsafe fn bind_buffer_memory(
        &self,
        memory: &n::Memory,
        offset: u64,
        buffer: &mut n::Buffer,
    ) -> Result<(), d::BindError> {
        profiling::scope!("bind_buffer_memory");
        let (size, name) = match buffer {
            n::Buffer::Unbound { size, name, .. } => (*size, name),
            n::Buffer::Bound { .. } => panic!("Unexpected Buffer::Bound"),
        };
        debug!("bind_buffer_memory of size {} at offset {}", size, offset);
        *buffer = match memory.heap {
            n::MemoryHeap::Native(ref heap) => {
                let options = conv::resource_options_from_storage_and_cache(
                    heap.storage_mode(),
                    heap.cpu_cache_mode(),
                );
                let raw = heap.new_buffer(size, options).unwrap_or_else(|| {
                    // TODO: disable hazard tracking?
                    self.shared.device.lock().new_buffer(size, options)
                });
                raw.set_label(name);
                n::Buffer::Bound {
                    raw,
                    options,
                    range: 0..size, //TODO?
                }
            }
            n::MemoryHeap::Public(mt, ref cpu_buffer) => {
                debug!(
                    "\tmapped to public heap with address {:?}",
                    cpu_buffer.as_ptr()
                );
                let (storage, cache) = MemoryTypes::describe(mt.0);
                let options = conv::resource_options_from_storage_and_cache(storage, cache);
                if offset == 0x0 && size == cpu_buffer.length() {
                    cpu_buffer.set_label(name);
                } else if self.shared.private_caps.supports_debug_markers {
                    cpu_buffer.add_debug_marker(
                        name,
                        NSRange {
                            location: offset,
                            length: size,
                        },
                    );
                }
                n::Buffer::Bound {
                    raw: cpu_buffer.clone(),
                    options,
                    range: offset..offset + size,
                }
            }
            n::MemoryHeap::Private => {
                //TODO: check for aliasing
                let options = MTLResourceOptions::StorageModePrivate
                    | MTLResourceOptions::CPUCacheModeDefaultCache;
                let raw = self.shared.device.lock().new_buffer(size, options);
                raw.set_label(name);
                n::Buffer::Bound {
                    raw,
                    options,
                    range: 0..size,
                }
            }
        };

        Ok(())
    }

    unsafe fn destroy_buffer(&self, buffer: n::Buffer) {
        if let n::Buffer::Bound { raw, range, .. } = buffer {
            debug!(
                "destroy_buffer {:?} occupying memory {:?}",
                raw.as_ptr(),
                range
            );
        }
    }

    unsafe fn create_buffer_view(
        &self,
        buffer: &n::Buffer,
        format_maybe: Option<format::Format>,
        sub: buffer::SubRange,
    ) -> Result<n::BufferView, buffer::ViewCreationError> {
        let (raw, base_range, options) = match *buffer {
            n::Buffer::Bound {
                ref raw,
                ref range,
                options,
            } => (raw, range, options),
            n::Buffer::Unbound { .. } => panic!("Unexpected Buffer::Unbound"),
        };
        let start = base_range.start + sub.offset;
        let size_rough = sub.size.unwrap_or(base_range.end - start);
        let format = match format_maybe {
            Some(fmt) => fmt,
            None => {
                return Err(buffer::ViewCreationError::UnsupportedFormat(format_maybe));
            }
        };
        let format_desc = format.surface_desc();
        if format_desc.aspects != format::Aspects::COLOR || format_desc.is_compressed() {
            // Vadlidator says "Linear texture: cannot create compressed, depth, or stencil textures"
            return Err(buffer::ViewCreationError::UnsupportedFormat(format_maybe));
        }

        //Note: we rely on SPIRV-Cross to use the proper 2D texel indexing here
        let texel_count = size_rough * 8 / format_desc.bits as u64;
        let col_count = cmp::min(texel_count, self.shared.private_caps.max_texture_size);
        let row_count = (texel_count + self.shared.private_caps.max_texture_size - 1)
            / self.shared.private_caps.max_texture_size;
        let mtl_format = self
            .shared
            .private_caps
            .map_format(format)
            .ok_or(buffer::ViewCreationError::UnsupportedFormat(format_maybe))?;

        let descriptor = metal::TextureDescriptor::new();
        descriptor.set_texture_type(MTLTextureType::D2);
        descriptor.set_width(col_count);
        descriptor.set_height(row_count);
        descriptor.set_mipmap_level_count(1);
        descriptor.set_pixel_format(mtl_format);
        descriptor.set_resource_options(options);
        descriptor.set_storage_mode(raw.storage_mode());
        descriptor.set_usage(metal::MTLTextureUsage::ShaderRead);

        let align_mask = self.shared.private_caps.buffer_alignment - 1;
        let stride = (col_count * (format_desc.bits as u64 / 8) + align_mask) & !align_mask;

        Ok(n::BufferView {
            raw: raw.new_texture_with_descriptor(&descriptor, start, stride),
        })
    }

    unsafe fn destroy_buffer_view(&self, _view: n::BufferView) {
        //nothing to do
    }

    unsafe fn create_image(
        &self,
        kind: image::Kind,
        mip_levels: image::Level,
        format: format::Format,
        tiling: image::Tiling,
        usage: image::Usage,
        _sparse: memory::SparseFlags,
        view_caps: image::ViewCapabilities,
    ) -> Result<n::Image, image::CreationError> {
        profiling::scope!("create_image");
        debug!(
            "create_image {:?} with {} mips of {:?} {:?} and usage {:?} with {:?}",
            kind, mip_levels, format, tiling, usage, view_caps
        );

        let is_cube = view_caps.contains(image::ViewCapabilities::KIND_CUBE);
        let mtl_format = self
            .shared
            .private_caps
            .map_format(format)
            .ok_or_else(|| image::CreationError::Format(format))?;

        let descriptor = metal::TextureDescriptor::new();

        let (mtl_type, num_layers) = match kind {
            image::Kind::D1(_, 1) => {
                assert!(!is_cube);
                (MTLTextureType::D1, None)
            }
            image::Kind::D1(_, layers) => {
                assert!(!is_cube);
                (MTLTextureType::D1Array, Some(layers))
            }
            image::Kind::D2(_, _, layers, 1) => {
                if is_cube && layers > 6 {
                    assert_eq!(layers % 6, 0);
                    (MTLTextureType::CubeArray, Some(layers / 6))
                } else if is_cube {
                    assert_eq!(layers, 6);
                    (MTLTextureType::Cube, None)
                } else if layers > 1 {
                    (MTLTextureType::D2Array, Some(layers))
                } else {
                    (MTLTextureType::D2, None)
                }
            }
            image::Kind::D2(_, _, 1, samples) if !is_cube => {
                descriptor.set_sample_count(samples as u64);
                (MTLTextureType::D2Multisample, None)
            }
            image::Kind::D2(..) => {
                error!(
                    "Multi-sampled array textures or cubes are not supported: {:?}",
                    kind
                );
                return Err(image::CreationError::Kind);
            }
            image::Kind::D3(..) => {
                assert!(!is_cube);
                if view_caps.contains(image::ViewCapabilities::KIND_2D_ARRAY) {
                    warn!("Unable to support 2D array views of 3D textures");
                }
                (MTLTextureType::D3, None)
            }
        };

        descriptor.set_texture_type(mtl_type);
        if let Some(count) = num_layers {
            descriptor.set_array_length(count as u64);
        }
        let extent = kind.extent();
        descriptor.set_width(extent.width as u64);
        descriptor.set_height(extent.height as u64);
        descriptor.set_depth(extent.depth as u64);
        descriptor.set_mipmap_level_count(mip_levels as u64);
        descriptor.set_pixel_format(mtl_format);
        descriptor.set_usage(conv::map_texture_usage(usage, tiling, view_caps));

        let base = format.base_format();
        let format_desc = base.0.desc();
        let mip_sizes = (0..mip_levels)
            .map(|level| {
                let pitches = n::Image::pitches_impl(extent.at_level(level), format_desc);
                num_layers.unwrap_or(1) as buffer::Offset * pitches[3]
            })
            .collect();

        let host_usage = image::Usage::TRANSFER_SRC | image::Usage::TRANSFER_DST;
        let host_visible = mtl_type == MTLTextureType::D2
            && mip_levels == 1
            && num_layers.is_none()
            && format_desc.aspects.contains(format::Aspects::COLOR)
            && tiling == image::Tiling::Linear
            && host_usage.contains(usage);

        Ok(n::Image {
            like: n::ImageLike::Unbound {
                descriptor,
                mip_sizes,
                host_visible,
                name: String::new(),
            },
            kind,
            mip_levels,
            format_desc,
            shader_channel: base.1.into(),
            mtl_format,
            mtl_type,
        })
    }

    unsafe fn get_image_requirements(&self, image: &n::Image) -> memory::Requirements {
        let (descriptor, mip_sizes, host_visible) = match image.like {
            n::ImageLike::Unbound {
                ref descriptor,
                ref mip_sizes,
                host_visible,
                ..
            } => (descriptor, mip_sizes, host_visible),
            n::ImageLike::Texture(..) | n::ImageLike::Buffer(..) => {
                panic!("Expected Image::Unbound")
            }
        };

        if self.shared.private_caps.resource_heaps {
            // We don't know what memory type the user will try to allocate the image with, so we test them
            // all get the most stringent ones. Note we don't check Shared because heaps can't use it
            let mut max_size = 0;
            let mut max_alignment = 0;
            let types = if host_visible {
                MemoryTypes::all()
            } else {
                MemoryTypes::PRIVATE
            };
            for (i, _) in self.memory_types.iter().enumerate() {
                if !types.contains(MemoryTypes::from_bits(1 << i).unwrap()) {
                    continue;
                }
                let (storage, cache_mode) = MemoryTypes::describe(i);
                descriptor.set_storage_mode(storage);
                descriptor.set_cpu_cache_mode(cache_mode);

                let requirements = self
                    .shared
                    .device
                    .lock()
                    .heap_texture_size_and_align(descriptor);
                max_size = cmp::max(max_size, requirements.size);
                max_alignment = cmp::max(max_alignment, requirements.align);
            }
            memory::Requirements {
                size: max_size,
                alignment: max_alignment,
                type_mask: types.bits(),
            }
        } else if host_visible {
            assert_eq!(mip_sizes.len(), 1);
            let mask = self.shared.private_caps.buffer_alignment - 1;
            memory::Requirements {
                size: (mip_sizes[0] + mask) & !mask,
                alignment: self.shared.private_caps.buffer_alignment,
                type_mask: MemoryTypes::all().bits(),
            }
        } else {
            memory::Requirements {
                size: mip_sizes.iter().sum(),
                alignment: 4,
                type_mask: MemoryTypes::PRIVATE.bits(),
            }
        }
    }

    unsafe fn get_image_subresource_footprint(
        &self,
        image: &n::Image,
        sub: image::Subresource,
    ) -> image::SubresourceFootprint {
        let num_layers = image.kind.num_layers() as buffer::Offset;
        let level_offset = (0..sub.level).fold(0, |offset, level| {
            let pitches = image.pitches(level);
            offset + num_layers * pitches[3]
        });
        let pitches = image.pitches(sub.level);
        let layer_offset = level_offset + sub.layer as buffer::Offset * pitches[3];
        image::SubresourceFootprint {
            slice: layer_offset..layer_offset + pitches[3],
            row_pitch: pitches[1] as _,
            depth_pitch: pitches[2] as _,
            array_pitch: pitches[3] as _,
        }
    }

    unsafe fn bind_image_memory(
        &self,
        memory: &n::Memory,
        offset: u64,
        image: &mut n::Image,
    ) -> Result<(), d::BindError> {
        profiling::scope!("bind_image_memory");
        let like = {
            let (descriptor, mip_sizes, name) = match image.like {
                n::ImageLike::Unbound {
                    ref descriptor,
                    ref mip_sizes,
                    ref name,
                    ..
                } => (descriptor, mip_sizes, name),
                n::ImageLike::Texture(..) | n::ImageLike::Buffer(..) => {
                    panic!("Expected Image::Unbound")
                }
            };

            match memory.heap {
                n::MemoryHeap::Native(ref heap) => {
                    let resource_options = conv::resource_options_from_storage_and_cache(
                        heap.storage_mode(),
                        heap.cpu_cache_mode(),
                    );
                    descriptor.set_resource_options(resource_options);
                    n::ImageLike::Texture(heap.new_texture(descriptor).unwrap_or_else(|| {
                        // TODO: disable hazard tracking?
                        let texture = self.shared.device.lock().new_texture(&descriptor);
                        texture.set_label(name);
                        texture
                    }))
                }
                n::MemoryHeap::Public(_memory_type, ref cpu_buffer) => {
                    assert_eq!(mip_sizes.len(), 1);
                    if offset == 0x0 && cpu_buffer.length() == mip_sizes[0] {
                        cpu_buffer.set_label(name);
                    } else if self.shared.private_caps.supports_debug_markers {
                        cpu_buffer.add_debug_marker(
                            name,
                            NSRange {
                                location: offset,
                                length: mip_sizes[0],
                            },
                        );
                    }
                    n::ImageLike::Buffer(n::Buffer::Bound {
                        raw: cpu_buffer.clone(),
                        range: offset..offset + mip_sizes[0] as u64,
                        options: MTLResourceOptions::StorageModeShared,
                    })
                }
                n::MemoryHeap::Private => {
                    descriptor.set_storage_mode(MTLStorageMode::Private);
                    let texture = self.shared.device.lock().new_texture(descriptor);
                    texture.set_label(name);
                    n::ImageLike::Texture(texture)
                }
            }
        };

        Ok(image.like = like)
    }

    unsafe fn destroy_image(&self, _image: n::Image) {
        //nothing to do
    }

    unsafe fn create_image_view(
        &self,
        image: &n::Image,
        kind: image::ViewKind,
        format: format::Format,
        swizzle: format::Swizzle,
        _usage: image::Usage,
        range: image::SubresourceRange,
    ) -> Result<n::ImageView, image::ViewCreationError> {
        profiling::scope!("create_image_view");

        let mtl_format = match self
            .shared
            .private_caps
            .map_format_with_swizzle(format, swizzle)
        {
            Some(f) => f,
            None => {
                error!("failed to swizzle format {:?} with {:?}", format, swizzle);
                return Err(image::ViewCreationError::BadFormat(format));
            }
        };
        let raw = image.like.as_texture();
        let full_range = image::SubresourceRange {
            aspects: image.format_desc.aspects,
            ..Default::default()
        };
        let mtl_type = if image.mtl_type == MTLTextureType::D2Multisample {
            if kind != image::ViewKind::D2 {
                error!("Requested {:?} for MSAA texture", kind);
            }
            image.mtl_type
        } else {
            conv::map_texture_type(kind)
        };

        let texture = if mtl_format == image.mtl_format
            && mtl_type == image.mtl_type
            && swizzle == format::Swizzle::NO
            && range == full_range
        {
            // Some images are marked as framebuffer-only, and we can't create aliases of them.
            // Also helps working around Metal bugs with aliased array textures.
            raw.to_owned()
        } else {
            raw.new_texture_view_from_slice(
                mtl_format,
                mtl_type,
                NSRange {
                    location: range.level_start as _,
                    length: range.resolve_level_count(image.mip_levels) as _,
                },
                NSRange {
                    location: range.layer_start as _,
                    length: range.resolve_layer_count(image.kind.num_layers()) as _,
                },
            )
        };

        Ok(n::ImageView {
            texture,
            mtl_format,
        })
    }

    unsafe fn destroy_image_view(&self, _view: n::ImageView) {}

    fn create_fence(&self, signaled: bool) -> Result<n::Fence, d::OutOfMemory> {
        debug!("Creating fence with signal={}", signaled);
        Ok(n::Fence::Idle { signaled })
    }

    unsafe fn reset_fence(&self, fence: &mut n::Fence) -> Result<(), d::OutOfMemory> {
        debug!("Resetting fence ptr {:?}", fence);
        *fence = n::Fence::Idle { signaled: false };
        Ok(())
    }

    unsafe fn wait_for_fence(
        &self,
        fence: &n::Fence,
        timeout_ns: u64,
    ) -> Result<bool, d::WaitError> {
        unsafe fn to_ns(duration: time::Duration) -> u64 {
            duration.as_secs() * 1_000_000_000 + duration.subsec_nanos() as u64
        }

        debug!("wait_for_fence {:?} for {} ms", fence, timeout_ns);
        match *fence {
            n::Fence::Idle { signaled } => {
                if !signaled {
                    warn!("Fence ptr {:?} is not pending, waiting not possible", fence);
                }
                Ok(signaled)
            }
            n::Fence::PendingSubmission(ref cmd_buf) => {
                if timeout_ns == !0 {
                    cmd_buf.wait_until_completed();
                    return Ok(true);
                }
                let start = time::Instant::now();
                loop {
                    if let metal::MTLCommandBufferStatus::Completed = cmd_buf.status() {
                        return Ok(true);
                    }
                    if to_ns(start.elapsed()) >= timeout_ns {
                        return Ok(false);
                    }
                    thread::sleep(time::Duration::from_millis(1));
                    self.shared.queue_blocker.lock().triage();
                }
            }
        }
    }

    unsafe fn get_fence_status(&self, fence: &n::Fence) -> Result<bool, d::DeviceLost> {
        Ok(match *fence {
            n::Fence::Idle { signaled } => signaled,
            n::Fence::PendingSubmission(ref cmd_buf) => match cmd_buf.status() {
                metal::MTLCommandBufferStatus::Completed => true,
                _ => false,
            },
        })
    }

    unsafe fn destroy_fence(&self, _fence: n::Fence) {
        //empty
    }

    fn create_event(&self) -> Result<n::Event, d::OutOfMemory> {
        Ok(n::Event(Arc::new(AtomicBool::new(false))))
    }

    unsafe fn get_event_status(&self, event: &n::Event) -> Result<bool, d::WaitError> {
        Ok(event.0.load(Ordering::Acquire))
    }

    unsafe fn set_event(&self, event: &mut n::Event) -> Result<(), d::OutOfMemory> {
        event.0.store(true, Ordering::Release);
        self.shared.queue_blocker.lock().triage();
        Ok(())
    }

    unsafe fn reset_event(&self, event: &mut n::Event) -> Result<(), d::OutOfMemory> {
        Ok(event.0.store(false, Ordering::Release))
    }

    unsafe fn destroy_event(&self, _event: n::Event) {
        //empty
    }

    unsafe fn create_query_pool(
        &self,
        ty: query::Type,
        count: query::Id,
    ) -> Result<n::QueryPool, query::CreationError> {
        match ty {
            query::Type::Occlusion => {
                let range = self
                    .shared
                    .visibility
                    .allocator
                    .lock()
                    .allocate_range(count)
                    .map_err(|_| {
                        error!("Not enough space to allocate an occlusion query pool");
                        d::OutOfMemory::Host
                    })?;
                Ok(n::QueryPool::Occlusion(range))
            }
            query::Type::Timestamp => {
                warn!("Timestamp queries are not really useful yet");
                Ok(n::QueryPool::Timestamp)
            }
            query::Type::PipelineStatistics(..) => Err(query::CreationError::Unsupported(ty)),
        }
    }

    unsafe fn destroy_query_pool(&self, pool: n::QueryPool) {
        match pool {
            n::QueryPool::Occlusion(range) => {
                self.shared.visibility.allocator.lock().free_range(range);
            }
            n::QueryPool::Timestamp => {}
        }
    }

    unsafe fn get_query_pool_results(
        &self,
        pool: &n::QueryPool,
        queries: Range<query::Id>,
        data: &mut [u8],
        stride: buffer::Stride,
        flags: query::ResultFlags,
    ) -> Result<bool, d::WaitError> {
        let is_ready = match *pool {
            n::QueryPool::Occlusion(ref pool_range) => {
                let visibility = &self.shared.visibility;
                let is_ready = if flags.contains(query::ResultFlags::WAIT) {
                    let mut guard = visibility.allocator.lock();
                    while !visibility.are_available(pool_range.start, &queries) {
                        visibility.condvar.wait(&mut guard);
                    }
                    true
                } else {
                    visibility.are_available(pool_range.start, &queries)
                };

                let size_data = mem::size_of::<u64>() as buffer::Offset;
                if stride as u64 == size_data
                    && flags.contains(query::ResultFlags::BITS_64)
                    && !flags.contains(query::ResultFlags::WITH_AVAILABILITY)
                {
                    // if stride is matching, copy everything in one go
                    ptr::copy_nonoverlapping(
                        (visibility.buffer.contents() as *const u8).offset(
                            (pool_range.start + queries.start) as isize * size_data as isize,
                        ),
                        data.as_mut_ptr(),
                        stride as usize * (queries.end - queries.start) as usize,
                    );
                } else {
                    // copy parts of individual entries
                    for i in 0..queries.end - queries.start {
                        let absolute_index = (pool_range.start + queries.start + i) as isize;
                        let value =
                            *(visibility.buffer.contents() as *const u64).offset(absolute_index);
                        let base = (visibility.buffer.contents() as *const u8)
                            .offset(visibility.availability_offset as isize);
                        let availability = *(base as *const u32).offset(absolute_index);
                        let data_ptr = data[i as usize * stride as usize..].as_mut_ptr();
                        if flags.contains(query::ResultFlags::BITS_64) {
                            *(data_ptr as *mut u64) = value;
                            if flags.contains(query::ResultFlags::WITH_AVAILABILITY) {
                                *(data_ptr as *mut u64).offset(1) = availability as u64;
                            }
                        } else {
                            *(data_ptr as *mut u32) = value as u32;
                            if flags.contains(query::ResultFlags::WITH_AVAILABILITY) {
                                *(data_ptr as *mut u32).offset(1) = availability;
                            }
                        }
                    }
                }

                is_ready
            }
            n::QueryPool::Timestamp => {
                for d in data.iter_mut() {
                    *d = 0;
                }
                true
            }
        };

        Ok(is_ready)
    }

    fn wait_idle(&self) -> Result<(), d::OutOfMemory> {
        command::QueueInner::wait_idle(&self.shared.queue);
        Ok(())
    }

    unsafe fn set_image_name(&self, image: &mut n::Image, name: &str) {
        match image {
            n::Image {
                like: n::ImageLike::Buffer(ref mut buf),
                ..
            } => self.set_buffer_name(buf, name),
            n::Image {
                like: n::ImageLike::Texture(ref tex),
                ..
            } => tex.set_label(name),
            n::Image {
                like:
                    n::ImageLike::Unbound {
                        name: ref mut unbound_name,
                        ..
                    },
                ..
            } => {
                *unbound_name = name.to_string();
            }
        };
    }

    unsafe fn set_buffer_name(&self, buffer: &mut n::Buffer, name: &str) {
        match buffer {
            n::Buffer::Unbound {
                name: ref mut unbound_name,
                ..
            } => {
                *unbound_name = name.to_string();
            }
            n::Buffer::Bound {
                ref raw, ref range, ..
            } => {
                if self.shared.private_caps.supports_debug_markers {
                    raw.add_debug_marker(
                        name,
                        NSRange {
                            location: range.start,
                            length: range.end - range.start,
                        },
                    );
                }
            }
        }
    }

    unsafe fn set_command_buffer_name(
        &self,
        command_buffer: &mut command::CommandBuffer,
        name: &str,
    ) {
        command_buffer.name = name.to_string();
    }

    unsafe fn set_semaphore_name(&self, _semaphore: &mut n::Semaphore, _name: &str) {}

    unsafe fn set_fence_name(&self, _fence: &mut n::Fence, _name: &str) {}

    unsafe fn set_framebuffer_name(&self, _framebuffer: &mut n::Framebuffer, _name: &str) {}

    unsafe fn set_render_pass_name(&self, render_pass: &mut n::RenderPass, name: &str) {
        render_pass.name = name.to_string();
    }

    unsafe fn set_descriptor_set_name(&self, _descriptor_set: &mut n::DescriptorSet, _name: &str) {
        // TODO
    }

    unsafe fn set_descriptor_set_layout_name(
        &self,
        _descriptor_set_layout: &mut n::DescriptorSetLayout,
        _name: &str,
    ) {
        // TODO
    }

    unsafe fn set_pipeline_layout_name(
        &self,
        _pipeline_layout: &mut n::PipelineLayout,
        _name: &str,
    ) {
        // TODO
    }

    unsafe fn set_display_power_state(
        &self,
        _display: &display::Display<Backend>,
        _power_state: &display::control::PowerState,
    ) -> Result<(), display::control::DisplayControlError> {
        unimplemented!()
    }

    unsafe fn register_device_event(
        &self,
        _device_event: &display::control::DeviceEvent,
        _fence: &mut <Backend as hal::Backend>::Fence,
    ) -> Result<(), display::control::DisplayControlError> {
        unimplemented!()
    }

    unsafe fn register_display_event(
        &self,
        _display: &display::Display<Backend>,
        _display_event: &display::control::DisplayEvent,
        _fence: &mut <Backend as hal::Backend>::Fence,
    ) -> Result<(), display::control::DisplayControlError> {
        unimplemented!()
    }

    fn start_capture(&self) {
        let device = self.shared.device.lock();
        let shared_capture_manager = CaptureManager::shared();
        let default_capture_scope = shared_capture_manager.new_capture_scope_with_device(&device);
        shared_capture_manager.set_default_capture_scope(&default_capture_scope);
        shared_capture_manager.start_capture_with_scope(&default_capture_scope);
        default_capture_scope.begin_scope();
    }

    fn stop_capture(&self) {
        let shared_capture_manager = CaptureManager::shared();
        if let Some(default_capture_scope) = shared_capture_manager.default_capture_scope() {
            default_capture_scope.end_scope();
        }
        shared_capture_manager.stop_capture();
    }
}

#[test]
fn test_send_sync() {
    fn foo<T: Send + Sync>() {}
    foo::<Device>()
}
