use hal::adapter::MemoryProperties;
use hal::pso::VertexInputRate;
use hal::queue::QueueFamilyId;
use hal::range::RangeArg;
use hal::{buffer, device, format, image, memory, pass, pool, pso, query, window};

use winapi::shared::dxgi::{
    IDXGIFactory,
    IDXGISwapChain,
    DXGI_SWAP_CHAIN_DESC,
    DXGI_SWAP_EFFECT_DISCARD,
};
use winapi::shared::minwindef::TRUE;
use winapi::shared::windef::HWND;
use winapi::shared::{dxgiformat, dxgitype, winerror};
use winapi::um::{d3d11, d3d11sdklayers, d3dcommon};
use winapi::Interface as _;

use wio::com::ComPtr;

use std::borrow::Borrow;
use std::cell::RefCell;
use std::ops::Range;
use std::sync::Arc;
use std::{fmt, mem, ptr};

use parking_lot::{Condvar, Mutex};

use {
    Backend,
    Buffer,
    BufferView,
    CommandBuffer,
    CommandPool,
    ComputePipeline,
    Descriptor,
    DescriptorPool,
    DescriptorSetLayout,
    Fence,
    Framebuffer,
    GraphicsPipeline,
    Image,
    ImageView,
    InternalBuffer,
    InternalImage,
    Memory,
    MemoryHeapFlags,
    PipelineBinding,
    PipelineLayout,
    QueryPool,
    RawFence,
    RegisterMapping,
    RegisterRemapping,
    RenderPass,
    Sampler,
    Semaphore,
    ShaderModule,
    SubpassDesc,
    Surface,
    Swapchain,
    ViewInfo,
};

use {conv, internal, shader};

struct InputLayout {
    raw: ComPtr<d3d11::ID3D11InputLayout>,
    required_bindings: u32,
    max_vertex_bindings: u32,
    topology: d3d11::D3D11_PRIMITIVE_TOPOLOGY,
    vertex_strides: Vec<u32>,
}

pub struct Device {
    raw: ComPtr<d3d11::ID3D11Device>,
    pub(crate) context: ComPtr<d3d11::ID3D11DeviceContext>,
    memory_properties: MemoryProperties,
    memory_heap_flags: [MemoryHeapFlags; 3],
    pub(crate) internal: internal::Internal,
}

impl fmt::Debug for Device {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str("Device")
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        if let Ok(debug) = self.raw.cast::<d3d11sdklayers::ID3D11Debug>() {
            unsafe {
                debug.ReportLiveDeviceObjects(d3d11sdklayers::D3D11_RLDO_DETAIL);
            }
        }
    }
}

unsafe impl Send for Device {}
unsafe impl Sync for Device {}

impl Device {
    pub fn as_raw(&self) -> *mut d3d11::ID3D11Device {
        self.raw.as_raw()
    }

    pub fn new(
        device: ComPtr<d3d11::ID3D11Device>,
        context: ComPtr<d3d11::ID3D11DeviceContext>,
        memory_properties: MemoryProperties,
    ) -> Self {
        Device {
            raw: device.clone(),
            context,
            memory_properties,
            memory_heap_flags: [
                MemoryHeapFlags::DEVICE_LOCAL,
                MemoryHeapFlags::HOST_COHERENT,
                MemoryHeapFlags::HOST_VISIBLE,
            ],
            internal: internal::Internal::new(&device),
        }
    }

    fn create_rasterizer_state(
        &self,
        rasterizer_desc: &pso::Rasterizer,
    ) -> Result<ComPtr<d3d11::ID3D11RasterizerState>, pso::CreationError> {
        let mut rasterizer = ptr::null_mut();
        let desc = conv::map_rasterizer_desc(rasterizer_desc);

        let hr = unsafe {
            self.raw
                .CreateRasterizerState(&desc, &mut rasterizer as *mut *mut _ as *mut *mut _)
        };

        if winerror::SUCCEEDED(hr) {
            Ok(unsafe { ComPtr::from_raw(rasterizer) })
        } else {
            Err(pso::CreationError::Other)
        }
    }

    fn create_blend_state(
        &self,
        blend_desc: &pso::BlendDesc,
    ) -> Result<ComPtr<d3d11::ID3D11BlendState>, pso::CreationError> {
        let mut blend = ptr::null_mut();
        let desc = conv::map_blend_desc(blend_desc);

        let hr = unsafe {
            self.raw
                .CreateBlendState(&desc, &mut blend as *mut *mut _ as *mut *mut _)
        };

        if winerror::SUCCEEDED(hr) {
            Ok(unsafe { ComPtr::from_raw(blend) })
        } else {
            Err(pso::CreationError::Other)
        }
    }

    fn create_depth_stencil_state(
        &self,
        depth_desc: &pso::DepthStencilDesc,
    ) -> Result<
        (
            ComPtr<d3d11::ID3D11DepthStencilState>,
            pso::State<pso::StencilValue>,
        ),
        pso::CreationError,
    > {
        let mut depth = ptr::null_mut();
        let (desc, stencil_ref) = conv::map_depth_stencil_desc(depth_desc);

        let hr = unsafe {
            self.raw
                .CreateDepthStencilState(&desc, &mut depth as *mut *mut _ as *mut *mut _)
        };

        if winerror::SUCCEEDED(hr) {
            Ok((unsafe { ComPtr::from_raw(depth) }, stencil_ref))
        } else {
            Err(pso::CreationError::Other)
        }
    }

