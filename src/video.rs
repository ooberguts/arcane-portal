use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use openh264::decoder::Decoder;
use openh264::encoder::{
    BitRate, Complexity, Encoder as Codec, EncoderConfig, FrameRate, FrameType, IntraFramePeriod,
    RateControlMode, UsageType,
};
use openh264::formats::{RgbaSliceU8, YUVBuffer, YUVSource};
use openh264::{OpenH264API, Timestamp};
use openh264_sys2::{
    ENCODER_OPTION_BITRATE, ENCODER_OPTION_MAX_BITRATE, SBitrateInfo, SPATIAL_LAYER_0,
    SPATIAL_LAYER_ALL,
};

use crate::Frame;

#[derive(Clone)]
pub(crate) struct Packet {
    pub bytes: Arc<[u8]>,
    pub timestamp_ms: u64,
    pub keyframe: bool,
}

#[derive(Default)]
pub(crate) struct Feedback {
    pub keyframe: bool,
    pub congested: bool,
    pub legacy_viewers: bool,
}

#[derive(Default)]
pub(crate) struct Encoder {
    codec: Option<Codec>,
    yuv: Option<YUVBuffer>,
    format: (usize, usize, u32),
    bitrate: u32,
    last_adjustment: Option<Instant>,
    last_congestion: Option<Instant>,
    last_keyframe: Option<Instant>,
}

impl Encoder {
    pub fn bandwidth_limited(&self, feedback: &Feedback) -> bool {
        feedback.congested && self.bitrate <= minimum_bitrate(nominal_bitrate(self.format))
    }

    pub fn update_picture(&mut self, width: u32, height: u32, rgba: &[u8]) {
        let dimensions = (width as usize, height as usize);
        let picture = RgbaSliceU8::new(rgba, dimensions);
        match &mut self.yuv {
            Some(yuv) if yuv.dimensions() == dimensions => yuv.read_rgba8(picture),
            _ => self.yuv = Some(YUVBuffer::from_rgba8_source(picture)),
        }
    }

    pub fn encode(
        &mut self,
        fps: u32,
        timestamp_ms: u64,
        now: Instant,
        feedback: &Feedback,
    ) -> Result<Option<Packet>, String> {
        let yuv = self.yuv.as_ref().ok_or("No screen frame to encode")?;
        let (width, height) = yuv.dimensions();
        let format = (width, height, fps);
        let nominal = nominal_bitrate(format);
        let minimum = minimum_bitrate(nominal);
        if feedback.congested {
            self.last_congestion = Some(now);
        }
        let since_adjustment = self
            .last_adjustment
            .map_or(Duration::ZERO, |at| now.saturating_duration_since(at));
        let quiet = self
            .last_congestion
            .is_none_or(|at| now.saturating_duration_since(at) >= Duration::from_secs(6));
        let mut bitrate = if self.bitrate == 0
            || (self.format != format && self.bitrate == nominal_bitrate(self.format) && quiet)
        {
            nominal
        } else {
            self.bitrate.clamp(minimum, nominal)
        };
        if feedback.congested && since_adjustment >= Duration::from_millis(850) {
            bitrate = (bitrate * 2 / 3).max(minimum);
        } else if quiet && since_adjustment >= Duration::from_secs(6) {
            bitrate = (bitrate * 6 / 5).min(nominal);
        }

        let codec = match &mut self.codec {
            Some(codec) if self.format == format => {
                if bitrate != self.bitrate {
                    update_bitrate(codec, nominal, bitrate)?;
                    self.bitrate = bitrate;
                    self.last_adjustment = Some(now);
                }
                codec
            }
            slot => {
                let mut config = EncoderConfig::new()
                    .usage_type(UsageType::ScreenContentRealTime)
                    .complexity(Complexity::Low)
                    .rate_control_mode(RateControlMode::Bitrate)
                    .bitrate(BitRate::from_bps(bitrate))
                    .max_frame_rate(FrameRate::from_hz(fps as f32))
                    // New and catching-up viewers can request a keyframe between these.
                    .intra_frame_period(IntraFramePeriod::from_num_frames(fps * 4))
                    .adaptive_quantization(false)
                    .background_detection(false)
                    .skip_frames(false);
                if width * height >= 640 * 480 {
                    let threads = std::thread::available_parallelism()
                        .map_or(1, usize::from)
                        .min(4);
                    if threads > 1 {
                        config = config.max_slice_len(32 * 1024).num_threads(threads as u16);
                    }
                }
                let codec = Codec::with_api_config(OpenH264API::from_source(), config)
                    .map_err(|error| format!("Couldn't start video encoding: {error}"))?;
                self.format = format;
                self.bitrate = bitrate;
                self.last_adjustment = Some(now);
                self.last_keyframe = None;
                slot.insert(codec)
            }
        };
        if feedback.keyframe
            && self
                .last_keyframe
                .is_some_and(|at| now.saturating_duration_since(at) >= Duration::from_millis(180))
        {
            codec.force_intra_frame();
        }
        let encoded = codec
            .encode_at(yuv, Timestamp::from_millis(timestamp_ms))
            .map_err(|error| format!("Couldn't encode the screen frame: {error}"))?;
        let keyframe = encoded.frame_type() == FrameType::IDR;
        if keyframe {
            self.last_keyframe = Some(now);
        }
        let bytes = encoded.to_vec();
        Ok((!bytes.is_empty()).then(|| Packet {
            bytes: bytes.into(),
            timestamp_ms,
            keyframe,
        }))
    }
}

