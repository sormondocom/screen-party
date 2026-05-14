//! Windows screen capture via DXGI Desktop Duplication API.
//!
//! Flow: D3D11 device → IDXGIOutputDuplication → AcquireNextFrame → copy
//! sub-region to a CPU-mappable staging texture → BGRA→RGBA conversion.
//!
//! DXGI_ERROR_ACCESS_LOST is surfaced as Backend error; recreate the capturer
//! on desktop transitions (lock screen, UAC prompt, fullscreen app launch).

use windows::{
    core::{Interface, HRESULT},
    Win32::{
        Foundation::HMODULE,
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::*,
            Dxgi::{Common::*, *},
        },
    },
};

use crate::{
    capturer::{CaptureError, Capturer, DisplayInfo},
    frame::{Frame, Rect},
};

// Documented DXGI HRESULT codes not re-exported by the windows crate.
const DXGI_ERROR_WAIT_TIMEOUT: HRESULT = HRESULT(0x887A0027_u32 as i32);
const DXGI_ERROR_ACCESS_LOST: HRESULT = HRESULT(0x887A0026_u32 as i32);

fn wrap(e: windows::core::Error) -> CaptureError {
    CaptureError::Backend(e.to_string())
}

// ── Display enumeration ──────────────────────────────────────────────────────

pub(super) fn list_displays() -> Result<Vec<DisplayInfo>, CaptureError> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().map_err(wrap)?;
        let mut displays = Vec::new();
        let mut global_id = 0u32;

        for adapter_idx in 0u32.. {
            let adapter = match factory.EnumAdapters1(adapter_idx) {
                Ok(a) => a,
                Err(_) => break,
            };
            for output_idx in 0u32.. {
                let output: IDXGIOutput = match adapter.EnumOutputs(output_idx) {
                    Ok(o) => o,
                    Err(_) => break,
                };
                // DXGI_OUTPUT_DESC.Monitor is HMONITOR, which requires the
                // Win32_Graphics_Gdi feature; without it GetDesc isn't generated.
                let desc = output.GetDesc().map_err(wrap)?;
                let r = desc.DesktopCoordinates;
                let raw = &desc.DeviceName;
                let name = String::from_utf16_lossy(
                    &raw[..raw.iter().position(|&c| c == 0).unwrap_or(raw.len())],
                );
                displays.push(DisplayInfo {
                    id:      global_id,
                    name,
                    x:       r.left,
                    y:       r.top,
                    width:   (r.right  - r.left) as u32,
                    height:  (r.bottom - r.top)  as u32,
                    // The primary monitor always has its top-left at the virtual
                    // desktop origin on Windows.
                    primary: r.left == 0 && r.top == 0,
                });
                global_id += 1;
            }
        }
        Ok(displays)
    }
}

// ── DxgiCapturer ────────────────────────────────────────────────────────────

pub(super) fn new_capturer(
    display: &DisplayInfo,
    region: Rect,
) -> Result<Box<dyn Capturer>, CaptureError> {
    let (adapter_idx, output_idx) = find_adapter_output(display.id)?;
    Ok(Box::new(DxgiCapturer::new(
        display.clone(),
        region,
        adapter_idx,
        output_idx,
    )?))
}

struct DxgiCapturer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    staging: ID3D11Texture2D,
    region: Rect,
    display: DisplayInfo,
}

// Safety: capture is pinned to a single thread.  D3D11 immediate contexts are
// not free-threaded; we never share this across threads.
unsafe impl Send for DxgiCapturer {}

impl DxgiCapturer {
    fn new(
        display: DisplayInfo,
        region: Rect,
        adapter_idx: u32,
        output_idx: u32,
    ) -> Result<Self, CaptureError> {
        unsafe {
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .map_err(wrap)?;
            let device = device.unwrap();
            let context = context.unwrap();

            let factory: IDXGIFactory1 = CreateDXGIFactory1().map_err(wrap)?;
            let adapter = factory
                .EnumAdapters1(adapter_idx)
                .map_err(|_| CaptureError::InvalidRegion)?;
            let output: IDXGIOutput = adapter
                .EnumOutputs(output_idx)
                .map_err(|_| CaptureError::InvalidRegion)?;
            let output1: IDXGIOutput1 = output.cast().map_err(wrap)?;
            let duplication = output1.DuplicateOutput(&device).map_err(wrap)?;

            let staging = staging_texture(&device, region.width, region.height)?;

            Ok(Self { device, context, duplication, staging, region, display })
        }
    }
}

