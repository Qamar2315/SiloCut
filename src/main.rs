#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod media;
mod editor;
mod timeline_ui;
mod preview;
mod export;
mod recorder;
mod codec;

use editor::EditorState;
use preview::PreviewEngine;
use std::path::PathBuf;

struct SiloCutApp {
    state: EditorState,
    preview: PreviewEngine,
    recorder: recorder::ScreenRecorder,
    export_in_progress: bool,
    export_status: String,
    show_export_dialog: bool,
    export_width: u32,
    export_height: u32,
    export_fps: u32,
    export_path: Option<PathBuf>,
    
    // Undo/Redo history stacks
    undo_stack: Vec<EditorState>,
    redo_stack: Vec<EditorState>,
    skip_undo_save: bool,
    // Pre-interaction snapshot, used to coalesce a continuous drag into a single
    // undo entry (captured on pointer-down, committed on release).
    interaction_baseline: Option<EditorState>,

    // Channel receiver for background export thread
    export_rx: Option<std::sync::mpsc::Receiver<Result<PathBuf, String>>>,
    // Progress (0.0–1.0) reported by the background export thread.
    export_progress: f32,
    export_progress_rx: Option<std::sync::mpsc::Receiver<f32>>,

    // Asynchronous audio decoding channels
    audio_decode_rx: std::sync::mpsc::Receiver<(usize, Result<Vec<f32>, String>)>,
    audio_decode_tx: std::sync::mpsc::Sender<(usize, Result<Vec<f32>, String>)>,
    decoding_assets: std::collections::HashSet<usize>,
}

impl SiloCutApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // Apply premium dark theme styling
        let mut visuals = egui::Visuals::dark();
        visuals.widgets.active.bg_fill = egui::Color32::from_rgb(99, 102, 241); // Indigo theme accent
        visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(79, 70, 229);
        cc.egui_ctx.set_visuals(visuals);

        let (audio_decode_tx, audio_decode_rx) = std::sync::mpsc::channel();

        Self {
            state: EditorState::new(),
            preview: PreviewEngine::new(),
            recorder: recorder::ScreenRecorder::new(),
            export_in_progress: false,
            export_status: String::new(),
            show_export_dialog: false,
            export_width: 1920,
            export_height: 1080,
            export_fps: 30,
            export_path: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            skip_undo_save: false,
            interaction_baseline: None,
            export_rx: None,
            export_progress: 0.0,
            export_progress_rx: None,
            audio_decode_rx,
            audio_decode_tx,
            decoding_assets: std::collections::HashSet::new(),
        }
    }

    /// Push a *pre-change* snapshot onto the undo stack (capped at 50 entries).
    fn push_undo_state(&mut self, snapshot: EditorState) {
        if self.undo_stack.len() >= 50 {
            self.undo_stack.remove(0);
        }
        self.undo_stack.push(snapshot);
        self.redo_stack.clear();
    }

    /// Whether two states differ in a way worth recording for undo. Only timeline
    /// content matters — selection, playhead, zoom and scroll are not undoable.
    fn timeline_differs(a: &EditorState, b: &EditorState) -> bool {
        a.video_tracks != b.video_tracks
            || a.audio_tracks != b.audio_tracks
            || a.assets.len() != b.assets.len()
            || a.next_clip_id != b.next_clip_id
            || a.next_track_id != b.next_track_id
    }

    fn undo(&mut self) {
        if let Some(prev_state) = self.undo_stack.pop() {
            self.redo_stack.push(self.state.clone());
            self.state = prev_state;
            let playhead = self.state.playhead_secs;
            self.preview.seek_to(&mut self.state, playhead);
            self.skip_undo_save = true;
        }
    }

    fn redo(&mut self) {
        if let Some(next_state) = self.redo_stack.pop() {
            self.undo_stack.push(self.state.clone());
            self.state = next_state;
            let playhead = self.state.playhead_secs;
            self.preview.seek_to(&mut self.state, playhead);
            self.skip_undo_save = true;
        }
    }
}

