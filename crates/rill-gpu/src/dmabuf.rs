//! W3 — dmabuf interop (specs/wgpu-renderer.md D2): a wgpu device that can
//! import Linux dma-bufs, and the import itself — fd → `vk::Image` →
//! first-class `wgpu::Texture`.
//!
//! This productionizes the W0 spike recipe: open the Vulkan adapter through
//! wgpu-hal with the four import extensions enabled (`device_from_raw`), wrap
//! it back into an ordinary wgpu device (`create_device_from_hal`), and bind
//! imported memory to images (`texture_from_raw` + `create_texture_from_hal`).
//! The compositor composites client buffers by *sampling* them, so imported
//! textures carry sampled/copy usage, not render-attachment.
//!
//! Tested without a live Wayland client by exporting a dmabuf from Vulkan
//! itself (`alloc_exported`) and importing it back — the same fd path a
//! client's buffer arrives through.

use std::ffi::CStr;
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};

use ash::vk;

/// DRM fourcc codes for the two ubiquitous 32-bit formats (little-endian
/// fourcc, as used on the wire by zwp_linux_dmabuf).
pub const DRM_FORMAT_ARGB8888: u32 = fourcc(b"AR24");
pub const DRM_FORMAT_XRGB8888: u32 = fourcc(b"XR24");
/// DRM_FORMAT_MOD_LINEAR: rows tightly packed at `stride`.
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;

const fn fourcc(b: &[u8; 4]) -> u32 {
    (b[0] as u32) | (b[1] as u32) << 8 | (b[2] as u32) << 16 | (b[3] as u32) << 24
}

/// Both formats decode as BGRA bytes in memory; X ignores alpha (the
/// compositor treats X-imports as opaque when compositing).
fn vk_format_for(fourcc: u32) -> Option<vk::Format> {
    match fourcc {
        DRM_FORMAT_ARGB8888 | DRM_FORMAT_XRGB8888 => Some(vk::Format::B8G8R8A8_UNORM),
        _ => None,
    }
}

fn wgpu_format_for(fourcc: u32) -> Option<wgpu::TextureFormat> {
    match fourcc {
        DRM_FORMAT_ARGB8888 | DRM_FORMAT_XRGB8888 => Some(wgpu::TextureFormat::Bgra8Unorm),
        _ => None,
    }
}

/// Everything needed to bind one single-plane dmabuf: geometry, format, and
/// the exporter's layout. (Multi-planar formats — NV12 video and friends —
/// are out of scope until something produces them.)
#[derive(Debug, Clone, Copy)]
pub struct DmabufPlan {
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub offset: u64,
    pub stride: u64,
}

/// The four device extensions that gate dmabuf import, plus their
/// dependencies (verified present on every driver on this box by the W0
/// spike).
const IMPORT_EXTENSIONS: &[&CStr] = &[
    ash::khr::external_memory_fd::NAME,
    ash::ext::external_memory_dma_buf::NAME,
    ash::ext::image_drm_format_modifier::NAME,
    ash::ext::queue_family_foreign::NAME,
    ash::khr::external_memory::NAME,
];

/// A wgpu device built with the dmabuf-import extensions enabled. The
/// `device`/`queue` are ordinary wgpu objects — every rill-gpu pipeline runs
/// on them unchanged; `import` is the extra capability.
pub struct DmabufDevice {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    adapter: wgpu::Adapter,
}

