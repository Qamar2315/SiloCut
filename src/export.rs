use crate::editor::EditorState;
use std::path::Path;
use std::collections::HashMap;
use symphonia::core::formats::{FormatReader, FormatOptions};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::formats::probe::Hint;
use openh264::formats::YUVBuffer;
use image::{ImageBuffer, Rgb};

// Manages sequential decoding of video files during export
struct AssetExportDecoder {
    format_reader: Box<dyn FormatReader>,
    video_track_id: u32,
    decoder: openh264::decoder::Decoder,
    time_base: symphonia_core::units::TimeBase,
    header_annexb: Vec<u8>,
    nal_length_size: usize,
    last_pts: u64,
}

impl AssetExportDecoder {
    fn new(path: &Path) -> Result<Self, String> {
        let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        let format_reader = symphonia::default::get_probe()
            .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
            .map_err(|e| e.to_string())?;

        let info = crate::codec::find_video_track(&*format_reader)
            .ok_or_else(|| format!("No video track found in {}", path.display()))?;

        let mut decoder = openh264::decoder::Decoder::new().map_err(|e| e.to_string())?;
        // Prime the decoder with SPS/PPS up front so the first frame decodes.
        if !info.annexb_header.is_empty() {
            let _ = decoder.decode(&info.annexb_header);
        }

        Ok(Self {
            format_reader,
            video_track_id: info.track_id,
            decoder,
            time_base: info.time_base,
            header_annexb: info.annexb_header,
            nal_length_size: info.nal_length_size,
            last_pts: 0,
        })
    }

    fn read_frame_at(&mut self, target_time_secs: f64) -> Result<Option<(Vec<u8>, usize, usize)>, String> {
        let current_secs = self.last_pts as f64 * self.time_base.numer.get() as f64 / self.time_base.denom.get() as f64;
        
        // Coarse seek if target time is backwards or jumping more than 1.5 seconds forward
        if target_time_secs < current_secs || target_time_secs > current_secs + 1.5 {
            let seek_to_time = symphonia_core::units::Time::try_from_secs_f64(target_time_secs)
                .unwrap_or_else(|| symphonia_core::units::Time::try_new(0, 0).unwrap());
            
            let seek_res = self.format_reader.seek(
                symphonia_core::formats::SeekMode::Coarse,
                symphonia_core::formats::SeekTo::Time {
                    time: seek_to_time,
                    track_id: Some(self.video_track_id),
                }
            );
            if seek_res.is_err() {
                let _ = self.format_reader.seek(
                    symphonia_core::formats::SeekMode::Coarse,
                    symphonia_core::formats::SeekTo::Time {
                        time: symphonia_core::units::Time::try_new(0, 0).unwrap(),
                        track_id: Some(self.video_track_id),
                    }
                );
            }
            // Reset decoder state on seek for clean decoding from the keyframe, then
            // re-prime with SPS/PPS (lost on reset).
            let mut new_dec = openh264::decoder::Decoder::new().map_err(|e| e.to_string())?;
            if !self.header_annexb.is_empty() {
                let _ = new_dec.decode(&self.header_annexb);
            }
            self.decoder = new_dec;
            self.last_pts = 0;
        }

        // Loop and decode packets
        let mut attempts = 0;
        while attempts < 5000 {
            attempts += 1;
            match self.format_reader.next_packet() {
                Ok(Some(packet)) => {
                    if packet.track_id != self.video_track_id {
                        continue;
                    }

                    self.last_pts = packet.pts.get() as u64;
                    let packet_time = packet.pts.get() as f64 * self.time_base.numer.get() as f64 / self.time_base.denom.get() as f64;

                    // Decode H.264 frame
                    let data = crate::codec::to_annex_b(&packet.data, self.nal_length_size);
                    if let Ok(Some(decoded)) = self.decoder.decode(&data) {
                        if packet_time >= target_time_secs - 0.04 {
                            use openh264::formats::YUVSource;
                            let (w, h) = decoded.dimensions();
                            let mut rgb_raw = vec![0u8; decoded.rgb8_len()];
                            decoded.write_rgb8(&mut rgb_raw);
                            return Ok(Some((rgb_raw, w, h)));
                        }
                    }
                }
                Ok(None) => break, // EOF
                Err(symphonia::core::errors::Error::ResetRequired) => {
                    self.last_pts = 0;
                    continue;
                }
                Err(_) => break, // Stop reading on critical errors
            }
        }

        Ok(None)
    }
}

