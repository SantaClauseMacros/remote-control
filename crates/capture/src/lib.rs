//! Screen capture via the **DXGI Desktop Duplication API**.
//!
//! This is the fastest capture path Windows offers for a normal (non-elevated,
//! no-driver) process: the compositor hands us the desktop image as a GPU
//! texture, plus the list of rectangles that changed since the previous frame.
//!
//! Milestone 2 uses the CPU-copy path ([`Capturer::grab`]) to prove capture
//! works and to measure its cost. Milestone 3 adds a zero-copy path that hands
//! the GPU texture straight to the encoder on the same D3D11 device.

use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    IDXGIAdapter, IDXGIDevice, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_DESC,
    DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTPUT_DESC,
};

/// A monitor's position and size in **virtual-desktop pixel** coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// A shared D3D11 device + immediate context.
///
/// Capture and (later) the hardware encoder must run on the **same** device for
/// textures to be shared without a CPU round-trip, so this is created once and
/// handed to both.
#[derive(Clone)]
pub struct D3dContext {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
}

impl D3dContext {
    /// Create a hardware D3D11 device with BGRA support (required for Desktop
    /// Duplication and for Direct2D/DWM interop).
    pub fn new() -> Result<Self> {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let feature_levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];

        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("D3D11CreateDevice (hardware)")?;
        }

        Ok(Self {
            device: device.ok_or_else(|| anyhow!("D3D11CreateDevice returned no device"))?,
            context: context.ok_or_else(|| anyhow!("D3D11CreateDevice returned no context"))?,
        })
    }
}

/// A captured desktop frame, copied into CPU memory as tightly-packed BGRA.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes, top-down, 8-bit B,G,R,A.
    pub bgra: Vec<u8>,
    /// QPC timestamp of the compositor present that produced this image.
    /// `0` means the image itself did not change (only the cursor moved).
    pub present_time_qpc: i64,
    /// How many compositor frames were coalesced into this capture. `> 1`
    /// means we are not keeping up with the display.
    pub accumulated_frames: u32,
    /// `true` when only the mouse pointer moved — safe to skip encoding.
    pub mouse_only: bool,
}

/// Outcome of a single [`Capturer::grab`] call.
pub enum Grab {
    /// A new frame is available.
    Frame(Frame),
    /// No change within the timeout — the screen is static. This is the common
    /// case and costs almost nothing.
    Timeout,
}

/// Duplicates one monitor's output.
pub struct Capturer {
    d3d: D3dContext,
    output_index: u32,
    dupl: IDXGIOutputDuplication,
    desc: DXGI_OUTDUPL_DESC,
    rect: DesktopRect,
    staging: Option<ID3D11Texture2D>,
    holding_frame: bool,
}

impl Capturer {
    /// Start duplicating output `output_index` (0 = primary enumeration order).
    pub fn new(d3d: D3dContext, output_index: u32) -> Result<Self> {
        let (dupl, rect) = Self::create_duplication(&d3d, output_index)?;
        let desc = unsafe { dupl.GetDesc() };
        tracing::info!(
            output = output_index,
            width = desc.ModeDesc.Width,
            height = desc.ModeDesc.Height,
            rect_x = rect.x,
            rect_y = rect.y,
            in_sysmem = desc.DesktopImageInSystemMemory.as_bool(),
            "desktop duplication started"
        );
        Ok(Self {
            d3d,
            output_index,
            dupl,
            desc,
            rect,
            staging: None,
            holding_frame: false,
        })
    }

    pub fn width(&self) -> u32 {
        self.desc.ModeDesc.Width
    }
    pub fn height(&self) -> u32 {
        self.desc.ModeDesc.Height
    }

    /// This monitor's rectangle in virtual-desktop pixels — what the host needs
    /// to translate a client's normalised pointer coordinates for injection.
    pub fn desktop_rect(&self) -> DesktopRect {
        self.rect
    }

    fn create_duplication(
        d3d: &D3dContext,
        output_index: u32,
    ) -> Result<(IDXGIOutputDuplication, DesktopRect)> {
        unsafe {
            let dxgi_device: IDXGIDevice =
                d3d.device.cast().context("ID3D11Device as IDXGIDevice")?;
            let adapter: IDXGIAdapter = dxgi_device
                .GetAdapter()
                .context("IDXGIDevice::GetAdapter")?;
            let output = adapter.EnumOutputs(output_index).map_err(|e| {
                if e.code() == DXGI_ERROR_NOT_FOUND {
                    anyhow!("no monitor at output index {output_index}")
                } else {
                    anyhow!("IDXGIAdapter::EnumOutputs({output_index}): {e}")
                }
            })?;

            let odesc: DXGI_OUTPUT_DESC = output.GetDesc().context("IDXGIOutput::GetDesc")?;
            let dc = odesc.DesktopCoordinates;
            let rect = DesktopRect {
                x: dc.left,
                y: dc.top,
                width: (dc.right - dc.left).max(0) as u32,
                height: (dc.bottom - dc.top).max(0) as u32,
            };

            let output1: IDXGIOutput1 = output.cast().context("IDXGIOutput as IDXGIOutput1")?;
            let dupl = output1
                .DuplicateOutput(&d3d.device)
                .context("IDXGIOutput1::DuplicateOutput")?;
            Ok((dupl, rect))
        }
    }

