#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod media;
mod editor;
mod timeline_ui;
mod preview;
mod export;

use editor::EditorState;
use preview::PreviewEngine;
use std::path::PathBuf;

struct SiloCutApp {
    state: EditorState,
    preview: PreviewEngine,
    export_in_progress: bool,
    export_status: String,
    show_export_dialog: bool,
    export_width: u32,
    export_height: u32,
    export_fps: u32,
    export_path: Option<PathBuf>,
}

impl SiloCutApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // Apply premium dark theme styling
        let mut visuals = egui::Visuals::dark();
        visuals.widgets.active.bg_fill = egui::Color32::from_rgb(99, 102, 241); // Indigo theme accent
        visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(79, 70, 229);
        cc.egui_ctx.set_visuals(visuals);

        Self {
            state: EditorState::new(),
            preview: PreviewEngine::new(),
            export_in_progress: false,
            export_status: String::new(),
            show_export_dialog: false,
            export_width: 1920,
            export_height: 1080,
            export_fps: 30,
            export_path: None,
        }
    }
}

impl eframe::App for SiloCutApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx();
        // Handle background updates for previews
        self.preview.update(&mut self.state, ctx);

        // Check for drag-and-drop file ingestion
        ctx.input(|i| {
            if !i.raw.dropped_files.is_empty() {
                for file in &i.raw.dropped_files {
                    if let Some(path) = &file.path {
                        let next_id = self.state.assets.len();
                        match media::MediaAsset::load(next_id, path) {
                            Ok(asset) => {
                                self.state.assets.push(asset);
                            }
                            Err(e) => {
                                eprintln!("Failed to load asset: {}", e);
                            }
                        }
                    }
                }
            }
        });

        // Top Menu Bar
        egui::TopBottomPanel::top("menu_bar").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Import Media...").clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("Media Files", &["mp4", "mkv", "mov", "wav", "mp3"])
                            .pick_file()
                        {
                            let next_id = self.state.assets.len();
                            match media::MediaAsset::load(next_id, &path) {
                                Ok(asset) => {
                                    self.state.assets.push(asset);
                                }
                                Err(e) => {
                                    eprintln!("Failed to load asset: {}", e);
                                }
                            }
                        }
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("Exit").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });

                ui.menu_button("Edit", |ui| {
                    if ui.button("Split Clip (Razor)").clicked() {
                        // Triggers razor split on active clip
                        let playhead = self.state.playhead_secs;
                        if let Some((is_video, track_idx, clip_idx)) = self.state.selected_clip {
                            self.state.razor_at(is_video, track_idx, clip_idx, playhead);
                        }
                    }
                });

                ui.menu_button("Export", |ui| {
                    if ui.button("Export Video...").clicked() {
                        self.show_export_dialog = true;
                        ui.close_menu();
                    }
                });

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label("SiloCut v1.0");
                });
            });
        });

        // Bottom Timeline Panel
        egui::TopBottomPanel::bottom("timeline_panel")
            .resizable(true)
            .default_height(250.0)
            .show(ctx, |ui| {
                timeline_ui::show_timeline(ui, &mut self.state);
            });

        // Left Panel - Media Bin
        egui::SidePanel::left("media_bin_panel")
            .resizable(true)
            .default_width(280.0)
            .show(ctx, |ui| {
                ui.heading("Project Media Bin");
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
                            let label = format!("{} - {}", type_str, asset.name);
                            
                            let response = ui.selectable_label(false, label);
                            
                            // Context menu to add to timeline
                            response.context_menu(|ui| {
                                if ui.button("Add to Timeline").clicked() {
                                    // Add to track 1 or 2 based on type
                                    let track_idx = 0;
                                    let timeline_start = self.state.playhead_secs;
                                    let timeline_end = timeline_start + asset.duration_secs;
                                    
                                    let clip = editor::Clip {
                                        id: self.state.next_clip_id,
                                        asset_id: asset.id,
                                        timeline_start,
                                        timeline_end,
                                        source_trim_start: 0.0,
                                        source_trim_end: asset.duration_secs,
                                    };
                                    self.state.next_clip_id += 1;
                                    
                                    if asset.is_video {
                                        if let Some(track) = self.state.video_tracks.get_mut(track_idx) {
                                            track.clips.push(clip);
                                        }
                                    } else {
                                        if let Some(track) = self.state.audio_tracks.get_mut(track_idx) {
                                            track.clips.push(clip);
                                        }
                                    }
                                    ui.close_menu();
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
        egui::CentralPanel::default().show(ctx, |ui| {
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
                .show(ctx, |ui| {
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

                            // Run export in background thread
                            std::thread::spawn(move || {
                                match export::export_timeline(&state_clone, &path_clone, width, height, fps) {
                                    Ok(_) => println!("Export Succeeded"),
                                    Err(e) => eprintln!("Export Failed: {}", e),
                                }
                            });
                            self.show_export_dialog = false;
                        }
                    });
                });
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

