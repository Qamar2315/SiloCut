//! Shared H.264 codec helpers used by the recorder, preview, and export engines.
//!
//! These centralise three concerns that were previously missing or duplicated and
//! were the root cause of the editor/recorder "not working":
//!
//!  * [`build_encoder`] returns an OpenH264 encoder with sane settings. The crate
//!    default ([`Encoder::new`]) targets **120 kbps**, enables frame-skipping, and
//!    never emits periodic keyframes — which yields an unwatchable, unseekable
//!    stream regardless of resolution.
//!  * [`find_video_track`] extracts the `avcC` (AVCDecoderConfigurationRecord) that
//!    symphonia exposes for MP4 video tracks and turns the contained SPS/PPS into an
//!    Annex-B header. Without priming the decoder with this header, OpenH264 never
//!    produces a single frame (blank preview / black export).
//!  * [`to_annex_b`] converts length-prefixed (AVCC) NAL units — how MP4 stores
//!    samples — into the Annex-B start-code format the decoder expects.

use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, RateControlMode, UsageType,
};
use openh264::OpenH264API;
use symphonia::core::codecs::video::well_known::extra_data::VIDEO_EXTRA_DATA_ID_AVC_DECODER_CONFIG;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::formats::FormatReader;
use symphonia::core::units::TimeBase;

/// Build an OpenH264 encoder tuned for `width`x`height` at `fps`.
///
/// `screen_content` selects the desktop-capture usage profile (sharp text, large
/// flat regions) over the camera-like profile used for general export footage.
///
/// `quality_scale` multiplies the auto-computed bitrate (1.0 = default; the export
/// dialog maps Low/Medium/High to 0.5/1.0/2.0).
///
/// The base bitrate scales with resolution and frame rate (≈0.1 bits per pixel per
/// second). Frame-skipping is disabled so no frames are dropped, and a keyframe is
/// emitted roughly once per second so the output is seekable.
pub fn build_encoder(
    width: u32,
    height: u32,
    fps: u32,
    screen_content: bool,
    quality_scale: f32,
) -> Result<Encoder, String> {
    let fps = fps.max(1);
    let pixels = width as u64 * height as u64;
    let base = pixels.saturating_mul(fps as u64) / 10;
    let bitrate = (base as f64 * quality_scale.clamp(0.1, 8.0) as f64)
        .clamp(500_000.0, 80_000_000.0) as u32;

    let config = EncoderConfig::new()
        .bitrate(BitRate::from_bps(bitrate))
        .max_frame_rate(FrameRate::from_hz(fps as f32))
        .rate_control_mode(RateControlMode::Bitrate)
        .skip_frames(false)
        .intra_frame_period(IntraFramePeriod::from_num_frames(fps))
        .usage_type(if screen_content {
            UsageType::ScreenContentRealTime
        } else {
            UsageType::CameraVideoRealTime
        });

    Encoder::with_api_config(OpenH264API::from_source(), config)
        .map_err(|e| format!("Failed to create H.264 encoder: {e}"))
}

/// Identifying information and decoder-priming data for a file's H.264 video track.
pub struct VideoTrackInfo {
    /// The symphonia track id used to filter demuxed packets.
    pub track_id: u32,
    /// Timestamp tick → seconds conversion for this track.
    pub time_base: TimeBase,
    /// Annex-B encoded SPS/PPS header to prime the decoder with (may be empty if the
    /// container did not carry an `avcC` record, e.g. raw Annex-B streams).
    pub annexb_header: Vec<u8>,
    /// NAL unit length prefix size used by the container's samples (1, 2, or 4).
    pub nal_length_size: usize,
}

fn default_time_base() -> TimeBase {
    use std::num::NonZeroU32;
    TimeBase::new(NonZeroU32::new(1).unwrap(), NonZeroU32::new(30).unwrap())
}