// 16-bit PCM Stereo WAV generator (Zero-Dependency)
fn write_wav_file(path: &Path, samples: &[f32], sample_rate: u32, channels: u16) -> Result<(), String> {
    use std::fs::File;
    use std::io::Write;

    let mut file = File::create(path).map_err(|e| e.to_string())?;
    
    let num_samples = samples.len();
    let data_size = num_samples * 2; // 16-bit signed integer = 2 bytes per sample
    let file_size = 36 + data_size;

    // Header
    file.write_all(b"RIFF").map_err(|e| e.to_string())?;
    file.write_all(&(file_size as u32).to_le_bytes()).map_err(|e| e.to_string())?;
    file.write_all(b"WAVE").map_err(|e| e.to_string())?;
    
    // Sub-chunk 1: fmt
    file.write_all(b"fmt ").map_err(|e| e.to_string())?;
    file.write_all(&16u32.to_le_bytes()).map_err(|e| e.to_string())?; // Subchunk1Size
    file.write_all(&1u16.to_le_bytes()).map_err(|e| e.to_string())?;  // AudioFormat (1 = PCM)
    file.write_all(&channels.to_le_bytes()).map_err(|e| e.to_string())?; // NumChannels
    file.write_all(&sample_rate.to_le_bytes()).map_err(|e| e.to_string())?; // SampleRate
    
    let byte_rate = sample_rate * channels as u32 * 2;
    file.write_all(&byte_rate.to_le_bytes()).map_err(|e| e.to_string())?; // ByteRate
    
    let block_align = channels * 2;
    file.write_all(&block_align.to_le_bytes()).map_err(|e| e.to_string())?; // BlockAlign
    file.write_all(&16u16.to_le_bytes()).map_err(|e| e.to_string())?;    // BitsPerSample
    
    // Sub-chunk 2: data
    file.write_all(b"data").map_err(|e| e.to_string())?;
    file.write_all(&(data_size as u32).to_le_bytes()).map_err(|e| e.to_string())?;

    // Write samples as 16-bit PCM
    let mut pcm_buffer = Vec::with_capacity(num_samples * 2);
    for &sample in samples {
        let clamped = sample.clamp(-1.0, 1.0);
        let pcm_sample = (clamped * 32767.0) as i16;
        pcm_buffer.extend_from_slice(&pcm_sample.to_le_bytes());
    }
    file.write_all(&pcm_buffer).map_err(|e| e.to_string())?;
    
    Ok(())
}

fn mix_track_audio(
    mixed_audio: &mut [f32],
    track: &crate::editor::Track,
    assets: &[crate::media::MediaAsset],
    sample_rate: u32,
) {
    for clip in &track.clips {
        if let Some(asset) = assets.iter().find(|a| a.id == clip.asset_id) {
            if let Some(ref samples) = asset.audio_samples {
                let start_idx = (clip.timeline_start * sample_rate as f64) as usize;
                let end_idx = (clip.timeline_end * sample_rate as f64) as usize;
                
                for idx in start_idx..end_idx {
                    if idx * 2 + 1 >= mixed_audio.len() {
                        break;
                    }
                    let t = idx as f64 / sample_rate as f64;
                    let clip_offset = t - clip.timeline_start;
                    let src_t = clip.source_trim_start + clip_offset;
                    
                    if src_t >= 0.0 && src_t < asset.duration_secs {
                        let src_sample_idx = (src_t * asset.audio_sample_rate as f64) as usize;
                        let asset_channels = asset.audio_channels as usize;
                        
                        if asset_channels > 0 {
                            let mut left_val = 0.0f32;
                            let mut right_val = 0.0f32;
                            
                            if asset_channels == 1 {
                                if src_sample_idx < samples.len() {
                                    left_val = samples[src_sample_idx];
                                    right_val = samples[src_sample_idx];
                                }
                            } else {
                                let base_idx = src_sample_idx * asset_channels;
                                if base_idx + 1 < samples.len() {
                                    left_val = samples[base_idx];
                                    right_val = samples[base_idx + 1];
                                }
                            }
                            
                            // Apply volume slider and fades
                            let mut volume_factor = track.volume;
                             if clip.fade_in_duration > 0.0 && clip_offset < clip.fade_in_duration {
                                 volume_factor *= (clip_offset / clip.fade_in_duration) as f32;
                             }
                             if clip.fade_out_duration > 0.0 && (clip.timeline_end - t) < clip.fade_out_duration {
                                 volume_factor *= ((clip.timeline_end - t) / clip.fade_out_duration) as f32;
                             }
                            
                            mixed_audio[idx * 2] += left_val * volume_factor;
                            mixed_audio[idx * 2 + 1] += right_val * volume_factor;
                        }
                    }
                }
            }
        }
    }
}

