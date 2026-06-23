use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::path::PathBuf;
use std::time::{Instant, Duration};
use openh264::encoder::Encoder;
use openh264::formats::YUVBuffer;
use rayon::prelude::*;

use windows_sys::Win32::Graphics::Gdi::{
    GetDC, CreateCompatibleDC, CreateCompatibleBitmap, SelectObject, BitBlt, GetDIBits,
    DeleteObject, DeleteDC, ReleaseDC, HDC, HBITMAP, HGDIOBJ, BITMAPINFO, BITMAPINFOHEADER,
    DIB_RGB_COLORS, BI_RGB, SRCCOPY,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN,
};
use windows_sys::Win32::Foundation::HWND;

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

            // Setup H.264 Encoder
            let mut encoder = Encoder::new().map_err(|e| format!("Encoder error: {}", e))?;

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
            let frame_duration_ms = 1000 / fps;

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

                // 4. H.264 encode and mux
                if let Ok(bitstream) = encoder.encode(&yuv_buffer) {
                    bitstream_buffer.clear();
                    bitstream.write_vec(&mut bitstream_buffer);
                    muxer.encode_video(&bitstream_buffer, frame_duration_ms)
                        .map_err(|e| format!("Muxer error: {}", e))?;
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