    fn create_input_layout(
        &self,
        vs: ComPtr<d3dcommon::ID3DBlob>,
        vertex_buffers: &[pso::VertexBufferDesc],
        attributes: &[pso::AttributeDesc],
        input_assembler: &pso::InputAssemblerDesc,
    ) -> Result<InputLayout, pso::CreationError> {
        let mut layout = ptr::null_mut();

        let mut vertex_strides = Vec::new();
        let mut required_bindings = 0u32;
        let mut max_vertex_bindings = 0u32;
        for buffer in vertex_buffers {
            required_bindings |= 1 << buffer.binding as u32;
            max_vertex_bindings = max_vertex_bindings.max(1u32 + buffer.binding as u32);

            while vertex_strides.len() <= buffer.binding as usize {
                vertex_strides.push(0);
            }

            vertex_strides[buffer.binding as usize] = buffer.stride;
        }

        let input_elements = attributes
            .iter()
            .filter_map(|attrib| {
                let buffer_desc = match vertex_buffers
                    .iter()
                    .find(|buffer_desc| buffer_desc.binding == attrib.binding)
                {
                    Some(buffer_desc) => buffer_desc,
                    None => {
                        // TODO:
                        // L
                        // error!("Couldn't find associated vertex buffer description {:?}", attrib.binding);
                        return Some(Err(pso::CreationError::Other));
                    }
                };

                let (slot_class, step_rate) = match buffer_desc.rate {
                    VertexInputRate::Vertex => (d3d11::D3D11_INPUT_PER_VERTEX_DATA, 0),
                    VertexInputRate::Instance(divisor) => {
                        (d3d11::D3D11_INPUT_PER_INSTANCE_DATA, divisor)
                    }
                };
                let format = attrib.element.format;

                Some(Ok(d3d11::D3D11_INPUT_ELEMENT_DESC {
                    SemanticName: "TEXCOORD\0".as_ptr() as *const _, // Semantic name used by SPIRV-Cross
                    SemanticIndex: attrib.location,
                    Format: match conv::map_format(format) {
                        Some(fm) => fm,
                        None => {
                            // TODO:
                            // error!("Unable to find DXGI format for {:?}", format);
                            return Some(Err(pso::CreationError::Other));
                        }
                    },
                    InputSlot: attrib.binding as _,
                    AlignedByteOffset: attrib.element.offset,
                    InputSlotClass: slot_class,
                    InstanceDataStepRate: step_rate as _,
                }))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let hr = unsafe {
            self.raw.CreateInputLayout(
                input_elements.as_ptr(),
                input_elements.len() as _,
                vs.GetBufferPointer(),
                vs.GetBufferSize(),
                &mut layout as *mut *mut _ as *mut *mut _,
            )
        };

        if winerror::SUCCEEDED(hr) {
            let topology = conv::map_topology(input_assembler.primitive);

            Ok(InputLayout {
                raw: unsafe { ComPtr::from_raw(layout) },
                required_bindings,
                max_vertex_bindings,
                topology,
                vertex_strides,
            })
        } else {
            Err(pso::CreationError::Other)
        }
    }

    fn create_vertex_shader(
        &self,
        blob: ComPtr<d3dcommon::ID3DBlob>,
    ) -> Result<ComPtr<d3d11::ID3D11VertexShader>, pso::CreationError> {
        let mut vs = ptr::null_mut();

        let hr = unsafe {
            self.raw.CreateVertexShader(
                blob.GetBufferPointer(),
                blob.GetBufferSize(),
                ptr::null_mut(),
                &mut vs as *mut *mut _ as *mut *mut _,
            )
        };

        if winerror::SUCCEEDED(hr) {
            Ok(unsafe { ComPtr::from_raw(vs) })
        } else {
            Err(pso::CreationError::Other)
        }
    }

    fn create_pixel_shader(
        &self,
        blob: ComPtr<d3dcommon::ID3DBlob>,
    ) -> Result<ComPtr<d3d11::ID3D11PixelShader>, pso::CreationError> {
        let mut ps = ptr::null_mut();

        let hr = unsafe {
            self.raw.CreatePixelShader(
                blob.GetBufferPointer(),
                blob.GetBufferSize(),
                ptr::null_mut(),
                &mut ps as *mut *mut _ as *mut *mut _,
            )
        };

        if winerror::SUCCEEDED(hr) {
            Ok(unsafe { ComPtr::from_raw(ps) })
        } else {
            Err(pso::CreationError::Other)
        }
    }

    fn create_geometry_shader(
        &self,
        blob: ComPtr<d3dcommon::ID3DBlob>,
    ) -> Result<ComPtr<d3d11::ID3D11GeometryShader>, pso::CreationError> {
        let mut gs = ptr::null_mut();

        let hr = unsafe {
            self.raw.CreateGeometryShader(
                blob.GetBufferPointer(),
                blob.GetBufferSize(),
                ptr::null_mut(),
                &mut gs as *mut *mut _ as *mut *mut _,
            )
        };

        if winerror::SUCCEEDED(hr) {
            Ok(unsafe { ComPtr::from_raw(gs) })
        } else {
            Err(pso::CreationError::Other)
        }
    }

    fn create_hull_shader(
        &self,
        blob: ComPtr<d3dcommon::ID3DBlob>,
    ) -> Result<ComPtr<d3d11::ID3D11HullShader>, pso::CreationError> {
        let mut hs = ptr::null_mut();

        let hr = unsafe {
            self.raw.CreateHullShader(
                blob.GetBufferPointer(),
                blob.GetBufferSize(),
                ptr::null_mut(),
                &mut hs as *mut *mut _ as *mut *mut _,
            )
        };

        if winerror::SUCCEEDED(hr) {
            Ok(unsafe { ComPtr::from_raw(hs) })
        } else {
            Err(pso::CreationError::Other)
        }
    }

    fn create_domain_shader(
        &self,
        blob: ComPtr<d3dcommon::ID3DBlob>,
    ) -> Result<ComPtr<d3d11::ID3D11DomainShader>, pso::CreationError> {
        let mut ds = ptr::null_mut();

        let hr = unsafe {
            self.raw.CreateDomainShader(
                blob.GetBufferPointer(),
                blob.GetBufferSize(),
                ptr::null_mut(),
                &mut ds as *mut *mut _ as *mut *mut _,
            )
        };

        if winerror::SUCCEEDED(hr) {
            Ok(unsafe { ComPtr::from_raw(ds) })
        } else {
            Err(pso::CreationError::Other)
        }
    }

    fn create_compute_shader(
        &self,
        blob: ComPtr<d3dcommon::ID3DBlob>,
    ) -> Result<ComPtr<d3d11::ID3D11ComputeShader>, pso::CreationError> {
        let mut cs = ptr::null_mut();

        let hr = unsafe {
            self.raw.CreateComputeShader(
                blob.GetBufferPointer(),
                blob.GetBufferSize(),
                ptr::null_mut(),
                &mut cs as *mut *mut _ as *mut *mut _,
            )
        };

        if winerror::SUCCEEDED(hr) {
            Ok(unsafe { ComPtr::from_raw(cs) })
        } else {
            Err(pso::CreationError::Other)
        }
    }

    // TODO: fix return type..
    fn extract_entry_point(
        stage: pso::Stage,
        source: &pso::EntryPoint<Backend>,
        layout: &PipelineLayout,
    ) -> Result<Option<ComPtr<d3dcommon::ID3DBlob>>, device::ShaderError> {
        // TODO: entrypoint stuff
        match *source.module {
            ShaderModule::Dxbc(ref _shader) => {
                unimplemented!()

                // Ok(Some(shader))
            }
            ShaderModule::Spirv(ref raw_data) => Ok(shader::compile_spirv_entrypoint(
                raw_data, stage, source, layout,
            )?),
        }
    }

    fn view_image_as_shader_resource(
        &self,
        info: &ViewInfo,
    ) -> Result<ComPtr<d3d11::ID3D11ShaderResourceView>, image::ViewError> {
        let mut desc: d3d11::D3D11_SHADER_RESOURCE_VIEW_DESC = unsafe { mem::zeroed() };
        desc.Format = info.format;
        if desc.Format == dxgiformat::DXGI_FORMAT_D32_FLOAT_S8X24_UINT {
            desc.Format = dxgiformat::DXGI_FORMAT_R32_FLOAT_X8X24_TYPELESS;
        }

        #[allow(non_snake_case)]
        let MostDetailedMip = info.range.levels.start as _;
        #[allow(non_snake_case)]
        let MipLevels = (info.range.levels.end - info.range.levels.start) as _;
        #[allow(non_snake_case)]
        let FirstArraySlice = info.range.layers.start as _;
        #[allow(non_snake_case)]
        let ArraySize = (info.range.layers.end - info.range.layers.start) as _;

        match info.view_kind {
            image::ViewKind::D1 => {
                desc.ViewDimension = d3dcommon::D3D11_SRV_DIMENSION_TEXTURE1D;
                *unsafe { desc.u.Texture1D_mut() } = d3d11::D3D11_TEX1D_SRV {
                    MostDetailedMip,
                    MipLevels,
                }
            }
            image::ViewKind::D1Array => {
                desc.ViewDimension = d3dcommon::D3D11_SRV_DIMENSION_TEXTURE1DARRAY;
                *unsafe { desc.u.Texture1DArray_mut() } = d3d11::D3D11_TEX1D_ARRAY_SRV {
                    MostDetailedMip,
                    MipLevels,
                    FirstArraySlice,
                    ArraySize,
                }
            }
            image::ViewKind::D2 => {
                desc.ViewDimension = d3dcommon::D3D11_SRV_DIMENSION_TEXTURE2D;
                *unsafe { desc.u.Texture2D_mut() } = d3d11::D3D11_TEX2D_SRV {
                    MostDetailedMip,
                    MipLevels,
                }
            }
            image::ViewKind::D2Array => {
                desc.ViewDimension = d3dcommon::D3D11_SRV_DIMENSION_TEXTURE2DARRAY;
                *unsafe { desc.u.Texture2DArray_mut() } = d3d11::D3D11_TEX2D_ARRAY_SRV {
                    MostDetailedMip,
                    MipLevels,
                    FirstArraySlice,
                    ArraySize,
                }
            }
            image::ViewKind::D3 => {
                desc.ViewDimension = d3dcommon::D3D11_SRV_DIMENSION_TEXTURE3D;
                *unsafe { desc.u.Texture3D_mut() } = d3d11::D3D11_TEX3D_SRV {
                    MostDetailedMip,
                    MipLevels,
                }
            }
            image::ViewKind::Cube => {
                desc.ViewDimension = d3dcommon::D3D11_SRV_DIMENSION_TEXTURECUBE;
                *unsafe { desc.u.TextureCube_mut() } = d3d11::D3D11_TEXCUBE_SRV {
                    MostDetailedMip,
                    MipLevels,
                }
            }
            image::ViewKind::CubeArray => {
                desc.ViewDimension = d3dcommon::D3D11_SRV_DIMENSION_TEXTURECUBEARRAY;
                *unsafe { desc.u.TextureCubeArray_mut() } = d3d11::D3D11_TEXCUBE_ARRAY_SRV {
                    MostDetailedMip,
                    MipLevels,
                    First2DArrayFace: FirstArraySlice,
                    NumCubes: ArraySize / 6,
                }
            }
        }

        let mut srv = ptr::null_mut();
        let hr = unsafe {
            self.raw.CreateShaderResourceView(
                info.resource,
                &desc,
                &mut srv as *mut *mut _ as *mut *mut _,
            )
        };

        if winerror::SUCCEEDED(hr) {
            Ok(unsafe { ComPtr::from_raw(srv) })
        } else {
            Err(image::ViewError::Unsupported)
        }
    }

    fn view_image_as_unordered_access(
        &self,
        info: &ViewInfo,
    ) -> Result<ComPtr<d3d11::ID3D11UnorderedAccessView>, image::ViewError> {
        let mut desc: d3d11::D3D11_UNORDERED_ACCESS_VIEW_DESC = unsafe { mem::zeroed() };
        desc.Format = info.format;

        #[allow(non_snake_case)]
        let MipSlice = info.range.levels.start as _;
        #[allow(non_snake_case)]
        let FirstArraySlice = info.range.layers.start as _;
        #[allow(non_snake_case)]
        let ArraySize = (info.range.layers.end - info.range.layers.start) as _;

        match info.view_kind {
            image::ViewKind::D1 => {
                desc.ViewDimension = d3d11::D3D11_UAV_DIMENSION_TEXTURE1D;
                *unsafe { desc.u.Texture1D_mut() } = d3d11::D3D11_TEX1D_UAV {
                    MipSlice: info.range.levels.start as _,
                }
            }
            image::ViewKind::D1Array => {
                desc.ViewDimension = d3d11::D3D11_UAV_DIMENSION_TEXTURE1DARRAY;
                *unsafe { desc.u.Texture1DArray_mut() } = d3d11::D3D11_TEX1D_ARRAY_UAV {
                    MipSlice,
                    FirstArraySlice,
                    ArraySize,
                }
            }
            image::ViewKind::D2 => {
                desc.ViewDimension = d3d11::D3D11_UAV_DIMENSION_TEXTURE2D;
                *unsafe { desc.u.Texture2D_mut() } = d3d11::D3D11_TEX2D_UAV {
                    MipSlice: info.range.levels.start as _,
                }
            }
            image::ViewKind::D2Array => {
                desc.ViewDimension = d3d11::D3D11_UAV_DIMENSION_TEXTURE2DARRAY;
                *unsafe { desc.u.Texture2DArray_mut() } = d3d11::D3D11_TEX2D_ARRAY_UAV {
                    MipSlice,
                    FirstArraySlice,
                    ArraySize,
                }
            }
            image::ViewKind::D3 => {
                desc.ViewDimension = d3d11::D3D11_UAV_DIMENSION_TEXTURE3D;
                *unsafe { desc.u.Texture3D_mut() } = d3d11::D3D11_TEX3D_UAV {
                    MipSlice,
                    FirstWSlice: FirstArraySlice,
                    WSize: ArraySize,
                }
            }
            _ => unimplemented!(),
        }

        let mut uav = ptr::null_mut();
        let hr = unsafe {
            self.raw.CreateUnorderedAccessView(
                info.resource,
                &desc,
                &mut uav as *mut *mut _ as *mut *mut _,
            )
        };

        if winerror::SUCCEEDED(hr) {
            Ok(unsafe { ComPtr::from_raw(uav) })
        } else {
            Err(image::ViewError::Unsupported)
        }
    }

    pub(crate) fn view_image_as_render_target(
        &self,
        info: &ViewInfo,
    ) -> Result<ComPtr<d3d11::ID3D11RenderTargetView>, image::ViewError> {
        let mut desc: d3d11::D3D11_RENDER_TARGET_VIEW_DESC = unsafe { mem::zeroed() };
        desc.Format = info.format;

        #[allow(non_snake_case)]
        let MipSlice = info.range.levels.start as _;
        #[allow(non_snake_case)]
        let FirstArraySlice = info.range.layers.start as _;
        #[allow(non_snake_case)]
        let ArraySize = (info.range.layers.end - info.range.layers.start) as _;

        match info.view_kind {
            image::ViewKind::D1 => {
                desc.ViewDimension = d3d11::D3D11_RTV_DIMENSION_TEXTURE1D;
                *unsafe { desc.u.Texture1D_mut() } = d3d11::D3D11_TEX1D_RTV { MipSlice }
            }
            image::ViewKind::D1Array => {
                desc.ViewDimension = d3d11::D3D11_RTV_DIMENSION_TEXTURE1DARRAY;
                *unsafe { desc.u.Texture1DArray_mut() } = d3d11::D3D11_TEX1D_ARRAY_RTV {
                    MipSlice,
                    FirstArraySlice,
                    ArraySize,
                }
            }
            image::ViewKind::D2 => {
                desc.ViewDimension = d3d11::D3D11_RTV_DIMENSION_TEXTURE2D;
                *unsafe { desc.u.Texture2D_mut() } = d3d11::D3D11_TEX2D_RTV { MipSlice }
            }
            image::ViewKind::D2Array => {
                desc.ViewDimension = d3d11::D3D11_RTV_DIMENSION_TEXTURE2DARRAY;
                *unsafe { desc.u.Texture2DArray_mut() } = d3d11::D3D11_TEX2D_ARRAY_RTV {
                    MipSlice,
                    FirstArraySlice,
                    ArraySize,
                }
            }
            image::ViewKind::D3 => {
                desc.ViewDimension = d3d11::D3D11_RTV_DIMENSION_TEXTURE3D;
                *unsafe { desc.u.Texture3D_mut() } = d3d11::D3D11_TEX3D_RTV {
                    MipSlice,
                    FirstWSlice: FirstArraySlice,
                    WSize: ArraySize,
                }
            }
            _ => unimplemented!(),
        }

        let mut rtv = ptr::null_mut();
        let hr = unsafe {
            self.raw.CreateRenderTargetView(
                info.resource,
                &desc,
                &mut rtv as *mut *mut _ as *mut *mut _,
            )
        };

        if winerror::SUCCEEDED(hr) {
            Ok(unsafe { ComPtr::from_raw(rtv) })
        } else {
            Err(image::ViewError::Unsupported)
        }
    }

    fn view_image_as_depth_stencil(
        &self,
        info: &ViewInfo,
    ) -> Result<ComPtr<d3d11::ID3D11DepthStencilView>, image::ViewError> {
        #![allow(non_snake_case)]

        let MipSlice = info.range.levels.start as _;
        let FirstArraySlice = info.range.layers.start as _;
        let ArraySize = (info.range.layers.end - info.range.layers.start) as _;
        assert_eq!(info.range.levels.start + 1, info.range.levels.end);
        assert!(info.range.layers.end <= info.kind.num_layers());

        let mut desc: d3d11::D3D11_DEPTH_STENCIL_VIEW_DESC = unsafe { mem::zeroed() };
        desc.Format = info.format;

        match info.view_kind {
            image::ViewKind::D2 => {
                desc.ViewDimension = d3d11::D3D11_DSV_DIMENSION_TEXTURE2D;
                *unsafe { desc.u.Texture2D_mut() } = d3d11::D3D11_TEX2D_DSV { MipSlice }
            }
            image::ViewKind::D2Array => {
                desc.ViewDimension = d3d11::D3D11_DSV_DIMENSION_TEXTURE2DARRAY;
                *unsafe { desc.u.Texture2DArray_mut() } = d3d11::D3D11_TEX2D_ARRAY_DSV {
                    MipSlice,
                    FirstArraySlice,
                    ArraySize,
                }
            }
            _ => unimplemented!(),
        }

        let mut dsv = ptr::null_mut();
        let hr = unsafe {
            self.raw.CreateDepthStencilView(
                info.resource,
                &desc,
                &mut dsv as *mut *mut _ as *mut *mut _,
            )
        };

        if winerror::SUCCEEDED(hr) {
            Ok(unsafe { ComPtr::from_raw(dsv) })
        } else {
            Err(image::ViewError::Unsupported)
        }
    }

    pub(crate) fn create_swapchain_impl(
        &self,
        config: &window::SwapchainConfig,
        window_handle: HWND,
        factory: ComPtr<IDXGIFactory>,
    ) -> Result<(ComPtr<IDXGISwapChain>, dxgiformat::DXGI_FORMAT), window::CreationError> {
        // TODO: use IDXGIFactory2 for >=11.1
        // TODO: this function should be able to fail (Result)?

        debug!("{:#?}", config);
        let non_srgb_format = conv::map_format_nosrgb(config.format).unwrap();

        let mut desc = DXGI_SWAP_CHAIN_DESC {
            BufferDesc: dxgitype::DXGI_MODE_DESC {
                Width: config.extent.width,
                Height: config.extent.height,
                // TODO: should this grab max value of all monitor hz? vsync
                //       will clamp to current monitor anyways?
                RefreshRate: dxgitype::DXGI_RATIONAL {
                    Numerator: 1,
                    Denominator: 60,
                },
                Format: non_srgb_format,
                ScanlineOrdering: dxgitype::DXGI_MODE_SCANLINE_ORDER_UNSPECIFIED,
                Scaling: dxgitype::DXGI_MODE_SCALING_UNSPECIFIED,
            },
            // TODO: msaa on backbuffer?
            SampleDesc: dxgitype::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: dxgitype::DXGI_USAGE_RENDER_TARGET_OUTPUT
                | dxgitype::DXGI_USAGE_SHADER_INPUT,
            BufferCount: config.image_count,
            OutputWindow: window_handle,
            // TODO:
            Windowed: TRUE,
            // TODO:
            SwapEffect: DXGI_SWAP_EFFECT_DISCARD,
            Flags: 0,
        };

        let dxgi_swapchain = {
            let mut swapchain: *mut IDXGISwapChain = ptr::null_mut();
            let hr = unsafe {
                factory.CreateSwapChain(
                    self.raw.as_raw() as *mut _,
                    &mut desc as *mut _,
                    &mut swapchain as *mut *mut _ as *mut *mut _,
                )
            };
            assert_eq!(hr, winerror::S_OK);

            unsafe { ComPtr::from_raw(swapchain) }
        };
        Ok((dxgi_swapchain, non_srgb_format))
    }
}

impl device::Device<Backend> for Device {
    unsafe fn allocate_memory(
        &self,
        mem_type: hal::MemoryTypeId,
        size: u64,
    ) -> Result<Memory, device::AllocationError> {
        let vec = Vec::with_capacity(size as usize);
        Ok(Memory {
            ty: self.memory_heap_flags[mem_type.0],
            properties: self.memory_properties.memory_types[mem_type.0].properties,
            size,
            mapped_ptr: vec.as_ptr() as *mut _,
            host_visible: Some(RefCell::new(vec)),
            local_buffers: RefCell::new(Vec::new()),
            local_images: RefCell::new(Vec::new()),
        })
    }

    unsafe fn create_command_pool(
        &self,
        _family: QueueFamilyId,
        _create_flags: pool::CommandPoolCreateFlags,
    ) -> Result<CommandPool, device::OutOfMemory> {
        // TODO:
        Ok(CommandPool {
            device: self.raw.clone(),
            internal: self.internal.clone(),
        })
    }

    unsafe fn destroy_command_pool(&self, _pool: CommandPool) {
        // automatic
    }

    unsafe fn create_render_pass<'a, IA, IS, ID>(
        &self,
        attachments: IA,
        subpasses: IS,
        _dependencies: ID,
    ) -> Result<RenderPass, device::OutOfMemory>
    where
        IA: IntoIterator,
        IA::Item: Borrow<pass::Attachment>,
        IS: IntoIterator,
        IS::Item: Borrow<pass::SubpassDesc<'a>>,
        ID: IntoIterator,
        ID::Item: Borrow<pass::SubpassDependency>,
    {
        Ok(RenderPass {
            attachments: attachments
                .into_iter()
                .map(|attachment| attachment.borrow().clone())
                .collect(),
            subpasses: subpasses
                .into_iter()
                .map(|desc| {
                    let desc = desc.borrow();
                    SubpassDesc {
                        color_attachments: desc
                            .colors
                            .iter()
                            .map(|color| color.borrow().clone())
                            .collect(),
                        depth_stencil_attachment: desc.depth_stencil.map(|d| *d),
                        input_attachments: desc
                            .inputs
                            .iter()
                            .map(|input| input.borrow().clone())
                            .collect(),
                        resolve_attachments: desc
                            .resolves
                            .iter()
                            .map(|resolve| resolve.borrow().clone())
                            .collect(),
                    }
                })
                .collect(),
        })
    }

    unsafe fn create_pipeline_layout<IS, IR>(
        &self,
        set_layouts: IS,
        _push_constant_ranges: IR,
    ) -> Result<PipelineLayout, device::OutOfMemory>
    where
        IS: IntoIterator,
        IS::Item: Borrow<DescriptorSetLayout>,
        IR: IntoIterator,
        IR::Item: Borrow<(pso::ShaderStageFlags, Range<u32>)>,
    {
        use pso::DescriptorType::*;

        let mut set_bindings = Vec::new();
        let mut set_remapping = Vec::new();

        // since we remapped the bindings in our descriptor set layouts to their own local space
        // (starting from register 0), we need to combine all the registers when creating our
        // pipeline layout. we do this by simply offsetting all the registers by the amount of
        // registers in the previous descriptor set layout
        let mut s_offset = 0;
        let mut t_offset = 0;
        let mut c_offset = 0;
        let mut u_offset = 0;

        fn get_descriptor_offset(ty: pso::DescriptorType, s: u32, t: u32, c: u32, u: u32) -> u32 {
            match ty {
                Sampler => s,
                SampledImage | UniformTexelBuffer => t,
                UniformBuffer | UniformBufferDynamic => c,
                StorageTexelBuffer | StorageBuffer | InputAttachment | StorageBufferDynamic
                | StorageImage => u,
                CombinedImageSampler => unreachable!(),
            }
        }

        for layout in set_layouts {
            let layout = layout.borrow();

            let bindings = &layout.bindings;

            let stages = [
                pso::ShaderStageFlags::VERTEX,
                pso::ShaderStageFlags::HULL,
                pso::ShaderStageFlags::DOMAIN,
                pso::ShaderStageFlags::GEOMETRY,
                pso::ShaderStageFlags::FRAGMENT,
                pso::ShaderStageFlags::COMPUTE,
            ];

            let mut optimized_bindings = Vec::new();

            // for every shader stage we get a range of descriptor handles that can be bound with
            // PS/VS/CSSetXX()
            for &stage in &stages {
                let mut state = None;

                for binding in bindings {
                    if !binding.stage.contains(stage) {
                        continue;
                    }

                    state = match state {
                        None => {
                            if binding.stage.contains(stage) {
                                let offset = binding.handle_offset;

                                Some((
                                    binding.ty,
                                    binding.binding_range.start,
                                    binding.binding_range.end,
                                    offset,
                                    offset,
                                ))
                            } else {
                                None
                            }
                        }
                        Some((
                            mut ty,
                            mut start,
                            mut end,
                            mut start_offset,
                            mut current_offset,
                        )) => {
                            // if we encounter another type or the binding/handle
                            // range is broken, push our current descriptor range
                            // and begin a new one.
                            if ty != binding.ty
                                || end != binding.binding_range.start
                                || current_offset + 1 != binding.handle_offset
                            {
                                let register_offset = get_descriptor_offset(
                                    ty, s_offset, t_offset, c_offset, u_offset,
                                );

                                optimized_bindings.push(PipelineBinding {
                                    stage,
                                    ty,
                                    binding_range: (register_offset + start)
                                        .. (register_offset + end),
                                    handle_offset: start_offset,
                                });

                                if binding.stage.contains(stage) {
                                    ty = binding.ty;
                                    start = binding.binding_range.start;
                                    end = binding.binding_range.end;

                                    start_offset = binding.handle_offset;
                                    current_offset = binding.handle_offset;

                                    Some((ty, start, end, start_offset, current_offset))
                                } else {
                                    None
                                }
                            } else {
                                end += 1;
                                current_offset += 1;

                                Some((ty, start, end, start_offset, current_offset))
                            }
                        }
                    }
                }

                // catch trailing descriptors
                if let Some((ty, start, end, start_offset, _)) = state {
                    let register_offset =
                        get_descriptor_offset(ty, s_offset, t_offset, c_offset, u_offset);

                    optimized_bindings.push(PipelineBinding {
                        stage,
                        ty,
                        binding_range: (register_offset + start) .. (register_offset + end),
                        handle_offset: start_offset,
                    });
                }
            }

            let offset_mappings = layout
                .register_remap
                .mapping
                .iter()
                .map(|register| {
                    let register_offset =
                        get_descriptor_offset(register.ty, s_offset, t_offset, c_offset, u_offset);

                    RegisterMapping {
                        ty: register.ty,
                        spirv_binding: register.spirv_binding,
                        hlsl_register: register.hlsl_register + register_offset as u8,
                        combined: register.combined,
                    }
                })
                .collect();

            set_bindings.push(optimized_bindings);
            set_remapping.push(RegisterRemapping {
                mapping: offset_mappings,
                num_s: layout.register_remap.num_s,
                num_t: layout.register_remap.num_t,
                num_c: layout.register_remap.num_c,
                num_u: layout.register_remap.num_u,
            });

            s_offset += layout.register_remap.num_s as u32;
            t_offset += layout.register_remap.num_t as u32;
            c_offset += layout.register_remap.num_c as u32;
            u_offset += layout.register_remap.num_u as u32;
        }

        Ok(PipelineLayout {
            set_bindings,
            set_remapping,
        })
    }

    unsafe fn create_pipeline_cache(
        &self,
        _data: Option<&[u8]>,
    ) -> Result<(), device::OutOfMemory> {
        Ok(())
    }

    unsafe fn get_pipeline_cache_data(&self, cache: &()) -> Result<Vec<u8>, device::OutOfMemory> {
        //empty
        Ok(Vec::new())
    }

    unsafe fn destroy_pipeline_cache(&self, _: ()) {
        //empty
    }

    unsafe fn merge_pipeline_caches<I>(&self, _: &(), _: I) -> Result<(), device::OutOfMemory>
    where
        I: IntoIterator,
        I::Item: Borrow<()>,
    {
        //empty
        Ok(())
    }

    unsafe fn create_graphics_pipeline<'a>(
        &self,
        desc: &pso::GraphicsPipelineDesc<'a, Backend>,
        _cache: Option<&()>,
    ) -> Result<GraphicsPipeline, pso::CreationError> {
        let build_shader = |stage: pso::Stage, source: Option<&pso::EntryPoint<'a, Backend>>| {
            let source = match source {
                Some(src) => src,
                None => return Ok(None),
            };

            Self::extract_entry_point(stage, source, desc.layout)
                .map_err(|err| pso::CreationError::Shader(err))
        };