impl DmabufDevice {
    /// Build on the best Vulkan adapter (discrete preferred). Returns `None`
    /// when no Vulkan adapter exists or the extensions are unavailable —
    /// callers fall back or fail loudly at the compositor level.
    pub fn new() -> Option<DmabufDevice> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..Default::default()
        });
        Self::new_on(&instance, None)
    }

    /// Build on an adapter from `instance` that can also present to
    /// `compatible` (the compositor's window surface, created from the same
    /// instance). The multi-GPU caveat lives here: presenting and importing
    /// on one device keeps client buffers on the GPU we composite with.
    pub fn new_on(
        instance: &wgpu::Instance,
        compatible: Option<&wgpu::Surface<'_>>,
    ) -> Option<DmabufDevice> {
        let mut adapters = instance.enumerate_adapters(wgpu::Backends::VULKAN);
        adapters.sort_by_key(|a| match a.get_info().device_type {
            wgpu::DeviceType::DiscreteGpu => 0,
            wgpu::DeviceType::IntegratedGpu => 1,
            wgpu::DeviceType::VirtualGpu => 2,
            wgpu::DeviceType::Cpu => 3,
            wgpu::DeviceType::Other => 4,
        });
        for adapter in adapters {
            if let Some(surface) = compatible
                && !adapter.is_surface_supported(surface)
            {
                continue;
            }
            if let Some(built) = Self::try_build(&adapter) {
                return Some(built);
            }
        }
        None
    }

    fn try_build(adapter: &wgpu::Adapter) -> Option<DmabufDevice> {
        let open_device = unsafe {
            let hal_adapter = adapter.as_hal::<wgpu::hal::api::Vulkan>()?;

            let mut exts = hal_adapter.required_device_extensions(wgpu::Features::empty());
            for want in IMPORT_EXTENSIONS {
                if !exts.iter().any(|e| e == want) {
                    exts.push(want);
                }
            }

            let raw_instance = hal_adapter.shared_instance().raw_instance();
            let phys = hal_adapter.raw_physical_device();

            // The driver must actually offer the import extensions.
            let available = raw_instance.enumerate_device_extension_properties(phys).ok()?;
            let has = |name: &CStr| {
                available
                    .iter()
                    .any(|e| e.extension_name_as_c_str() == Ok(name))
            };
            if !IMPORT_EXTENSIONS.iter().all(|e| has(e)) {
                return None;
            }

            // First graphics-capable queue family (family 0 on virtually all
            // hardware, but look it up rather than assume).
            let family_index = raw_instance
                .get_physical_device_queue_family_properties(phys)
                .iter()
                .position(|p| p.queue_flags.contains(vk::QueueFlags::GRAPHICS))?
                as u32;

            let mut features =
                hal_adapter.physical_device_features(&exts, wgpu::Features::empty());
            let priorities = [1.0f32];
            let queue_infos = [vk::DeviceQueueCreateInfo::default()
                .queue_family_index(family_index)
                .queue_priorities(&priorities)];
            let ext_ptrs: Vec<*const std::ffi::c_char> = exts.iter().map(|e| e.as_ptr()).collect();
            let info = vk::DeviceCreateInfo::default()
                .queue_create_infos(&queue_infos)
                .enabled_extension_names(&ext_ptrs);
            let info = features.add_to_device_create(info);

            let raw_device = raw_instance.create_device(phys, &info, None).ok()?;
            hal_adapter
                .device_from_raw(
                    raw_device,
                    None,
                    &exts,
                    wgpu::Features::empty(),
                    &wgpu::MemoryHints::Performance,
                    family_index,
                    0,
                )
                .ok()?
        };

        let (device, queue) = unsafe {
            adapter.create_device_from_hal(
                open_device,
                &wgpu::DeviceDescriptor {
                    label: Some("rill-dmabuf"),
                    required_features: wgpu::Features::empty(),
                    // The adapter's real limits: a compositor's swapchain is
                    // the output size (4K+), far past downlevel's 2048 cap.
                    required_limits: adapter.limits(),
                    memory_hints: wgpu::MemoryHints::Performance,
                    trace: wgpu::Trace::Off,
                },
            )
        }
        .ok()?;

        Some(DmabufDevice { device, queue, adapter: adapter.clone() })
    }

    pub fn adapter_name(&self) -> String {
        self.adapter.get_info().name
    }

    /// The adapter the device was built on (surface capability queries).
    pub fn adapter(&self) -> &wgpu::Adapter {
        &self.adapter
    }

    /// Run `f` with the raw ash handles (device, physical device, instance).
    fn with_raw<R>(
        &self,
        f: impl FnOnce(&ash::Device, vk::PhysicalDevice, &ash::Instance) -> R,
    ) -> R {
        unsafe {
            let hal = self
                .device
                .as_hal::<wgpu::hal::api::Vulkan>()
                .expect("dmabuf device is always vulkan");
            let raw = hal.raw_device().clone();
            let phys = hal.raw_physical_device();
            let instance = hal.shared_instance().raw_instance().clone();
            drop(hal);
            f(&raw, phys, &instance)
        }
    }

    /// The DRM modifiers this device supports for `fourcc` (what the
    /// compositor's zwp_linux_dmabuf global advertises). Empty for unknown
    /// formats.
    pub fn supported_modifiers(&self, fourcc: u32) -> Vec<u64> {
        let Some(format) = vk_format_for(fourcc) else { return Vec::new() };
        self.with_raw(|_, phys, instance| unsafe {
            // Two-call pattern: count, then fill.
            let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
            let mut props = vk::FormatProperties2::default().push_next(&mut list);
            instance.get_physical_device_format_properties2(phys, format, &mut props);
            let count = list.drm_format_modifier_count as usize;
            let mut storage = vec![vk::DrmFormatModifierPropertiesEXT::default(); count];
            let filled = {
                let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
                    .drm_format_modifier_properties(&mut storage);
                let mut props = vk::FormatProperties2::default().push_next(&mut list);
                instance.get_physical_device_format_properties2(phys, format, &mut props);
                list.drm_format_modifier_count as usize
            };
            storage.iter().take(filled).map(|p| p.drm_format_modifier).collect()
        })
    }

    /// Import a single-plane dmabuf as a sampleable wgpu texture. Consumes
    /// the fd on success (Vulkan takes ownership); closes it on failure.
    pub fn import(&self, plan: &DmabufPlan, fd: OwnedFd) -> Result<wgpu::Texture, String> {
        let vk_format =
            vk_format_for(plan.fourcc).ok_or_else(|| format!("fourcc {:#x}", plan.fourcc))?;
        let wgpu_format = wgpu_format_for(plan.fourcc).unwrap();
        if plan.width == 0 || plan.height == 0 {
            return Err("zero-sized dmabuf".into());
        }

        let size =
            wgpu::Extent3d { width: plan.width, height: plan.height, depth_or_array_layers: 1 };
        // Compositing samples client buffers; copies cover screenshots and
        // readback tests. No render-attachment — we never draw into a
        // client's buffer.
        let vk_usage = vk::ImageUsageFlags::SAMPLED
            | vk::ImageUsageFlags::TRANSFER_SRC
            | vk::ImageUsageFlags::TRANSFER_DST;
        let wgpu_usage = wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::COPY_DST;
        let hal_usage = wgpu::TextureUses::RESOURCE
            | wgpu::TextureUses::COPY_SRC
            | wgpu::TextureUses::COPY_DST;

        // --- Raw Vulkan: image + imported memory --------------------------
        let (image, memory, raw_for_drop) = self.with_raw(|raw, phys, instance| unsafe {
            let mut external =
                vk::ExternalMemoryImageCreateInfo::default()
                    .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let plane_layouts = [vk::SubresourceLayout {
                offset: plan.offset,
                size: 0, // must be zero for import
                row_pitch: plan.stride,
                array_pitch: 0,
                depth_pitch: 0,
            }];
            let mut drm = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
                .drm_format_modifier(plan.modifier)
                .plane_layouts(&plane_layouts);
            let info = vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(vk_format)
                .extent(vk::Extent3D {
                    width: plan.width,
                    height: plan.height,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .usage(vk_usage)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .push_next(&mut external)
                .push_next(&mut drm);
            let image = raw
                .create_image(&info, None)
                .map_err(|e| format!("create_image: {e}"))?;

            // Which memory types can hold this fd?
            let fd_loader = ash::khr::external_memory_fd::Device::new(instance, raw);
            let mut fd_props = vk::MemoryFdPropertiesKHR::default();
            fd_loader
                .get_memory_fd_properties(
                    vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                    fd.as_raw_fd_borrow(),
                    &mut fd_props,
                )
                .map_err(|e| {
                    raw.destroy_image(image, None);
                    format!("get_memory_fd_properties: {e}")
                })?;
            let reqs = raw.get_image_memory_requirements(image);
            let allowed = reqs.memory_type_bits & fd_props.memory_type_bits;
            let mem_props = instance.get_physical_device_memory_properties(phys);
            let Some(type_index) =
                (0..mem_props.memory_type_count).find(|i| allowed & (1 << i) != 0)
            else {
                raw.destroy_image(image, None);
                return Err("no compatible memory type for dmabuf".into());
            };

            // allocate_memory consumes the fd on success only.
            let raw_fd = fd.into_raw_fd();
            let mut import =
                vk::ImportMemoryFdInfoKHR::default()
                    .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
                    .fd(raw_fd);
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
            let alloc = vk::MemoryAllocateInfo::default()
                .allocation_size(reqs.size)
                .memory_type_index(type_index)
                .push_next(&mut import)
                .push_next(&mut dedicated);
            let memory = raw.allocate_memory(&alloc, None).map_err(|e| {
                drop(OwnedFd::from_raw_fd(raw_fd)); // close: Vulkan didn't take it
                raw.destroy_image(image, None);
                format!("allocate_memory: {e}")
            })?;
            raw.bind_image_memory(image, memory, 0).map_err(|e| {
                raw.free_memory(memory, None);
                raw.destroy_image(image, None);
                format!("bind_image_memory: {e}")
            })?;
            Ok::<_, String>((image, memory, raw.clone()))
        })?;

        // --- Wrap into wgpu ----------------------------------------------
        let hal_desc = wgpu::hal::TextureDescriptor {
            label: Some("dmabuf-import"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu_format,
            usage: hal_usage,
            memory_flags: wgpu::hal::MemoryFlags::empty(),
            view_formats: vec![],
        };
        let drop_callback: wgpu::hal::DropCallback = Box::new(move || unsafe {
            raw_for_drop.destroy_image(image, None);
            raw_for_drop.free_memory(memory, None);
        });
        let texture = unsafe {
            let hal = self
                .device
                .as_hal::<wgpu::hal::api::Vulkan>()
                .expect("dmabuf device is always vulkan");
            let hal_texture = hal.texture_from_raw(image, &hal_desc, Some(drop_callback));
            drop(hal);
            self.device.create_texture_from_hal::<wgpu::hal::api::Vulkan>(
                hal_texture,
                &wgpu::TextureDescriptor {
                    label: Some("dmabuf-import"),
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu_format,
                    usage: wgpu_usage,
                    view_formats: &[],
                },
            )
        };
        Ok(texture)
    }

    /// Allocate a linear BGRA image with exportable memory and hand back its
    /// dmabuf fd + layout. This is how the import path is tested without a
    /// live client (a client's buffer arrives through the identical fd path);
    /// the image and memory are destroyed before returning — the fd keeps the
    /// underlying buffer alive.
    pub fn alloc_exported(&self, width: u32, height: u32) -> Result<(OwnedFd, DmabufPlan), String> {
        let vk_format = vk_format_for(DRM_FORMAT_ARGB8888).unwrap();
        self.with_raw(|raw, phys, instance| unsafe {
            let mut external =
                vk::ExternalMemoryImageCreateInfo::default()
                    .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let modifiers = [DRM_FORMAT_MOD_LINEAR];
            let mut drm = vk::ImageDrmFormatModifierListCreateInfoEXT::default()
                .drm_format_modifiers(&modifiers);
            let info = vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(vk_format)
                .extent(vk::Extent3D { width, height, depth: 1 })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .usage(vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .push_next(&mut external)
                .push_next(&mut drm);
            let image = raw
                .create_image(&info, None)
                .map_err(|e| format!("create_image (export): {e}"))?;

            let reqs = raw.get_image_memory_requirements(image);
            let mem_props = instance.get_physical_device_memory_properties(phys);
            let Some(type_index) =
                (0..mem_props.memory_type_count).find(|i| reqs.memory_type_bits & (1 << i) != 0)
            else {
                raw.destroy_image(image, None);
                return Err("no memory type for export image".into());
            };
            let mut export = vk::ExportMemoryAllocateInfo::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
            let alloc = vk::MemoryAllocateInfo::default()
                .allocation_size(reqs.size)
                .memory_type_index(type_index)
                .push_next(&mut export)
                .push_next(&mut dedicated);
            let memory = raw.allocate_memory(&alloc, None).map_err(|e| {
                raw.destroy_image(image, None);
                format!("allocate_memory (export): {e}")
            })?;
            raw.bind_image_memory(image, memory, 0).map_err(|e| {
                raw.free_memory(memory, None);
                raw.destroy_image(image, None);
                format!("bind_image_memory (export): {e}")
            })?;

            // Layout of memory plane 0 (linear ⇒ plain offset+stride).
            let layout = raw.get_image_subresource_layout(
                image,
                vk::ImageSubresource {
                    aspect_mask: vk::ImageAspectFlags::MEMORY_PLANE_0_EXT,
                    mip_level: 0,
                    array_layer: 0,
                },
            );

            let fd_loader = ash::khr::external_memory_fd::Device::new(instance, raw);
            let get_info = vk::MemoryGetFdInfoKHR::default()
                .memory(memory)
                .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let raw_fd = fd_loader.get_memory_fd(&get_info).map_err(|e| {
                raw.free_memory(memory, None);
                raw.destroy_image(image, None);
                format!("get_memory_fd: {e}")
            })?;
            let fd = OwnedFd::from_raw_fd(raw_fd);

            // The fd holds its own reference to the buffer.
            raw.destroy_image(image, None);
            raw.free_memory(memory, None);

            Ok((
                fd,
                DmabufPlan {
                    width,
                    height,
                    fourcc: DRM_FORMAT_ARGB8888,
                    modifier: DRM_FORMAT_MOD_LINEAR,
                    offset: layout.offset,
                    stride: layout.row_pitch,
                },
            ))
        })
    }
}

/// Borrow the raw fd without consuming ownership (for property queries that
/// don't take the fd).
trait AsRawFdBorrow {
    fn as_raw_fd_borrow(&self) -> i32;
}

impl AsRawFdBorrow for OwnedFd {
    fn as_raw_fd_borrow(&self) -> i32 {
        use std::os::fd::AsRawFd;
        self.as_raw_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device() -> Option<DmabufDevice> {
        let d = DmabufDevice::new();
        if d.is_none() {
            eprintln!("skip: no dmabuf-capable Vulkan device");
        }
        d
    }

    #[test]
    fn advertises_modifiers_for_argb8888() {
        let Some(d) = device() else { return };
        eprintln!("dmabuf device: {}", d.adapter_name());
        let mods = d.supported_modifiers(DRM_FORMAT_ARGB8888);
        assert!(!mods.is_empty(), "no modifiers for ARGB8888");
        // Unknown fourcc → empty, not a panic.
        assert!(d.supported_modifiers(0xDEAD_BEEF).is_empty());
    }

    #[test]
    fn export_then_import_round_trips_pixels() {
        let Some(d) = device() else { return };
        // Export a 8x8 linear dmabuf from Vulkan, import it back as a wgpu
        // texture — the identical fd path a client buffer arrives through.
        let (fd, plan) = d.alloc_exported(8, 8).expect("export dmabuf");
        assert_eq!(plan.modifier, DRM_FORMAT_MOD_LINEAR);
        assert!(plan.stride >= 8 * 4);
        let texture = d.import(&plan, fd).expect("import dmabuf");

        // Write a pattern through wgpu, read it back through wgpu: proves the
        // imported memory is genuinely bound and usable both directions.
        let pixels: Vec<u8> = (0..8u32 * 8)
            .flat_map(|i| [(i % 251) as u8, (i * 7 % 251) as u8, (i * 13 % 251) as u8, 255])
            .collect();
        d.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(8 * 4),
                rows_per_image: Some(8),
            },
            wgpu::Extent3d { width: 8, height: 8, depth_or_array_layers: 1 },
        );

        let readback = d.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: 256 * 8, // 256-aligned rows
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = d.device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(256),
                    rows_per_image: Some(8),
                },
            },
            wgpu::Extent3d { width: 8, height: 8, depth_or_array_layers: 1 },
        );
        d.queue.submit([encoder.finish()]);

        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        loop {
            let _ = d.device.poll(wgpu::PollType::Wait);
            match rx.try_recv() {
                Ok(r) => {
                    r.expect("map");
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => continue,
                Err(e) => panic!("{e}"),
            }
        }
        let data = slice.get_mapped_range();
        for row in 0..8usize {
            let got = &data[row * 256..row * 256 + 32];
            let want = &pixels[row * 32..row * 32 + 32];
            assert_eq!(got, want, "row {row} mismatch through the dmabuf");
        }
        drop(data);

        // Dropping the texture runs the drop callback (destroys image+memory)
        // without exploding.
        drop(texture);
        let _ = d.device.poll(wgpu::PollType::Wait);
    }

    #[test]
    fn import_rejects_bad_plans() {
        let Some(d) = device() else { return };
        let (fd, plan) = d.alloc_exported(4, 4).expect("export");
        // Unknown fourcc fails cleanly (and still closes the fd).
        let bad = DmabufPlan { fourcc: 0x1234_5678, ..plan };
        assert!(d.import(&bad, fd).is_err());
        // Zero-size fails.
        let (fd2, plan2) = d.alloc_exported(4, 4).expect("export");
        let zero = DmabufPlan { width: 0, ..plan2 };
        assert!(d.import(&zero, fd2).is_err());
    }
}
