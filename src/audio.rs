//! System-output audio in a fixed 48 kHz, stereo, signed 16-bit LE wire format.
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, mpsc},
    thread,
};

pub const SAMPLE_RATE: u32 = 48_000;
pub const PACKET_BYTES: usize = 960 * 2 * 2;
type SharedError = Arc<Mutex<Option<String>>>;

fn set_error(error: &SharedError, message: String) {
    if let Ok(mut slot) = error.lock() {
        *slot = Some(message);
    }
}
fn read_error(error: &SharedError) -> Option<String> {
    error.lock().ok().and_then(|e| e.clone())
}

// Fractional phase is kept across callback boundaries, avoiding drift at non-48 kHz rates.
struct Packetizer<F> {
    publish: F,
    bytes: Vec<u8>,
    phase: u64,
    source_rate: u32,
}
impl<F: Fn(Arc<[u8]>)> Packetizer<F> {
    fn new(publish: F, source_rate: u32) -> Self {
        Self {
            publish,
            bytes: Vec::with_capacity(PACKET_BYTES),
            phase: 0,
            source_rate,
        }
    }
    fn frame(&mut self, left: f32, right: f32) {
        self.phase += SAMPLE_RATE as u64;
        while self.phase >= self.source_rate as u64 {
            self.phase -= self.source_rate as u64;
            for x in [left, right] {
                let n = (x.clamp(-1.0, 1.0) * 32767.0).round() as i16;
                self.bytes.extend_from_slice(&n.to_le_bytes());
            }
            if self.bytes.len() == PACKET_BYTES {
                (self.publish)(Arc::from(std::mem::replace(
                    &mut self.bytes,
                    Vec::with_capacity(PACKET_BYTES),
                )));
            }
        }
    }
}

pub struct AudioCapture {
    stop: Option<mpsc::Sender<()>>,
    error: SharedError,
}
impl AudioCapture {
    pub fn start(publish: impl Fn(Arc<[u8]>) + Send + 'static) -> Result<Self, String> {
        let error = Arc::new(Mutex::new(None));
        let (stop, stopped) = mpsc::channel();
        let (ready, started) = mpsc::sync_channel(1);
        let worker_error = error.clone();
        thread::Builder::new()
            .name("system-audio-capture".into())
            .spawn(move || {
                capture_worker(publish, worker_error, ready, stopped);
            })
            .map_err(|e| format!("Couldn't start audio capture thread: {e}"))?;
        started
            .recv()
            .map_err(|_| "Audio capture stopped during startup".to_owned())??;
        Ok(Self {
            stop: Some(stop),
            error,
        })
    }
    pub fn error(&self) -> Option<String> {
        read_error(&self.error)
    }
}
impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop.take();
    }
}

#[cfg(target_os = "windows")]
fn capture_worker<F: Fn(Arc<[u8]>) + Send + 'static>(
    publish: F,
    error: SharedError,
    ready: mpsc::SyncSender<Result<(), String>>,
    stopped: mpsc::Receiver<()>,
) {
    let start = || -> Result<cpal::Stream, String> {
        let device = cpal::default_host()
            .default_output_device()
            .ok_or("No system audio output device is available")?;
        let config = device.default_output_config().map_err(|e| e.to_string())?;
        let channels = config.channels() as usize;
        let rate = config.sample_rate().0;
        let mut packetizer = Packetizer::new(publish, rate);
        let err = move |e| set_error(&error, format!("System audio capture stopped: {e}"));
        macro_rules! input {
            ($ty:ty, $convert:expr) => {{
                device.build_input_stream(
                    &config.clone().into(),
                    move |data: &[$ty], _| {
                        for frame in data.chunks_exact(channels) {
                            let left = ($convert)(frame[0]);
                            let right = if channels > 1 {
                                ($convert)(frame[1])
                            } else {
                                left
                            };
                            packetizer.frame(left, right);
                        }
                    },
                    err,
                    None,
                )
            }};
        }
        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => input!(f32, |x: f32| x),
            cpal::SampleFormat::F64 => input!(f64, |x: f64| x as f32),
            cpal::SampleFormat::I32 => input!(i32, |x: i32| x as f32 / 2147483648.0),
            cpal::SampleFormat::U32 => input!(u32, |x: u32| (x as f64 / 2147483648.0 - 1.0) as f32),
            cpal::SampleFormat::I16 => input!(i16, |x: i16| x as f32 / 32768.0),
            cpal::SampleFormat::U16 => input!(u16, |x: u16| (x as f32 - 32768.0) / 32768.0),
            other => return Err(format!("Unsupported system audio sample format: {other:?}")),
        }
        .map_err(|e| format!("Couldn't capture system audio: {e}"))?;
        stream
            .play()
            .map_err(|e| format!("Couldn't start system audio: {e}"))?;
        Ok(stream)
    };
    match start() {
        Ok(_stream) => {
            let _ = ready.send(Ok(()));
            let _ = stopped.recv();
        }
        Err(e) => {
            let _ = ready.send(Err(e));
        }
    }
}

