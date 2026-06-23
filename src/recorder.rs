use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::path::PathBuf;
use std::time::{Instant, Duration};
use openh264::formats::YUVBuffer;
use rayon::prelude::*;

use windows_sys::Win32::Graphics::Gdi::{
    GetDC, CreateCompatibleDC, CreateCompatibleBitmap, SelectObject, BitBlt, GetDIBits,
    DeleteObject, DeleteDC, ReleaseDC, HDC, HBITMAP, HGDIOBJ, BITMAPINFO, BITMAPINFOHEADER,
    DIB_RGB_COLORS, BI_RGB, SRCCOPY,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN,
    GetCursorInfo, GetIconInfo, DrawIconEx, CURSORINFO, ICONINFO, CURSOR_SHOWING, DI_NORMAL,
};
use windows_sys::Win32::Foundation::HWND;

/// Composites the current mouse cursor onto `hdc` at its on-screen position so the
/// recording includes the pointer. Best-effort: does nothing if the cursor is
/// hidden or can't be queried.
unsafe fn draw_cursor_onto(hdc: HDC) {
    let mut ci: CURSORINFO = std::mem::zeroed();
    ci.cbSize = std::mem::size_of::<CURSORINFO>() as u32;
    if GetCursorInfo(&mut ci) == 0 || ci.flags != CURSOR_SHOWING {
        return;
    }

    let mut icon_info: ICONINFO = std::mem::zeroed();
    if GetIconInfo(ci.hCursor, &mut icon_info) == 0 {
        return;
    }

    let x = ci.ptScreenPos.x - icon_info.xHotspot as i32;
    let y = ci.ptScreenPos.y - icon_info.yHotspot as i32;
    DrawIconEx(hdc, x, y, ci.hCursor, 0, 0, 0, 0, DI_NORMAL);

    // GetIconInfo creates bitmaps the caller must free.
    if icon_info.hbmMask != 0 {
        DeleteObject(icon_info.hbmMask as HGDIOBJ);
    }
    if icon_info.hbmColor != 0 {
        DeleteObject(icon_info.hbmColor as HGDIOBJ);
    }
}

struct GdiCaptureGuard {
    hwnd: HWND,
    hdc_screen: HDC,
    hdc_mem: HDC,
    hbitmap: HBITMAP,
    old_bitmap: HGDIOBJ,
}

impl Drop for GdiCaptureGuard {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.hdc_mem, self.old_bitmap);
            DeleteObject(self.hbitmap as HGDIOBJ);
            DeleteDC(self.hdc_mem);
            ReleaseDC(self.hwnd, self.hdc_screen);
        }
    }
}

struct RecordingGuard(Arc<AtomicBool>);
impl Drop for RecordingGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

pub struct ScreenRecorder {
    is_recording: Arc<AtomicBool>,
    thread_handle: Option<std::thread::JoinHandle<Result<PathBuf, String>>>,
}

impl ScreenRecorder {
    pub fn new() -> Self {
        Self {
            is_recording: Arc::new(AtomicBool::new(false)),
            thread_handle: None,
        }
    }

