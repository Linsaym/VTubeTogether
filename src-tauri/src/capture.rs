use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, ensure};
use openh264::{
    OpenH264API, Timestamp,
    encoder::{
        BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod,
        Profile, RateControlMode, UsageType, VuiConfig,
    },
    formats::YUVSlices,
};
use serde::Serialize;
use tokio::sync::watch;
use windows::Win32::{
    Foundation::{HWND, POINT, RECT},
    Graphics::{
        Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute},
        Gdi::ClientToScreen,
    },
    UI::WindowsAndMessaging::{GetClientRect, GetWindowRect, IsIconic},
};
use windows_capture::{
    capture::{CaptureControl, Context, GraphicsCaptureApiHandler},
    frame::Frame,
    graphics_capture_api::InternalCaptureControl,
    settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
    },
    window::Window,
};
use yuv::{
    YuvChromaSubsampling, YuvConversionMode, YuvPlanarImageMut, YuvRange, YuvStandardMatrix,
    bgra_to_yuv420,
};

use crate::transport::VideoFrame;

pub const FRAME_INTERVAL: Duration = Duration::from_nanos(33_333_333);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowChoice {
    id: String,
    title: String,
    is_vtube_studio: bool,
}

pub fn list_windows() -> Result<Vec<WindowChoice>> {
    let mut windows: Vec<_> = Window::enumerate()?
        .into_iter()
        .filter_map(|window| {
            let title = window.title().ok()?;
            if title.trim().is_empty() || window.process_id().ok()? == std::process::id() {
                return None;
            }
            let is_vtube_studio = title.to_lowercase().contains("vtube studio")
                || window
                    .process_name()
                    .is_ok_and(|name| name.to_lowercase().contains("vtube studio"));
            Some(WindowChoice {
                id: (window.as_raw_hwnd() as usize).to_string(),
                title,
                is_vtube_studio,
            })
        })
        .collect();
    windows.sort_by(|a, b| {
        b.is_vtube_studio
            .cmp(&a.is_vtube_studio)
            .then(a.title.cmp(&b.title))
    });
    Ok(windows)
}

pub fn start(
    id: &str,
    frames: watch::Sender<Option<Arc<YuvFrame>>>,
) -> Result<CaptureControl<Capture, anyhow::Error>> {
    let handle = id.parse::<usize>().context("Выберите окно заново")?;
    let window = Window::from_raw_hwnd(handle as *mut _);
    ensure!(
        window.is_valid(),
        "Окно закрыто или недоступно. Выберите его заново"
    );
    let settings = Settings::new(
        window,
        CursorCaptureSettings::WithoutCursor,
        DrawBorderSettings::Default,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        (handle, frames),
    );
    Capture::start_free_threaded(settings)
        .map_err(|err| anyhow::anyhow!("Не удалось захватить окно: {err:?}"))
}

pub type YuvFrame = YuvPlanarImageMut<'static, u8>;

pub struct Capture {
    handle: usize,
    frames: watch::Sender<Option<Arc<YuvFrame>>>,
    next_capture: Instant,
    scaled: Vec<u8>,
}

impl GraphicsCaptureApiHandler for Capture {
    type Flags = (usize, watch::Sender<Option<Arc<YuvFrame>>>);
    type Error = anyhow::Error;

    fn new(context: Context<Self::Flags>) -> Result<Self> {
        let (handle, frames) = context.flags;
        Ok(Self {
            handle,
            frames,
            next_capture: Instant::now(),
            scaled: Vec::new(),
        })
    }

