//! Background generation of small "poster frame" thumbnails for timeline clips.
//!
//! A single worker thread decodes the first frame of each requested video asset
//! (or loads an image), downscales it, and returns the RGBA pixels. The GUI thread
//! turns those into egui textures and draws them on the clips.

use openh264::decoder::Decoder;
use openh264::formats::YUVSource;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

/// Target thumbnail height in pixels (width follows the source aspect ratio).
const THUMB_H: u32 = 64;

pub struct ThumbResponse {
    pub asset_id: usize,
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

pub struct ThumbnailService {
    tx: Sender<(usize, PathBuf, bool)>,
    pub rx: Receiver<ThumbResponse>,
}

impl ThumbnailService {
    pub fn new() -> Self {
        let (cmd_tx, cmd_rx) = channel::<(usize, PathBuf, bool)>();
        let (resp_tx, resp_rx) = channel::<ThumbResponse>();

        std::thread::spawn(move || {
            while let Ok((asset_id, path, is_image)) = cmd_rx.recv() {
                if let Some((rgba, w, h)) = decode_thumb(&path, is_image) {
                    let _ = resp_tx.send(ThumbResponse { asset_id, width: w, height: h, rgba });
                }
            }
        });

        Self { tx: cmd_tx, rx: resp_rx }
    }

    /// Queue a thumbnail decode for an asset.
    pub fn request(&self, asset_id: usize, path: PathBuf, is_image: bool) {
        let _ = self.tx.send((asset_id, path, is_image));
    }
}

fn decode_thumb(path: &Path, is_image: bool) -> Option<(Vec<u8>, usize, usize)> {
    if is_image {
        let rgba = image::open(path).ok()?.to_rgba8();
        let (w, h) = (rgba.width() as usize, rgba.height() as usize);
        return Some(downscale_rgba(&rgba.into_raw(), w, h));
    }

    let file = std::fs::File::open(path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut fmt = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .ok()?;

    let info = crate::codec::find_video_track(&*fmt)?;
    let mut dec = Decoder::new().ok()?;
    if !info.annexb_header.is_empty() {
        let _ = dec.decode(&info.annexb_header);
    }

    let mut attempts = 0;
    while attempts < 600 {
        attempts += 1;
        match fmt.next_packet() {
            Ok(Some(pkt)) => {
                if pkt.track_id != info.track_id {
                    continue;
                }
                let data = crate::codec::to_annex_b(&pkt.data, info.nal_length_size);
                if let Ok(Some(yuv)) = dec.decode(&data) {
                    let (w, h) = yuv.dimensions();
                    let mut rgb = vec![0u8; w * h * 3];
                    yuv.write_rgb8(&mut rgb);
                    let mut rgba = vec![0u8; w * h * 4];
                    for i in 0..w * h {
                        rgba[i * 4] = rgb[i * 3];
                        rgba[i * 4 + 1] = rgb[i * 3 + 1];
                        rgba[i * 4 + 2] = rgb[i * 3 + 2];
                        rgba[i * 4 + 3] = 255;
                    }
                    return Some(downscale_rgba(&rgba, w, h));
                }
            }
            _ => break,
        }
    }
    None
}

fn downscale_rgba(rgba: &[u8], w: usize, h: usize) -> (Vec<u8>, usize, usize) {
    if w == 0 || h == 0 {
        return (rgba.to_vec(), w, h);
    }
    let new_h = THUMB_H as usize;
    let new_w = (((w as f32 / h as f32) * new_h as f32).round() as usize).max(1);
    if let Some(buf) = image::RgbaImage::from_raw(w as u32, h as u32, rgba.to_vec()) {
        let resized = image::imageops::resize(
            &buf,
            new_w as u32,
            new_h as u32,
            image::imageops::FilterType::Triangle,
        );
        return (resized.into_raw(), new_w, new_h);
    }
    (rgba.to_vec(), w, h)
}
