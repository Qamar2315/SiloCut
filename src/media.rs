use std::path::{Path, PathBuf};
use std::sync::Arc;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::formats::probe::Hint;

#[derive(Clone, Debug)]
pub struct MediaAsset {
    pub id: usize,
    pub path: PathBuf,
    pub name: String,
    pub duration_secs: f64,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub is_video: bool,
    pub audio_samples: Option<Arc<Vec<f32>>>,
    pub audio_channels: u32,
    pub audio_sample_rate: u32,
}

impl MediaAsset {
    pub fn load_metadata(id: usize, path: &Path) -> Result<Self, String> {
        let file = std::fs::File::open(path)
            .map_err(|e| format!("Failed to open file: {}", e))?;
        
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        
        let format = symphonia::default::get_probe()
            .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
            .map_err(|e| format!("Failed to probe file: {}", e))?;

        // Find video and audio tracks
        let mut video_track = None;
        let mut audio_track = None;
        
        for track in format.tracks() {
            if let Some(ref params) = track.codec_params {
                match params {
                    symphonia::core::codecs::CodecParameters::Video(_) => {
                        if video_track.is_none() {
                            video_track = Some(track.clone());
                        }
                    }
                    symphonia::core::codecs::CodecParameters::Audio(_) => {
                        if audio_track.is_none() {
                            audio_track = Some(track.clone());
                        }
                    }
                    _ => {}
                }
            }
        }
        
        let name = path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();
            
        let is_video = video_track.is_some();
        let mut width = 0;
        let mut height = 0;
        let mut fps = 0.0;
        let mut duration_secs = 0.0;
        
        if let Some(ref vt) = video_track {
            if let Some(symphonia::core::codecs::CodecParameters::Video(ref video_params)) = vt.codec_params {
                width = video_params.width.unwrap_or(0) as u32;
                height = video_params.height.unwrap_or(0) as u32;
            }
            // `time_base` for MP4 is 1/timescale (e.g. 1/90000), so denom/numer is the
            // timescale, not the frame rate. Only trust it when it lands in a plausible
            // FPS range; otherwise fall back to 30.
            fps = vt.time_base
                .map(|tb| tb.denom.get() as f64 / tb.numer.get() as f64)
                .filter(|f| (1.0..=240.0).contains(f))
                .unwrap_or(30.0);

            // Duration must come from the `duration` field (in timebase ticks):
            // symphonia leaves `num_frames` unset for video tracks, so the previous
            // num_frames-based calculation always yielded 0 for video-only files
            // (e.g. screen recordings), producing zero-length clips.
            if let (Some(tb), Some(dur)) = (vt.time_base, vt.duration) {
                duration_secs = dur.get() as f64 * tb.numer.get() as f64 / tb.denom.get() as f64;
            }
        }
        
        let mut audio_channels = 0;
        let mut audio_sample_rate = 0;
        
        if let Some(ref at) = audio_track {
            if let Some(symphonia::core::codecs::CodecParameters::Audio(ref audio_params)) = at.codec_params {
                audio_channels = audio_params.channels
                    .as_ref()
                    .map(|c| c.count() as u32)
                    .unwrap_or(0);
                audio_sample_rate = audio_params.sample_rate.unwrap_or(0);
                
                if let (Some(tb), Some(dur)) = (at.time_base, at.duration) {
                    let d = dur.get() as f64 * tb.numer.get() as f64 / tb.denom.get() as f64;
                    if !is_video {
                        duration_secs = d;
                    }
                }
            }
        }
        
        if duration_secs == 0.0 {
            for track in format.tracks() {
                if let (Some(tb), Some(dur)) = (track.time_base, track.duration) {
                    let d = dur.get() as f64 * tb.numer.get() as f64 / tb.denom.get() as f64;
                    if d > duration_secs {
                        duration_secs = d;
                    }
                }
            }
        }
        
        Ok(Self {
            id,
            path: path.to_path_buf(),
            name,
            duration_secs,
            width,
            height,
            fps,
            is_video,
            audio_samples: None,
            audio_channels,
            audio_sample_rate,
        })
    }

    pub fn decode_audio_samples(path: &Path) -> Result<Vec<f32>, String> {
        let file = std::fs::File::open(path)
            .map_err(|e| format!("Failed to open file: {}", e))?;
        
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        
        let mut format = symphonia::default::get_probe()
            .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
            .map_err(|e| format!("Failed to probe file: {}", e))?;
            
        let mut audio_track = None;
        for track in format.tracks() {
            if let Some(ref params) = track.codec_params {
                if let symphonia::core::codecs::CodecParameters::Audio(_) = params {
                    audio_track = Some(track.clone());
                    break;
                }
            }
        }
        
        let at = audio_track.ok_or_else(|| "No audio track found for decoding".to_string())?;
        let audio_params = match at.codec_params {
            Some(symphonia::core::codecs::CodecParameters::Audio(p)) => p,
            _ => return Err("Unexpected track parameters".to_string()),
        };
        
        let mut decoder = symphonia::default::get_codecs()
            .make_audio_decoder(&audio_params, &AudioDecoderOptions::default())
            .map_err(|e| format!("Failed to create audio decoder: {}", e))?;
            
        let mut samples_vec = Vec::new();
        let audio_track_id = at.id;
        
        while let Ok(Some(packet)) = format.next_packet() {
            if packet.track_id == audio_track_id {
                match decoder.decode(&packet) {
                    Ok(audio_buf) => {
                        audio_buf.copy_to_vec_interleaved(&mut samples_vec);
                    }
                    Err(symphonia::core::errors::Error::DecodeError(_)) => {
                        // Skip decoding errors
                    }
                    Err(e) => {
                        return Err(format!("Audio decode error: {}", e));
                    }
                }
            }
        }
        
        if samples_vec.is_empty() {
            return Err("No audio samples decoded".to_string());
        }
        
        Ok(samples_vec)
    }
}