impl eframe::App for SiloCutApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Clone the context (cheap Arc) so we can still hold a handle to it for
        // ctx-level calls while passing `ui` mutably to the panels' `show_inside`.
        let ctx = ui.ctx().clone();

        if self.recorder.is_recording() || !self.decoding_assets.is_empty() || self.export_in_progress {
            ctx.request_repaint();
        }

        // Drain export progress updates from the background thread.
        if let Some(ref prx) = self.export_progress_rx {
            while let Ok(p) = prx.try_recv() {
                self.export_progress = p;
            }
        }

        // Receive background audio decoding updates
        while let Ok((asset_id, res)) = self.audio_decode_rx.try_recv() {
            self.decoding_assets.remove(&asset_id);
            match res {
                Ok(samples) => {
                    if let Some(asset) = self.state.assets.iter_mut().find(|a| a.id == asset_id) {
                        asset.audio_samples = Some(std::sync::Arc::new(samples));
                        // If it's an audio-only file, compute the duration
                        if !asset.is_video && asset.audio_channels > 0 && asset.audio_sample_rate > 0 {
                            asset.duration_secs = asset.audio_samples.as_ref().unwrap().len() as f64
                                / (asset.audio_channels as f64 * asset.audio_sample_rate as f64);
                        }
                    }
                }
                Err(e) => {
                    self.export_status = format!("Audio decoding failed: {}", e);
                }
            }
        }

        // 1. Check for background export thread updates
        if let Some(ref rx) = self.export_rx {
            if let Ok(res) = rx.try_recv() {
                self.export_in_progress = false;
                self.export_progress_rx = None;
                match res {
                    Ok(p) => {
                        let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
                        let wav_name = p.with_extension("wav").file_name()
                            .unwrap_or_default().to_string_lossy().to_string();
                        self.export_status = format!("Exported {} (audio: {})", name, wav_name);
                    }
                    Err(e) => {
                        self.export_status = format!("Export failed: {}", e);
                    }
                }
                self.export_rx = None;
            }
        }

        // Save state snapshot before UI layout/interaction to detect timeline changes
        let last_frame_state = self.state.clone();

        // Handle background updates for previews
        self.preview.update(&mut self.state, &ctx);

        // Check for drag-and-drop file ingestion
        ctx.input(|i| {
            if !i.raw.dropped_files.is_empty() {
                for file in &i.raw.dropped_files {
                    if let Some(path) = &file.path {
                        let next_id = self.state.assets.len();
                        match media::MediaAsset::load_metadata(next_id, path) {
                            Ok(asset) => {
                                let asset_id = asset.id;
                                let has_audio = asset.audio_channels > 0 && asset.audio_sample_rate > 0;
                                self.state.assets.push(asset);
                                
                                if has_audio {
                                    self.decoding_assets.insert(asset_id);
                                    let path_clone = path.clone();
                                    let tx_clone = self.audio_decode_tx.clone();
                                    std::thread::spawn(move || {
                                        let res = media::MediaAsset::decode_audio_samples(&path_clone);
                                        let _ = tx_clone.send((asset_id, res));
                                    });
                                }
                            }
                            Err(e) => {
                                eprintln!("Failed to load asset metadata: {}", e);
                            }
                        }
                    }
                }
            }
        });

        // Capture keyboard hotkeys
        if !ctx.egui_wants_keyboard_input() {
            // Space: Toggle Play/Pause
            if ctx.input(|i| i.key_pressed(egui::Key::Space)) {
                if self.state.is_playing {
                    self.preview.stop_playback(&mut self.state);
                } else {
                    self.preview.start_playback(&mut self.state);
                }
            }

            // Left Arrow: Step back 1 frame (1/30th sec)
            if ctx.input(|i| i.key_pressed(egui::Key::ArrowLeft)) {
                let new_time = (self.state.playhead_secs - 1.0 / 30.0).max(0.0);
                self.preview.seek_to(&mut self.state, new_time);
            }

            // Right Arrow: Step forward 1 frame (1/30th sec)
            if ctx.input(|i| i.key_pressed(egui::Key::ArrowRight)) {
                let new_time = self.state.playhead_secs + 1.0 / 30.0;
                self.preview.seek_to(&mut self.state, new_time);
            }

            // Delete or Backspace: Remove selected clip
            if ctx.input(|i| i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace)) {
                if let Some((is_video, track_idx, clip_idx)) = self.state.selected_clip {
                    let tracks = if is_video { &mut self.state.video_tracks } else { &mut self.state.audio_tracks };
                    if track_idx < tracks.len() && clip_idx < tracks[track_idx].clips.len() {
                        tracks[track_idx].clips.remove(clip_idx);
                        self.state.selected_clip = None;
                    }
                }
            }

            // Ctrl+Z: Undo
            if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Z)) {
                self.undo();
            }

            // Ctrl+Y: Redo
            if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Y)) {
                self.redo();
            }
        }

        // Top Menu Bar
        egui::Panel::top("menu_bar").show_inside(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Import Media...").clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("Media Files", &["mp4", "mkv", "mov", "wav", "mp3"])
                            .pick_file()
                        {
                            let next_id = self.state.assets.len();
                            match media::MediaAsset::load_metadata(next_id, &path) {
                                Ok(asset) => {
                                    let asset_id = asset.id;
                                    let has_audio = asset.audio_channels > 0 && asset.audio_sample_rate > 0;
                                    self.state.assets.push(asset);
                                    
                                    if has_audio {
                                        self.decoding_assets.insert(asset_id);
                                        let path_clone = path.clone();
                                        let tx_clone = self.audio_decode_tx.clone();
                                        std::thread::spawn(move || {
                                            let res = media::MediaAsset::decode_audio_samples(&path_clone);
                                            let _ = tx_clone.send((asset_id, res));
                                        });
                                    }
                                }
                                Err(e) => {
                                    eprintln!("Failed to load asset metadata: {}", e);
                                }
                            }
                        }
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Exit").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });

                ui.menu_button("Edit", |ui| {
                    if ui.add_enabled(!self.undo_stack.is_empty(), egui::Button::new("Undo (Ctrl+Z)")).clicked() {
                        self.undo();
                        ui.close();
                    }
                    if ui.add_enabled(!self.redo_stack.is_empty(), egui::Button::new("Redo (Ctrl+Y)")).clicked() {
                        self.redo();
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Split Clip (Razor)").clicked() {
                        let playhead = self.state.playhead_secs;
                        if let Some((is_video, track_idx, clip_idx)) = self.state.selected_clip {
                            self.state.razor_at(is_video, track_idx, clip_idx, playhead);
                        }
                        ui.close();
                    }
                });

                ui.menu_button("Export", |ui| {
                    if ui.button("Export Video & Audio...").clicked() {
                        self.show_export_dialog = true;
                        ui.close();
                    }
                });

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.export_in_progress {
                        ui.spinner();
                        ui.label(format!("Rendering… {:.0}%", self.export_progress * 100.0));
                    } else if !self.export_status.is_empty() {
                        ui.label(&self.export_status);
                    } else {
                        ui.label("SiloCut v1.4");
                    }
                });
            });
        });

        // Bottom Timeline Panel
        egui::Panel::bottom("timeline_panel")
            .resizable(true)
            .default_size(250.0)
            .show_inside(ui, |ui| {
                timeline_ui::show_timeline(ui, &mut self.state);
            });

        // Left Panel - Media Bin
        egui::Panel::left("media_bin_panel")
            .resizable(true)
            .default_size(280.0)
            .show_inside(ui, |ui| {
                ui.heading("Project Media Bin");
                ui.separator();

                // Screen Recorder Section
                ui.group(|ui| {
                    ui.label(egui::RichText::new("Screen Recorder").strong());
                    ui.horizontal(|ui| {
                        if self.recorder.is_recording() {
                            // Flashing red dot
                            let time = ui.input(|i| i.time);
                            let alpha = (((time * 5.0).sin() + 1.0) * 127.5) as u8;
                            let dot_color = egui::Color32::from_rgba_unmultiplied(239, 68, 68, alpha);
                            
                            let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                            ui.painter().circle_filled(rect.center(), 5.0, dot_color);
                            
                            ui.label(egui::RichText::new("REC").color(egui::Color32::from_rgb(239, 68, 68)).strong());
                            
                            if ui.button("Stop").clicked() {
                                match self.recorder.stop() {
                                    Ok(path) => {
                                        let next_id = self.state.assets.len();
                                        match media::MediaAsset::load_metadata(next_id, &path) {
                                            Ok(asset) => {
                                                self.state.assets.push(asset);
                                                self.export_status = "Recording saved and imported!".to_string();
                                            }
                                            Err(e) => {
                                                self.export_status = format!("Failed to load recorded asset: {}", e);
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        self.export_status = format!("Recording error: {}", e);
                                    }
                                }
                            }
                        } else {
                            if ui.button("Start Recording").clicked() {
                                if let Some(path) = rfd::FileDialog::new()
                                    .add_filter("MP4 Video", &["mp4"])
                                    .set_file_name("recording.mp4")
                                    .save_file()
                                {
                                    if let Err(e) = self.recorder.start(path, 30) {
                                        self.export_status = format!("Failed to start recording: {}", e);
                                    } else {
                                        self.export_status = "Recording started...".to_string();
                                    }
                                }
                            }
                        }
                    });
                });
                ui.separator();

                if self.state.assets.is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(40.0);
                        ui.label("Drag and drop files here");
                        ui.label("or click Import Media in File menu");
                    });
                } else {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        for asset in &self.state.assets {
                            let type_str = if asset.is_video { "▶ Video" } else { "♫ Audio" };
                            let is_decoding = self.decoding_assets.contains(&asset.id);
                            let label = if is_decoding {
                                format!("{} - {} (Decoding Audio...)", type_str, asset.name)
                            } else {
                                format!("{} - {}", type_str, asset.name)
                            };
                            
                            let response = ui.selectable_label(false, label);
                            
                            // Context menu to add to timeline
                            response.context_menu(|ui| {
                                let btn = egui::Button::new("Add to Timeline");
                                if ui.add_enabled(!is_decoding, btn).clicked() {
                                    let timeline_start = self.state.playhead_secs;
                                    let timeline_end = timeline_start + asset.duration_secs;
                                    
                                    let clip = editor::Clip {
                                        id: self.state.next_clip_id,
                                        asset_id: asset.id,
                                        timeline_start,
                                        timeline_end,
                                        source_trim_start: 0.0,
                                        source_trim_end: asset.duration_secs,
                                        fade_in_duration: 0.0,
                                        fade_out_duration: 0.0,
                                    };
                                    self.state.next_clip_id += 1;
                                    
                                    if asset.is_video {
                                        if self.state.video_tracks.is_empty() {
                                            self.state.video_tracks.push(editor::Track {
                                                id: self.state.next_track_id,
                                                name: "Video 1".to_string(),
                                                is_video: true,
                                                is_muted: false,
                                                is_hidden: false,
                                                is_solo: false,
                                                volume: 1.0,
                                                clips: Vec::new(),
                                            });
                                            self.state.next_track_id += 1;
                                        }
                                        self.state.video_tracks[0].clips.push(clip);
                                    } else {
                                        if self.state.audio_tracks.is_empty() {
                                            self.state.audio_tracks.push(editor::Track {
                                                id: self.state.next_track_id,
                                                name: "Audio 1".to_string(),
                                                is_video: false,
                                                is_muted: false,
                                                is_hidden: false,
                                                is_solo: false,
                                                volume: 1.0,
                                                clips: Vec::new(),
                                            });
                                            self.state.next_track_id += 1;
                                        }
                                        self.state.audio_tracks[0].clips.push(clip);
                                    }
                                    ui.close();
                                }
                            });
                            
                            response.on_hover_text(format!(
                                "Path: {}\nDuration: {:.2}s\nResolution: {}x{}\nFPS: {:.2}",
                                asset.path.to_string_lossy(),
                                asset.duration_secs,
                                asset.width,
                                asset.height,
                                asset.fps
                            ));
                        }
                    });
                }
            });

        // Central Panel - Preview Viewport
        egui::CentralPanel::default().show_inside(ui, |ui| {
            ui.heading("Preview Viewport");
            ui.separator();

            ui.vertical_centered(|ui| {
                // Video display box
                let aspect_ratio = 16.0 / 9.0;
                let display_width = (ui.available_width() - 20.0).max(100.0);
                let display_height = display_width / aspect_ratio;
                
                let (rect, _response) = ui.allocate_exact_size(
                    egui::vec2(display_width, display_height),
                    egui::Sense::hover()
                );

                // Draw background placeholder
                ui.painter().rect_filled(
                    rect,
                    4.0,
                    egui::Color32::from_black_alpha(200)
                );

                if let Some(texture) = &self.preview.current_texture {
                    ui.painter().image(
                        texture.id(),
                        rect,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        egui::Color32::WHITE
                    );
                } else {
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "No Source Active",
                        egui::FontId::proportional(20.0),
                        egui::Color32::GRAY
                    );
                }

                ui.add_space(10.0);

                // Playback Controls
                ui.horizontal(|ui| {
                    ui.add_space(ui.available_width() / 2.0 - 100.0);
                    
                    if ui.button("⏪").clicked() {
                        self.preview.seek_to(&mut self.state, 0.0);
                    }

                    let play_pause_label = if self.state.is_playing { "⏸" } else { "▶" };
                    if ui.button(play_pause_label).clicked() {
                        if self.state.is_playing {
                            self.preview.stop_playback(&mut self.state);
                        } else {
                            self.preview.start_playback(&mut self.state);
                        }
                    }

                    // Format timestamp
                    let minutes = (self.state.playhead_secs / 60.0) as u32;
                    let seconds = (self.state.playhead_secs % 60.0) as u32;
                    let ms = ((self.state.playhead_secs % 1.0) * 100.0) as u32;
                    ui.label(format!("Time: {:02}:{:02}.{:02}", minutes, seconds, ms));
                });
            });
        });

        // Export Dialog Window
        if self.show_export_dialog {
            egui::Window::new("Export Settings")
                .collapsible(false)
                .resizable(false)
                .show(&ctx, |ui| {
                    egui::Grid::new("export_grid").show(ui, |ui| {
                        ui.label("Resolution:");
                        ui.horizontal(|ui| {
                            ui.radio_value(&mut self.export_width, 1920, "1080p");
                            self.export_height = if self.export_width == 1920 { 1080 } else { 720 };
                            ui.radio_value(&mut self.export_width, 1280, "720p");
                        });
                        ui.end_row();

                        ui.label("Frame Target:");
                        ui.horizontal(|ui| {
                            ui.radio_value(&mut self.export_fps, 30, "30 FPS");
                            ui.radio_value(&mut self.export_fps, 60, "60 FPS");
                        });
                        ui.end_row();
                    });

                    ui.add_space(10.0);

                    ui.horizontal(|ui| {
                        if ui.button("Select Output File...").clicked() {
                            if let Some(path) = rfd::FileDialog::new()
                                .add_filter("MP4 Video", &["mp4"])
                                .save_file()
                            {
                                self.export_path = Some(path);
                            }
                        }

                        if let Some(path) = &self.export_path {
                            ui.label(path.file_name().unwrap_or_default().to_string_lossy());
                        } else {
                            ui.label("No file selected");
                        }
                    });

                    ui.separator();

                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            self.show_export_dialog = false;
                        }

                        let can_export = self.export_path.is_some() && !self.export_in_progress;
                        if ui.add_enabled(can_export, egui::Button::new("Export")).clicked() {
                            self.export_in_progress = true;
                            self.export_status = "Exporting...".to_string();
                            
                            let state_clone = self.state.clone();
                            let path_clone = self.export_path.clone().unwrap();
                            let width = self.export_width;
                            let height = self.export_height;
                            let fps = self.export_fps;

                            let (tx, rx) = std::sync::mpsc::channel();
                            self.export_rx = Some(rx);
                            let (ptx, prx) = std::sync::mpsc::channel();
                            self.export_progress_rx = Some(prx);
                            self.export_progress = 0.0;

                            // Run export in background thread
                            std::thread::spawn(move || {
                                let res = export::export_timeline(
                                    &state_clone, &path_clone, width, height, fps,
                                    |p| { let _ = ptx.send(p); },
                                );
                                let _ = tx.send(res.map(|_| path_clone));
                            });
                            self.show_export_dialog = false;
                        }
                    });
                });
        }

        // --- Undo/redo bookkeeping ---
        // Coalesce a continuous drag into one undo entry: snapshot state at the start
        // of a pointer interaction and commit it only on release. Discrete changes
        // (menu actions, keyboard shortcuts) are committed immediately. In all cases
        // we record the *pre-change* state so the first undo actually reverts.
        let pointer_down = ctx.input(|i| i.pointer.primary_down());

        if self.skip_undo_save {
            // An undo/redo restored state this frame — don't re-record it.
            self.skip_undo_save = false;
            self.interaction_baseline = None;
        } else if pointer_down {
            if self.interaction_baseline.is_none() {
                self.interaction_baseline = Some(last_frame_state);
            }
        } else if let Some(baseline) = self.interaction_baseline.take() {
            // A drag/click interaction just ended; record one entry if it changed.
            if Self::timeline_differs(&baseline, &self.state) {
                self.push_undo_state(baseline);
            }
        } else if Self::timeline_differs(&last_frame_state, &self.state) {
            // Discrete (non-pointer) change such as a keyboard shortcut.
            self.push_undo_state(last_frame_state);
        }
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("SiloCut - Zero-Dependency NLE")
            .with_inner_size([1280.0, 720.0]),
        ..Default::default()
    };
    
    eframe::run_native(
        "silocut",
        options,
        Box::new(|cc| Ok(Box::new(SiloCutApp::new(cc)))),
    )
}