fn nominal_bitrate((width, height, fps): (usize, usize, u32)) -> u32 {
    (width as u64 * height as u64 * u64::from(fps) / 6).clamp(750_000, 14_000_000) as u32
}

fn minimum_bitrate(nominal: u32) -> u32 {
    (nominal / 4).max(450_000)
}

fn update_bitrate(codec: &mut Codec, nominal: u32, bitrate: u32) -> Result<(), String> {
    let mut maximum = SBitrateInfo {
        iLayer: SPATIAL_LAYER_0,
        iBitrate: nominal as i32,
    };
    let mut target = SBitrateInfo {
        iLayer: SPATIAL_LAYER_ALL,
        iBitrate: bitrate as i32,
    };
    // The previous encode initialized this single-layer encoder. Both options take
    // a writable SBitrateInfo for the duration of the call and keep no pointer.
    let (max_status, target_status) = unsafe {
        let api = codec.raw_api();
        let max_status = api.set_option(ENCODER_OPTION_MAX_BITRATE, (&raw mut maximum).cast());
        let target_status = api.set_option(ENCODER_OPTION_BITRATE, (&raw mut target).cast());
        (max_status, target_status)
    };
    if max_status != 0 || target_status != 0 {
        return Err(format!(
            "Couldn't adjust the video bitrate (OpenH264 {max_status}/{target_status})"
        ));
    }
    Ok(())
}

pub(crate) fn decode(decoder: &mut Decoder, bytes: &[u8]) -> Result<Option<Frame>, String> {
    let Some(yuv) = decoder.decode(bytes).map_err(|error| error.to_string())? else {
        return Ok(None);
    };
    let (width, height) = yuv.dimensions();
    if !(2..=1920).contains(&width) || !(2..=1080).contains(&height) {
        return Err("Invalid video frame dimensions".into());
    }
    let mut rgba = vec![0; width * height * 4];
    yuv.write_rgba8(&mut rgba);
    Ok(Some(Frame {
        width: width as u32,
        height: height as u32,
        rgba,
    }))
}

pub(crate) const PLAYOUT_DELAY: Duration = Duration::from_millis(220);
const REFILL_DELAY: Duration = Duration::from_millis(100);
const LATE_FRAME: Duration = Duration::from_millis(35);
const MAX_DRIFT: Duration = Duration::from_millis(440);
const MAX_PLAYOUT_FRAMES: usize = 20;

#[derive(Default)]
pub(crate) struct Playout {
    origin: Option<(u64, Instant)>,
    pending: VecDeque<(Instant, Arc<Frame>)>,
}

