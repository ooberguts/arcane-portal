use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::Frame;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub enum Resolution {
    #[default]
    Auto,
    P480,
    P720,
    P1080,
}

impl Resolution {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::P480 => "480p",
            Self::P720 => "720p",
            Self::P1080 => "1080p",
        }
    }

    fn initial_height(self) -> u32 {
        match self {
            Self::P480 => 480,
            Self::Auto | Self::P720 => 720,
            Self::P1080 => 1080,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub enum FrameRate {
    #[default]
    Auto,
    Fps15,
    Fps24,
    Fps30,
    Fps60,
}

impl FrameRate {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Fps15 => "15 fps",
            Self::Fps24 => "24 fps",
            Self::Fps30 => "30 fps",
            Self::Fps60 => "60 fps",
        }
    }

    fn initial_fps(self) -> u32 {
        match self {
            Self::Fps15 => 15,
            Self::Fps24 => 24,
            Self::Auto | Self::Fps30 => 30,
            Self::Fps60 => 60,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct VideoSettings {
    pub resolution: Resolution,
    pub frame_rate: FrameRate,
}

impl VideoSettings {
    pub fn initial_format(self) -> (u32, u32) {
        (
            self.resolution.initial_height(),
            self.frame_rate.initial_fps(),
        )
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Condvar, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use fast_image_resize::{FilterType, ResizeAlg, ResizeOptions, Resizer};
    use image::RgbaImage;
    use jpeg_encoder::{ColorType, Encoder, SamplingFactor};

    use super::*;

    const CAPTURE_POLL: Duration = Duration::from_millis(100);
    const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(8);
    const STATIC_FRAME_INTERVAL: Duration = Duration::from_secs(2);
    const AUTO_COOLDOWN: Duration = Duration::from_secs(1);
    const AUTO_UPGRADE_COOLDOWN: Duration = Duration::from_secs(3);
    const NETWORK_DOWNGRADE_DELAY: Duration = Duration::from_secs(2);
    const NETWORK_UPGRADE_DELAY: Duration = Duration::from_secs(12);
    const RESOLUTIONS: [u32; 3] = [480, 720, 1080];
    const FRAME_RATES: [u32; 4] = [15, 24, 30, 60];
    const SOURCE_GONE: &str =
        "The selected source changed or closed. Refresh the list and select it again.";

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Format {
        height: u32,
        fps: u32,
    }

    struct Controls {
        settings: VideoSettings,
        format: Format,
        network_fps_limit: Option<u32>,
        encode_seconds: f64,
        samples: u8,
        last_change: Instant,
        last_network_pressure: Option<Instant>,
        bandwidth_limited_since: Option<Instant>,
    }

    impl Controls {
        fn new(settings: VideoSettings) -> Self {
            let (height, fps) = settings.initial_format();
            Self {
                settings,
                format: Format { height, fps },
                network_fps_limit: None,
                encode_seconds: 0.0,
                samples: 0,
                last_change: Instant::now(),
                last_network_pressure: None,
                bandwidth_limited_since: None,
            }
        }

        fn set_settings(&mut self, settings: VideoSettings) {
            let mut format = self.format;
            if self.settings.resolution != settings.resolution {
                format.height = settings.resolution.initial_height();
            }
            if self.settings.frame_rate != settings.frame_rate {
                format.fps = settings.frame_rate.initial_fps();
            }
            if settings != self.settings {
                self.settings = settings;
                self.set_format(format, Instant::now());
            }
        }

        fn set_format(&mut self, format: Format, now: Instant) {
            self.format = format;
            self.samples = 0;
            self.last_change = now;
            self.bandwidth_limited_since = None;
        }

        fn record_delivery(
            &mut self,
            format: Format,
            elapsed: Option<Duration>,
            now: Instant,
            congested: bool,
            bandwidth_limited: bool,
        ) {
            if self.format != format {
                return;
            }
            if let Some(elapsed) = elapsed {
                let seconds = elapsed.as_secs_f64();
                self.encode_seconds = if self.samples == 0 {
                    seconds
                } else {
                    self.encode_seconds * 0.8 + seconds * 0.2
                };
                self.samples = self.samples.saturating_add(1);
            }
            if congested {
                self.last_network_pressure = Some(now);
            }
            if bandwidth_limited {
                self.bandwidth_limited_since.get_or_insert(now);
            } else {
                self.bandwidth_limited_since = None;
            }
            let age = now.saturating_duration_since(self.last_change);
            if self
                .bandwidth_limited_since
                .is_some_and(|at| now.saturating_duration_since(at) >= NETWORK_DOWNGRADE_DELAY)
            {
                let target = lower_network_format(self.settings, format);
                if target != format {
                    self.set_format(target, now);
                    return;
                }
            }
            if self.samples < 8 || age < AUTO_COOLDOWN {
                return;
            }

            // Leave a little CPU headroom for scheduling and preview work.
            let mut capacity = 0.85 / self.encode_seconds.max(0.001);
            if let Some(network) = self.network_fps_limit {
                capacity = capacity.min(f64::from(network));
            }
            let target = adaptive_format(
                self.settings,
                format,
                capacity,
                age >= AUTO_UPGRADE_COOLDOWN
                    && self.last_network_pressure.is_none_or(|at| {
                        now.saturating_duration_since(at) >= NETWORK_UPGRADE_DELAY
                    }),
            );
            if target != format {
                self.set_format(target, now);
            }
        }
    }

    fn lower_network_format(settings: VideoSettings, format: Format) -> Format {
        if settings.resolution == Resolution::Auto
            && let Some(height) = RESOLUTIONS
                .into_iter()
                .rev()
                .find(|height| *height < format.height)
        {
            return Format { height, ..format };
        }
        if settings.frame_rate == FrameRate::Auto
            && let Some(fps) = FRAME_RATES.into_iter().rev().find(|fps| *fps < format.fps)
        {
            return Format { fps, ..format };
        }
        format
    }

    fn adaptive_format(
        settings: VideoSettings,
        format: Format,
        capacity: f64,
        can_upgrade: bool,
    ) -> Format {
        let auto_resolution = settings.resolution == Resolution::Auto;
        let auto_fps = settings.frame_rate == FrameRate::Auto;
        let target = f64::from(format.fps);
        let minimum_smooth_fps = if auto_fps { target.min(30.0) } else { target };
        let smaller = RESOLUTIONS
            .into_iter()
            .rev()
            .find(|height| *height < format.height);
        if auto_resolution
            && capacity < minimum_smooth_fps * 0.95
            && let Some(height) = smaller
        {
            return Format { height, ..format };
        }
        if auto_fps && capacity < target * 0.95 {
            return Format {
                fps: sustainable_fps(capacity),
                ..format
            };
        }
        if !can_upgrade {
            return format;
        }
        if auto_resolution
            && let Some(height) = RESOLUTIONS
                .into_iter()
                .find(|height| *height > format.height)
        {
            // Require enough encoding headroom to raise the resolution at the current frame rate.
            let projected = capacity * (f64::from(format.height) / f64::from(height)).powi(2);
            if projected >= target * 1.2 {
                return Format { height, ..format };
            }
        }
        if auto_fps
            && let Some(fps) = FRAME_RATES.into_iter().find(|fps| *fps > format.fps)
            && capacity >= f64::from(fps) * 1.2
        {
            return Format { fps, ..format };
        }
        format
    }

    fn sustainable_fps(capacity: f64) -> u32 {
        FRAME_RATES
            .into_iter()
            .rev()
            .find(|fps| f64::from(*fps) <= capacity)
            .unwrap_or(15)
    }

    struct State {
        cancelled: AtomicBool,
        frame: Mutex<Option<Arc<Frame>>>,
        error: Mutex<Option<String>>,
        controls: Mutex<Controls>,
    }

    impl State {
        fn new(settings: VideoSettings) -> Self {
            Self {
                cancelled: AtomicBool::new(false),
                frame: Mutex::new(None),
                error: Mutex::new(None),
                controls: Mutex::new(Controls::new(settings)),
            }
        }

        fn set_error(&self, message: String) {
            let mut error = self.error.lock().unwrap();
            error.get_or_insert(message);
        }

        fn active(&self) -> bool {
            !self.cancelled.load(Ordering::Acquire) && self.error.lock().unwrap().is_none()
        }

        fn format(&self) -> Format {
            self.controls.lock().unwrap().format
        }
    }

    struct CapturedFrame {
        image: RgbaImage,
        read_time: Duration,
    }

    // A slow encoder should always pick up the newest capture rather than a queued stale one.
    #[derive(Default)]
    struct LatestFrame {
        frame: Mutex<Option<CapturedFrame>>,
        ready: Condvar,
    }

    impl LatestFrame {
        fn put(&self, frame: CapturedFrame) {
            *self.frame.lock().unwrap() = Some(frame);
            self.ready.notify_one();
        }

        fn take(&self, timeout: Duration) -> Option<CapturedFrame> {
            let frame = self.frame.lock().unwrap();
            let (mut frame, _) = self
                .ready
                .wait_timeout_while(frame, timeout, |frame| frame.is_none())
                .unwrap();
            frame.take()
        }
    }

    #[derive(Default)]
    struct FramePacer {
        next: Option<Instant>,
        fps: u32,
    }

    impl FramePacer {
        fn permits(&mut self, now: Instant, fps: u32) -> bool {
            let interval = Duration::from_secs_f64(1.0 / f64::from(fps));
            if self.fps == fps
                && let Some(next) = self.next
            {
                if now + Duration::from_millis(1) < next {
                    return false;
                }
                self.next = Some((next + interval).max(now + Duration::from_millis(1)));
            } else {
                self.next = Some(now + interval);
            }
            self.fps = fps;
            true
        }
    }

    pub struct Capture {
        state: Arc<State>,
    }

    impl Capture {
        pub fn start(
            source: Source,
            settings: VideoSettings,
            publisher: crate::network::VideoPublisher,
        ) -> Result<Self, String> {
            let state = Arc::new(State::new(settings));
            let capture_state = state.clone();
            thread::Builder::new()
                .name("arcane-capture".into())
                .spawn(move || {
                    if !capture_state.active() {
                        return;
                    }
                    let frames = Arc::new(LatestFrame::default());
                    let result = platform::capture(
                        &source,
                        &capture_state,
                        frames.clone(),
                        |native_running| {
                            publish_frames(
                                &frames,
                                &capture_state,
                                native_running,
                                || publisher.feedback(),
                                |motion, jpeg| publisher.publish(motion, jpeg),
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

        pub fn set_settings(&self, settings: VideoSettings) {
            self.state.controls.lock().unwrap().set_settings(settings);
        }

        pub fn set_network_fps_limit(&self, fps_limit: Option<u32>) {
            self.state.controls.lock().unwrap().network_fps_limit = fps_limit;
        }

        pub fn format(&self) -> (u32, u32) {
            let format = self.state.format();
            (format.height, format.fps)
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
        receiver: &LatestFrame,
        state: &State,
        native_running: &dyn Fn() -> bool,
        feedback: impl Fn() -> crate::video::Feedback,
        publish: impl Fn(Option<crate::video::Packet>, Option<Arc<[u8]>>),
    ) -> Result<(), String> {
        let began = Instant::now();
        let mut next_frame = began;
        let mut last_legacy_sent = began;
        let mut source = None::<CapturedFrame>;
        let mut source_changed = false;
        let mut last_height = None;
        let mut preview = None::<Arc<Frame>>;
        let mut last_jpeg = None::<Arc<[u8]>>;
        let mut resizer = Resizer::new();
        let resize_options = ResizeOptions::new()
            .resize_alg(ResizeAlg::Convolution(FilterType::CatmullRom))
            .use_alpha(false);
        let mut encoder = crate::video::Encoder::default();

        while state.active() {
            if !native_running() {
                return Err("The screen capture session ended.".into());
            }
            let wait = if source.is_some() {
                next_frame
                    .saturating_duration_since(Instant::now())
                    .min(CAPTURE_POLL)
            } else {
                CAPTURE_POLL
            };
            if let Some(frame) = receiver.take(wait) {
                source = Some(frame);
                source_changed = true;
            }
            let Some(source) = &source else {
                if began.elapsed() >= FIRST_FRAME_TIMEOUT {
                    return Err("The selected source produced no frames. Make sure it is visible and try again.".into());
                }
                continue;
            };
            if !state.active() {
                break;
            }
            let format = state.format();
            let started = Instant::now();
            if started < next_frame && last_height == Some(format.height) {
                continue;
            }
            let picture_changed = source_changed || last_height != Some(format.height);
            if picture_changed {
                let image = &source.image;
                let (width, height) = output_size(image.width(), image.height(), format.height);
                let (width, height) = ((width & !1).max(2), (height & !1).max(2));
                let rgba = if image.dimensions() == (width, height) {
                    image.as_raw().clone()
                } else {
                    let mut resized = RgbaImage::new(width, height);
                    resizer
                        .resize(image, &mut resized, &resize_options)
                        .map_err(|error| format!("Couldn't resize the screen frame: {error}"))?;
                    resized.into_raw()
                };
                encoder.update_picture(width, height, &rgba);
                let frame = Arc::new(Frame {
                    width,
                    height,
                    rgba,
                });
                *state.frame.lock().unwrap() = Some(frame.clone());
                preview = Some(frame);
                last_height = Some(format.height);
            }
            let feedback = feedback();
            let packet = encoder.encode(
                format.fps,
                started.duration_since(began).as_millis() as u64,
                started,
                &feedback,
            )?;
            let mut legacy = None;
            if feedback.legacy_viewers {
                if picture_changed || last_jpeg.is_none() {
                    last_jpeg = Some(encode_frame(preview.as_ref().unwrap(), format.height)?);
                }
                if picture_changed
                    || last_legacy_sent == began
                    || last_legacy_sent.elapsed() >= STATIC_FRAME_INTERVAL
                {
                    legacy = last_jpeg.clone();
                    last_legacy_sent = started;
                }
            } else {
                last_jpeg = None;
                last_legacy_sent = began;
            }
            if !state.active() {
                break;
            }
            publish(packet, legacy);
            let interval = Duration::from_secs_f64(1.0 / f64::from(format.fps));
            next_frame = (next_frame + interval).max(started + interval / 2);
            // Capture readback and encoding run on separate workers.
            state.controls.lock().unwrap().record_delivery(
                format,
                source_changed.then(|| started.elapsed().max(source.read_time)),
                Instant::now(),
                feedback.congested,
                encoder.bandwidth_limited(&feedback),
            );
            source_changed = false;
        }
        Ok(())
    }

    fn output_size(width: u32, height: u32, max_height: u32) -> (u32, u32) {
        let max_width = (max_height * 16).div_ceil(9);
        let scale = (f64::from(max_width) / f64::from(width))
            .min(f64::from(max_height) / f64::from(height))
            .min(1.0);
        let scaled = |size: u32| ((f64::from(size) * scale).round() as u32).max(1);
        (scaled(width), scaled(height))
    }

    fn encode_frame(frame: &Frame, height: u32) -> Result<Arc<[u8]>, String> {
        let compression = match height {
            480 => 65,
            720 => 74,
            _ => 80,
        };
        let mut jpeg = Vec::new();
        let width = u16::try_from(frame.width)
            .map_err(|_| "Screen frame is too wide to encode".to_owned())?;
        let height = u16::try_from(frame.height)
            .map_err(|_| "Screen frame is too tall to encode".to_owned())?;
        let mut encoder = Encoder::new(&mut jpeg, compression);
        // Preserve vertical chroma detail for small text while reducing bandwidth and CPU work.
        encoder.set_sampling_factor(SamplingFactor::F_2_1);
        encoder
            .encode(&frame.rgba, width, height, ColorType::Rgba)
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
            assert_eq!(output_size(3840, 2160, 1080), (1920, 1080));
            assert_eq!(output_size(1080, 1920, 720), (405, 720));
            assert_eq!(output_size(320, 180, 1080), (320, 180));
            assert_eq!(output_size(3440, 1440, 720), (1280, 536));
        }

        #[test]
        fn auto_adapts_only_the_selected_fields() {
            let high = Format {
                height: 1080,
                fps: 60,
            };
            let detailed = VideoSettings {
                resolution: Resolution::P1080,
                frame_rate: FrameRate::Auto,
            };
            assert_eq!(
                adaptive_format(detailed, high, 25.0, false),
                Format { fps: 24, ..high }
            );
            let smooth = VideoSettings {
                resolution: Resolution::Auto,
                frame_rate: FrameRate::Fps60,
            };
            assert_eq!(
                adaptive_format(smooth, high, 25.0, false),
                Format {
                    height: 720,
                    ..high
                }
            );
            let manual = VideoSettings {
                resolution: Resolution::P1080,
                frame_rate: FrameRate::Fps60,
            };
            assert_eq!(adaptive_format(manual, high, 10.0, true), high);
        }

        #[test]
        fn auto_waits_for_headroom_before_raising_quality() {
            let settings = VideoSettings::default();
            let nominal = Format {
                height: 720,
                fps: 24,
            };
            assert_eq!(adaptive_format(settings, nominal, 110.0, false), nominal);
            assert_eq!(
                adaptive_format(settings, nominal, 110.0, true),
                Format {
                    height: 1080,
                    ..nominal
                }
            );
            assert_eq!(
                adaptive_format(settings, nominal, 40.0, true),
                Format { fps: 30, ..nominal }
            );
        }

        #[test]
        fn network_pressure_lowers_auto_resolution_before_frame_rate() {
            let mut controls = Controls::new(VideoSettings::default());
            let now = Instant::now();
            let original = controls.format;
            assert_eq!(
                original,
                Format {
                    height: 720,
                    fps: 30
                }
            );
            controls.record_delivery(original, None, now, true, true);
            assert_eq!(controls.format, original);
            let down_at = now + NETWORK_DOWNGRADE_DELAY;
            controls.record_delivery(original, None, down_at, true, true);
            let reduced = Format {
                height: 480,
                fps: 30,
            };
            assert_eq!(controls.format, reduced);
            for second in 1..=10 {
                controls.record_delivery(
                    reduced,
                    Some(Duration::from_millis(2)),
                    down_at + Duration::from_secs(second),
                    false,
                    false,
                );
            }
            assert_eq!(controls.format, reduced);
            controls.record_delivery(
                reduced,
                Some(Duration::from_millis(2)),
                down_at + NETWORK_UPGRADE_DELAY,
                false,
                false,
            );
            assert_eq!(controls.format, original);
        }

        #[test]
        fn settings_apply_to_a_static_source_without_restarting_capture() {
            let state = Arc::new(State::new(VideoSettings {
                resolution: Resolution::P720,
                frame_rate: FrameRate::Fps15,
            }));
            let capture = Capture {
                state: state.clone(),
            };
            let frames = Arc::new(LatestFrame::default());
            let receiver = frames.clone();
            let (published, publication) = std::sync::mpsc::channel();
            let worker = thread::spawn(move || {
                publish_frames(
                    &receiver,
                    &state,
                    &|| true,
                    crate::video::Feedback::default,
                    |packet, _| {
                        if let Some(packet) = packet {
                            let _ = published.send(packet);
                        }
                    },
                )
            });
            frames.put(CapturedFrame {
                image: RgbaImage::from_pixel(1280, 720, image::Rgba([88, 34, 130, 255])),
                read_time: Duration::ZERO,
            });
            let first = publication.recv_timeout(Duration::from_secs(3)).unwrap();
            assert!(first.keyframe);
            let mut decoder = openh264::decoder::Decoder::new().unwrap();
            assert_eq!(
                crate::video::decode(&mut decoder, &first.bytes)
                    .unwrap()
                    .unwrap()
                    .height,
                720
            );

            capture.set_settings(VideoSettings {
                resolution: Resolution::P480,
                frame_rate: FrameRate::Fps60,
            });
            assert_eq!(capture.format(), (480, 60));
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let packet = publication
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .unwrap();
                if crate::video::decode(&mut decoder, &packet.bytes)
                    .unwrap()
                    .is_some_and(|frame| frame.height == 480)
                {
                    break;
                }
            }

            drop(capture);
            worker.join().unwrap().unwrap();
            drop(frames);
        }

        #[test]
        fn slow_encoder_receives_latest_frame() {
            let frames = LatestFrame::default();
            for value in [1, 2, 3] {
                frames.put(CapturedFrame {
                    image: RgbaImage::from_pixel(1, 1, image::Rgba([value, 0, 0, 255])),
                    read_time: Duration::ZERO,
                });
            }
            assert_eq!(
                frames.take(Duration::ZERO).unwrap().image.get_pixel(0, 0)[0],
                3
            );
            assert!(frames.take(Duration::ZERO).is_none());
        }

        #[test]
        fn current_pacing_changes_as_soon_as_fps_changes() {
            let start = Instant::now();
            let mut pace = FramePacer::default();
            assert!(pace.permits(start, 15));
            assert!(!pace.permits(start + Duration::from_millis(20), 15));
            assert!(pace.permits(start + Duration::from_millis(20), 60));
            assert!(pace.permits(start + Duration::from_millis(37), 60));
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
            state: &Arc<State>,
            frames: Arc<LatestFrame>,
            consume: impl FnOnce(&dyn Fn() -> bool) -> Result<(), String>,
        ) -> Result<(), String> {
            match source.kind {
                SourceKind::Display => {
                    let monitor = Monitor::enumerate()
                        .map_err(|error| format!("Couldn't list displays: {error}"))?
                        .into_iter()
                        .find(|display| display_id(display) == source.id)
                        .ok_or_else(|| SOURCE_GONE.to_owned())?;
                    stream(monitor, state, frames, consume)
                }
                SourceKind::Window => {
                    let window = Window::enumerate()
                        .map_err(|error| format!("Couldn't list windows: {error}"))?
                        .into_iter()
                        .find(|window| window_source(window).as_ref() == Some(source))
                        .ok_or_else(|| SOURCE_GONE.to_owned())?;
                    stream(window, state, frames, consume)
                }
            }
        }

        fn stream<T: TryInto<GraphicsCaptureItemType> + Send + 'static>(
            item: T,
            state: &Arc<State>,
            frames: Arc<LatestFrame>,
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
                (state.clone(), frames),
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
            frames: Arc<LatestFrame>,
            pacing: FramePacer,
        }

        impl GraphicsCaptureApiHandler for Handler {
            type Flags = (Arc<State>, Arc<LatestFrame>);
            type Error = String;

            fn new(context: Context<Self::Flags>) -> Result<Self, Self::Error> {
                let (state, frames) = context.flags;
                Ok(Self {
                    state,
                    frames,
                    pacing: FramePacer::default(),
                })
            }

            fn on_frame_arrived(
                &mut self,
                frame: &mut NativeFrame,
                _control: InternalCaptureControl,
            ) -> Result<(), Self::Error> {
                let now = Instant::now();
                if !self.state.active() || !self.pacing.permits(now, self.state.format().fps) {
                    return Ok(());
                }

                match frame.buffer() {
                    Ok(mut buffer) => {
                        let (width, height, pitch) =
                            (buffer.width(), buffer.height(), buffer.row_pitch());
                        if let Some(image) =
                            pack_rows(width, height, pitch as usize, buffer.as_raw_buffer(), false)
                        {
                            self.frames.put(CapturedFrame {
                                image,
                                read_time: now.elapsed(),
                            });
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
            state: &Arc<State>,
            frames: Arc<LatestFrame>,
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
            // Keep the native session at the supported ceiling, so either setting can change live.
            let (width, height) = output_size(width, height, Resolution::P1080.initial_height());
            let config = SCStreamConfiguration::new()
                .with_width(width)
                .with_height(height)
                .with_fps(FrameRate::Fps60.initial_fps())
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
            let pacing = Mutex::new(FramePacer::default());
            stream
                .add_output_handler(
                    move |sample: CMSampleBuffer, kind| {
                        let now = Instant::now();
                        if kind == SCStreamOutputType::Screen
                            && frame_state.active()
                            && sample
                                .frame_status()
                                .is_none_or(|status| status.has_content())
                            && pacing
                                .lock()
                                .unwrap()
                                .permits(now, frame_state.format().fps)
                            && let Some(image) = sample_image(&sample)
                        {
                            frames.put(CapturedFrame {
                                image,
                                read_time: now.elapsed(),
                            });
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
        _settings: VideoSettings,
        _publisher: crate::network::VideoPublisher,
    ) -> Result<Self, String> {
        Err("Screen sharing is available on Windows and macOS.".into())
    }

    pub fn frame(&self) -> Option<Arc<Frame>> {
        None
    }

    pub fn error(&self) -> Option<String> {
        None
    }

    pub fn set_settings(&self, _settings: VideoSettings) {}

    pub fn set_network_fps_limit(&self, _fps_limit: Option<u32>) {}

    pub fn format(&self) -> (u32, u32) {
        VideoSettings::default().initial_format()
    }
}