    /// Re-establish duplication after `DXGI_ERROR_ACCESS_LOST` (mode change,
    /// full-screen app takeover, session lock, UAC secure desktop, …).
    fn recreate(&mut self) -> Result<()> {
        self.release_if_held();
        let (dupl, rect) = Self::create_duplication(&self.d3d, self.output_index)?;
        self.dupl = dupl;
        self.rect = rect;
        self.desc = unsafe { self.dupl.GetDesc() };
        self.staging = None;
        tracing::info!("desktop duplication re-established");
        Ok(())
    }

    fn release_if_held(&mut self) {
        if self.holding_frame {
            unsafe {
                let _ = self.dupl.ReleaseFrame();
            }
            self.holding_frame = false;
        }
    }

    /// Grab the next frame, waiting at most `timeout_ms` for the screen to
    /// change. `Timeout` is normal and cheap.
    pub fn grab(&mut self, timeout_ms: u32) -> Result<Grab> {
        self.release_if_held();

        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;

        let acquire = unsafe {
            self.dupl
                .AcquireNextFrame(timeout_ms, &mut info, &mut resource)
        };
        if let Err(e) = acquire {
            return match e.code() {
                c if c == DXGI_ERROR_WAIT_TIMEOUT => Ok(Grab::Timeout),
                c if c == DXGI_ERROR_ACCESS_LOST => {
                    self.recreate()?;
                    Ok(Grab::Timeout)
                }
                _ => Err(e).context("IDXGIOutputDuplication::AcquireNextFrame"),
            };
        }
        self.holding_frame = true;

        let resource = resource.ok_or_else(|| anyhow!("AcquireNextFrame gave no resource"))?;
        let src: ID3D11Texture2D = resource.cast().context("desktop resource as texture")?;

        let mouse_only = info.LastPresentTime == 0;
        let frame = self
            .copy_to_cpu(&src, info)
            .context("copying frame to system memory")?;

        self.release_if_held();
        Ok(Grab::Frame(Frame {
            mouse_only,
            ..frame
        }))
    }

    fn ensure_staging(&mut self, w: u32, h: u32) -> Result<ID3D11Texture2D> {
        if let Some(tex) = &self.staging {
            let mut d = D3D11_TEXTURE2D_DESC::default();
            unsafe { tex.GetDesc(&mut d) };
            if d.Width == w && d.Height == h {
                return Ok(tex.clone());
            }
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
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
        let mut tex: Option<ID3D11Texture2D> = None;
        unsafe {
            self.d3d
                .device
                .CreateTexture2D(&desc, None, Some(&mut tex))
                .context("CreateTexture2D (staging)")?;
        }
        let tex = tex.ok_or_else(|| anyhow!("CreateTexture2D returned nothing"))?;
        self.staging = Some(tex.clone());
        Ok(tex)
    }

    fn copy_to_cpu(
        &mut self,
        src: &ID3D11Texture2D,
        info: DXGI_OUTDUPL_FRAME_INFO,
    ) -> Result<Frame> {
        let mut sd = D3D11_TEXTURE2D_DESC::default();
        unsafe { src.GetDesc(&mut sd) };
        let (w, h) = (sd.Width, sd.Height);

        let staging = self.ensure_staging(w, h)?;
        let staging_res: ID3D11Resource = staging.cast().unwrap();
        let src_res: ID3D11Resource = src.cast().unwrap();

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        let bgra = unsafe {
            self.d3d.context.CopyResource(&staging_res, &src_res);
            self.d3d
                .context
                .Map(&staging_res, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .context("Map(staging)")?;

            let row_bytes = (w * 4) as usize;
            let mut out = vec![0u8; row_bytes * h as usize];
            let src_pitch = mapped.RowPitch as usize;
            let base = mapped.pData as *const u8;
            for y in 0..h as usize {
                let s = base.add(y * src_pitch);
                let d = out.as_mut_ptr().add(y * row_bytes);
                std::ptr::copy_nonoverlapping(s, d, row_bytes);
            }
            self.d3d.context.Unmap(&staging_res, 0);
            out
        };

        Ok(Frame {
            width: w,
            height: h,
            bgra,
            present_time_qpc: info.LastPresentTime,
            accumulated_frames: info.AccumulatedFrames,
            mouse_only: false,
        })
    }
}

impl Drop for Capturer {
    fn drop(&mut self) {
        self.release_if_held();
    }
}

/// Convenience: capture one frame from the primary output. Mainly for probes.
pub fn grab_one(timeout_ms: u32) -> Result<Frame> {
    let d3d = D3dContext::new()?;
    let mut cap = Capturer::new(d3d, 0)?;
    let deadline = Instant::now() + std::time::Duration::from_millis(timeout_ms as u64 + 2000);
    loop {
        match cap.grab(timeout_ms)? {
            Grab::Frame(f) => return Ok(f),
            Grab::Timeout if Instant::now() < deadline => continue,
            Grab::Timeout => return Err(anyhow!("no frame within timeout")),
        }
    }
}
