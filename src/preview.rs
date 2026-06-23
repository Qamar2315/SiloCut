use crate::editor::EditorState;
use std::path::PathBuf;
use std::sync::mpsc::{Sender, Receiver, channel};
use symphonia::core::formats::{FormatReader, FormatOptions, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::formats::probe::Hint;
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;
use rayon::prelude::*;

enum DecoderCommand {
    RequestFrame {
        asset_id: usize,
        path: PathBuf,
        target_secs: f64,
    },
}

struct DecodedFrameResponse {
    asset_id: usize,
    frame_idx: usize,
    width: usize,
    height: usize,
    rgba: Vec<u8>,
}

struct CachedFrame {
    width: usize,
    height: usize,
    rgba: Vec<u8>,
}

pub struct PreviewEngine {
    pub current_texture: Option<egui::TextureHandle>,
    current_frame_key: Option<(usize, usize)>,
    last_requested_frame: Option<(usize, usize)>,
    frame_cache: std::collections::HashMap<(usize, usize), CachedFrame>,
    cmd_tx: Sender<DecoderCommand>,
    resp_rx: Receiver<DecodedFrameResponse>,
    
    // Rodio audio objects. `_audio_stream` is held only to keep the output device
    // alive for the lifetime of the engine (dropping it would stop playback).
    audio_sink: Option<rodio::Sink>,
    _audio_stream: Option<rodio::OutputStream>,
    last_audio_queue_time: f64,
}

// Fast integer BT.601 limited-range YUV420p to RGBA conversion
fn yuv420p_to_rgba(
    width: usize,
    _height: usize,
    y_plane: &[u8],
    u_plane: &[u8],
    v_plane: &[u8],
    y_stride: usize,
    u_stride: usize,
    v_stride: usize,
    out_rgba: &mut [u8],
) {
    let row_size = width * 4;
    out_rgba
        .par_chunks_exact_mut(row_size)
        .enumerate()
        .for_each(|(y_idx, row_rgba)| {
            let y_row_offset = y_idx * y_stride;
            let u_row_offset = (y_idx / 2) * u_stride;
            let v_row_offset = (y_idx / 2) * v_stride;

            for x_idx in 0..width {
                let y_val = y_plane[y_row_offset + x_idx] as i32;
                let u_val = u_plane[u_row_offset + (x_idx / 2)] as i32 - 128;
                let v_val = v_plane[v_row_offset + (x_idx / 2)] as i32 - 128;

                // BT.601 integer formula
                let c = y_val - 16;
                let r = ((298 * c + 409 * v_val + 128) >> 8).clamp(0, 255) as u8;
                let g = ((298 * c - 100 * u_val - 208 * v_val + 128) >> 8).clamp(0, 255) as u8;
                let b = ((298 * c + 516 * u_val + 128) >> 8).clamp(0, 255) as u8;

                let offset = x_idx * 4;
                row_rgba[offset] = r;
                row_rgba[offset + 1] = g;
                row_rgba[offset + 2] = b;
                row_rgba[offset + 3] = 255;
            }
        });
}

impl PreviewEngine {
    pub fn new() -> Self {
        let (cmd_tx, cmd_rx) = channel::<DecoderCommand>();
        let (resp_tx, resp_rx) = channel::<DecodedFrameResponse>();
        
        // Spawn background video decoder thread
        std::thread::spawn(move || {
            let mut current_path: Option<PathBuf> = None;
            let mut format: Option<Box<dyn FormatReader>> = None;
            let mut decoder: Option<Decoder> = None;
            let mut video_track_id: Option<u32> = None;
            let mut time_base: Option<symphonia_core::units::TimeBase> = None;
            let mut header_annexb: Vec<u8> = Vec::new();
            let mut nal_length_size: usize = 4;
            let mut last_decoded_secs = -999.0f64;

            while let Ok(mut cmd) = cmd_rx.recv() {
                // Coalesce: drain intermediate commands to process only the latest one
                while let Ok(next_cmd) = cmd_rx.try_recv() {
                    cmd = next_cmd;
                }
                match cmd {
                    DecoderCommand::RequestFrame { asset_id, path, target_secs } => {
                        let need_open = current_path.as_ref() != Some(&path) || format.is_none();
                        let need_seek = need_open || target_secs < last_decoded_secs || (target_secs - last_decoded_secs) > 1.5;

                        if need_open {
                            current_path = Some(path.clone());
                            format = None;
                            decoder = None;
                            video_track_id = None;
                            time_base = None;
                            header_annexb = Vec::new();
                            nal_length_size = 4;

                            if let Ok(file) = std::fs::File::open(&path) {
                                let mss = MediaSourceStream::new(Box::new(file), Default::default());
                                let mut hint = Hint::new();
                                if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                                    hint.with_extension(ext);
                                }

                                if let Ok(fmt) = symphonia::default::get_probe().probe(
                                    &hint, mss, FormatOptions::default(), MetadataOptions::default()
                                ) {
                                    if let Some(info) = crate::codec::find_video_track(&*fmt) {
                                        video_track_id = Some(info.track_id);
                                        time_base = Some(info.time_base);
                                        header_annexb = info.annexb_header;
                                        nal_length_size = info.nal_length_size;
                                    }
                                    format = Some(fmt);
                                    if let Ok(mut new_dec) = Decoder::new() {
                                        // Prime the decoder with SPS/PPS so the first
                                        // frame can be decoded (MP4 stores these in the
                                        // avcC box, not inline with the samples).
                                        if !header_annexb.is_empty() {
                                            let _ = new_dec.decode(&header_annexb);
                                        }
                                        decoder = Some(new_dec);
                                    }
                                }
                            }
                        }

                        if format.is_none() || decoder.is_none() || video_track_id.is_none() {
                            continue;
                        }

                        let fmt = format.as_mut().unwrap();
                        let dec = decoder.as_mut().unwrap();
                        let vid_id = video_track_id.unwrap();
                        let tb = time_base.unwrap();

                        if need_seek {
                            // Reset decoder to clear the decoded-picture buffer, then
                            // re-prime with SPS/PPS (the reset discards them).
                            if let Ok(mut new_dec) = Decoder::new() {
                                if !header_annexb.is_empty() {
                                    let _ = new_dec.decode(&header_annexb);
                                }
                                *dec = new_dec;
                            }

                            let seek_ts = target_secs * (tb.denom.get() as f64) / (tb.numer.get() as f64);
                            let _ = fmt.seek(
                                SeekMode::Coarse,
                                SeekTo::Timestamp {
                                    ts: symphonia_core::units::Timestamp::new(seek_ts as i64),
                                    track_id: vid_id,
                                }
                            );
                            last_decoded_secs = -999.0;
                        }

                        let mut decoded_frame = None;
                        let mut attempts = 0;

                        while attempts < 5000 {
                            attempts += 1;
                            match fmt.next_packet() {
                                Ok(Some(packet)) => {
                                    if packet.track_id == vid_id {
                                        let data = crate::codec::to_annex_b(&packet.data, nal_length_size);
                                        if let Ok(Some(yuv)) = dec.decode(&data) {
                                            let pts_secs = packet.pts.get() as f64 * (tb.numer.get() as f64) / (tb.denom.get() as f64);
                                            last_decoded_secs = pts_secs;
                                            
                                            if pts_secs >= target_secs || (target_secs - pts_secs).abs() < 0.05 {
                                                let (w, h) = yuv.dimensions();
                                                let (y_stride, u_stride, v_stride) = yuv.strides();
                                                let mut rgba = vec![0u8; w * h * 4];
                                                yuv420p_to_rgba(
                                                    w, h,
                                                    yuv.y(), yuv.u(), yuv.v(),
                                                    y_stride, u_stride, v_stride,
                                                    &mut rgba
                                                );
                                                decoded_frame = Some((rgba, w, h, pts_secs));
                                                break;
                                            }
                                        }
                                    }
                                }
                                Ok(None) => break,
                                Err(symphonia::core::errors::Error::ResetRequired) => {
                                    last_decoded_secs = -999.0;
                                    continue;
                                }
                                Err(_) => break,
                            }
                        }

                        if let Some((rgba, w, h, pts_secs)) = decoded_frame {
                            let fps = tb.denom.get() as f64 / tb.numer.get() as f64;
                            let frame_idx = (pts_secs * fps).round() as usize;

                            let _ = resp_tx.send(DecodedFrameResponse {
                                asset_id,
                                frame_idx,
                                width: w,
                                height: h,
                                rgba,
                            });
                        }
                    }
                }
            }
        });

        // Initialize Rodio dynamic audio output
        let mut audio_sink = None;
        let mut audio_stream = None;
        if let Ok((stream, stream_handle)) = rodio::OutputStream::try_default() {
            if let Ok(sink) = rodio::Sink::try_new(&stream_handle) {
                audio_sink = Some(sink);
                audio_stream = Some(stream);
            }
        }

        Self {
            current_texture: None,
            current_frame_key: None,
            last_requested_frame: None,
            frame_cache: std::collections::HashMap::new(),
            cmd_tx,
            resp_rx,
            audio_sink,
            _audio_stream: audio_stream,
            last_audio_queue_time: 0.0,
        }
    }
    
    // Finds active video clip and mapped source timestamp at current playhead
    fn get_active_video_clip_and_time<'a>(&self, state: &'a EditorState, time_secs: f64) -> Option<(&'a crate::editor::Clip, &'a crate::media::MediaAsset, f64)> {
        let has_solo_video = state.video_tracks.iter().any(|t| t.is_solo);
        for track in state.video_tracks.iter().rev() {
            let is_active = !track.is_hidden && (!has_solo_video || track.is_solo);
            if !is_active {
                continue;
            }
            for clip in &track.clips {
                if time_secs >= clip.timeline_start && time_secs < clip.timeline_end {
                    let clip_offset = time_secs - clip.timeline_start;
                    let source_time = clip.source_trim_start + clip_offset;
                    if let Some(asset) = state.assets.iter().find(|a| a.id == clip.asset_id) {
                        return Some((clip, asset, source_time));
                    }
                }
            }
        }
        None
    }

    pub fn update(&mut self, state: &mut EditorState, ctx: &egui::Context) {
        // 1. Advance timeline playhead if playing
        if state.is_playing {
            let dt = ctx.input(|i| i.stable_dt) as f64;
            state.playhead_secs += dt;

            // Stop playback at end of the timeline
            let mut max_dur = 0.0f64;
            for track in state.video_tracks.iter().chain(state.audio_tracks.iter()) {
                for clip in &track.clips {
                    max_dur = max_dur.max(clip.timeline_end);
                }
            }
            if state.playhead_secs >= max_dur && max_dur > 0.0 {
                state.playhead_secs = max_dur;
                state.is_playing = false;
                if let Some(ref sink) = self.audio_sink {
                    sink.pause();
                }
            }
        }

        // 2. Clear frame cache if it consumes too much memory
        if self.frame_cache.len() > 300 {
            self.frame_cache.clear();
        }

        // 3. Request video frame decode/fetch
        if let Some((clip, asset, source_time)) = self.get_active_video_clip_and_time(state, state.playhead_secs) {
            let frame_idx = (source_time * asset.fps).round() as usize;
            let frame_key = (asset.id, frame_idx);

            if self.frame_cache.contains_key(&frame_key) {
                // Instantly upload texture if frame is cached and not currently displayed
                if self.current_frame_key != Some(frame_key) {
                    if let Some(cached) = self.frame_cache.get(&frame_key) {
                        let playhead = state.playhead_secs;
                        let mut alpha = 1.0f32;
                        if clip.fade_in_duration > 0.0 && (playhead - clip.timeline_start) < clip.fade_in_duration {
                            alpha *= ((playhead - clip.timeline_start) / clip.fade_in_duration) as f32;
                        }
                        if clip.fade_out_duration > 0.0 && (clip.timeline_end - playhead) < clip.fade_out_duration {
                            alpha *= ((clip.timeline_end - playhead) / clip.fade_out_duration) as f32;
                        }
                        alpha = alpha.clamp(0.0, 1.0);

                        let mut rgba = cached.rgba.clone();
                        if alpha < 1.0 {
                            rgba.par_chunks_exact_mut(4).for_each(|pixel| {
                                pixel[0] = (pixel[0] as f32 * alpha) as u8;
                                pixel[1] = (pixel[1] as f32 * alpha) as u8;
                                pixel[2] = (pixel[2] as f32 * alpha) as u8;
                            });
                        }

                        let color_image = egui::ColorImage::from_rgba_unmultiplied(
                            [cached.width, cached.height],
                            &rgba
                        );
                        self.current_texture = Some(ctx.load_texture(
                            "video_preview",
                            color_image,
                            Default::default()
                        ));
                        self.current_frame_key = Some(frame_key);
                    }
                }
            } else {
                // Request frame from background thread if not already sent
                if self.last_requested_frame != Some(frame_key) {
                    let _ = self.cmd_tx.send(DecoderCommand::RequestFrame {
                        asset_id: asset.id,
                        path: asset.path.clone(),
                        target_secs: source_time,
                    });
                    self.last_requested_frame = Some(frame_key);
                }
            }
        } else {
            self.current_texture = None;
            self.current_frame_key = None;
        }

        // 4. Ingest any newly decoded frame responses from background thread
        while let Ok(resp) = self.resp_rx.try_recv() {
            let frame_key = (resp.asset_id, resp.frame_idx);
            self.frame_cache.insert(frame_key, CachedFrame {
                width: resp.width,
                height: resp.height,
                rgba: resp.rgba
            });

            // Update displayed texture if response matches currently desired frame
            if let Some((clip, asset, source_time)) = self.get_active_video_clip_and_time(state, state.playhead_secs) {
                let current_idx = (source_time * asset.fps).round() as usize;
                if (asset.id, current_idx) == frame_key {
                    if let Some(cached) = self.frame_cache.get(&frame_key) {
                        let playhead = state.playhead_secs;
                        let mut alpha = 1.0f32;
                        if clip.fade_in_duration > 0.0 && (playhead - clip.timeline_start) < clip.fade_in_duration {
                            alpha *= ((playhead - clip.timeline_start) / clip.fade_in_duration) as f32;
                        }
                        if clip.fade_out_duration > 0.0 && (clip.timeline_end - playhead) < clip.fade_out_duration {
                            alpha *= ((clip.timeline_end - playhead) / clip.fade_out_duration) as f32;
                        }
                        alpha = alpha.clamp(0.0, 1.0);

                        let mut rgba = cached.rgba.clone();
                        if alpha < 1.0 {
                            rgba.par_chunks_exact_mut(4).for_each(|pixel| {
                                pixel[0] = (pixel[0] as f32 * alpha) as u8;
                                pixel[1] = (pixel[1] as f32 * alpha) as u8;
                                pixel[2] = (pixel[2] as f32 * alpha) as u8;
                            });
                        }

                        let color_image = egui::ColorImage::from_rgba_unmultiplied(
                            [cached.width, cached.height],
                            &rgba
                        );
                        self.current_texture = Some(ctx.load_texture(
                            "video_preview",
                            color_image,
                            Default::default()
                        ));
                        self.current_frame_key = Some(frame_key);
                    }
                }
            }
        }

        // 5. Manage audio output dynamic queueing
        if state.is_playing {
            if let Some(ref sink) = self.audio_sink {
                // Handle seeking/drifting: reset audio playhead if it goes out of sync
                if (state.playhead_secs - self.last_audio_queue_time).abs() > 0.8 {
                    sink.stop();
                    sink.play();
                    self.last_audio_queue_time = state.playhead_secs;
                }

                // Keep around 300ms queued ahead
                while sink.len() < 3 {
                    let start_t = self.last_audio_queue_time;
                    let end_t = start_t + 0.1;
                    
                    let sample_rate = 44100;
                    let channels = 2;
                    let num_samples = (0.1 * sample_rate as f64) as usize * channels;
                    let mut mixed_samples = vec![0.0f32; num_samples];

                    let has_solo_video = state.video_tracks.iter().any(|t| t.is_solo);
                    let has_solo_audio = state.audio_tracks.iter().any(|t| t.is_solo);

                    // Mix video tracks
                    for track in &state.video_tracks {
                        let is_active = !track.is_hidden && (!has_solo_video || track.is_solo);
                        if !is_active {
                            continue;
                        }
                        for clip in &track.clips {
                            let overlap_start = start_t.max(clip.timeline_start);
                            let overlap_end = end_t.min(clip.timeline_end);

                            if overlap_start < overlap_end {
                                if let Some(asset) = state.assets.iter().find(|a| a.id == clip.asset_id) {
                                    if let Some(ref samples) = asset.audio_samples {
                                        let num_frames = 4410; // 0.1 * 44100
                                        for i in 0..num_frames {
                                            let t = start_t + (i as f64 / 44100.0);
                                            if t >= clip.timeline_start && t < clip.timeline_end {
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

                                                        let mut volume_factor = track.volume;
                                                        if clip.fade_in_duration > 0.0 && clip_offset < clip.fade_in_duration {
                                                            volume_factor *= (clip_offset / clip.fade_in_duration) as f32;
                                                        }
                                                        if clip.fade_out_duration > 0.0 && (clip.timeline_end - t) < clip.fade_out_duration {
                                                            volume_factor *= ((clip.timeline_end - t) / clip.fade_out_duration) as f32;
                                                        }

                                                        mixed_samples[i * 2] += left_val * volume_factor;
                                                        mixed_samples[i * 2 + 1] += right_val * volume_factor;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Mix audio tracks
                    for track in &state.audio_tracks {
                        let is_active = !track.is_muted && (!has_solo_audio || track.is_solo);
                        if !is_active {
                            continue;
                        }
                        for clip in &track.clips {
                            let overlap_start = start_t.max(clip.timeline_start);
                            let overlap_end = end_t.min(clip.timeline_end);

                            if overlap_start < overlap_end {
                                if let Some(asset) = state.assets.iter().find(|a| a.id == clip.asset_id) {
                                    if let Some(ref samples) = asset.audio_samples {
                                        let num_frames = 4410; // 0.1 * 44100
                                        for i in 0..num_frames {
                                            let t = start_t + (i as f64 / 44100.0);
                                            if t >= clip.timeline_start && t < clip.timeline_end {
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

                                                        let mut volume_factor = track.volume;
                                                        if clip.fade_in_duration > 0.0 && clip_offset < clip.fade_in_duration {
                                                            volume_factor *= (clip_offset / clip.fade_in_duration) as f32;
                                                        }
                                                        if clip.fade_out_duration > 0.0 && (clip.timeline_end - t) < clip.fade_out_duration {
                                                            volume_factor *= ((clip.timeline_end - t) / clip.fade_out_duration) as f32;
                                                        }

                                                        mixed_samples[i * 2] += left_val * volume_factor;
                                                        mixed_samples[i * 2 + 1] += right_val * volume_factor;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Clamp to prevent digital clipping
                    for val in &mut mixed_samples {
                        *val = val.clamp(-1.0, 1.0);
                    }

                    // Push samples to the output sink
                    let source = rodio::buffer::SamplesBuffer::new(
                        channels as u16,
                        sample_rate,
                        mixed_samples,
                    );
                    sink.append(source);
                    self.last_audio_queue_time = end_t;
                }
            }
        }
        
        // Request repaint to keep GUI rendering fluid during playback
        if state.is_playing {
            ctx.request_repaint();
        }
    }

    pub fn start_playback(&mut self, state: &mut EditorState) {
        state.is_playing = true;
        if let Some(ref sink) = self.audio_sink {
            // Re-sync queue playhead
            sink.stop();
            sink.play();
            self.last_audio_queue_time = state.playhead_secs;
        }
    }

    pub fn stop_playback(&mut self, state: &mut EditorState) {
        state.is_playing = false;
        if let Some(ref sink) = self.audio_sink {
            sink.pause();
        }
    }

    pub fn seek_to(&mut self, state: &mut EditorState, time_secs: f64) {
        state.playhead_secs = time_secs;
        if let Some(ref sink) = self.audio_sink {
            sink.stop();
            if state.is_playing {
                sink.play();
            }
            self.last_audio_queue_time = time_secs;
        }
        self.last_requested_frame = None;
    }
}