impl Capturer for DxgiCapturer {
    fn next_frame(&mut self) -> Result<Frame, CaptureError> {
        unsafe {
            loop {
                let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
                let mut resource: Option<IDXGIResource> = None;

                match self.duplication.AcquireNextFrame(33, &mut info, &mut resource) {
                    Ok(()) => {
                        let texture: ID3D11Texture2D =
                            resource.unwrap().cast().map_err(wrap)?;

                        let src_box = D3D11_BOX {
                            left: self.region.x,
                            top: self.region.y,
                            front: 0,
                            right: self.region.x + self.region.width,
                            bottom: self.region.y + self.region.height,
                            back: 1,
                        };
                        self.context.CopySubresourceRegion(
                            &self.staging,
                            0,
                            0,
                            0,
                            0,
                            &texture,
                            0,
                            Some(&src_box),
                        );

                        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                        self.context
                            .Map(
                                &self.staging,
                                0,
                                D3D11_MAP_READ,
                                0,
                                Some(&mut mapped),
                            )
                            .map_err(wrap)?;

                        let w = self.region.width;
                        let h = self.region.height;
                        let mut rgba = Vec::with_capacity((w * h * 4) as usize);

                        for row in 0..h {
                            let row_ptr = (mapped.pData as *const u8)
                                .add((row * mapped.RowPitch) as usize);
                            for col in 0..w {
                                let p = row_ptr.add((col * 4) as usize);
                                // Desktop Duplication gives BGRA; store as RGBA.
                                rgba.push(*p.add(2));
                                rgba.push(*p.add(1));
                                rgba.push(*p.add(0));
                                rgba.push(*p.add(3));
                            }
                        }

                        self.context.Unmap(&self.staging, 0);
                        self.duplication.ReleaseFrame().map_err(wrap)?;
                        return Ok(Frame::new(w, h, rgba));
                    }
                    Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => continue,
                    Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                        return Err(CaptureError::Backend(
                            "desktop access lost — recreate capturer".into(),
                        ));
                    }
                    Err(e) => return Err(wrap(e)),
                }
            }
        }
    }

    fn region(&self) -> Rect {
        self.region
    }

    fn set_region(&mut self, region: Rect) -> Result<(), CaptureError> {
        if region.x + region.width > self.display.width
            || region.y + region.height > self.display.height
        {
            return Err(CaptureError::InvalidRegion);
        }
        if region.width != self.region.width || region.height != self.region.height {
            self.staging =
                unsafe { staging_texture(&self.device, region.width, region.height) }?;
        }
        self.region = region;
        Ok(())
    }

    fn display_info(&self) -> &DisplayInfo {
        &self.display
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

unsafe fn staging_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<ID3D11Texture2D, CaptureError> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut tex: Option<ID3D11Texture2D> = None;
    device.CreateTexture2D(&desc, None, Some(&mut tex)).map_err(wrap)?;
    Ok(tex.unwrap())
}

fn find_adapter_output(display_id: u32) -> Result<(u32, u32), CaptureError> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().map_err(wrap)?;
        let mut global_id = 0u32;

        for adapter_idx in 0u32.. {
            let adapter = match factory.EnumAdapters1(adapter_idx) {
                Ok(a) => a,
                Err(_) => break,
            };
            for output_idx in 0u32.. {
                match adapter.EnumOutputs(output_idx) {
                    Ok(_) => {}
                    Err(_) => break,
                }
                if global_id == display_id {
                    return Ok((adapter_idx, output_idx));
                }
                global_id += 1;
            }
        }
        Err(CaptureError::InvalidRegion)
    }
}
