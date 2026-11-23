use std::sync::Arc;

use crate::Frame;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Quality {
    Low,
    #[default]
    Standard,
    High,
}

impl Quality {
    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "480p / 15 fps",
            Self::Standard => "720p / 24 fps",
            Self::High => "1080p / 30 fps",
        }
    }

    pub fn height(self) -> u32 {
        match self {
            Self::Low => 480,
            Self::Standard => 720,
            Self::High => 1080,
        }
    }

    pub fn fps(self) -> u32 {
        match self {
            Self::Low => 15,
            Self::Standard => 24,
            Self::High => 30,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub kind: SourceKind,
    pub id: u32,
    pub label: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Display,
    Window,
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
mod native {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{self, Receiver};
    use std::thread;
    use std::time::{Duration, Instant};

    use image::codecs::jpeg::JpegEncoder;
    use image::{RgbaImage, imageops};

    use super::*;

    const CAPTURE_POLL: Duration = Duration::from_millis(100);
    const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(8);
    const STATIC_FRAME_INTERVAL: Duration = Duration::from_secs(2);
    const SOURCE_GONE: &str =
        "The selected source changed or closed. Refresh the list and select it again.";

    #[derive(Default)]
    struct State {
        cancelled: AtomicBool,
        frame: Mutex<Option<Arc<Frame>>>,
        error: Mutex<Option<String>>,
    }

    impl State {
        fn set_error(&self, message: String) {
            let mut error = self.error.lock().unwrap();
            error.get_or_insert(message);
        }

        fn active(&self) -> bool {
            !self.cancelled.load(Ordering::Acquire) && self.error.lock().unwrap().is_none()
        }
    }

    pub struct Capture {
        state: Arc<State>,
    }

    impl Capture {
        pub fn start(
            source: Source,
            quality: Quality,
            publish: impl Fn(Arc<[u8]>) + Send + 'static,
        ) -> Result<Self, String> {
            let state = Arc::new(State::default());
            let capture_state = state.clone();
            thread::Builder::new()
                .name("arcane-capture".into())
                .spawn(move || {
                    if !capture_state.active() {
                        return;
                    }
                    let (frames, receiver) = mpsc::sync_channel(1);
                    let result = platform::capture(
                        &source,
                        quality,
                        &capture_state,
                        frames,
                        |native_running| {
                            publish_frames(
                                &receiver,
                                quality,
                                &capture_state,
                                native_running,
                                publish,
                            )
                        },
                    );
                    if let Err(error) = result {
                        capture_state.set_error(error);
                    }
                })
                .map_err(|error| format!("Couldn't start the capture worker: {error}"))?;
            Ok(Self { state })
        }

        pub fn frame(&self) -> Option<Arc<Frame>> {
            self.state.frame.lock().unwrap().clone()
        }

        pub fn error(&self) -> Option<String> {
            self.state.error.lock().unwrap().clone()
        }
    }

    impl Drop for Capture {
        fn drop(&mut self) {
            self.state.cancelled.store(true, Ordering::Release);
        }
    }

    pub fn sources() -> Result<Vec<Source>, String> {
        platform::sources()
    }

    fn publish_frames(
        receiver: &Receiver<RgbaImage>,
        quality: Quality,
        state: &State,
        native_running: &dyn Fn() -> bool,
        publish: impl Fn(Arc<[u8]>),
    ) -> Result<(), String> {
        let began = Instant::now();
        let mut last_sent = began;
        let mut last_jpeg = None::<Arc<[u8]>>;

        while state.active() {
            if !native_running() {
                return Err("The screen capture session ended.".into());
            }
            match receiver.recv_timeout(CAPTURE_POLL) {
                Ok(image) => {
                    let (width, height) = output_size(image.width(), image.height(), quality);
                    let image = if image.dimensions() == (width, height) {
                        image
                    } else {
                        imageops::thumbnail(&image, width, height)
                    };
                    let jpeg = encode_frame(&image, quality)?;
                    if !state.active() {
                        break;
                    }
                    *state.frame.lock().unwrap() = Some(Arc::new(Frame {
                        width,
                        height,
                        rgba: image.into_raw(),
                    }));
                    publish(jpeg.clone());
                    last_jpeg = Some(jpeg);
                    last_sent = Instant::now();
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(jpeg) = &last_jpeg {
                        if last_sent.elapsed() >= STATIC_FRAME_INTERVAL && state.active() {
                            publish(jpeg.clone());
                            last_sent = Instant::now();
                        }
                    } else if began.elapsed() >= FIRST_FRAME_TIMEOUT {
                        return Err("The selected source produced no frames. Make sure it is visible and try again.".into());
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("The capture source stopped sending frames.".into());
                }
            }
        }
        Ok(())
    }

    fn output_size(width: u32, height: u32, quality: Quality) -> (u32, u32) {
        let max_height = quality.height();
        let max_width = (max_height * 16).div_ceil(9);
        let scale = (f64::from(max_width) / f64::from(width))
            .min(f64::from(max_height) / f64::from(height))
            .min(1.0);
        let scaled = |size: u32| ((f64::from(size) * scale).round() as u32).max(1);
        (scaled(width), scaled(height))
    }

    fn encode_frame(image: &RgbaImage, quality: Quality) -> Result<Arc<[u8]>, String> {
        let compression = match quality {
            Quality::Low => 65,
            Quality::Standard => 74,
            Quality::High => 80,
        };
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, compression)
            .encode_image(image)
            .map_err(|error| format!("Couldn't encode the screen frame: {error}"))?;
        Ok(jpeg.into())
    }

    fn pack_rows(
        width: u32,
        height: u32,
        pitch: usize,
        data: &[u8],
        bgra: bool,
    ) -> Option<RgbaImage> {
        let row_bytes = (width as usize).checked_mul(4)?;
        if row_bytes == 0 || height == 0 || pitch < row_bytes {
            return None;
        }
        let count = height as usize;
        let needed = pitch.checked_mul(count - 1)?.checked_add(row_bytes)?;
        if data.len() < needed {
            return None;
        }
        let mut rgba = Vec::with_capacity(row_bytes.checked_mul(count)?);
        for row in data.chunks(pitch).take(count) {
            rgba.extend_from_slice(&row[..row_bytes]);
        }
        for pixel in rgba.as_chunks_mut::<4>().0 {
            if bgra {
                pixel.swap(0, 2);
            }
            pixel[3] = 255;
        }
        RgbaImage::from_raw(width, height, rgba)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn resolution_preserves_aspect_ratio_and_never_upscales() {
            assert_eq!(output_size(3840, 2160, Quality::High), (1920, 1080));
            assert_eq!(output_size(1080, 1920, Quality::Standard), (405, 720));
            assert_eq!(output_size(320, 180, Quality::High), (320, 180));
            assert_eq!(output_size(3440, 1440, Quality::Standard), (1280, 536));
        }

        #[test]
        fn pitched_bgra_rows_are_packed_without_padding() {
            let input = [3, 2, 1, 0, 101, 102, 103, 104, 9, 8, 7, 0];
            let pixels = pack_rows(1, 2, 8, &input, true).unwrap();
            assert_eq!(pixels.as_raw(), &[1, 2, 3, 255, 7, 8, 9, 255]);
            assert!(pack_rows(1, 2, 8, &input[..11], true).is_none());
        }
    }

    #[cfg(target_os = "windows")]
    mod platform {
        use std::sync::mpsc::SyncSender;

        use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
        use windows_capture::frame::Frame as NativeFrame;
        use windows_capture::graphics_capture_api::InternalCaptureControl;
        use windows_capture::monitor::Monitor;
        use windows_capture::settings::{
            ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
            GraphicsCaptureItemType, MinimumUpdateIntervalSettings, SecondaryWindowSettings,
            Settings,
        };
        use windows_capture::window::Window;

        use super::*;

        fn display_id(monitor: &Monitor) -> u32 {
            monitor.as_raw_hmonitor() as usize as u32
        }

        fn window_source(window: &Window) -> Option<Source> {
            let pid = window.process_id().ok()?;
            if pid == std::process::id() {
                return None;
            }
            let title = window.title().ok()?;
            let title = title.trim();
            if title.is_empty() || title == "Program Manager" {
                return None;
            }
            Some(Source {
                id: window.as_raw_hwnd() as usize as u32,
                kind: SourceKind::Window,
                label: title.to_owned(),
            })
        }

        pub(super) fn sources() -> Result<Vec<Source>, String> {
            let monitors =
                Monitor::enumerate().map_err(|error| format!("Couldn't list displays: {error}"))?;
            let mut sources: Vec<_> = monitors
                .iter()
                .enumerate()
                .map(|(index, monitor)| {
                    let display = format!("Display {}", index + 1);
                    let label = match (monitor.width(), monitor.height()) {
                        (Ok(width), Ok(height)) => format!("{display} · {width} × {height}"),
                        _ => display,
                    };
                    Source {
                        id: display_id(monitor),
                        kind: SourceKind::Display,
                        label,
                    }
                })
                .collect();
            if let Ok(windows) = Window::enumerate() {
                sources.extend(windows.iter().filter_map(window_source));
            }
            Ok(sources)
        }

        pub(super) fn capture(
            source: &Source,
            quality: Quality,
            state: &Arc<State>,
            frames: SyncSender<RgbaImage>,
            consume: impl FnOnce(&dyn Fn() -> bool) -> Result<(), String>,
        ) -> Result<(), String> {
            match source.kind {
                SourceKind::Display => {
                    let monitor = Monitor::enumerate()
                        .map_err(|error| format!("Couldn't list displays: {error}"))?
                        .into_iter()
                        .find(|display| display_id(display) == source.id)
                        .ok_or_else(|| SOURCE_GONE.to_owned())?;
                    stream(monitor, state, quality, frames, consume)
                }
                SourceKind::Window => {
                    let window = Window::enumerate()
                        .map_err(|error| format!("Couldn't list windows: {error}"))?
                        .into_iter()
                        .find(|window| window_source(window).as_ref() == Some(source))
                        .ok_or_else(|| SOURCE_GONE.to_owned())?;
                    stream(window, state, quality, frames, consume)
                }
            }
        }

        fn stream<T: TryInto<GraphicsCaptureItemType> + Send + 'static>(
            item: T,
            state: &Arc<State>,
            quality: Quality,
            frames: SyncSender<RgbaImage>,
            consume: impl FnOnce(&dyn Fn() -> bool) -> Result<(), String>,
        ) -> Result<(), String> {
            let settings = Settings::new(
                item,
                CursorCaptureSettings::Default,
                DrawBorderSettings::Default,
                SecondaryWindowSettings::Default,
                MinimumUpdateIntervalSettings::Default,
                DirtyRegionSettings::Default,
                ColorFormat::Rgba8,
                (state.clone(), frames, quality),
            );
            if !state.active() {
                return Ok(());
            }
            let control = Handler::start_free_threaded(settings)
                .map_err(|error| format!("Couldn't start Windows Graphics Capture: {error}"))?;
            let result = consume(&|| !control.is_finished());
            let _ = control.stop();
            result
        }

        struct Handler {
            state: Arc<State>,
            frames: SyncSender<RgbaImage>,
            interval: Duration,
            next_frame: Instant,
        }

        impl GraphicsCaptureApiHandler for Handler {
            type Flags = (Arc<State>, SyncSender<RgbaImage>, Quality);
            type Error = String;

            fn new(context: Context<Self::Flags>) -> Result<Self, Self::Error> {
                let (state, frames, quality) = context.flags;
                Ok(Self {
                    state,
                    frames,
                    interval: Duration::from_secs_f64(1.0 / f64::from(quality.fps())),
                    next_frame: Instant::now(),
                })
            }

            fn on_frame_arrived(
                &mut self,
                frame: &mut NativeFrame,
                _control: InternalCaptureControl,
            ) -> Result<(), Self::Error> {
                let now = Instant::now();
                if !self.state.active() || now + Duration::from_millis(1) < self.next_frame {
                    return Ok(());
                }
                self.next_frame = (self.next_frame + self.interval).max(now);

                match frame.buffer() {
                    Ok(mut buffer) => {
                        let (width, height, pitch) =
                            (buffer.width(), buffer.height(), buffer.row_pitch());
                        if let Some(image) =
                            pack_rows(width, height, pitch as usize, buffer.as_raw_buffer(), false)
                        {
                            let _ = self.frames.try_send(image);
                        }
                    }
                    Err(error) => self
                        .state
                        .set_error(format!("Couldn't read the Windows screen frame: {error}")),
                }
                Ok(())
            }

            fn on_closed(&mut self) -> Result<(), Self::Error> {
                self.state
                    .set_error("The shared display or window was closed.".into());
                Ok(())
            }
        }
    }

    #[cfg(target_os = "macos")]
    mod platform {
        use std::sync::mpsc::SyncSender;

        use screencapturekit::cv::CVPixelBufferLockFlags;
        use screencapturekit::prelude::*;

        use super::*;

        fn shareable_content() -> Result<SCShareableContent, String> {
            SCShareableContent::create()
                .with_exclude_desktop_windows(true)
                .with_on_screen_windows_only(true)
                .get()
                .map_err(|error| format!("Couldn't list capture sources. Allow Arcane Portal in System Settings → Privacy & Security → Screen & System Audio Recording, then relaunch the app. {error}"))
        }

        fn window_source(window: &SCWindow) -> Option<Source> {
            let app = window.owning_application()?;
            let bounds = window.frame().size;
            if app.process_id() as u32 == std::process::id()
                || !window.is_on_screen()
                || window.window_layer() != 0
                || bounds.width < 100.0
                || bounds.height < 60.0
            {
                return None;
            }
            let app_name = app.application_name();
            let title = window.title().unwrap_or_default();
            let label = if title.trim().is_empty() || title == app_name {
                app_name
            } else {
                format!("{app_name} · {title}")
            };
            Some(Source {
                kind: SourceKind::Window,
                id: window.window_id(),
                label,
            })
        }

        pub(super) fn sources() -> Result<Vec<Source>, String> {
            let content = shareable_content()?;
            let mut sources: Vec<_> = content
                .displays()
                .iter()
                .enumerate()
                .map(|(index, display)| Source {
                    id: display.display_id(),
                    kind: SourceKind::Display,
                    label: format!(
                        "Display {} · {} × {}",
                        index + 1,
                        display.width(),
                        display.height()
                    ),
                })
                .collect();
            sources.extend(content.windows().iter().filter_map(window_source));
            Ok(sources)
        }

        pub(super) fn capture(
            source: &Source,
            quality: Quality,
            state: &Arc<State>,
            frames: SyncSender<RgbaImage>,
            consume: impl FnOnce(&dyn Fn() -> bool) -> Result<(), String>,
        ) -> Result<(), String> {
            let content = shareable_content()?;
            let (filter, width, height) = match source.kind {
                SourceKind::Display => {
                    let display = content
                        .displays()
                        .into_iter()
                        .find(|display| display.display_id() == source.id)
                        .ok_or_else(|| SOURCE_GONE.to_owned())?;
                    let filter = SCContentFilter::create()
                        .with_display(&display)
                        .with_excluding_windows(&[])
                        .build();
                    (filter, display.width(), display.height())
                }
                SourceKind::Window => {
                    let window = content
                        .windows()
                        .into_iter()
                        .find(|window| window_source(window).as_ref() == Some(source))
                        .ok_or_else(|| SOURCE_GONE.to_owned())?;
                    let size = window.frame().size;
                    let filter = SCContentFilter::create().with_window(&window).build();
                    (
                        filter,
                        size.width.round() as u32,
                        size.height.round() as u32,
                    )
                }
            };
            let filter =
                filter.map_err(|error| format!("Couldn't select the capture source: {error}"))?;
            let (width, height) = output_size(width, height, quality);
            let config = SCStreamConfiguration::new()
                .with_width(width)
                .with_height(height)
                .with_fps(quality.fps())
                .with_queue_depth(3)
                .with_shows_cursor(true)
                .with_pixel_format(PixelFormat::BGRA);
            let error_state = state.clone();
            let errors = ErrorHandler::new(move |error| {
                error_state.set_error(format!("Screen capture stopped: {error}"))
            });
            let mut stream = SCStream::new_with_delegate(&filter, &config, errors)
                .map_err(|error| format!("Couldn't prepare ScreenCaptureKit: {error}"))?;
            let frame_state = state.clone();
            stream
                .add_output_handler(
                    move |sample: CMSampleBuffer, kind| {
                        if kind == SCStreamOutputType::Screen
                            && frame_state.active()
                            && let Some(image) = sample_image(&sample)
                        {
                            let _ = frames.try_send(image);
                        }
                    },
                    SCStreamOutputType::Screen,
                )
                .map_err(|error| format!("Couldn't prepare screen frames: {error}"))?;
            if !state.active() {
                return Ok(());
            }
            stream
                .start_capture()
                .map_err(|error| format!("Couldn't start ScreenCaptureKit: {error}"))?;
            let result = consume(&|| true);
            let _ = stream.stop_capture();
            result
        }

        fn sample_image(sample: &CMSampleBuffer) -> Option<RgbaImage> {
            if sample
                .frame_status()
                .is_some_and(|status| !status.has_content())
            {
                return None;
            }
            let pixels = sample.pixel_buffer()?;
            let lock = pixels.lock(CVPixelBufferLockFlags::READ_ONLY).ok()?;
            // The BGRA buffer is packed into owned memory while it is locked for reading.
            let data = unsafe { lock.as_slice()? };
            pack_rows(
                lock.width() as u32,
                lock.height() as u32,
                lock.bytes_per_row(),
                data,
                true,
            )
        }
    }
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
pub use native::{Capture, sources};

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn sources() -> Result<Vec<Source>, String> {
    Err("Screen sharing is available on Windows and macOS.".into())
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub struct Capture;

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
impl Capture {
    pub fn start(
        _source: Source,
        _quality: Quality,
        _publish: impl Fn(Arc<[u8]>) + Send + 'static,
    ) -> Result<Self, String> {
        Err("Screen sharing is available on Windows and macOS.".into())
    }

    pub fn frame(&self) -> Option<Arc<Frame>> {
        None
    }

    pub fn error(&self) -> Option<String> {
        None
    }
}