        let vs = build_shader(pso::Stage::Vertex, Some(&desc.shaders.vertex))?.unwrap();
        let ps = build_shader(pso::Stage::Fragment, desc.shaders.fragment.as_ref())?;
        let gs = build_shader(pso::Stage::Geometry, desc.shaders.geometry.as_ref())?;
        let ds = build_shader(pso::Stage::Domain, desc.shaders.domain.as_ref())?;
        let hs = build_shader(pso::Stage::Hull, desc.shaders.hull.as_ref())?;

        let layout = self.create_input_layout(
            vs.clone(),
            &desc.vertex_buffers,
            &desc.attributes,
            &desc.input_assembler,
        )?;
        let rasterizer_state = self.create_rasterizer_state(&desc.rasterizer)?;
        let blend_state = self.create_blend_state(&desc.blender)?;
        let depth_stencil_state = Some(self.create_depth_stencil_state(&desc.depth_stencil)?);

        let vs = self.create_vertex_shader(vs)?;
        let ps = if let Some(blob) = ps {
            Some(self.create_pixel_shader(blob)?)
        } else {
            None
        };
        let gs = if let Some(blob) = gs {
            Some(self.create_geometry_shader(blob)?)
        } else {
            None
        };
        let ds = if let Some(blob) = ds {
            Some(self.create_domain_shader(blob)?)
        } else {
            None
        };
        let hs = if let Some(blob) = hs {
            Some(self.create_hull_shader(blob)?)
        } else {
            None
        };

