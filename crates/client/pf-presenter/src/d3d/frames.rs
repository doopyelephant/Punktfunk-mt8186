//! The CPU I420 lane: three `R8_UNORM` textures sized to the frame, filled by
//! `UpdateSubresource`, each with the view the CSC pass samples.

use anyhow::{Context as _, Result};
use pf_client_core::video::CpuPlanarFrame;
use windows::Win32::d3d11::{
    ID3D11Device5, ID3D11DeviceContext4, ID3D11ShaderResourceView, ID3D11Texture2D,
    D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::dxgi::{DXGI_FORMAT_R8_UNORM, DXGI_SAMPLE_DESC};

pub(super) struct CpuPlanes {
    /// Luma size; the chroma planes follow `CpuPlanarFrame::plane_dims`.
    size: (u32, u32),
    textures: [ID3D11Texture2D; 3],
    pub(super) views: [Option<ID3D11ShaderResourceView>; 3],
}

impl CpuPlanes {
    fn new(device: &ID3D11Device5, f: &CpuPlanarFrame) -> Result<CpuPlanes> {
        let plane = |i: usize| -> Result<(ID3D11Texture2D, ID3D11ShaderResourceView)> {
            let (width, height) = f.plane_dims(i);
            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_SHADER_RESOURCE as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let (mut texture, mut view) = (None, None);
            // SAFETY: COM calls on the live device over the local descriptor; each out-param
            // is a local `Option` checked below.
            unsafe {
                device
                    .CreateTexture2D(&desc, None, Some(&mut texture))
                    .ok()
                    .context("create a CPU plane texture")?;
                let texture = texture.as_ref().context("CreateTexture2D returned none")?;
                device
                    .CreateShaderResourceView(texture, None, Some(&mut view))
                    .ok()
                    .context("create a CPU plane view")?;
            }
            Ok((
                texture.context("CreateTexture2D returned none")?,
                view.context("CreateShaderResourceView returned none")?,
            ))
        };
        let (y, cb, cr) = (plane(0)?, plane(1)?, plane(2)?);
        Ok(CpuPlanes {
            size: (f.width, f.height),
            textures: [y.0, cb.0, cr.0],
            views: [Some(y.1), Some(cb.1), Some(cr.1)],
        })
    }

    pub(super) fn size(&self) -> (u32, u32) {
        self.size
    }

    /// Copy `f`'s planes into `slot`, rebuilding the textures when the frame size changes.
    pub(super) fn upload(
        slot: &mut Option<CpuPlanes>,
        device: &ID3D11Device5,
        context: &ID3D11DeviceContext4,
        f: &CpuPlanarFrame,
    ) -> Result<()> {
        if slot.as_ref().is_none_or(|p| p.size != (f.width, f.height)) {
            *slot = Some(CpuPlanes::new(device, f)?);
        }
        let planes = slot.as_ref().context("CPU planes")?;
        for (i, texture) in planes.textures.iter().enumerate() {
            let data = f.plane(i);
            let (width, _) = f.plane_dims(i);
            // SAFETY: a COM call on the live context; `data` is the tightly packed plane of
            // `width` × the texture's height bytes, read before the call returns.
            unsafe {
                context.UpdateSubresource(texture, 0, None, data.as_ptr().cast(), width, 0);
            }
        }
        Ok(())
    }
}
