//! 帧适配层：GPU 表面 → 自包含 CPU NV12。
//!
//! V0.1 使用 CPU readback。这一层是「接口不变、实现可替换」的关键：
//! 未来换成 GPU 转换（Video Processor MFT / shader）时，上游 Capture 与下游 Encoder 都不需要改。

use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::core::Interface;

use crate::convert::{bgra_to_nv12, bgra_to_nv12_region, BgraImageView, Nv12Image, Region};
use crate::error::{MediaError, MediaResult};

/// BGRA8 表面的 readback + 转换适配器。
///
/// 持有 staging 纹理，尺寸变化时自动重建。
/// `region` 为 Some 时只转换该子矩形（区域录制）：回读仍是整屏，
/// 但转换开销与区域面积成正比。
pub struct FrameAdapter {
    context: ID3D11DeviceContext,
    staging: Option<ID3D11Texture2D>,
    size: (u32, u32),
    region: Option<Region>,
}

impl FrameAdapter {
    pub fn new(context: ID3D11DeviceContext, region: Option<Region>) -> Self {
        Self {
            context,
            staging: None,
            size: (0, 0),
            region,
        }
    }

    /// 把 GPU 表面拷贝到 CPU 可读的 staging 纹理。
    ///
    /// 调用方必须在本方法返回后立即 `frame.Close()`，以尽早归还 WGC 帧池缓冲区。
    pub fn stage(
        &mut self,
        device: &ID3D11Device,
        source: &ID3D11Texture2D,
        size: (u32, u32),
    ) -> MediaResult<()> {
        if self.staging.is_none() || self.size != size {
            self.staging = Some(create_staging_texture(device, size)?);
            self.size = size;
        }
        let staging = self.staging.as_ref().expect("staging 刚被创建");
        unsafe {
            let dst: ID3D11Resource = staging
                .cast()
                .map_err(|e| MediaError::win32("staging->ID3D11Resource", e))?;
            let src: ID3D11Resource = source
                .cast()
                .map_err(|e| MediaError::win32("texture->ID3D11Resource", e))?;
            // 同一 immediate context 上 CopyResource 之后的 Map 已隐式同步，不需要额外 fence
            self.context.CopyResource(&dst, &src);
        }
        Ok(())
    }

    /// Map staging → 用 RowPitch 逐行转 NV12 → Unmap。
    pub fn readback_and_convert(&mut self) -> MediaResult<Nv12Image> {
        let staging = self
            .staging
            .as_ref()
            .ok_or_else(|| MediaError::Internal("readback 前必须调用 stage()".into()))?;
        let (width, height) = self.size;

        unsafe {
            let resource: ID3D11Resource = staging
                .cast()
                .map_err(|e| MediaError::win32("staging->ID3D11Resource", e))?;
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&resource, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| MediaError::win32("Map(staging)", e))?;

            let pitch = mapped.RowPitch as usize;
            // 必须用 RowPitch 而不是 width*4：RowPitch 通常是 256 字节对齐的
            let view = BgraImageView {
                data: std::slice::from_raw_parts(mapped.pData as *const u8, pitch * height as usize),
                pitch,
                width,
                height,
            };
            let converted = match self.region {
                Some(region) => bgra_to_nv12_region(&view, region),
                None => bgra_to_nv12(&view),
            };
            self.context.Unmap(&resource, 0);
            converted
        }
    }
}

fn create_staging_texture(device: &ID3D11Device, size: (u32, u32)) -> MediaResult<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: size.0,
        Height: size.1,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    unsafe {
        let mut texture: Option<ID3D11Texture2D> = None;
        device
            .CreateTexture2D(&desc, None, Some(&mut texture))
            .map_err(|e| MediaError::win32("CreateTexture2D(staging)", e))?;
        texture.ok_or_else(|| MediaError::Internal("CreateTexture2D 未返回纹理".into()))
    }
}