/// Locate the first usable H.264 video track in `fmt` and gather everything the
/// decoders need: its id, time base, an Annex-B SPS/PPS priming header, and the
/// sample NAL length size.
pub fn find_video_track(fmt: &dyn FormatReader) -> Option<VideoTrackInfo> {
    for t in fmt.tracks() {
        if let Some(CodecParameters::Video(ref vp)) = t.codec_params {
            if vp.width.is_none() || vp.height.is_none() {
                continue;
            }
            let time_base = t.time_base.unwrap_or_else(default_time_base);

            let mut annexb_header = Vec::new();
            let mut nal_length_size = 4;
            for ed in &vp.extra_data {
                if ed.id == VIDEO_EXTRA_DATA_ID_AVC_DECODER_CONFIG {
                    if let Some((hdr, ls)) = parse_avcc(&ed.data) {
                        annexb_header = hdr;
                        nal_length_size = ls;
                    }
                }
            }

            return Some(VideoTrackInfo {
                track_id: t.id,
                time_base,
                annexb_header,
                nal_length_size,
            });
        }
    }
    None
}

/// Parse an `AVCDecoderConfigurationRecord` (`avcC`) into an Annex-B SPS/PPS blob
/// and the configured NAL length size. Returns `None` if the record is malformed.
fn parse_avcc(avcc: &[u8]) -> Option<(Vec<u8>, usize)> {
    // configurationVersion(1) profile(1) compat(1) level(1) lengthSizeMinusOne(1) numSPS(1)
    if avcc.len() < 7 || avcc[0] != 1 {
        return None;
    }
    let nal_length_size = ((avcc[4] & 0x03) + 1) as usize;
    let mut out = Vec::new();
    let mut pos = 5usize;

    let push_nal = |out: &mut Vec<u8>, data: &[u8], pos: &mut usize, count: usize| -> bool {
        for _ in 0..count {
            if *pos + 2 > data.len() {
                return false;
            }
            let len = u16::from_be_bytes([data[*pos], data[*pos + 1]]) as usize;
            *pos += 2;
            if *pos + len > data.len() {
                return false;
            }
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&data[*pos..*pos + len]);
            *pos += len;
        }
        true
    };

    let num_sps = (avcc[pos] & 0x1f) as usize;
    pos += 1;
    if !push_nal(&mut out, avcc, &mut pos, num_sps) {
        return None;
    }

    if pos < avcc.len() {
        let num_pps = avcc[pos] as usize;
        pos += 1;
        if !push_nal(&mut out, avcc, &mut pos, num_pps) {
            return None;
        }
    }

    if out.is_empty() {
        None
    } else {
        Some((out, nal_length_size))
    }
}