    fn on_frame_arrived(&mut self, frame: &mut Frame<'_>, _: InternalCaptureControl) -> Result<()> {
        let now = Instant::now();
        // Compositor timestamps can jitter around a 60 Hz refresh boundary.
        if now + Duration::from_millis(2) < self.next_capture {
            return Ok(());
        }
        self.next_capture = (self.next_capture + FRAME_INTERVAL).max(now + FRAME_INTERVAL / 2);
        let width = frame.width().min(frame.desc().Width);
        let height = frame.height().min(frame.desc().Height);
        let hwnd = HWND(self.handle as *mut _);
        let Some((x, y, width, height)) = client_crop(hwnd, width, height) else {
            return Ok(());
        };

        let mut buffer = frame.buffer_crop(x, y, x + width, y + height)?;
        let mut stride = buffer.row_pitch();
        let mut pixels = buffer.as_raw_buffer();
        let scale = (1920.0 / f64::from(width))
            .min(1080.0 / f64::from(height))
            .min(1.0);
        let out_width = ((f64::from(width) * scale) as u32 & !1).max(2);
        let out_height = ((f64::from(height) * scale) as u32 & !1).max(2);
        if (width, height) != (out_width, out_height) {
            let out_stride = out_width * 4;
            self.scaled.resize((out_stride * out_height) as usize, 0);
            for (row_index, row) in self
                .scaled
                .chunks_exact_mut(out_stride as usize)
                .enumerate()
            {
                let input_row = row_index * height as usize / out_height as usize * stride as usize;
                for (column, pixel) in row.chunks_exact_mut(4).enumerate() {
                    let input = input_row + column * width as usize / out_width as usize * 4;
                    pixel.copy_from_slice(&pixels[input..input + 4]);
                }
            }
            pixels = &mut self.scaled;
            stride = out_stride;
        }
        let mut yuv = YuvFrame::alloc(out_width, out_height, YuvChromaSubsampling::Yuv420);
        bgra_to_yuv420(
            &mut yuv,
            pixels,
            stride,
            YuvRange::Limited,
            YuvStandardMatrix::Bt709,
            YuvConversionMode::Balanced,
        )?;
        self.frames.send_replace(Some(Arc::new(yuv)));
        Ok(())
    }
}

fn client_crop(hwnd: HWND, width: u32, height: u32) -> Option<(u32, u32, u32, u32)> {
    let mut client = RECT::default();
    let mut frame = RECT::default();
    let mut outer = RECT::default();
    let mut origin = POINT::default();
    // These coordinates use physical pixels in the per-monitor-aware process.
    unsafe {
        if IsIconic(hwnd).as_bool() {
            return None;
        }
        GetClientRect(hwnd, &mut client).ok()?;
        if !ClientToScreen(hwnd, &mut origin).as_bool() {
            return None;
        }
        GetWindowRect(hwnd, &mut outer).ok()?;
        let result = DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&mut frame as *mut RECT).cast(),
            size_of::<RECT>() as u32,
        );
        if result.is_err()
            || (outer.right - outer.left == width as i32
                && outer.bottom - outer.top == height as i32)
        {
            frame = outer;
        }
    }
    let left = (origin.x - frame.left).clamp(0, width as i32);
    let top = (origin.y - frame.top).clamp(0, height as i32);
    let right = (origin.x + client.right - frame.left).clamp(left, width as i32);
    let bottom = (origin.y + client.bottom - frame.top).clamp(top, height as i32);
    let crop_width = (right - left) as u32 & !1;
    let crop_height = (bottom - top) as u32 & !1;
    (crop_width >= 16 && crop_height >= 16).then_some((
        left as u32,
        top as u32,
        crop_width,
        crop_height,
    ))
}

pub fn encoder() -> Result<Encoder> {
    let config = EncoderConfig::new()
        .usage_type(UsageType::ScreenContentRealTime)
        .rate_control_mode(RateControlMode::Bitrate)
        .bitrate(BitRate::from_bps(3_000_000))
        .max_frame_rate(FrameRate::from_hz(30.0))
        .skip_frames(false)
        .profile(Profile::Baseline)
        .complexity(Complexity::Low)
        .num_threads(2)
        .intra_frame_period(IntraFramePeriod::from_num_frames(15))
        .vui(VuiConfig::bt709());
    Ok(Encoder::with_api_config(
        OpenH264API::from_source(),
        config,
    )?)
}

pub fn encode(
    encoder: &mut Encoder,
    image: &YuvFrame,
    timestamp: u64,
) -> Result<Option<VideoFrame>> {
    let image = YUVSlices::new(
        (
            image.y_plane.borrow(),
            image.u_plane.borrow(),
            image.v_plane.borrow(),
        ),
        (image.width as usize, image.height as usize),
        (
            image.y_stride as usize,
            image.u_stride as usize,
            image.v_stride as usize,
        ),
    );
    let bits = encoder.encode_at(&image, Timestamp::from_millis(timestamp))?;
    if matches!(bits.frame_type(), FrameType::Invalid | FrameType::Skip) {
        return Ok(None);
    }
    Ok(Some(VideoFrame {
        key: bits.frame_type() == FrameType::IDR,
        data: bits.to_vec(),
    }))
}