    pub fn start(&mut self, output_path: PathBuf, fps: u32) -> Result<(), String> {
        if self.is_recording.load(Ordering::SeqCst) {
            return Err("Recording is already in progress.".to_string());
        }

        let is_recording = self.is_recording.clone();
        is_recording.store(true, Ordering::SeqCst);

        let handle = std::thread::spawn(move || -> Result<PathBuf, String> {
            let _recording_guard = RecordingGuard(is_recording.clone());
            let width = unsafe { GetSystemMetrics(SM_CXSCREEN) } as usize;
            let height = unsafe { GetSystemMetrics(SM_CYSCREEN) } as usize;
            
            // Width and height must be even for OpenH264 encoding
            let width = width & !1;
            let height = height & !1;

            if width == 0 || height == 0 {
                return Err("Failed to query monitor dimensions.".to_string());
            }

            // Setup Win32 GDI resources
            let hwnd = 0 as HWND; // Desktop window
            let hdc_screen = unsafe { GetDC(hwnd) };
            if hdc_screen == 0 {
                return Err("Failed to get desktop DC.".to_string());
            }

            let hdc_mem = unsafe { CreateCompatibleDC(hdc_screen) };
            if hdc_mem == 0 {
                unsafe { ReleaseDC(hwnd, hdc_screen) };
                return Err("Failed to create compatible DC.".to_string());
            }

            let hbitmap = unsafe { CreateCompatibleBitmap(hdc_screen, width as i32, height as i32) };
            if hbitmap == 0 {
                unsafe {
                    DeleteDC(hdc_mem);
                    ReleaseDC(hwnd, hdc_screen);
                }
                return Err("Failed to create compatible bitmap.".to_string());
            }

            let old_bitmap = unsafe { SelectObject(hdc_mem, hbitmap as HGDIOBJ) };

            let _guard = GdiCaptureGuard {
                hwnd,
                hdc_screen,
                hdc_mem,
                hbitmap,
                old_bitmap,
            };

            // Setup Output MP4 Muxer
            let file = std::fs::File::create(&output_path)
                .map_err(|e| format!("Failed to create output file: {}", e))?;
            let mut writer = std::io::BufWriter::new(file);
            let mut muxer = mp4e::Mp4e::new(&mut writer);
            muxer.set_video_track(width as u32, height as u32, mp4e::Codec::AVC);

            // Setup H.264 Encoder tuned for screen capture. The OpenH264 default
            // (120 kbps, frame-skipping on, no periodic keyframes) is unusable for
            // full-screen recording; see `codec::build_encoder`.
            let mut encoder = crate::codec::build_encoder(width as u32, height as u32, fps, true)?;

            // Bitmap Info Header for GetDIBits
            let mut bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width as i32,
                    biHeight: -(height as i32), // Negative height indicates top-down DIB
                    biPlanes: 1,
                    biBitCount: 32, // 32-bit BGRA
                    biCompression: BI_RGB as u32,
                    biSizeImage: 0,
                    biXPelsPerMeter: 0,
                    biYPelsPerMeter: 0,
                    biClrUsed: 0,
                    biClrImportant: 0,
                },
                bmiColors: [windows_sys::Win32::Graphics::Gdi::RGBQUAD { rgbBlue: 0, rgbGreen: 0, rgbRed: 0, rgbReserved: 0 }; 1],
            };

            // Preallocated buffers
            let mut bgra_buffer = vec![0u8; width * height * 4];
            let mut rgb_buffer = vec![0u8; width * height * 3];
            let mut bitstream_buffer = Vec::new();

            let frame_duration = Duration::from_secs_f64(1.0 / fps as f64);

            // Track real wall-clock time between captured frames so the recording
            // plays back at the correct speed even when capture/encoding can't keep
            // up with the target FPS (otherwise the video would appear sped up).
            let mut prev_capture = Instant::now();

            while is_recording.load(Ordering::SeqCst) {
                let frame_start = Instant::now();

                // 1. GDI Capture
                unsafe {
                    BitBlt(
                        hdc_mem,
                        0, 0, width as i32, height as i32,
                        hdc_screen,
                        0, 0,
                        SRCCOPY,
                    );

                    // Composite the mouse cursor into the captured frame.
                    draw_cursor_onto(hdc_mem);

                    GetDIBits(
                        hdc_mem,
                        hbitmap,
                        0,
                        height as u32,
                        bgra_buffer.as_mut_ptr() as *mut _,
                        &mut bmi as *mut _ as *mut _,
                        DIB_RGB_COLORS,
                    );
                }

                // 2. Rayon BGRA -> RGB8 conversion
                rgb_buffer.par_chunks_exact_mut(3)
                    .zip(bgra_buffer.par_chunks_exact(4))
                    .for_each(|(rgb, bgra)| {
                        rgb[0] = bgra[2]; // R
                        rgb[1] = bgra[1]; // G
                        rgb[2] = bgra[0]; // B
                    });

                // 3. RGB8 -> YUV420p
                let yuv_buffer = YUVBuffer::from_rgb_source(
                    openh264::formats::RgbSliceU8::new(&rgb_buffer, (width, height))
                );

                // 4. H.264 encode and mux, tagging the frame with the real elapsed
                //    time since the previous captured frame.
                if let Ok(bitstream) = encoder.encode(&yuv_buffer) {
                    bitstream_buffer.clear();
                    bitstream.write_vec(&mut bitstream_buffer);
                    if !bitstream_buffer.is_empty() {
                        let now = Instant::now();
                        let dur_ms = (now - prev_capture).as_millis().max(1) as u32;
                        prev_capture = now;
                        muxer.encode_video(&bitstream_buffer, dur_ms)
                            .map_err(|e| format!("Muxer error: {}", e))?;
                    }
                }

                // 5. Pacing: sleep to maintain FPS
                let elapsed = frame_start.elapsed();
                if elapsed < frame_duration {
                    std::thread::sleep(frame_duration - elapsed);
                }
            }

            // Flush muxer
            muxer.flush().map_err(|e| format!("Muxer flush error: {}", e))?;

            Ok(output_path)
        });

        self.thread_handle = Some(handle);
        Ok(())
    }

    pub fn stop(&mut self) -> Result<PathBuf, String> {
        if !self.is_recording.load(Ordering::SeqCst) {
            return Err("No recording in progress.".to_string());
        }

        self.is_recording.store(false, Ordering::SeqCst);

        if let Some(handle) = self.thread_handle.take() {
            match handle.join() {
                Ok(res) => res,
                Err(_) => Err("Recorder thread panicked.".to_string()),
            }
        } else {
            Err("Recording thread handle was missing.".to_string())
        }
    }

    pub fn is_recording(&self) -> bool {
        self.is_recording.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records the real screen for ~1.2s and verifies the result is a valid,
    /// non-zero-duration, decodable MP4. Ignored by default because it requires an
    /// interactive desktop session (GDI screen capture); run with
    /// `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn record_screen_produces_decodable_video() {
        let tmp = std::env::temp_dir().join(format!("silocut_rec_test_{}.mp4", std::process::id()));

        let mut rec = ScreenRecorder::new();
        rec.start(tmp.clone(), 15).expect("start recording");
        std::thread::sleep(Duration::from_millis(1200));
        let path = rec.stop().expect("stop recording");

        assert!(path.exists(), "recording file should exist");
        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size > 1000, "recording suspiciously small: {size} bytes");

        // media.rs must report a non-zero duration for this video-only MP4 (the bug
        // that previously made recordings un-editable).
        let asset = crate::media::MediaAsset::load_metadata(0, &path).expect("load metadata");
        assert!(asset.is_video, "should be detected as a video asset");
        assert!(asset.duration_secs > 0.0, "duration must be > 0, got {}", asset.duration_secs);
        assert!(asset.width >= 2 && asset.height >= 2, "bad dims {}x{}", asset.width, asset.height);

        // The MP4 must decode through the exact path the editor preview/export use.
        let file = std::fs::File::open(&path).unwrap();
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
            .expect("probe recording");
        let info = crate::codec::find_video_track(&*fmt).expect("video track in recording");

        let mut dec = openh264::decoder::Decoder::new().unwrap();
        if !info.annexb_header.is_empty() {
            let _ = dec.decode(&info.annexb_header);
        }
        let mut decoded = 0;
        while let Ok(Some(pkt)) = fmt.next_packet() {
            if pkt.track_id != info.track_id {
                continue;
            }
            let annexb = crate::codec::to_annex_b(&pkt.data, info.nal_length_size);
            if let Ok(Some(_)) = dec.decode(&annexb) {
                decoded += 1;
            }
        }

        let _ = std::fs::remove_file(&path);
        assert!(decoded > 0, "no frames decoded from the screen recording");
        eprintln!(
            "recorded {:.2}s, {}x{}, {} bytes, decoded {} frames",
            asset.duration_secs, asset.width, asset.height, size, decoded
        );
    }
}