impl Playout {
    pub fn push(&mut self, timestamp_ms: u64, now: Instant, frame: Arc<Frame>) {
        let due = self.origin.and_then(|(first, earliest)| {
            let offset = timestamp_ms.checked_sub(first)?;
            earliest.checked_add(Duration::from_millis(offset))
        });
        let due = match due {
            Some(due)
                if due.saturating_duration_since(now) <= MAX_DRIFT
                    && now.saturating_duration_since(due) <= MAX_DRIFT =>
            {
                if self.pending.is_empty() && now.saturating_duration_since(due) > LATE_FRAME {
                    let due = now + REFILL_DELAY;
                    self.origin = Some((timestamp_ms, due));
                    due
                } else {
                    due
                }
            }
            _ => {
                let due = now + PLAYOUT_DELAY;
                self.origin = Some((timestamp_ms, due));
                self.pending.clear();
                due
            }
        };
        if self.pending.len() == MAX_PLAYOUT_FRAMES {
            self.pending.pop_front();
        }
        self.pending.push_back((due, frame));
    }

    pub fn next_due(&self) -> Option<Instant> {
        self.pending.front().map(|(due, _)| *due)
    }

    pub fn due(&mut self, now: Instant) -> Option<Arc<Frame>> {
        let mut frame = None;
        while self.pending.front().is_some_and(|(due, _)| *due <= now) {
            frame = self.pending.pop_front().map(|(_, frame)| frame);
        }
        frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_buffer_paces_frames_that_arrive_in_a_burst() {
        let now = Instant::now();
        let mut playout = Playout::default();
        let delay = now + PLAYOUT_DELAY;
        playout.push(0, now, frame(1));
        playout.push(42, now + Duration::from_millis(178), frame(2));
        playout.push(84, now + Duration::from_millis(178), frame(3));
        assert!(playout.due(delay - Duration::from_millis(1)).is_none());
        assert_eq!(playout.due(delay).unwrap().width, 1);
        assert_eq!(
            playout
                .due(delay + Duration::from_millis(42))
                .unwrap()
                .width,
            2
        );
        assert_eq!(
            playout
                .due(delay + Duration::from_millis(84))
                .unwrap()
                .width,
            3
        );
    }

    #[test]
    fn long_starvation_refills_without_discarding_the_catch_up_burst() {
        let now = Instant::now();
        let mut playout = Playout::default();
        playout.push(0, now, frame(1));
        assert_eq!(playout.due(now + PLAYOUT_DELAY).unwrap().width, 1);
        let recovery = now + PLAYOUT_DELAY + Duration::from_millis(200);
        for (timestamp, width) in [(42, 2), (84, 3), (126, 4)] {
            playout.push(timestamp, recovery, frame(width));
        }
        let due = recovery + REFILL_DELAY;
        assert!(playout.due(recovery).is_none());
        assert_eq!(playout.due(due).unwrap().width, 2);
        assert_eq!(
            playout.due(due + Duration::from_millis(42)).unwrap().width,
            3
        );
        assert_eq!(
            playout.due(due + Duration::from_millis(84)).unwrap().width,
            4
        );
    }

    #[test]
    fn bitrate_changes_keep_the_codec_reference_chain() {
        let now = Instant::now();
        let mut encoder = Encoder::default();
        let mut decoder = Decoder::new().unwrap();
        encoder.update_picture(640, 480, &vec![148; 640 * 480 * 4]);
        let first = encoder
            .encode(30, 0, now, &Feedback::default())
            .unwrap()
            .unwrap();
        assert!(first.keyframe);
        assert!(decode(&mut decoder, &first.bytes).unwrap().is_some());
        let high = encoder.bitrate;
        let feedback = Feedback {
            congested: true,
            ..Feedback::default()
        };
        let second = encoder
            .encode(30, 1000, now + Duration::from_secs(1), &feedback)
            .unwrap()
            .unwrap();
        assert!(encoder.bitrate < high);
        assert!(!second.keyframe);
        assert!(decode(&mut decoder, &second.bytes).unwrap().is_some());
        let low = encoder.bitrate;
        let third = encoder
            .encode(30, 8000, now + Duration::from_secs(8), &Feedback::default())
            .unwrap()
            .unwrap();
        assert!(encoder.bitrate > low);
        assert!(!third.keyframe);
        assert!(decode(&mut decoder, &third.bytes).unwrap().is_some());
    }

    fn frame(width: u32) -> Arc<Frame> {
        Arc::new(Frame {
            width,
            height: 1,
            rgba: Vec::new(),
        })
    }
}