pub fn export_timeline(
    state: &EditorState,
    path: &Path,
    width: u32,
    height: u32,
    fps: u32,
    quality_scale: f32,
    progress: impl Fn(f32),
) -> Result<(), String> {
    // Determine export duration
    let mut max_duration = 0.0f64;
    for track in &state.video_tracks {
        for clip in &track.clips {
            max_duration = max_duration.max(clip.timeline_end);
        }
    }
    for track in &state.audio_tracks {
        for clip in &track.clips {
            max_duration = max_duration.max(clip.timeline_end);
        }
    }

    if max_duration <= 0.0 {
        return Err("No video or audio clips on the timeline to export.".to_string());
    }

    // 1. Render and Mix Companion Audio (WAV file)
    let sample_rate = 44100;
    let channels = 2;
    let total_audio_frames = (max_duration * sample_rate as f64) as usize;
    let mut mixed_audio = vec![0.0f32; total_audio_frames * channels];

    let has_solo_video = state.video_tracks.iter().any(|t| t.is_solo);
    let has_solo_audio = state.audio_tracks.iter().any(|t| t.is_solo);

    // Mix audio from video tracks (camera recordings)
    for track in &state.video_tracks {
        let is_active = !track.is_hidden && (!has_solo_video || track.is_solo);
        if !is_active {
            continue;
        }
        mix_track_audio(&mut mixed_audio, track, &state.assets, sample_rate);
    }

    // Mix audio from audio tracks
    for track in &state.audio_tracks {
        let is_active = !track.is_muted && (!has_solo_audio || track.is_solo);
        if !is_active {
            continue;
        }
        mix_track_audio(&mut mixed_audio, track, &state.assets, sample_rate);
    }

    // Write output WAV file
    let mut wav_path = path.to_path_buf();
    wav_path.set_extension("wav");
    write_wav_file(&wav_path, &mixed_audio, sample_rate, channels as u16)?;

    // 2. Render Video (MP4 file)
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut writer = std::io::BufWriter::new(file);
    let mut muxer = mp4e::Mp4e::new(&mut writer);
    muxer.set_video_track(width, height, mp4e::Codec::AVC);

    // Set up H.264 Encoder. The OpenH264 default (120 kbps) would render the export
    // unwatchable; codec::build_encoder scales the bitrate to the resolution.
    let mut encoder = crate::codec::build_encoder(width, height, fps, false, quality_scale)?;
    let mut bitstream_buffer = Vec::new();

    // Cache of open asset decoders, and of decoded still images.
    let mut decoders: HashMap<usize, AssetExportDecoder> = HashMap::new();
    let mut image_cache: HashMap<usize, image::RgbImage> = HashMap::new();

    let total_frames = (max_duration * fps as f64) as usize;
    let frame_duration_ms = 1000 / fps;

    for frame_idx in 0..total_frames {
        if total_frames > 0 {
            progress(frame_idx as f32 / total_frames as f32);
        }
        let time_secs = frame_idx as f64 / fps as f64;

        // Find active clip with the highest track priority (top-most video track)
        let mut active_clip_info: Option<(&crate::editor::Clip, &crate::media::MediaAsset)> = None;
        for track in state.video_tracks.iter().rev() {
            let is_active = !track.is_hidden && (!has_solo_video || track.is_solo);
            if !is_active {
                continue;
            }
            for clip in &track.clips {
                if time_secs >= clip.timeline_start && time_secs < clip.timeline_end {
                    if let Some(asset) = state.assets.iter().find(|a| a.id == clip.asset_id) {
                        active_clip_info = Some((clip, asset));
                        break;
                    }
                }
            }
            if active_clip_info.is_some() {
                break;
            }
        }

        let mut output_frame_written = false;

        if let Some((clip, asset)) = active_clip_info {
            if asset.is_image {
                // Still image: decode once, resize to the export size, apply fades.
                if !image_cache.contains_key(&asset.id) {
                    if let Ok(img) = image::open(&asset.path) {
                        image_cache.insert(asset.id, img.to_rgb8());
                    }
                }
                if let Some(src) = image_cache.get(&asset.id) {
                    let resized =
                        image::imageops::resize(src, width, height, image::imageops::FilterType::Triangle);
                    let mut rgb_export_data = resized.into_raw();

                    let mut alpha = 1.0f32;
                    if clip.fade_in_duration > 0.0 && (time_secs - clip.timeline_start) < clip.fade_in_duration {
                        alpha *= ((time_secs - clip.timeline_start) / clip.fade_in_duration) as f32;
                    }
                    if clip.fade_out_duration > 0.0 && (clip.timeline_end - time_secs) < clip.fade_out_duration {
                        alpha *= ((clip.timeline_end - time_secs) / clip.fade_out_duration) as f32;
                    }
                    alpha = alpha.clamp(0.0, 1.0);
                    if alpha < 1.0 {
                        for val in &mut rgb_export_data {
                            *val = (*val as f32 * alpha) as u8;
                        }
                    }

                    let yuv_buffer = YUVBuffer::from_rgb_source(
                        openh264::formats::RgbSliceU8::new(&rgb_export_data, (width as usize, height as usize)),
                    );
                    if let Ok(bitstream) = encoder.encode(&yuv_buffer) {
                        bitstream_buffer.clear();
                        bitstream.write_vec(&mut bitstream_buffer);
                        muxer.encode_video(&bitstream_buffer, frame_duration_ms)
                            .map_err(|e| e.to_string())?;
                        output_frame_written = true;
                    }
                }
            } else {
            // Open decoder for asset if not cached
            if !decoders.contains_key(&asset.id) {
                if let Ok(decoder) = AssetExportDecoder::new(&asset.path) {
                    decoders.insert(asset.id, decoder);
                }
            }

            if let Some(decoder) = decoders.get_mut(&asset.id) {
                // Map timeline time to source trim time
                let source_time = clip.source_trim_start + (time_secs - clip.timeline_start);
                
                if let Ok(Some((rgb_raw, src_w, src_h))) = decoder.read_frame_at(source_time) {
                    // Resize image to export resolution if dimensions mismatch
                    let mut rgb_export_data = if src_w != width as usize || src_h != height as usize {
                        let src_img = ImageBuffer::<Rgb<u8>, Vec<u8>>::from_raw(src_w as u32, src_h as u32, rgb_raw)
                            .ok_or_else(|| "Failed to wrap decoded frame as ImageBuffer".to_string())?;
                        
                        let dst_img = image::imageops::resize(
                            &src_img, 
                            width, 
                            height, 
                            image::imageops::FilterType::Triangle
                        );
                        dst_img.into_raw()
                     } else {
                        rgb_raw
                     };

                    // Apply video opacity fades
                    let mut alpha = 1.0f32;
                     if clip.fade_in_duration > 0.0 && (time_secs - clip.timeline_start) < clip.fade_in_duration {
                         alpha *= ((time_secs - clip.timeline_start) / clip.fade_in_duration) as f32;
                     }
                     if clip.fade_out_duration > 0.0 && (clip.timeline_end - time_secs) < clip.fade_out_duration {
                         alpha *= ((clip.timeline_end - time_secs) / clip.fade_out_duration) as f32;
                     }
                     alpha = alpha.clamp(0.0, 1.0);

                    if alpha < 1.0 {
                        for val in &mut rgb_export_data {
                            *val = (*val as f32 * alpha) as u8;
                        }
                    }

                    // Convert RGB8 back to YUV420p YUVBuffer
                    let yuv_buffer = YUVBuffer::from_rgb_source(
                        openh264::formats::RgbSliceU8::new(&rgb_export_data, (width as usize, height as usize))
                    );

                    // Encode frame
                     if let Ok(bitstream) = encoder.encode(&yuv_buffer) {
                         bitstream_buffer.clear();
                         bitstream.write_vec(&mut bitstream_buffer);
                         muxer.encode_video(&bitstream_buffer, frame_duration_ms)
                             .map_err(|e| e.to_string())?;
                         output_frame_written = true;
                     }
                }
            }
            }
        }

        // If no active clip or decoding failed, write a black frame
        if !output_frame_written {
            let black_rgb = vec![0u8; width as usize * height as usize * 3];
            let yuv_buffer = YUVBuffer::from_rgb_source(
                openh264::formats::RgbSliceU8::new(&black_rgb, (width as usize, height as usize))
            );
              if let Ok(bitstream) = encoder.encode(&yuv_buffer) {
                  bitstream_buffer.clear();
                  bitstream.write_vec(&mut bitstream_buffer);
                  muxer.encode_video(&bitstream_buffer, frame_duration_ms)
                      .map_err(|e| e.to_string())?;
              }
        }
    }

    muxer.flush().map_err(|e| e.to_string())?;
    progress(1.0);
    Ok(())
}
