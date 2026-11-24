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
    last_keyframe: Option<Instant>,
}

impl Encoder {
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
        let nominal =
            (width as u64 * height as u64 * u64::from(fps) / 6).clamp(750_000, 11_000_000) as u32;
        let format = (width, height, fps);
        let since_adjustment = self
            .last_adjustment
            .map(|at| now.saturating_duration_since(at));
        let mut bitrate = self.bitrate;
        if self.format != format {
            bitrate = nominal;
        } else if feedback.congested
            && since_adjustment.is_some_and(|age| age >= Duration::from_secs(1))
        {
            bitrate = (bitrate * 3 / 4).max((nominal / 4).max(450_000));
        } else if !feedback.congested
            && since_adjustment.is_some_and(|age| age >= Duration::from_secs(8))
        {
            bitrate = (bitrate * 5 / 4).min(nominal);
        }
        if self.codec.is_none() || self.format != format || bitrate != self.bitrate {
            let config = EncoderConfig::new()
                .usage_type(UsageType::ScreenContentRealTime)
                .complexity(Complexity::Low)
                .rate_control_mode(RateControlMode::Bitrate)
                .bitrate(BitRate::from_bps(bitrate))
                .max_frame_rate(FrameRate::from_hz(fps as f32))
                .intra_frame_period(IntraFramePeriod::from_num_frames(fps * 2))
                .adaptive_quantization(false)
                .background_detection(false)
                .skip_frames(true);
            self.codec = Some(
                Codec::with_api_config(OpenH264API::from_source(), config)
                    .map_err(|error| format!("Couldn't start video encoding: {error}"))?,
            );
            self.format = format;
            self.bitrate = bitrate;
            self.last_adjustment = Some(now);
            self.last_keyframe = None;
        }
        let codec = self.codec.as_mut().unwrap();
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

const PLAYOUT_DELAY: Duration = Duration::from_millis(85);
const MAX_DRIFT: Duration = Duration::from_millis(160);
const MAX_PLAYOUT_FRAMES: usize = 10;

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
                due
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
        let frame = |width| {
            Arc::new(Frame {
                width,
                height: 1,
                rgba: Vec::new(),
            })
        };
        let mut playout = Playout::default();
        playout.push(0, now, frame(1));
        playout.push(42, now + Duration::from_millis(20), frame(2));
        playout.push(84, now + Duration::from_millis(20), frame(3));
        assert!(playout.due(now + Duration::from_millis(80)).is_none());
        assert_eq!(
            playout.due(now + Duration::from_millis(95)).unwrap().width,
            1
        );
        assert_eq!(
            playout.due(now + Duration::from_millis(135)).unwrap().width,
            2
        );
        assert_eq!(
            playout.due(now + Duration::from_millis(175)).unwrap().width,
            3
        );
    }
}