#[cfg(target_os = "macos")]
fn capture_worker<F: Fn(Arc<[u8]>) + Send + 'static>(
    publish: F,
    error: SharedError,
    ready: mpsc::SyncSender<Result<(), String>>,
    stopped: mpsc::Receiver<()>,
) {
    use screencapturekit::prelude::*;
    let start = || -> Result<SCStream, String> {
        let content = SCShareableContent::create()
            .get()
            .map_err(|e| format!("Couldn't list displays for system audio: {e}"))?;
        let display = content
            .displays()
            .into_iter()
            .next()
            .ok_or("No display is available for system audio")?;
        let filter = SCContentFilter::create()
            .with_display(&display)
            .with_excluding_windows(&[])
            .build()
            .map_err(|e| e.to_string())?;
        let config = SCStreamConfiguration::new()
            .with_width(2)
            .with_height(2)
            .with_fps(1)
            .with_captures_audio(true)
            .with_excludes_current_process_audio(true)
            .with_sample_rate(48000)
            .with_channel_count(2);
        let err = error.clone();
        let delegate = ErrorHandler::new(move |e| {
            set_error(&err, format!("System audio capture stopped: {e}"))
        });
        let mut stream =
            SCStream::new_with_delegate(&filter, &config, delegate).map_err(|e| e.to_string())?;
        let packetizer = Mutex::new(Packetizer::new(publish, SAMPLE_RATE));
        stream
            .add_output_handler(
                move |sample: CMSampleBuffer, _| {
                    if let (Ok(list), Ok(mut packetizer)) =
                        (sample.audio_buffer_list(), packetizer.lock())
                    {
                        let buffers: Vec<_> = (&list).into_iter().collect();
                        if buffers.len() >= 2 {
                            for (l, r) in buffers[0]
                                .data()
                                .as_chunks::<4>()
                                .0
                                .iter()
                                .zip(buffers[1].data().as_chunks::<4>().0)
                            {
                                packetizer.frame(f32::from_le_bytes(*l), f32::from_le_bytes(*r));
                            }
                        } else if let Some(buffer) = buffers.first() {
                            for frame in buffer.data().as_chunks::<8>().0 {
                                packetizer.frame(
                                    f32::from_le_bytes(frame[..4].try_into().unwrap()),
                                    f32::from_le_bytes(frame[4..].try_into().unwrap()),
                                );
                            }
                        }
                    }
                },
                SCStreamOutputType::Audio,
            )
            .map_err(|e| e.to_string())?;
        stream
            .start_capture()
            .map_err(|e| format!("Couldn't start system audio capture: {e}"))?;
        Ok(stream)
    };
    match start() {
        Ok(stream) => {
            let _ = ready.send(Ok(()));
            let _ = stopped.recv();
            let _ = stream.stop_capture();
        }
        Err(e) => {
            let _ = ready.send(Err(e));
        }
    }
}