/// Convert length-prefixed (AVCC) NAL units into an Annex-B byte stream.
///
/// If `data` already begins with an Annex-B start code it is returned unchanged.
/// `nal_length_size` is the prefix width (1, 2, or 4 bytes) reported by `avcC`.
pub fn to_annex_b(data: &[u8], nal_length_size: usize) -> Vec<u8> {
    if data.len() >= 4 && (data[0..4] == [0, 0, 0, 1] || data[0..3] == [0, 0, 1]) {
        return data.to_vec();
    }
    let ls = nal_length_size.clamp(1, 4);
    let mut out = Vec::with_capacity(data.len() + 16);
    let mut i = 0;
    while i + ls <= data.len() {
        let mut len = 0usize;
        for k in 0..ls {
            len = (len << 8) | data[i + k] as usize;
        }
        i += ls;
        if len == 0 || i + len > data.len() {
            break;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&data[i..i + len]);
        i += len;
    }
    // If nothing parsed (unexpected layout) fall back to the original bytes so the
    // decoder at least gets a chance rather than receiving an empty buffer.
    if out.is_empty() {
        data.to_vec()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openh264::decoder::Decoder;
    use openh264::formats::{RgbSliceU8, YUVBuffer, YUVSource};
    use std::io::Cursor;

    fn synth_rgb(w: u32, h: u32, f: u32) -> Vec<u8> {
        let mut rgb = vec![0u8; (w * h * 3) as usize];
        for y in 0..h {
            for x in 0..w {
                let idx = ((y * w + x) * 3) as usize;
                rgb[idx] = ((x + f * 10) % 256) as u8;
                rgb[idx + 1] = ((y + f * 5) % 256) as u8;
                rgb[idx + 2] = ((x + y + f) % 256) as u8;
            }
        }
        rgb
    }

    #[test]
    fn avcc_parse_roundtrip() {
        // version, profile, compat, level, lengthSizeMinusOne=3 -> 0xFF, numSPS=1 (0xE1)
        let sps = [0x67u8, 0x42, 0x00, 0x0a];
        let pps = [0x68u8, 0xce, 0x38, 0x80];
        let mut avcc = vec![1, 0x42, 0x00, 0x0a, 0xff, 0xe1];
        avcc.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        avcc.extend_from_slice(&sps);
        avcc.push(1); // numPPS
        avcc.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        avcc.extend_from_slice(&pps);

        let (hdr, ls) = parse_avcc(&avcc).expect("avcC should parse");
        assert_eq!(ls, 4);
        let expected: Vec<u8> = [&[0, 0, 0, 1][..], &sps[..], &[0, 0, 0, 1][..], &pps[..]].concat();
        assert_eq!(hdr, expected);
    }

    #[test]
    fn to_annex_b_converts_length_prefixed() {
        let nal = [0x65u8, 0x11, 0x22, 0x33];
        let mut data = (nal.len() as u32).to_be_bytes().to_vec();
        data.extend_from_slice(&nal);
        let out = to_annex_b(&data, 4);
        assert_eq!(out, [&[0, 0, 0, 1][..], &nal[..]].concat());
    }

    /// End-to-end: encode synthetic frames with our encoder config, mux them into an
    /// MP4 with `mp4e`, then demux with symphonia and decode with OpenH264 after
    /// priming it from the `avcC`. This exercises the exact pipeline the recorder,
    /// preview, and export engines rely on.
    #[test]
    fn encode_mux_demux_decode_roundtrip() {
        let (w, h, fps) = (320u32, 240u32, 30u32);

        // --- Encode + mux to an in-memory MP4 ---
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut cursor = Cursor::new(&mut buf);
            let mut muxer = mp4e::Mp4e::new(&mut cursor);
            muxer.set_video_track(w, h, mp4e::Codec::AVC);
            let mut encoder = build_encoder(w, h, fps, true, 1.0).expect("encoder");
            let mut bitstream = Vec::new();
            for f in 0..15 {
                let rgb = synth_rgb(w, h, f);
                let yuv = YUVBuffer::from_rgb_source(RgbSliceU8::new(&rgb, (w as usize, h as usize)));
                let bs = encoder.encode(&yuv).expect("encode");
                bitstream.clear();
                bs.write_vec(&mut bitstream);
                muxer.encode_video(&bitstream, 1000 / fps).expect("mux");
            }
            muxer.flush().expect("flush");
        }
        assert!(buf.len() > 200, "muxed MP4 unexpectedly small: {} bytes", buf.len());

        // --- Write to a temp file and demux + decode ---
        let tmp = std::env::temp_dir().join(format!("silocut_codec_test_{}.mp4", std::process::id()));
        std::fs::write(&tmp, &buf).expect("write temp mp4");

        let file = std::fs::File::open(&tmp).expect("open temp mp4");
        let mss = symphonia::core::io::MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = symphonia::core::formats::probe::Hint::new();
        hint.with_extension("mp4");
        let mut fmt = symphonia::default::get_probe()
            .probe(
                &hint,
                mss,
                symphonia::core::formats::FormatOptions::default(),
                symphonia::core::meta::MetadataOptions::default(),
            )
            .expect("probe");

        let info = find_video_track(&*fmt).expect("video track found");
        assert!(!info.annexb_header.is_empty(), "avcC SPS/PPS header must be present");

        let mut decoder = Decoder::new().expect("decoder");
        // Prime with SPS/PPS — without this no frame ever decodes.
        let _ = decoder.decode(&info.annexb_header);

        let mut decoded = 0;
        let mut dims = (0usize, 0usize);
        while let Ok(Some(packet)) = fmt.next_packet() {
            if packet.track_id != info.track_id {
                continue;
            }
            let annexb = to_annex_b(&packet.data, info.nal_length_size);
            if let Ok(Some(yuv)) = decoder.decode(&annexb) {
                decoded += 1;
                dims = yuv.dimensions();
            }
        }

        let _ = std::fs::remove_file(&tmp);
        assert!(decoded > 0, "decoder produced no frames from the muxed MP4");
        assert_eq!(dims, (w as usize, h as usize), "decoded frame dimensions mismatch");
    }
}
