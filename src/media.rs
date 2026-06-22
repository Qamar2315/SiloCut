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
    pub fn load(id: usize, path: &Path) -> Result<Self, String> {
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
            fps = vt.time_base
                .map(|tb| tb.denom.get() as f64 / tb.numer.get() as f64)
                .unwrap_or(30.0);
                
            if let (Some(tb), Some(num_frames)) = (vt.time_base, vt.num_frames) {
                duration_secs = num_frames as f64 * (tb.numer.get() as f64) / (tb.denom.get() as f64);
            }
        }
        
        let mut audio_samples = None;
        let mut audio_channels = 0;
        let mut audio_sample_rate = 0;
        
        if let Some(ref at) = audio_track {
            if let Some(symphonia::core::codecs::CodecParameters::Audio(ref audio_params)) = at.codec_params {
                let mut decoder = symphonia::default::get_codecs()
                    .make_audio_decoder(audio_params, &AudioDecoderOptions::default())
                    .map_err(|e| format!("Failed to create audio decoder: {}", e))?;
                    
                audio_channels = audio_params.channels
                    .as_ref()
                    .map(|c| c.count() as u32)
                    .unwrap_or(0);
                audio_sample_rate = audio_params.sample_rate.unwrap_or(0);
                
                let mut samples_vec = Vec::new();
                let audio_track_id = at.id;
                
                while let Ok(Some(packet)) = format.next_packet() {
                    if packet.track_id == audio_track_id {
                        match decoder.decode(&packet) {
                            Ok(audio_buf) => {
                                let spec = audio_buf.spec();
                                audio_channels = spec.channels().count() as u32;
                                audio_sample_rate = spec.rate();
                                
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
                
                if !samples_vec.is_empty() {
                    if !is_video && audio_channels > 0 && audio_sample_rate > 0 {
                        duration_secs = samples_vec.len() as f64 / (audio_channels as f64 * audio_sample_rate as f64);
                    }
                    audio_samples = Some(Arc::new(samples_vec));
                }
            }
        }
        
        if duration_secs == 0.0 {
            for track in format.tracks() {
                if let (Some(tb), Some(num_frames)) = (track.time_base, track.num_frames) {
                    let d = num_frames as f64 * (tb.numer.get() as f64) / (tb.denom.get() as f64);
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
            audio_samples,
            audio_channels,
            audio_sample_rate,
        })
    }
}