        Ok(GraphicsPipeline {
            vs,
            gs,
            ds,
            hs,
            ps,
            topology: layout.topology,
            input_layout: layout.raw,
            rasterizer_state,
            blend_state,
            depth_stencil_state,
            baked_states: desc.baked_states.clone(),
            required_bindings: layout.required_bindings,
            max_vertex_bindings: layout.max_vertex_bindings,
            strides: layout.vertex_strides,
        })
    }

    unsafe fn create_compute_pipeline<'a>(
        &self,
        desc: &pso::ComputePipelineDesc<'a, Backend>,
        _cache: Option<&()>,
    ) -> Result<ComputePipeline, pso::CreationError> {
        let build_shader = |stage: pso::Stage, source: Option<&pso::EntryPoint<'a, Backend>>| {
            let source = match source {
                Some(src) => src,
                None => return Ok(None),
            };

            Self::extract_entry_point(stage, source, desc.layout)
                .map_err(|err| pso::CreationError::Shader(err))
        };

        let cs = build_shader(pso::Stage::Compute, Some(&desc.shader))?.unwrap();
        let cs = self.create_compute_shader(cs)?;

        Ok(ComputePipeline { cs })
    }

    unsafe fn create_framebuffer<I>(
        &self,
        _renderpass: &RenderPass,
        attachments: I,
        extent: image::Extent,
    ) -> Result<Framebuffer, device::OutOfMemory>
    where
        I: IntoIterator,
        I::Item: Borrow<ImageView>,
    {
        Ok(Framebuffer {
            attachments: attachments
                .into_iter()
                .map(|att| att.borrow().clone())
                .collect(),
            layers: extent.depth as _,
        })
    }

    unsafe fn create_shader_module(
        &self,
        raw_data: &[u32],
    ) -> Result<ShaderModule, device::ShaderError> {
        Ok(ShaderModule::Spirv(raw_data.into()))
    }

    unsafe fn create_buffer(
        &self,
        size: u64,
        usage: buffer::Usage,
    ) -> Result<Buffer, buffer::CreationError> {
        use buffer::Usage;

        let mut bind = 0;

        if usage.contains(Usage::UNIFORM) {
            bind |= d3d11::D3D11_BIND_CONSTANT_BUFFER;
        }
        if usage.contains(Usage::VERTEX) {
            bind |= d3d11::D3D11_BIND_VERTEX_BUFFER;
        }
        if usage.contains(Usage::INDEX) {
            bind |= d3d11::D3D11_BIND_INDEX_BUFFER;
        }

        // TODO: >=11.1
        if usage.intersects(Usage::UNIFORM_TEXEL | Usage::STORAGE_TEXEL | Usage::TRANSFER_SRC) {
            bind |= d3d11::D3D11_BIND_SHADER_RESOURCE;
        }

        if usage.intersects(Usage::TRANSFER_DST | Usage::STORAGE) {
            bind |= d3d11::D3D11_BIND_UNORDERED_ACCESS;
        }

        // if `D3D11_BIND_CONSTANT_BUFFER` intersects with any other bind flag, we need to handle
        // it by creating two buffers. one with `D3D11_BIND_CONSTANT_BUFFER` and one with the rest
        let needs_disjoint_cb = bind & d3d11::D3D11_BIND_CONSTANT_BUFFER != 0
            && bind != d3d11::D3D11_BIND_CONSTANT_BUFFER;

        if needs_disjoint_cb {
            bind ^= d3d11::D3D11_BIND_CONSTANT_BUFFER;
        }

        fn up_align(x: u64, alignment: u64) -> u64 {
            (x + alignment - 1) & !(alignment - 1)
        }

        // constant buffer size need to be divisible by 16
        let size = if usage.contains(Usage::UNIFORM) {
            up_align(size, 16)
        } else {
            up_align(size, 4)
        };

        Ok(Buffer {
            internal: InternalBuffer {
                raw: ptr::null_mut(),
                disjoint_cb: if needs_disjoint_cb {
                    Some(ptr::null_mut())
                } else {
                    None
                },
                srv: None,
                uav: None,
                usage,
            },
            ty: MemoryHeapFlags::empty(),
            bound_range: 0 .. 0,
            host_ptr: ptr::null_mut(),
            bind,
            requirements: memory::Requirements {
                size,
                alignment: 1,
                type_mask: MemoryHeapFlags::all().bits(),
            },
        })
    }

    unsafe fn get_buffer_requirements(&self, buffer: &Buffer) -> memory::Requirements {
        buffer.requirements
    }

    unsafe fn bind_buffer_memory(
        &self,
        memory: &Memory,
        offset: u64,
        buffer: &mut Buffer,
    ) -> Result<(), device::BindError> {
        debug!(
            "usage={:?}, props={:b}",
            buffer.internal.usage, memory.properties
        );

        #[allow(non_snake_case)]
        let MiscFlags = if buffer.bind
            & (d3d11::D3D11_BIND_SHADER_RESOURCE | d3d11::D3D11_BIND_UNORDERED_ACCESS)
            != 0
        {
            d3d11::D3D11_RESOURCE_MISC_BUFFER_ALLOW_RAW_VIEWS
        } else {
            0
        };

        let initial_data = memory
            .host_visible
            .as_ref()
            .map(|p| d3d11::D3D11_SUBRESOURCE_DATA {
                pSysMem: p.borrow().as_ptr().offset(offset as isize) as _,
                SysMemPitch: 0,
                SysMemSlicePitch: 0,
            });

        let raw = match memory.ty {
            MemoryHeapFlags::DEVICE_LOCAL => {
                // device local memory
                let desc = d3d11::D3D11_BUFFER_DESC {
                    ByteWidth: buffer.requirements.size as _,
                    Usage: d3d11::D3D11_USAGE_DEFAULT,
                    BindFlags: buffer.bind,
                    CPUAccessFlags: 0,
                    MiscFlags,
                    StructureByteStride: if buffer
                        .internal
                        .usage
                        .contains(buffer::Usage::TRANSFER_SRC)
                    {
                        4
                    } else {
                        0
                    },
                };

                let mut buffer: *mut d3d11::ID3D11Buffer = ptr::null_mut();
                let hr = self.raw.CreateBuffer(
                    &desc,
                    if let Some(data) = initial_data {
                        &data
                    } else {
                        ptr::null_mut()
                    },
                    &mut buffer as *mut *mut _ as *mut *mut _,
                );

                if !winerror::SUCCEEDED(hr) {
                    return Err(device::BindError::WrongMemory);
                }

                ComPtr::from_raw(buffer)
            }
            MemoryHeapFlags::HOST_VISIBLE | MemoryHeapFlags::HOST_COHERENT => {
                let desc = d3d11::D3D11_BUFFER_DESC {
                    ByteWidth: buffer.requirements.size as _,
                    // TODO: dynamic?
                    Usage: d3d11::D3D11_USAGE_DEFAULT,
                    BindFlags: buffer.bind,
                    CPUAccessFlags: 0,
                    MiscFlags,
                    StructureByteStride: if buffer
                        .internal
                        .usage
                        .contains(buffer::Usage::TRANSFER_SRC)
                    {
                        4
                    } else {
                        0
                    },
                };

                let mut buffer: *mut d3d11::ID3D11Buffer = ptr::null_mut();
                let hr = self.raw.CreateBuffer(
                    &desc,
                    if let Some(data) = initial_data {
                        &data
                    } else {
                        ptr::null_mut()
                    },
                    &mut buffer as *mut *mut _ as *mut *mut _,
                );

                if !winerror::SUCCEEDED(hr) {
                    return Err(device::BindError::WrongMemory);
                }

                ComPtr::from_raw(buffer)
            }
            _ => unimplemented!(),
        };

        let disjoint_cb = if buffer.internal.disjoint_cb.is_some() {
            let desc = d3d11::D3D11_BUFFER_DESC {
                ByteWidth: buffer.requirements.size as _,
                Usage: d3d11::D3D11_USAGE_DEFAULT,
                BindFlags: d3d11::D3D11_BIND_CONSTANT_BUFFER,
                CPUAccessFlags: 0,
                MiscFlags: 0,
                StructureByteStride: 0,
            };

            let mut buffer: *mut d3d11::ID3D11Buffer = ptr::null_mut();
            let hr = self.raw.CreateBuffer(
                &desc,
                if let Some(data) = initial_data {
                    &data
                } else {
                    ptr::null_mut()
                },
                &mut buffer as *mut *mut _ as *mut *mut _,
            );

            if !winerror::SUCCEEDED(hr) {
                return Err(device::BindError::WrongMemory);
            }

            Some(buffer)
        } else {
            None
        };

        let srv = if buffer.bind & d3d11::D3D11_BIND_SHADER_RESOURCE != 0 {
            let mut desc = mem::zeroed::<d3d11::D3D11_SHADER_RESOURCE_VIEW_DESC>();
            desc.Format = dxgiformat::DXGI_FORMAT_R32_TYPELESS;
            desc.ViewDimension = d3dcommon::D3D11_SRV_DIMENSION_BUFFEREX;
            *desc.u.BufferEx_mut() = d3d11::D3D11_BUFFEREX_SRV {
                FirstElement: 0,
                // TODO: enforce alignment through HAL limits
                NumElements: buffer.requirements.size as u32 / 4,
                Flags: d3d11::D3D11_BUFFEREX_SRV_FLAG_RAW,
            };

            let mut srv = ptr::null_mut();
            let hr = self.raw.CreateShaderResourceView(
                raw.as_raw() as *mut _,
                &desc,
                &mut srv as *mut *mut _ as *mut *mut _,
            );

            if !winerror::SUCCEEDED(hr) {
                error!("CreateShaderResourceView failed: 0x{:x}", hr);

                return Err(device::BindError::WrongMemory);
            }

            Some(srv)
        } else {
            None
        };

        let uav = if buffer.bind & d3d11::D3D11_BIND_UNORDERED_ACCESS != 0 {
            let mut desc = mem::zeroed::<d3d11::D3D11_UNORDERED_ACCESS_VIEW_DESC>();
            desc.Format = dxgiformat::DXGI_FORMAT_R32_TYPELESS;
            desc.ViewDimension = d3d11::D3D11_UAV_DIMENSION_BUFFER;
            *desc.u.Buffer_mut() = d3d11::D3D11_BUFFER_UAV {
                FirstElement: 0,
                NumElements: buffer.requirements.size as u32 / 4,
                Flags: d3d11::D3D11_BUFFER_UAV_FLAG_RAW,
            };

            let mut uav = ptr::null_mut();
            let hr = self.raw.CreateUnorderedAccessView(
                raw.as_raw() as *mut _,
                &desc,
                &mut uav as *mut *mut _ as *mut *mut _,
            );

            if !winerror::SUCCEEDED(hr) {
                error!("CreateUnorderedAccessView failed: 0x{:x}", hr);

                return Err(device::BindError::WrongMemory);
            }

            Some(uav)
        } else {
            None
        };

        let internal = InternalBuffer {
            raw: raw.into_raw(),
            disjoint_cb,
            srv,
            uav,
            usage: buffer.internal.usage,
        };
        let range = offset .. buffer.requirements.size;

        memory.bind_buffer(range.clone(), internal.clone());

        let host_ptr = if let Some(vec) = &memory.host_visible {
            vec.borrow().as_ptr() as *mut _
        } else {
            ptr::null_mut()
        };

        buffer.internal = internal;
        buffer.ty = memory.ty;
        buffer.host_ptr = host_ptr;
        buffer.bound_range = range;

        Ok(())
    }

    unsafe fn create_buffer_view<R: RangeArg<u64>>(
        &self,
        _buffer: &Buffer,
        _format: Option<format::Format>,
        _range: R,
    ) -> Result<BufferView, buffer::ViewCreationError> {
        unimplemented!()
    }

    unsafe fn create_image(
        &self,
        kind: image::Kind,
        mip_levels: image::Level,
        format: format::Format,
        tiling: image::Tiling,
        usage: image::Usage,
        view_caps: image::ViewCapabilities,
    ) -> Result<Image, image::CreationError> {
        use image::Usage;
        //
        // TODO: create desc

        let surface_desc = format.base_format().0.desc();
        let bytes_per_texel = surface_desc.bits / 8;
        let ext = kind.extent();
        let size = (ext.width * ext.height * ext.depth) as u64 * bytes_per_texel as u64;
        let compressed = surface_desc.is_compressed();
        let depth = format.is_depth();

        let mut bind = 0;

        if usage.intersects(Usage::TRANSFER_SRC | Usage::SAMPLED | Usage::STORAGE) {
            bind |= d3d11::D3D11_BIND_SHADER_RESOURCE;
        }

        // we cant get RTVs or UAVs on compressed & depth formats
        if !compressed && !depth {
            if usage.intersects(Usage::COLOR_ATTACHMENT | Usage::TRANSFER_DST) {
                bind |= d3d11::D3D11_BIND_RENDER_TARGET;
            }

            if usage.intersects(Usage::TRANSFER_DST | Usage::STORAGE) {
                bind |= d3d11::D3D11_BIND_UNORDERED_ACCESS;
            }
        }

        if usage.contains(Usage::DEPTH_STENCIL_ATTACHMENT) {
            bind |= d3d11::D3D11_BIND_DEPTH_STENCIL;
        }

        debug!("{:b}", bind);

        Ok(Image {
            internal: InternalImage {
                raw: ptr::null_mut(),
                copy_srv: None,
                srv: None,
                unordered_access_views: Vec::new(),
                depth_stencil_views: Vec::new(),
                render_target_views: Vec::new(),
            },
            decomposed_format: conv::DecomposedDxgiFormat::UNKNOWN,
            kind,
            mip_levels,
            format,
            usage,
            tiling,
            view_caps,
            bind,
            requirements: memory::Requirements {
                size: size,
                alignment: 1,
                type_mask: MemoryHeapFlags::DEVICE_LOCAL.bits(),
            },
        })
    }

    unsafe fn get_image_requirements(&self, image: &Image) -> memory::Requirements {
        image.requirements
    }

    unsafe fn get_image_subresource_footprint(
        &self,
        _image: &Image,
        _sub: image::Subresource,
    ) -> image::SubresourceFootprint {
        unimplemented!()
    }

    unsafe fn bind_image_memory(
        &self,
        memory: &Memory,
        offset: u64,
        image: &mut Image,
    ) -> Result<(), device::BindError> {
        use image::Usage;
        use memory::Properties;

        let base_format = image.format.base_format();
        let format_desc = base_format.0.desc();

        let compressed = format_desc.is_compressed();
        let depth = image.format.is_depth();
        let stencil = image.format.is_stencil();

        let (bind, usage, cpu) = if memory.properties == Properties::DEVICE_LOCAL {
            (image.bind, d3d11::D3D11_USAGE_DEFAULT, 0)
        } else if memory.properties
            == (Properties::DEVICE_LOCAL | Properties::CPU_VISIBLE | Properties::CPU_CACHED)
        {
            (
                image.bind,
                d3d11::D3D11_USAGE_DYNAMIC,
                d3d11::D3D11_CPU_ACCESS_WRITE,
            )
        } else if memory.properties == (Properties::CPU_VISIBLE | Properties::CPU_CACHED) {
            (
                0,
                d3d11::D3D11_USAGE_STAGING,
                d3d11::D3D11_CPU_ACCESS_READ | d3d11::D3D11_CPU_ACCESS_WRITE,
            )
        } else {
            unimplemented!()
        };

        let dxgi_format = conv::map_format(image.format).unwrap();
        let decomposed = conv::DecomposedDxgiFormat::from_dxgi_format(dxgi_format);
        let bpp = format_desc.bits as u32 / 8;

        let (view_kind, resource) = match image.kind {
            image::Kind::D1(width, layers) => {
                let initial_data =
                    memory
                        .host_visible
                        .as_ref()
                        .map(|_p| d3d11::D3D11_SUBRESOURCE_DATA {
                            pSysMem: memory.mapped_ptr.offset(offset as isize) as _,
                            SysMemPitch: 0,
                            SysMemSlicePitch: 0,
                        });

                let desc = d3d11::D3D11_TEXTURE1D_DESC {
                    Width: width,
                    MipLevels: image.mip_levels as _,
                    ArraySize: layers as _,
                    Format: decomposed.typeless,
                    Usage: usage,
                    BindFlags: bind,
                    CPUAccessFlags: cpu,
                    MiscFlags: 0,
                };

                let mut resource = ptr::null_mut();
                let hr = self.raw.CreateTexture1D(
                    &desc,
                    if let Some(data) = initial_data {
                        &data
                    } else {
                        ptr::null_mut()
                    },
                    &mut resource as *mut *mut _ as *mut *mut _,
                );

                if !winerror::SUCCEEDED(hr) {
                    error!("CreateTexture1D failed: 0x{:x}", hr);

                    return Err(device::BindError::WrongMemory);
                }

                (image::ViewKind::D1Array, resource)
            }
            image::Kind::D2(width, height, layers, _) => {
                let mut initial_datas = Vec::new();

                for _layer in 0 .. layers {
                    for level in 0 .. image.mip_levels {
                        let width = image.kind.extent().at_level(level).width;

                        // TODO: layer offset?
                        initial_datas.push(d3d11::D3D11_SUBRESOURCE_DATA {
                            pSysMem: memory.mapped_ptr.offset(offset as isize) as _,
                            SysMemPitch: width * bpp,
                            SysMemSlicePitch: 0,
                        });
                    }
                }

                let desc = d3d11::D3D11_TEXTURE2D_DESC {
                    Width: width,
                    Height: height,
                    MipLevels: image.mip_levels as _,
                    ArraySize: layers as _,
                    Format: decomposed.typeless,
                    SampleDesc: dxgitype::DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: usage,
                    BindFlags: bind,
                    CPUAccessFlags: cpu,
                    MiscFlags: if image.view_caps.contains(image::ViewCapabilities::KIND_CUBE) {
                        d3d11::D3D11_RESOURCE_MISC_TEXTURECUBE
                    } else {
                        0
                    },
                };

                let mut resource = ptr::null_mut();
                let hr = self.raw.CreateTexture2D(
                    &desc,
                    if !depth {
                        initial_datas.as_ptr()
                    } else {
                        ptr::null_mut()
                    },
                    &mut resource as *mut *mut _ as *mut *mut _,
                );

                if !winerror::SUCCEEDED(hr) {
                    error!("CreateTexture2D failed: 0x{:x}", hr);

                    return Err(device::BindError::WrongMemory);
                }

                (image::ViewKind::D2Array, resource)
            }
            image::Kind::D3(width, height, depth) => {
                let initial_data =
                    memory
                        .host_visible
                        .as_ref()
                        .map(|_p| d3d11::D3D11_SUBRESOURCE_DATA {
                            pSysMem: memory.mapped_ptr.offset(offset as isize) as _,
                            SysMemPitch: width * bpp,
                            SysMemSlicePitch: width * height * bpp,
                        });

                let desc = d3d11::D3D11_TEXTURE3D_DESC {
                    Width: width,
                    Height: height,
                    Depth: depth,
                    MipLevels: image.mip_levels as _,
                    Format: decomposed.typeless,
                    Usage: usage,
                    BindFlags: bind,
                    CPUAccessFlags: cpu,
                    MiscFlags: 0,
                };

                let mut resource = ptr::null_mut();
                let hr = self.raw.CreateTexture3D(
                    &desc,
                    if let Some(data) = initial_data {
                        &data
                    } else {
                        ptr::null_mut()
                    },
                    &mut resource as *mut *mut _ as *mut *mut _,
                );

                if !winerror::SUCCEEDED(hr) {
                    error!("CreateTexture3D failed: 0x{:x}", hr);

                    return Err(device::BindError::WrongMemory);
                }

                (image::ViewKind::D3, resource)
            }
        };

        let mut unordered_access_views = Vec::new();

        if image.usage.contains(Usage::TRANSFER_DST) && !compressed && !depth {
            for mip in 0 .. image.mip_levels {
                let view = ViewInfo {
                    resource: resource,
                    kind: image.kind,
                    caps: image::ViewCapabilities::empty(),
                    view_kind,
                    // TODO: we should be using `uav_format` rather than `copy_uav_format`, and share
                    //       the UAVs when the formats are identical
                    format: decomposed.copy_uav.unwrap(),
                    range: image::SubresourceRange {
                        aspects: format::Aspects::COLOR,
                        levels: mip .. (mip + 1),
                        layers: 0 .. image.kind.num_layers(),
                    },
                };

                unordered_access_views.push(
                    self.view_image_as_unordered_access(&view)
                        .map_err(|_| device::BindError::WrongMemory)?,
                );
            }
        }

        let (copy_srv, srv) = if image.usage.contains(image::Usage::TRANSFER_SRC) {
            let mut view = ViewInfo {
                resource: resource,
                kind: image.kind,
                caps: image::ViewCapabilities::empty(),
                view_kind,
                format: decomposed.copy_srv.unwrap(),
                range: image::SubresourceRange {
                    aspects: format::Aspects::COLOR,
                    levels: 0 .. image.mip_levels,
                    layers: 0 .. image.kind.num_layers(),
                },
            };

            let copy_srv = if !compressed {
                Some(
                    self.view_image_as_shader_resource(&view)
                        .map_err(|_| device::BindError::WrongMemory)?,
                )
            } else {
                None
            };

            view.format = decomposed.srv.unwrap();

            let srv = if !depth && !stencil {
                Some(
                    self.view_image_as_shader_resource(&view)
                        .map_err(|_| device::BindError::WrongMemory)?,
                )
            } else {
                None
            };

            (copy_srv, srv)
        } else {
            (None, None)
        };

        let mut render_target_views = Vec::new();

        if (image.usage.contains(image::Usage::COLOR_ATTACHMENT)
            || image.usage.contains(image::Usage::TRANSFER_DST))
            && !compressed
            && !depth
        {
            for layer in 0 .. image.kind.num_layers() {
                for mip in 0 .. image.mip_levels {
                    let view = ViewInfo {
                        resource: resource,
                        kind: image.kind,
                        caps: image::ViewCapabilities::empty(),
                        view_kind,
                        format: decomposed.rtv.unwrap(),
                        range: image::SubresourceRange {
                            aspects: format::Aspects::COLOR,
                            levels: mip .. (mip + 1),
                            layers: layer .. (layer + 1),
                        },
                    };

                    render_target_views.push(
                        self.view_image_as_render_target(&view)
                            .map_err(|_| device::BindError::WrongMemory)?,
                    );
                }
            }
        };

        let mut depth_stencil_views = Vec::new();

        if depth {
            for layer in 0 .. image.kind.num_layers() {
                for mip in 0 .. image.mip_levels {
                    let view = ViewInfo {
                        resource: resource,
                        kind: image.kind,
                        caps: image::ViewCapabilities::empty(),
                        view_kind,
                        format: decomposed.dsv.unwrap(),
                        range: image::SubresourceRange {
                            aspects: format::Aspects::COLOR,
                            levels: mip .. (mip + 1),
                            layers: layer .. (layer + 1),
                        },
                    };

                    depth_stencil_views.push(
                        self.view_image_as_depth_stencil(&view)
                            .map_err(|_| device::BindError::WrongMemory)?,
                    );
                }
            }
        }

        let internal = InternalImage {
            raw: resource,
            copy_srv,
            srv,
            unordered_access_views,
            depth_stencil_views,
            render_target_views,
        };

        image.decomposed_format = decomposed;
        image.internal = internal;

        Ok(())
    }

    unsafe fn create_image_view(
        &self,
        image: &Image,
        view_kind: image::ViewKind,
        format: format::Format,
        _swizzle: format::Swizzle,
        range: image::SubresourceRange,
    ) -> Result<ImageView, image::ViewError> {
        let is_array = image.kind.num_layers() > 1;

        let info = ViewInfo {
            resource: image.internal.raw,
            kind: image.kind,
            caps: image.view_caps,
            // D3D11 doesn't allow looking at a single slice of an array as a non-array
            view_kind: if is_array && view_kind == image::ViewKind::D2 {
                image::ViewKind::D2Array
            } else if is_array && view_kind == image::ViewKind::D1 {
                image::ViewKind::D1Array
            } else {
                view_kind
            },
            format: conv::map_format(format).ok_or(image::ViewError::BadFormat(format))?,
            range,
        };

        let srv_info = ViewInfo {
            format: conv::viewable_format(info.format),
            ..info.clone()
        };

        Ok(ImageView {
            format,
            srv_handle: if image.usage.intersects(image::Usage::SAMPLED) {
                Some(self.view_image_as_shader_resource(&srv_info)?)
            } else {
                None
            },
            rtv_handle: if image.usage.contains(image::Usage::COLOR_ATTACHMENT) {
                Some(self.view_image_as_render_target(&info)?)
            } else {
                None
            },
            uav_handle: if image.usage.contains(image::Usage::STORAGE) {
                Some(self.view_image_as_unordered_access(&info)?)
            } else {
                None
            },
            dsv_handle: if image.usage.contains(image::Usage::DEPTH_STENCIL_ATTACHMENT) {
                Some(self.view_image_as_depth_stencil(&info)?)
            } else {
                None
            },
        })
    }

    unsafe fn create_sampler(
        &self,
        info: image::SamplerInfo,
    ) -> Result<Sampler, device::AllocationError> {
        assert!(info.normalized);

        let op = match info.comparison {
            Some(_) => d3d11::D3D11_FILTER_REDUCTION_TYPE_COMPARISON,
            None => d3d11::D3D11_FILTER_REDUCTION_TYPE_STANDARD,
        };

        let desc = d3d11::D3D11_SAMPLER_DESC {
            Filter: conv::map_filter(
                info.min_filter,
                info.mag_filter,
                info.mip_filter,
                op,
                info.anisotropic,
            ),
            AddressU: conv::map_wrapping(info.wrap_mode.0),
            AddressV: conv::map_wrapping(info.wrap_mode.1),
            AddressW: conv::map_wrapping(info.wrap_mode.2),
            MipLODBias: info.lod_bias.into(),
            MaxAnisotropy: match info.anisotropic {
                image::Anisotropic::Off => 0,
                image::Anisotropic::On(aniso) => aniso as _,
            },
            ComparisonFunc: info.comparison.map_or(0, |comp| conv::map_comparison(comp)),
            BorderColor: info.border.into(),
            MinLOD: info.lod_range.start.into(),
            MaxLOD: info.lod_range.end.into(),
        };

        let mut sampler = ptr::null_mut();
        let hr = self
            .raw
            .CreateSamplerState(&desc, &mut sampler as *mut *mut _ as *mut *mut _);

        assert_eq!(true, winerror::SUCCEEDED(hr));

        Ok(Sampler {
            sampler_handle: ComPtr::from_raw(sampler),
        })
    }

    // TODO: make use of `max_sets`
    unsafe fn create_descriptor_pool<I>(
        &self,
        _max_sets: usize,
        ranges: I,
        _flags: pso::DescriptorPoolCreateFlags,
    ) -> Result<DescriptorPool, device::OutOfMemory>
    where
        I: IntoIterator,
        I::Item: Borrow<pso::DescriptorRangeDesc>,
    {
        let count = ranges
            .into_iter()
            .map(|r| {
                let r = r.borrow();

                r.count
                    * match r.ty {
                        pso::DescriptorType::CombinedImageSampler => 2,
                        _ => 1,
                    }
            })
            .sum::<usize>();

        Ok(DescriptorPool::with_capacity(count))
    }

    unsafe fn create_descriptor_set_layout<I, J>(
        &self,
        layout_bindings: I,
        _immutable_samplers: J,
    ) -> Result<DescriptorSetLayout, device::OutOfMemory>
    where
        I: IntoIterator,
        I::Item: Borrow<pso::DescriptorSetLayoutBinding>,
        J: IntoIterator,
        J::Item: Borrow<Sampler>,
    {
        use pso::DescriptorType::*;

        let mut bindings = Vec::new();

        let mut mapping = Vec::new();
        let mut num_t = 0;
        let mut num_s = 0;
        let mut num_c = 0;
        let mut num_u = 0;

        // we check how many hlsl registers we should use
        for binding in layout_bindings {
            let binding = binding.borrow();

            let hlsl_reg = match binding.ty {
                Sampler => {
                    num_s += 1;
                    num_s
                }
                CombinedImageSampler => {
                    num_t += 1;
                    num_s += 1;
                    num_t
                }
                SampledImage | UniformTexelBuffer => {
                    num_t += 1;
                    num_t
                }
                UniformBuffer | UniformBufferDynamic => {
                    num_c += 1;
                    num_c
                }
                StorageTexelBuffer | StorageBuffer | InputAttachment | StorageBufferDynamic
                | StorageImage => {
                    num_u += 1;
                    num_u
                }
            } - 1;

            // we decompose combined image samplers into a separate sampler and image internally
            if binding.ty == pso::DescriptorType::CombinedImageSampler {
                // TODO: for now we have to make combined image samplers share registers since
                //       spirv-cross doesn't support setting the register of the sampler/texture
                //       pair to separate values (only one `DescriptorSet` decorator)
                let shared_reg = num_s.max(num_t);

                num_s = shared_reg;
                num_t = shared_reg;

                let sampler_reg = num_s - 1;
                let image_reg = num_t - 1;

                mapping.push(RegisterMapping {
                    ty: pso::DescriptorType::Sampler,
                    spirv_binding: binding.binding,
                    hlsl_register: sampler_reg as u8,
                    combined: true,
                });
                mapping.push(RegisterMapping {
                    ty: pso::DescriptorType::SampledImage,
                    spirv_binding: binding.binding,
                    hlsl_register: image_reg as u8,
                    combined: true,
                });

                bindings.push(PipelineBinding {
                    stage: binding.stage_flags,
                    ty: pso::DescriptorType::Sampler,
                    binding_range: sampler_reg .. (sampler_reg + 1),
                    handle_offset: 0,
                });
                bindings.push(PipelineBinding {
                    stage: binding.stage_flags,
                    ty: pso::DescriptorType::SampledImage,
                    binding_range: image_reg .. (image_reg + 1),
                    handle_offset: 0,
                });
            } else {
                mapping.push(RegisterMapping {
                    ty: binding.ty,
                    spirv_binding: binding.binding,
                    hlsl_register: hlsl_reg as u8,
                    combined: false,
                });

                bindings.push(PipelineBinding {
                    stage: binding.stage_flags,
                    ty: binding.ty,
                    binding_range: hlsl_reg .. (hlsl_reg + 1),
                    handle_offset: 0,
                });
            }
        }

        // we sort the internal descriptor's handle (the actual dx interface) by some categories to
        // make it easier to group api calls together
        bindings.sort_unstable_by(|a, b| {
            (b.ty as u32)
                .cmp(&(a.ty as u32))
                .then(a.binding_range.start.cmp(&b.binding_range.start))
                .then(a.stage.cmp(&b.stage))
        });

        // we assign the handle (interface ptr) offset according to what register type the
        // descriptor is. the final layout of the handles should look like this:
        //
        //       0..num_s     num_s..num_t  num_t..num_c  num_c..handle_len
        //   +----------+----------------+-------------+------------------+
        //   |          |                |             |                  |
        //   +----------+----------------+-------------+------------------+
        //   0                                                   handle_len
        //
        let mut s = 0;
        let mut t = 0;
        let mut c = 0;
        let mut u = 0;
        for mut binding in bindings.iter_mut() {
            match binding.ty {
                Sampler => {
                    binding.handle_offset = s;
                    s += 1;
                }
                SampledImage | UniformTexelBuffer => {
                    binding.handle_offset = num_s + t;
                    t += 1;
                }
                UniformBuffer | UniformBufferDynamic => {
                    binding.handle_offset = num_s + num_t + c;
                    c += 1;
                }
                StorageTexelBuffer | StorageBuffer | InputAttachment | StorageBufferDynamic
                | StorageImage => {
                    binding.handle_offset = num_s + num_t + num_c + u;
                    u += 1;
                }
                CombinedImageSampler => unreachable!(),
            }
        }

        Ok(DescriptorSetLayout {
            bindings,
            handle_count: num_s + num_t + num_c + num_u,
            register_remap: RegisterRemapping {
                mapping,
                num_s: num_s as _,
                num_t: num_t as _,
                num_c: num_c as _,
                num_u: num_u as _,
            },
        })
    }

    unsafe fn write_descriptor_sets<'a, I, J>(&self, write_iter: I)
    where
        I: IntoIterator<Item = pso::DescriptorSetWrite<'a, Backend, J>>,
        J: IntoIterator,
        J::Item: Borrow<pso::Descriptor<'a, Backend>>,
    {
        for write in write_iter {
            //println!("WriteDescriptorSets({:?})", write.set.handles);
            let target_binding = write.binding;
            let (ty, first_offset, second_offset) = write.set.get_handle_offset(target_binding);
            assert!((first_offset as usize) < write.set.len);
            assert!((second_offset as usize) < write.set.len);

            for descriptor in write.descriptors {
                let handle = write.set.handles.offset(first_offset as isize);
                let second_handle = write.set.handles.offset(second_offset as isize);

                //println!("  Write(offset={}, handle={:?}) <= {:?}", first_offset, handle, ty);

                match *descriptor.borrow() {
                    pso::Descriptor::Buffer(buffer, ref _range) => match ty {
                        pso::DescriptorType::UniformBuffer
                        | pso::DescriptorType::UniformBufferDynamic => {
                            if buffer.ty == MemoryHeapFlags::HOST_COHERENT {
                                let old_buffer = (*handle).0 as *mut _;

                                write.set.add_flush(old_buffer, buffer);
                            }

                            *handle = if let Some(buffer) = buffer.internal.disjoint_cb {
                                Descriptor(buffer as *mut _)
                            } else {
                                Descriptor(buffer.internal.raw as *mut _)
                            };
                        }
                        pso::DescriptorType::StorageBuffer => {
                            if buffer.ty == MemoryHeapFlags::HOST_COHERENT {
                                let old_buffer = (*handle).0 as *mut _;

                                write.set.add_flush(old_buffer, buffer);
                                write.set.add_invalidate(old_buffer, buffer);
                            }

                            *handle = Descriptor(buffer.internal.uav.unwrap() as *mut _);
                        }
                        _ => unreachable!(),
                    },
                    pso::Descriptor::Image(image, _layout) => match ty {
                        pso::DescriptorType::SampledImage => {
                            *handle =
                                Descriptor(image.srv_handle.clone().unwrap().as_raw() as *mut _);
                        }
                        pso::DescriptorType::StorageImage => {
                            *handle =
                                Descriptor(image.uav_handle.clone().unwrap().as_raw() as *mut _);
                        }
                        pso::DescriptorType::InputAttachment => {
                            *handle =
                                Descriptor(image.srv_handle.clone().unwrap().as_raw() as *mut _);
                        }
                        _ => unreachable!(),
                    },
                    pso::Descriptor::Sampler(sampler) => {
                        *handle = Descriptor(sampler.sampler_handle.as_raw() as *mut _);
                    }
                    pso::Descriptor::CombinedImageSampler(image, _layout, sampler) => {
                        *handle = Descriptor(sampler.sampler_handle.as_raw() as *mut _);
                        *second_handle =
                            Descriptor(image.srv_handle.clone().unwrap().as_raw() as *mut _);
                    }
                    pso::Descriptor::UniformTexelBuffer(_buffer_view) => {}
                    pso::Descriptor::StorageTexelBuffer(_buffer_view) => {}
                }
            }
        }
    }

    unsafe fn copy_descriptor_sets<'a, I>(&self, copy_iter: I)
    where
        I: IntoIterator,
        I::Item: Borrow<pso::DescriptorSetCopy<'a, Backend>>,
    {
        for copy in copy_iter {
            let copy = copy.borrow();

            for offset in 0 .. copy.count {
                let (dst_ty, dst_handle_offset, dst_second_handle_offset) = copy
                    .dst_set
                    .get_handle_offset(copy.dst_binding + offset as u32);
                let (src_ty, src_handle_offset, src_second_handle_offset) = copy
                    .src_set
                    .get_handle_offset(copy.src_binding + offset as u32);
                assert_eq!(dst_ty, src_ty);

                let dst_handle = copy.dst_set.handles.offset(dst_handle_offset as isize);
                let src_handle = copy.dst_set.handles.offset(src_handle_offset as isize);

                match dst_ty {
                    pso::DescriptorType::CombinedImageSampler => {
                        let dst_second_handle = copy
                            .dst_set
                            .handles
                            .offset(dst_second_handle_offset as isize);
                        let src_second_handle = copy
                            .dst_set
                            .handles
                            .offset(src_second_handle_offset as isize);

                        *dst_handle = *src_handle;
                        *dst_second_handle = *src_second_handle;
                    }
                    _ => *dst_handle = *src_handle,
                }
            }
        }
    }

    unsafe fn map_memory<R>(&self, memory: &Memory, range: R) -> Result<*mut u8, device::MapError>
    where
        R: RangeArg<u64>,
    {
        assert_eq!(memory.host_visible.is_some(), true);

        Ok(memory
            .mapped_ptr
            .offset(*range.start().unwrap_or(&0) as isize))
    }

    unsafe fn unmap_memory(&self, memory: &Memory) {
        assert_eq!(memory.host_visible.is_some(), true);
    }

    unsafe fn flush_mapped_memory_ranges<'a, I, R>(
        &self,
        ranges: I,
    ) -> Result<(), device::OutOfMemory>
    where
        I: IntoIterator,
        I::Item: Borrow<(&'a Memory, R)>,
        R: RangeArg<u64>,
    {
        let _scope = debug_scope!(&self.context, "FlushMappedRanges");

        // go through every range we wrote to
        for range in ranges.into_iter() {
            let &(memory, ref range) = range.borrow();
            let range = memory.resolve(range);

            let _scope = debug_scope!(&self.context, "Range({:?})", range);
            memory.flush(&self.context, range);
        }

        Ok(())
    }

    unsafe fn invalidate_mapped_memory_ranges<'a, I, R>(
        &self,
        ranges: I,
    ) -> Result<(), device::OutOfMemory>
    where
        I: IntoIterator,
        I::Item: Borrow<(&'a Memory, R)>,
        R: RangeArg<u64>,
    {
        let _scope = debug_scope!(&self.context, "InvalidateMappedRanges");

        // go through every range we want to read from
        for range in ranges.into_iter() {
            let &(memory, ref range) = range.borrow();
            let range = *range.start().unwrap_or(&0) .. *range.end().unwrap_or(&memory.size);

            let _scope = debug_scope!(&self.context, "Range({:?})", range);
            memory.invalidate(
                &self.context,
                range,
                self.internal.working_buffer.clone(),
                self.internal.working_buffer_size,
            );
        }

        Ok(())
    }

    fn create_semaphore(&self) -> Result<Semaphore, device::OutOfMemory> {
        // TODO:
        Ok(Semaphore)
    }

    fn create_fence(&self, signalled: bool) -> Result<Fence, device::OutOfMemory> {
        Ok(Arc::new(RawFence {
            mutex: Mutex::new(signalled),
            condvar: Condvar::new(),
        }))
    }

    unsafe fn reset_fence(&self, fence: &Fence) -> Result<(), device::OutOfMemory> {
        *fence.mutex.lock() = false;
        Ok(())
    }

    unsafe fn wait_for_fence(
        &self,
        fence: &Fence,
        timeout_ns: u64,
    ) -> Result<bool, device::OomOrDeviceLost> {
        use std::time::{Duration, Instant};

        debug!("wait_for_fence {:?} for {} ns", fence, timeout_ns);
        let mut guard = fence.mutex.lock();
        match timeout_ns {
            0 => Ok(*guard),
            0xFFFFFFFFFFFFFFFF => {
                while !*guard {
                    fence.condvar.wait(&mut guard);
                }
                Ok(true)
            }
            _ => {
                let total = Duration::from_nanos(timeout_ns as u64);
                let now = Instant::now();
                while !*guard {
                    let duration = match total.checked_sub(now.elapsed()) {
                        Some(dur) => dur,
                        None => return Ok(false),
                    };
                    let result = fence.condvar.wait_for(&mut guard, duration);
                    if result.timed_out() {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
        }
    }

    unsafe fn get_fence_status(&self, fence: &Fence) -> Result<bool, device::DeviceLost> {
        Ok(*fence.mutex.lock())
    }

    fn create_event(&self) -> Result<(), device::OutOfMemory> {
        unimplemented!()
    }

    unsafe fn get_event_status(&self, event: &()) -> Result<bool, device::OomOrDeviceLost> {
        unimplemented!()
    }

    unsafe fn set_event(&self, event: &()) -> Result<(), device::OutOfMemory> {
        unimplemented!()
    }

    unsafe fn reset_event(&self, event: &()) -> Result<(), device::OutOfMemory> {
        unimplemented!()
    }

    unsafe fn free_memory(&self, memory: Memory) {
        for (_range, internal) in memory.local_buffers.borrow_mut().iter() {
            (*internal.raw).Release();
            if let Some(srv) = internal.srv {
                (*srv).Release();
            }
        }
    }

    unsafe fn create_query_pool(
        &self,
        _query_ty: query::Type,
        _count: query::Id,
    ) -> Result<QueryPool, query::CreationError> {
        unimplemented!()
    }

    unsafe fn destroy_query_pool(&self, _pool: QueryPool) {
        unimplemented!()
    }

    unsafe fn get_query_pool_results(
        &self,
        _pool: &QueryPool,
        _queries: Range<query::Id>,
        _data: &mut [u8],
        _stride: buffer::Offset,
        _flags: query::ResultFlags,
    ) -> Result<bool, device::OomOrDeviceLost> {
        unimplemented!()
    }

    unsafe fn destroy_shader_module(&self, _shader_lib: ShaderModule) {}

    unsafe fn destroy_render_pass(&self, _rp: RenderPass) {
        //unimplemented!()
    }

    unsafe fn destroy_pipeline_layout(&self, _layout: PipelineLayout) {
        //unimplemented!()
    }

    unsafe fn destroy_graphics_pipeline(&self, _pipeline: GraphicsPipeline) {}

    unsafe fn destroy_compute_pipeline(&self, _pipeline: ComputePipeline) {}

    unsafe fn destroy_framebuffer(&self, _fb: Framebuffer) {}

    unsafe fn destroy_buffer(&self, _buffer: Buffer) {}

    unsafe fn destroy_buffer_view(&self, _view: BufferView) {
        unimplemented!()
    }

    unsafe fn destroy_image(&self, _image: Image) {
        // TODO:
        // unimplemented!()
    }

    unsafe fn destroy_image_view(&self, _view: ImageView) {
        //unimplemented!()
    }

    unsafe fn destroy_sampler(&self, _sampler: Sampler) {}

    unsafe fn destroy_descriptor_pool(&self, _pool: DescriptorPool) {
        //unimplemented!()
    }

    unsafe fn destroy_descriptor_set_layout(&self, _layout: DescriptorSetLayout) {
        //unimplemented!()
    }

    unsafe fn destroy_fence(&self, _fence: Fence) {
        // unimplemented!()
    }

    unsafe fn destroy_semaphore(&self, _semaphore: Semaphore) {
        //unimplemented!()
    }

    unsafe fn destroy_event(&self, _event: ()) {
        //unimplemented!()
    }

    unsafe fn create_swapchain(
        &self,
        surface: &mut Surface,
        config: window::SwapchainConfig,
        _old_swapchain: Option<Swapchain>,
    ) -> Result<(Swapchain, Vec<Image>), window::CreationError> {
        let (dxgi_swapchain, non_srgb_format) =
            self.create_swapchain_impl(&config, surface.wnd_handle, surface.factory.clone())?;

        let resource = {
            let mut resource: *mut d3d11::ID3D11Resource = ptr::null_mut();
            assert_eq!(
                winerror::S_OK,
                dxgi_swapchain.GetBuffer(
                    0 as _,
                    &d3d11::ID3D11Resource::uuidof(),
                    &mut resource as *mut *mut _ as *mut *mut _,
                )
            );
            resource
        };

        let kind = image::Kind::D2(config.extent.width, config.extent.height, 1, 1);
        let decomposed =
            conv::DecomposedDxgiFormat::from_dxgi_format(conv::map_format(config.format).unwrap());

        let mut view_info = ViewInfo {
            resource,
            kind,
            caps: image::ViewCapabilities::empty(),
            view_kind: image::ViewKind::D2,
            format: decomposed.rtv.unwrap(),
            // TODO: can these ever differ for backbuffer?
            range: image::SubresourceRange {
                aspects: format::Aspects::COLOR,
                levels: 0 .. 1,
                layers: 0 .. 1,
            },
        };
        let rtv = self.view_image_as_render_target(&view_info).unwrap();

        view_info.format = non_srgb_format;
        view_info.view_kind = image::ViewKind::D2Array;
        let copy_srv = self.view_image_as_shader_resource(&view_info).unwrap();

        let images = (0 .. config.image_count)
            .map(|_i| {
                // returning the 0th buffer for all images seems like the right thing to do. we can
                // only get write access to the first buffer in the case of `_SEQUENTIAL` flip model,
                // and read access to the rest
                let internal = InternalImage {
                    raw: resource,
                    copy_srv: Some(copy_srv.clone()),
                    srv: None,
                    unordered_access_views: Vec::new(),
                    depth_stencil_views: Vec::new(),
                    render_target_views: vec![rtv.clone()],
                };

                Image {
                    kind,
                    usage: config.image_usage,
                    format: config.format,
                    view_caps: image::ViewCapabilities::empty(),
                    // NOTE: not the actual format of the backbuffer(s)
                    decomposed_format: decomposed.clone(),
                    mip_levels: 1,
                    internal,
                    bind: 0, // TODO: ?
                    requirements: memory::Requirements {
                        // values don't really matter
                        size: 1,
                        alignment: 1,
                        type_mask: MemoryHeapFlags::DEVICE_LOCAL.bits(),
                    },
                    tiling: image::Tiling::Optimal,
                }
            })
            .collect();

        Ok((Swapchain { dxgi_swapchain }, images))
    }

    unsafe fn destroy_swapchain(&self, _swapchain: Swapchain) {
        // automatic
    }

    fn wait_idle(&self) -> Result<(), device::OutOfMemory> {
        Ok(())
        // unimplemented!()
    }

    unsafe fn set_image_name(&self, _image: &mut Image, _name: &str) {
        // TODO
    }

    unsafe fn set_buffer_name(&self, _buffer: &mut Buffer, _name: &str) {
        // TODO
    }

    unsafe fn set_command_buffer_name(&self, _command_buffer: &mut CommandBuffer, _name: &str) {
        // TODO
    }

    unsafe fn set_semaphore_name(&self, _semaphore: &mut Semaphore, _name: &str) {
        // TODO
    }

    unsafe fn set_fence_name(&self, _fence: &mut Fence, _name: &str) {
        // TODO
    }

    unsafe fn set_framebuffer_name(&self, _framebuffer: &mut Framebuffer, _name: &str) {
        // TODO
    }

    unsafe fn set_render_pass_name(&self, _render_pass: &mut RenderPass, _name: &str) {
        // TODO
    }
}