struct Playback {
    queue: VecDeque<[f32; 2]>,
    phase: u64,
    current: [f32; 2],
    primed: bool,
}
impl Playback {
    fn next(&mut self, rate: u32) -> [f32; 2] {
        if !self.primed {
            if self.queue.len() < (SAMPLE_RATE / 25) as usize {
                return [0.0; 2];
            }
            self.primed = true;
        }
        self.phase += SAMPLE_RATE as u64;
        while self.phase >= rate as u64 {
            self.phase -= rate as u64;
            let Some(frame) = self.queue.pop_front() else {
                self.primed = false;
                self.phase = 0;
                self.current = [0.0; 2];
                break;
            };
            self.current = frame;
        }
        self.current
    }
}
pub struct AudioOutput {
    stop: Option<mpsc::Sender<()>>,
    error: SharedError,
    playback: Arc<Mutex<Playback>>,
}
impl AudioOutput {
    pub fn start() -> Result<Self, String> {
        let error = Arc::new(Mutex::new(None));
        let playback = Arc::new(Mutex::new(Playback {
            queue: VecDeque::new(),
            phase: 0,
            current: [0.0; 2],
            primed: false,
        }));
        let (stop, stopped) = mpsc::channel();
        let (ready, started) = mpsc::sync_channel(1);
        let state = playback.clone();
        let err = error.clone();
        thread::Builder::new()
            .name("system-audio-playback".into())
            .spawn(move || {
                let start = || -> Result<cpal::Stream, String> {
                    let device = cpal::default_host()
                        .default_output_device()
                        .ok_or("No audio output device is available")?;
                    let config = device.default_output_config().map_err(|e| e.to_string())?;
                    let channels = config.channels() as usize;
                    let rate = config.sample_rate().0;
                    let error_callback =
                        move |e| set_error(&err, format!("Audio playback stopped: {e}"));
                    macro_rules! output {
                        ($ty:ty, $convert:expr) => {{
                            device.build_output_stream(
                                &config.clone().into(),
                                move |data: &mut [$ty], _| {
                                    if let Ok(mut playback) = state.lock() {
                                        for frame in data.chunks_mut(channels) {
                                            let [l, r] = playback.next(rate);
                                            for (i, sample) in frame.iter_mut().enumerate() {
                                                *sample = ($convert)(if channels == 1 {
                                                    (l + r) * 0.5
                                                } else if i == 0 {
                                                    l
                                                } else if i == 1 {
                                                    r
                                                } else {
                                                    (l + r) * 0.5
                                                });
                                            }
                                        }
                                    }
                                },
                                error_callback,
                                None,
                            )
                        }};
                    }
                    let stream = match config.sample_format() {
                        cpal::SampleFormat::F32 => output!(f32, |x: f32| x),
                        cpal::SampleFormat::F64 => output!(f64, |x: f32| x as f64),
                        cpal::SampleFormat::I32 => {
                            output!(i32, |x: f32| (x.clamp(-1.0, 1.0) * 2147483647.0) as i32)
                        }
                        cpal::SampleFormat::U32 => {
                            output!(u32, |x: f32| ((x.clamp(-1.0, 1.0) as f64 * 0.5 + 0.5)
                                * 4294967295.0)
                                as u32)
                        }
                        cpal::SampleFormat::I16 => {
                            output!(i16, |x: f32| (x.clamp(-1.0, 1.0) * 32767.0) as i16)
                        }
                        cpal::SampleFormat::U16 => {
                            output!(u16, |x: f32| ((x.clamp(-1.0, 1.0) * 0.5 + 0.5) * 65535.0)
                                as u16)
                        }
                        other => {
                            return Err(format!(
                                "Unsupported audio output sample format: {other:?}"
                            ));
                        }
                    }
                    .map_err(|e| format!("Couldn't prepare audio playback: {e}"))?;
                    stream
                        .play()
                        .map_err(|e| format!("Couldn't start audio playback: {e}"))?;
                    Ok(stream)
                };
                match start() {
                    Ok(_stream) => {
                        let _ = ready.send(Ok(()));
                        let _ = stopped.recv();
                    }
                    Err(e) => {
                        let _ = ready.send(Err(e));
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        started
            .recv()
            .map_err(|_| "Audio playback stopped during startup".to_owned())??;
        Ok(Self {
            stop: Some(stop),
            error,
            playback,
        })
    }
    pub fn push(&self, packet: &[u8]) {
        if packet.len() != PACKET_BYTES {
            return;
        }
        if let Ok(mut state) = self.playback.lock() {
            // Keep at most 200 ms: discard old audio when a slow receiver falls behind.
            while state.queue.len() + packet.len() / 4 > SAMPLE_RATE as usize / 5 {
                state.queue.pop_front();
            }
            for frame in packet.as_chunks::<4>().0 {
                state.queue.push_back([
                    i16::from_le_bytes([frame[0], frame[1]]) as f32 / 32768.0,
                    i16::from_le_bytes([frame[2], frame[3]]) as f32 / 32768.0,
                ]);
            }
        }
    }
    pub fn error(&self) -> Option<String> {
        read_error(&self.error)
    }
}
impl Drop for AudioOutput {
    fn drop(&mut self) {
        self.stop.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn packetizer_converts_and_resamples() {
        let packets = Arc::new(Mutex::new(Vec::new()));
        let out = packets.clone();
        let mut p = Packetizer::new(move |packet| out.lock().unwrap().push(packet), 44_100);
        for _ in 0..882 {
            p.frame(1.0, -1.0);
        }
        let packets = packets.lock().unwrap();
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].len(), PACKET_BYTES);
        assert_eq!(&packets[0][..4], &[255, 127, 1, 128]);
    }
}
