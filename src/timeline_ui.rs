use crate::editor::{EditorState, Clip, EditTool, Track};
use egui::{pos2, Rect, Color32, Vec2, Stroke, FontId, Align2};

#[derive(Clone, Copy, Debug, PartialEq)]
enum DragState {
    None,
    Playhead,
    ClipMove {
        clip_id: usize,
        is_video: bool,
        drag_start_mouse_x: f32,
        drag_start_mouse_y: f32,
        drag_start_clip_start: f64,
        drag_start_track_idx: usize,
    },
    ClipTrimLeft {
        clip_id: usize,
        is_video: bool,
        drag_start_mouse_x: f32,
        drag_start_clip_start: f64,
        drag_start_clip_end: f64,
        drag_start_trim_start: f64,
    },
    ClipTrimRight {
        clip_id: usize,
        is_video: bool,
        drag_start_mouse_x: f32,
        drag_start_clip_end: f64,
        drag_start_clip_start: f64,
        drag_start_trim_end: f64,
    },
}

pub fn show_timeline(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    thumbs: &std::collections::HashMap<usize, egui::TextureHandle>,
) {
    let header_width = 160.0;
    let ruler_height = 30.0;
    let track_height = 60.0;
    let track_spacing = 8.0;
    
    let num_video_tracks = state.video_tracks.len();
    let num_audio_tracks = state.audio_tracks.len();
    let total_tracks = num_video_tracks + num_audio_tracks;
    let total_height = ruler_height + (total_tracks as f32) * (track_height + track_spacing);

    // 1. Toolbar and Controls
    ui.horizontal(|ui| {
        ui.label(format!("Playhead: {:.2}s", state.playhead_secs));
        ui.separator();
        
        ui.label("Tool:");
        ui.radio_value(&mut state.selected_tool, EditTool::Select, "Select");
        ui.radio_value(&mut state.selected_tool, EditTool::Razor, "Razor");
        ui.separator();

        ui.label("Zoom:");
        ui.add(egui::Slider::new(&mut state.zoom_factor, 5.0..=100.0).show_value(true));
        if ui.button("Reset").clicked() {
            state.zoom_factor = 15.0;
        }
        if ui.button("Fit").clicked() {
            let mut max_t = 1.0f64;
            for tr in state.video_tracks.iter().chain(state.audio_tracks.iter()) {
                for c in &tr.clips {
                    max_t = max_t.max(c.timeline_end);
                }
            }
            let visible = (ui.ctx().content_rect().width() - header_width - 16.0).max(200.0);
            state.zoom_factor = (visible / max_t as f32).clamp(5.0, 100.0);
            state.scroll_offset = 0.0;
        }
        ui.separator();

        if ui.button("➕ Video Track").clicked() {
            let next_idx = state.video_tracks.len() + 1;
            state.video_tracks.push(Track {
                id: state.next_track_id,
                name: format!("Video {}", next_idx),
                is_video: true,
                is_muted: false,
                is_hidden: false,
                is_solo: false,
                volume: 1.0,
                clips: Vec::new(),
            });
            state.next_track_id += 1;
        }

        if ui.button("➕ Audio Track").clicked() {
            let next_idx = state.audio_tracks.len() + 1;
            state.audio_tracks.push(Track {
                id: state.next_track_id,
                name: format!("Audio {}", next_idx),
                is_video: false,
                is_muted: false,
                is_hidden: false,
                is_solo: false,
                volume: 1.0,
                clips: Vec::new(),
            });
            state.next_track_id += 1;
        }

        // Split selected clip at playhead button
        if let Some((is_video, track_idx, clip_idx)) = state.selected_clip {
            let tracks = if is_video { &state.video_tracks } else { &state.audio_tracks };
            if track_idx < tracks.len() && clip_idx < tracks[track_idx].clips.len() {
                let clip = &tracks[track_idx].clips[clip_idx];
                if state.playhead_secs > clip.timeline_start && state.playhead_secs < clip.timeline_end {
                    if ui.button("✂ Split").clicked() {
                        state.razor_at(is_video, track_idx, clip_idx, state.playhead_secs);
                    }
                }
            }
        }

        // Selected clip fades
        if let Some((is_video, track_idx, clip_idx)) = state.selected_clip {
            let tracks = if is_video { &mut state.video_tracks } else { &mut state.audio_tracks };
            if track_idx < tracks.len() && clip_idx < tracks[track_idx].clips.len() {
                let clip = &mut tracks[track_idx].clips[clip_idx];
                ui.separator();
                ui.label("Fades:");
                ui.horizontal(|ui| {
                    ui.label("In:");
                    ui.add(egui::DragValue::new(&mut clip.fade_in_duration).speed(0.05).range(0.0..=5.0).suffix("s"));
                    ui.label("Out:");
                    ui.add(egui::DragValue::new(&mut clip.fade_out_duration).speed(0.05).range(0.0..=5.0).suffix("s"));
                });
            }
        }
    });

    ui.add_space(4.0);

    // Calculate max timeline duration to set scroll limits
    let mut max_time = 60.0f64; // minimum duration shown
    for track in &state.video_tracks {
        for clip in &track.clips {
            max_time = max_time.max(clip.timeline_end);
        }
    }
    for track in &state.audio_tracks {
        for clip in &track.clips {
            max_time = max_time.max(clip.timeline_end);
        }
    }
    max_time = max_time.max(state.playhead_secs);
    let timeline_width_secs = max_time + 10.0; // 10 seconds of padding at the end

    let timeline_visible_width = ui.available_width() - header_width - 16.0;
    let max_scroll = ((timeline_width_secs as f32 * state.zoom_factor) - timeline_visible_width).max(0.0);
    state.scroll_offset = state.scroll_offset.clamp(0.0, max_scroll);

    // During playback, auto-scroll so the playhead stays visible.
    if state.is_playing && max_scroll > 0.0 {
        let playhead_x = state.playhead_secs as f32 * state.zoom_factor - state.scroll_offset;
        let margin = 40.0;
        if playhead_x > timeline_visible_width - margin {
            state.scroll_offset =
                (state.playhead_secs as f32 * state.zoom_factor - (timeline_visible_width - margin))
                    .clamp(0.0, max_scroll);
        } else if playhead_x < margin {
            state.scroll_offset =
                (state.playhead_secs as f32 * state.zoom_factor - margin).clamp(0.0, max_scroll);
        }
    }

    let mut video_track_to_delete = None;
    let mut audio_track_to_delete = None;

    // 2. Timeline Layout (Headers + Canvas)
    ui.horizontal(|ui| {
        // Headers Column
        ui.vertical(|ui| {
            // Ruler empty spacer
            ui.allocate_space(Vec2::new(header_width, ruler_height));
            
            // Video track headers
            for (idx, track) in state.video_tracks.iter_mut().enumerate() {
                ui.allocate_ui_with_layout(
                    Vec2::new(header_width, track_height),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut track.name)
                                .font(FontId::proportional(11.0))
                                .desired_width(90.0));
                            
                            // Mute/Hide button
                            let hidden_text = if track.is_hidden { "🙈" } else { "👁" };
                            if ui.selectable_label(track.is_hidden, hidden_text).on_hover_text("Hide Video Track").clicked() {
                                track.is_hidden = !track.is_hidden;
                            }

                            // Solo button
                            let solo_color = if track.is_solo { egui::Color32::from_rgb(250, 160, 50) } else { egui::Color32::GRAY };
                            let solo_btn = egui::Button::new(egui::RichText::new("S").color(solo_color));
                            if ui.add(solo_btn).on_hover_text("Solo Track").clicked() {
                                track.is_solo = !track.is_solo;
                            }

                            // Delete track button
                            if ui.button("❌").on_hover_text("Delete Track").clicked() {
                                video_track_to_delete = Some(idx);
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label("Vol:");
                            ui.add(egui::Slider::new(&mut track.volume, 0.0..=1.0).show_value(false));
                        });
                    }
                );
                ui.add_space(track_spacing);
            }
            
            // Audio track headers
            for (idx, track) in state.audio_tracks.iter_mut().enumerate() {
                ui.allocate_ui_with_layout(
                    Vec2::new(header_width, track_height),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut track.name)
                                .font(FontId::proportional(11.0))
                                .desired_width(90.0));
                            
                            // Mute button
                            let muted_text = if track.is_muted { "🔇" } else { "🔊" };
                            if ui.selectable_label(track.is_muted, muted_text).on_hover_text("Mute Audio Track").clicked() {
                                track.is_muted = !track.is_muted;
                            }

                            // Solo button
                            let solo_color = if track.is_solo { egui::Color32::from_rgb(250, 160, 50) } else { egui::Color32::GRAY };
                            let solo_btn = egui::Button::new(egui::RichText::new("S").color(solo_color));
                            if ui.add(solo_btn).on_hover_text("Solo Track").clicked() {
                                track.is_solo = !track.is_solo;
                            }

                            // Delete track button
                            if ui.button("❌").on_hover_text("Delete Track").clicked() {
                                audio_track_to_delete = Some(idx);
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label("Vol:");
                            ui.add(egui::Slider::new(&mut track.volume, 0.0..=1.0).show_value(false));
                        });
                    }
                );
                ui.add_space(track_spacing);
            }
        });

        // Timeline Track lanes Canvas
        let timeline_size = Vec2::new(timeline_visible_width, total_height);
        let (response, painter) = ui.allocate_painter(timeline_size, egui::Sense::click_and_drag());
        let canvas_rect = response.rect;

        // Custom clip painter that respects the boundaries of the canvas
        let clip_painter = painter.with_clip_rect(canvas_rect);

        // Map helpers
        let zoom_factor = state.zoom_factor;
        let scroll_offset = state.scroll_offset;
        let time_to_x = move |t: f64| -> f32 {
            canvas_rect.left() + (t as f32) * zoom_factor - scroll_offset
        };
        let x_to_time = move |x: f32| -> f64 {
            ((x - canvas_rect.left() + scroll_offset) / zoom_factor) as f64
        };

        // Draw ruler background
        let ruler_rect = Rect::from_min_max(
            pos2(canvas_rect.left(), canvas_rect.top()),
            pos2(canvas_rect.right(), canvas_rect.top() + ruler_height)
        );
        clip_painter.rect_filled(ruler_rect, 0.0, Color32::from_rgb(30, 30, 35));

        // Draw track backgrounds
        for is_video in [true, false] {
            let tracks = if is_video { &state.video_tracks } else { &state.audio_tracks };
            for track_idx in 0..tracks.len() {
                let top_y = get_track_y(canvas_rect.top(), is_video, track_idx, num_video_tracks);
                let bottom_y = top_y + track_height;
                let lane_rect = Rect::from_min_max(
                    pos2(canvas_rect.left(), top_y),
                    pos2(canvas_rect.right(), bottom_y)
                );
                let fill_color = if is_video {
                    Color32::from_rgb(25, 25, 30)
                } else {
                    Color32::from_rgb(20, 28, 20)
                };
                clip_painter.rect_filled(lane_rect, 0.0, fill_color);

                // Draw track divider line
                clip_painter.line_segment(
                    [pos2(canvas_rect.left(), bottom_y), pos2(canvas_rect.right(), bottom_y)],
                    Stroke::new(1.0, Color32::from_rgb(45, 45, 50))
                );
            }
        }

        // Draw Ruler ticks and labels
        let start_time = x_to_time(canvas_rect.left()).max(0.0).floor();
        let end_time = x_to_time(canvas_rect.right()).ceil();
        let (tick_interval, label_interval) = if state.zoom_factor >= 20.0 {
            (1.0, 5.0)
        } else if state.zoom_factor >= 5.0 {
            (5.0, 10.0)
        } else {
            (10.0, 30.0)
        };

        let start_tick = (start_time / tick_interval).floor() * tick_interval;
        let mut t = start_tick;
        while t <= end_time {
            if t >= 0.0 {
                let x = time_to_x(t);
                let is_label = (t / label_interval - (t / label_interval).round()).abs() < 1e-5;
                let tick_len = if is_label { 12.0 } else { 6.0 };
                let stroke_color = if is_label { Color32::from_rgb(180, 180, 180) } else { Color32::from_rgb(100, 100, 100) };

                clip_painter.line_segment(
                    [pos2(x, canvas_rect.top() + ruler_height - tick_len), pos2(x, canvas_rect.top() + ruler_height)],
                    Stroke::new(1.0, stroke_color)
                );

                if is_label {
                    let m = (t / 60.0).floor() as i32;
                    let s = (t % 60.0) as i32;
                    let text = format!("{}:{:02}", m, s);
                    clip_painter.text(
                        pos2(x, canvas_rect.top() + 4.0),
                        Align2::CENTER_TOP,
                        text,
                        FontId::proportional(10.0),
                        Color32::from_rgb(200, 200, 200)
                    );
                }
            }
            t += tick_interval;
        }

        // Draw Clips
        for is_video in [true, false] {
            let tracks = if is_video { &state.video_tracks } else { &state.audio_tracks };
            for (track_idx, track) in tracks.iter().enumerate() {
                let top_y = get_track_y(canvas_rect.top(), is_video, track_idx, num_video_tracks);
                let bottom_y = top_y + track_height;

                for (clip_idx, clip) in track.clips.iter().enumerate() {
                    let x_start = time_to_x(clip.timeline_start);
                    let x_end = time_to_x(clip.timeline_end);

                    if x_end < canvas_rect.left() || x_start > canvas_rect.right() {
                        continue;
                    }

                    let clip_rect = Rect::from_min_max(
                        pos2(x_start, top_y + 2.0),
                        pos2(x_end, bottom_y - 2.0)
                    );

                    let is_selected = state.selected_clip == Some((is_video, track_idx, clip_idx));
                    let fill_color = if is_video {
                        if is_selected { Color32::from_rgb(120, 140, 240) } else { Color32::from_rgb(90, 110, 200) }
                    } else {
                        if is_selected { Color32::from_rgb(100, 200, 160) } else { Color32::from_rgb(70, 170, 130) }
                    };

                    let stroke = if is_selected {
                        Stroke::new(2.5, Color32::from_rgb(255, 215, 0)) // gold outline
                    } else {
                        Stroke::new(1.0, Color32::from_rgb(50, 50, 50))
                    };

                    clip_painter.rect(clip_rect, 4.0, fill_color, stroke, egui::StrokeKind::Inside);

                    // Draw Fades visual overlay
                    if clip.fade_in_duration > 0.0 {
                        let fade_in_w = (clip.fade_in_duration as f32 * state.zoom_factor).min(clip_rect.width());
                        let p1 = pos2(clip_rect.left(), clip_rect.bottom());
                        let p2 = pos2(clip_rect.left() + fade_in_w, clip_rect.top());
                        let p3 = pos2(clip_rect.left() + fade_in_w, clip_rect.bottom());
                        clip_painter.add(egui::Shape::convex_polygon(
                            vec![p1, p2, p3],
                            Color32::from_white_alpha(30),
                            Stroke::new(1.0, Color32::from_white_alpha(80)),
                        ));
                    }
                    if clip.fade_out_duration > 0.0 {
                        let fade_out_w = (clip.fade_out_duration as f32 * state.zoom_factor).min(clip_rect.width());
                        let p1 = pos2(clip_rect.right() - fade_out_w, clip_rect.top());
                        let p2 = pos2(clip_rect.right(), clip_rect.bottom());
                        let p3 = pos2(clip_rect.right() - fade_out_w, clip_rect.bottom());
                        clip_painter.add(egui::Shape::convex_polygon(
                            vec![p1, p2, p3],
                            Color32::from_white_alpha(30),
                            Stroke::new(1.0, Color32::from_white_alpha(80)),
                        ));
                    }

                    // Per-clip visuals: a waveform for audio-track clips, or a poster
                    // thumbnail at the left of video/image clips.
                    let asset = state.assets.iter().find(|a| a.id == clip.asset_id);
                    let mut label_x = x_start + 6.0;
                    if !is_video {
                        if let Some(asset) = asset {
                            if let Some(ref samples) = asset.audio_samples {
                                draw_waveform(&clip_painter, clip_rect, clip, asset, samples);
                            }
                        }
                    } else if let Some(tex) = thumbs.get(&clip.asset_id) {
                        let sz = tex.size();
                        let aspect = sz[0] as f32 / (sz[1].max(1) as f32);
                        let pw = (aspect * clip_rect.height()).min(clip_rect.width());
                        let poster =
                            Rect::from_min_max(clip_rect.min, pos2(clip_rect.left() + pw, clip_rect.bottom()));
                        clip_painter.image(
                            tex.id(),
                            poster,
                            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                            Color32::WHITE,
                        );
                        label_x = (clip_rect.left() + pw + 4.0).min(clip_rect.right());
                    }

                    // Label details
                    let asset_name = asset
                        .map(|a| a.name.clone())
                        .unwrap_or_else(|| format!("Clip {}", clip.id));
                    let label_text = format!("{} [{:.1}s - {:.1}s]", asset_name, clip.source_trim_start, clip.source_trim_end);

                    let text_painter = clip_painter.with_clip_rect(clip_rect);
                    text_painter.text(
                        pos2(label_x, top_y + track_height / 2.0),
                        Align2::LEFT_CENTER,
                        label_text,
                        FontId::proportional(11.0),
                        Color32::WHITE
                    );

                    // Trim handle indicators
                    let handle_width = 8.0;
                    if clip_rect.width() > handle_width * 2.0 {
                        let left_handle_rect = Rect::from_min_max(
                            pos2(x_start, top_y + 2.0),
                            pos2(x_start + handle_width, bottom_y - 2.0)
                        );
                        clip_painter.rect_filled(left_handle_rect, 2.0, Color32::from_black_alpha(40));

                        let right_handle_rect = Rect::from_min_max(
                            pos2(x_end - handle_width, top_y + 2.0),
                            pos2(x_end, bottom_y - 2.0)
                        );
                        clip_painter.rect_filled(right_handle_rect, 2.0, Color32::from_black_alpha(40));
                    }
                }
            }
        }

        // Draw Playhead
        let playhead_x = time_to_x(state.playhead_secs);
        if playhead_x >= canvas_rect.left() && playhead_x <= canvas_rect.right() {
            clip_painter.line_segment(
                [pos2(playhead_x, canvas_rect.top()), pos2(playhead_x, canvas_rect.bottom())],
                Stroke::new(1.5, Color32::from_rgb(220, 50, 50))
            );

            let handle_points = vec![
                pos2(playhead_x - 6.0, canvas_rect.top()),
                pos2(playhead_x + 6.0, canvas_rect.top()),
                pos2(playhead_x + 6.0, canvas_rect.top() + 8.0),
                pos2(playhead_x, canvas_rect.top() + 14.0),
                pos2(playhead_x - 6.0, canvas_rect.top() + 8.0),
            ];

            clip_painter.add(egui::Shape::convex_polygon(
                handle_points,
                Color32::from_rgb(220, 50, 50),
                Stroke::new(1.0, Color32::WHITE),
            ));
        }

        // 3. Mouse Interaction & Event Handling
        let drag_state_id = ui.make_persistent_id("timeline_drag_state");
        let mut drag_state = ui.data(|d| d.get_temp::<DragState>(drag_state_id)).unwrap_or(DragState::None);

        let pointer = ui.input(|i| i.pointer.clone());
        let hover_pos = pointer.hover_pos();
        let is_primary_down = pointer.primary_down();
        let is_primary_clicked = pointer.primary_clicked();
        let is_primary_released = pointer.primary_released();

        if is_primary_clicked {
            if let Some(click_pos) = hover_pos {
                if canvas_rect.contains(click_pos) {
                    if click_pos.y <= canvas_rect.top() + ruler_height {
                        // Click playhead
                        drag_state = DragState::Playhead;
                        let t = x_to_time(click_pos.x);
                        state.playhead_secs = t.max(0.0);
                    } else {
                        // Click in tracks
                        let mut clicked_something = false;
                        'find_clip: for is_video in [true, false] {
                            let tracks = if is_video { &state.video_tracks } else { &state.audio_tracks };
                            for (track_idx, track) in tracks.iter().enumerate() {
                                let top_y = get_track_y(canvas_rect.top(), is_video, track_idx, num_video_tracks);
                                let bottom_y = top_y + track_height;

                                for (clip_idx, clip) in track.clips.iter().enumerate() {
                                    let x_start = time_to_x(clip.timeline_start);
                                    let x_end = time_to_x(clip.timeline_end);
                                    let clip_rect = Rect::from_min_max(
                                        pos2(x_start, top_y + 2.0),
                                        pos2(x_end, bottom_y - 2.0)
                                    );

                                    if clip_rect.contains(click_pos) {
                                        clicked_something = true;
                                        if state.selected_tool == EditTool::Razor {
                                            // Razor tool: split at click position
                                            let clicked_time = x_to_time(click_pos.x);
                                            state.razor_at(is_video, track_idx, clip_idx, clicked_time);
                                            drag_state = DragState::None;
                                        } else {
                                            // Select tool
                                            state.selected_clip = Some((is_video, track_idx, clip_idx));
                                            let edge_threshold = 8.0;
                                            if click_pos.x <= x_start + edge_threshold {
                                                drag_state = DragState::ClipTrimLeft {
                                                    clip_id: clip.id,
                                                    is_video,
                                                    drag_start_mouse_x: click_pos.x,
                                                    drag_start_clip_start: clip.timeline_start,
                                                    drag_start_clip_end: clip.timeline_end,
                                                    drag_start_trim_start: clip.source_trim_start,
                                                };
                                            } else if click_pos.x >= x_end - edge_threshold {
                                                drag_state = DragState::ClipTrimRight {
                                                    clip_id: clip.id,
                                                    is_video,
                                                    drag_start_mouse_x: click_pos.x,
                                                    drag_start_clip_end: clip.timeline_end,
                                                    drag_start_clip_start: clip.timeline_start,
                                                    drag_start_trim_end: clip.source_trim_end,
                                                };
                                            } else {
                                                drag_state = DragState::ClipMove {
                                                    clip_id: clip.id,
                                                    is_video,
                                                    drag_start_mouse_x: click_pos.x,
                                                    drag_start_mouse_y: click_pos.y,
                                                    drag_start_clip_start: clip.timeline_start,
                                                    drag_start_track_idx: track_idx,
                                                };
                                            }
                                        }
                                        break 'find_clip;
                                    }
                                }
                            }
                        }

                        if !clicked_something {
                            state.selected_clip = None;
                        }
                    }
                }
            }
        }

        if is_primary_down {
            if let Some(curr_pos) = hover_pos {
                match drag_state {
                    DragState::None => {}
                    DragState::Playhead => {
                        let t = x_to_time(curr_pos.x);
                        state.playhead_secs = t.max(0.0);
                    }
                    DragState::ClipMove {
                        clip_id,
                        is_video,
                        drag_start_mouse_x,
                        drag_start_mouse_y: _,
                        drag_start_clip_start,
                        drag_start_track_idx: _,
                    } => {
                        let delta_x = curr_pos.x - drag_start_mouse_x;
                        let delta_t = (delta_x / state.zoom_factor) as f64;
                        let mut target_start = drag_start_clip_start + delta_t;

                        if let Some((_, orig_track_idx, orig_clip_idx)) = find_clip_details(state, clip_id) {
                            let clip_duration = {
                                let tracks = if is_video { &state.video_tracks } else { &state.audio_tracks };
                                let clip = &tracks[orig_track_idx].clips[orig_clip_idx];
                                clip.timeline_end - clip.timeline_start
                            };

                            // Snapping logic
                            let snap_threshold = (8.0 / state.zoom_factor) as f64;
                            let snapped_start = find_snap_time(target_start, Some(clip_id), state, snap_threshold);
                            let target_end = target_start + clip_duration;
                            let snapped_end = find_snap_time(target_end, Some(clip_id), state, snap_threshold);

                            let start_snap_diff = (snapped_start - target_start).abs();
                            let end_snap_diff = (snapped_end - target_end).abs();

                            if start_snap_diff < snap_threshold && start_snap_diff <= end_snap_diff {
                                target_start = snapped_start;
                            } else if end_snap_diff < snap_threshold {
                                target_start = snapped_end - clip_duration;
                            }

                            target_start = target_start.max(0.0);
                            let mut target_end = target_start + clip_duration;

                            // Determine track from hover position
                            let target_track_idx = get_hovered_track_idx(curr_pos.y, canvas_rect.top(), is_video, num_video_tracks);

                            let tracks = if is_video { &mut state.video_tracks } else { &mut state.audio_tracks };
                            if target_track_idx < tracks.len() {
                                let clip_to_move = tracks[orig_track_idx].clips[orig_clip_idx].clone();
                                let target_track = &tracks[target_track_idx];

                                let mut valid = is_placement_valid(target_track, target_start, target_end, Some(clip_id));

                                if !valid {
                                    // Clamp to gap/neighbors
                                    let mut sorted_clips: Vec<&Clip> = target_track.clips.iter()
                                        .filter(|c| c.id != clip_id)
                                        .collect();
                                    sorted_clips.sort_by(|a, b| a.timeline_start.partial_cmp(&b.timeline_start).unwrap());

                                    let mut left_neighbor: Option<&Clip> = None;
                                    let mut right_neighbor: Option<&Clip> = None;
                                    for &c in &sorted_clips {
                                        if c.timeline_end <= target_start {
                                            left_neighbor = Some(c);
                                        } else {
                                            right_neighbor = Some(c);
                                            break;
                                        }
                                    }

                                    if let Some(ln) = left_neighbor {
                                        if target_start < ln.timeline_end {
                                            target_start = ln.timeline_end;
                                            target_end = target_start + clip_duration;
                                        }
                                    }
                                    if let Some(rn) = right_neighbor {
                                        if target_end > rn.timeline_start {
                                            target_start = rn.timeline_start - clip_duration;
                                            target_end = rn.timeline_start;

                                            if let Some(ln) = left_neighbor {
                                                if target_start < ln.timeline_end {
                                                    // Won't fit, revert
                                                    target_start = clip_to_move.timeline_start;
                                                }
                                            }
                                        }
                                    }

                                    valid = is_placement_valid(target_track, target_start, target_end, Some(clip_id));
                                }

                                if valid && target_start >= 0.0 {
                                    let mut updated_clip = clip_to_move;
                                    updated_clip.timeline_start = target_start;
                                    updated_clip.timeline_end = target_start + clip_duration;

                                    let tracks = if is_video { &mut state.video_tracks } else { &mut state.audio_tracks };
                                    tracks[orig_track_idx].clips.remove(orig_clip_idx);
                                    tracks[target_track_idx].clips.push(updated_clip);
                                    tracks[target_track_idx].clips.sort_by(|a, b| a.timeline_start.partial_cmp(&b.timeline_start).unwrap());

                                    let new_idx = tracks[target_track_idx].clips.iter().position(|c| c.id == clip_id).unwrap();
                                    state.selected_clip = Some((is_video, target_track_idx, new_idx));
                                }
                            }
                        }
                    }
                    DragState::ClipTrimLeft {
                        clip_id,
                        is_video,
                        drag_start_mouse_x,
                        drag_start_clip_start,
                        drag_start_clip_end,
                        drag_start_trim_start,
                    } => {
                        let delta_x = curr_pos.x - drag_start_mouse_x;
                        let delta_t = (delta_x / state.zoom_factor) as f64;
                        let mut target_start = drag_start_clip_start + delta_t;

                        let snap_threshold = (8.0 / state.zoom_factor) as f64;
                        target_start = find_snap_time(target_start, Some(clip_id), state, snap_threshold);

                        let min_start_by_source = drag_start_clip_start - drag_start_trim_start;
                        target_start = target_start.max(min_start_by_source);

                        let max_start_by_dur = drag_start_clip_end - 0.05;
                        target_start = target_start.min(max_start_by_dur);

                        if let Some((_, track_idx, clip_idx)) = find_clip_details(state, clip_id) {
                            let tracks = if is_video { &state.video_tracks } else { &state.audio_tracks };
                            let track = &tracks[track_idx];
                            let left_neighbor = if clip_idx > 0 { Some(&track.clips[clip_idx - 1]) } else { None };

                            if let Some(ln) = left_neighbor {
                                target_start = target_start.max(ln.timeline_end);
                            }

                            if target_start >= 0.0 && target_start <= max_start_by_dur {
                                let tracks = if is_video { &mut state.video_tracks } else { &mut state.audio_tracks };
                                let clip = &mut tracks[track_idx].clips[clip_idx];
                                clip.timeline_start = target_start;
                                clip.source_trim_start = drag_start_trim_start + (target_start - drag_start_clip_start);
                            }
                        }
                    }
                    DragState::ClipTrimRight {
                        clip_id,
                        is_video,
                        drag_start_mouse_x,
                        drag_start_clip_end,
                        drag_start_clip_start,
                        drag_start_trim_end,
                    } => {
                        let delta_x = curr_pos.x - drag_start_mouse_x;
                        let delta_t = (delta_x / state.zoom_factor) as f64;
                        let mut target_end = drag_start_clip_end + delta_t;

                        let snap_threshold = (8.0 / state.zoom_factor) as f64;
                        target_end = find_snap_time(target_end, Some(clip_id), state, snap_threshold);

                        if let Some((_, track_idx, clip_idx)) = find_clip_details(state, clip_id) {
                            let asset_duration = {
                                let tracks = if is_video { &state.video_tracks } else { &state.audio_tracks };
                                let clip = &tracks[track_idx].clips[clip_idx];
                                state.assets.iter()
                                    .find(|a| a.id == clip.asset_id)
                                    // Still images have no real source length, so allow
                                    // extending them freely on the timeline.
                                    .map(|a| if a.is_image { 86_400.0 } else { a.duration_secs })
                                    .unwrap_or(3600.0)
                            };

                            let max_end_by_source = drag_start_clip_end + (asset_duration - drag_start_trim_end);
                            target_end = target_end.min(max_end_by_source);

                            let min_end_by_dur = drag_start_clip_start + 0.05;
                            target_end = target_end.max(min_end_by_dur);

                            let tracks = if is_video { &state.video_tracks } else { &state.audio_tracks };
                            let track = &tracks[track_idx];
                            let right_neighbor = if clip_idx + 1 < track.clips.len() { Some(&track.clips[clip_idx + 1]) } else { None };

                            if let Some(rn) = right_neighbor {
                                target_end = target_end.min(rn.timeline_start);
                            }

                            if target_end >= min_end_by_dur {
                                let tracks = if is_video { &mut state.video_tracks } else { &mut state.audio_tracks };
                                let clip = &mut tracks[track_idx].clips[clip_idx];
                                clip.timeline_end = target_end;
                                clip.source_trim_end = drag_start_trim_end + (target_end - drag_start_clip_end);
                            }
                        }
                    }
                }
            }
        }

        if is_primary_released {
            drag_state = DragState::None;
        }

        ui.data_mut(|d| d.insert_temp(drag_state_id, drag_state));
    });

    // 4. Scroll offset slider
    if max_scroll > 0.0 {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("Scroll Timeline:");
            ui.add(egui::Slider::new(&mut state.scroll_offset, 0.0..=max_scroll).show_value(false));
        });
    }

    // Apply track deletions if triggered during rendering
    if let Some(idx) = video_track_to_delete {
        state.video_tracks.remove(idx);
        state.selected_clip = None;
    }
    if let Some(idx) = audio_track_to_delete {
        state.audio_tracks.remove(idx);
        state.selected_clip = None;
    }
}

// Draw a translucent amplitude waveform of `samples` across the clip's body,
// mapped over the clip's source trim range.
fn draw_waveform(
    painter: &egui::Painter,
    rect: Rect,
    clip: &Clip,
    asset: &crate::media::MediaAsset,
    samples: &[f32],
) {
    let channels = asset.audio_channels.max(1) as usize;
    let sr = asset.audio_sample_rate.max(1) as f64;
    let mid_y = rect.center().y;
    let half_h = (rect.height() * 0.5 - 3.0).max(1.0);
    let span = (clip.source_trim_end - clip.source_trim_start).max(1e-6);
    let w = rect.width().max(1.0);
    let color = Color32::from_rgba_unmultiplied(230, 255, 240, 70);

    let mut x = rect.left();
    while x < rect.right() {
        let f = ((x - rect.left()) / w).clamp(0.0, 1.0) as f64;
        let src_t = clip.source_trim_start + f * span;
        let idx = (src_t * sr) as usize * channels;
        let amp = samples.get(idx).copied().unwrap_or(0.0).abs().min(1.0);
        let h = amp * half_h;
        if h > 0.5 {
            painter.line_segment(
                [pos2(x, mid_y - h), pos2(x, mid_y + h)],
                Stroke::new(1.0, color),
            );
        }
        x += 2.0;
    }
}

// Helpers
fn get_track_y(top_offset: f32, is_video: bool, track_idx: usize, num_video_tracks: usize) -> f32 {
    let ruler_height = 30.0;
    let track_height = 60.0;
    let track_spacing = 8.0;

    if is_video {
        top_offset + ruler_height + (track_idx as f32) * (track_height + track_spacing)
    } else {
        top_offset + ruler_height + ((num_video_tracks + track_idx) as f32) * (track_height + track_spacing)
    }
}

fn get_hovered_track_idx(mouse_y: f32, top_offset: f32, is_video: bool, num_video_tracks: usize) -> usize {
    let ruler_height = 30.0;
    let track_height = 60.0;
    let track_spacing = 8.0;

    let rel_y = mouse_y - (top_offset + ruler_height);

    if is_video {
        let idx = (rel_y / (track_height + track_spacing)) as usize;
        idx.min(num_video_tracks.saturating_sub(1))
    } else {
        let audio_offset = (num_video_tracks as f32) * (track_height + track_spacing);
        let rel_audio_y = rel_y - audio_offset;
        let idx = (rel_audio_y / (track_height + track_spacing)) as usize;
        idx
    }
}

fn find_clip_details(state: &EditorState, clip_id: usize) -> Option<(bool, usize, usize)> {
    for (t_idx, track) in state.video_tracks.iter().enumerate() {
        if let Some(c_idx) = track.clips.iter().position(|c| c.id == clip_id) {
            return Some((true, t_idx, c_idx));
        }
    }
    for (t_idx, track) in state.audio_tracks.iter().enumerate() {
        if let Some(c_idx) = track.clips.iter().position(|c| c.id == clip_id) {
            return Some((false, t_idx, c_idx));
        }
    }
    None
}

fn is_placement_valid(track: &crate::editor::Track, start: f64, end: f64, exclude_clip_id: Option<usize>) -> bool {
    if start < 0.0 {
        return false;
    }
    for clip in &track.clips {
        if Some(clip.id) == exclude_clip_id {
            continue;
        }
        if start < clip.timeline_end && end > clip.timeline_start {
            return false;
        }
    }
    true
}

fn find_snap_time(
    target_time: f64,
    exclude_clip_id: Option<usize>,
    state: &EditorState,
    snap_threshold_secs: f64,
) -> f64 {
    let mut best_snap = target_time;
    let mut min_diff = snap_threshold_secs;

    // Snap to 0.0
    let diff = (target_time - 0.0).abs();
    if diff < min_diff {
        min_diff = diff;
        best_snap = 0.0;
    }

    // Snap to playhead
    let diff = (target_time - state.playhead_secs).abs();
    if diff < min_diff {
        min_diff = diff;
        best_snap = state.playhead_secs;
    }

    // Snap to other clips' start/end
    let mut check_clips = |clips: &[Clip]| {
        for clip in clips {
            if Some(clip.id) == exclude_clip_id {
                continue;
            }
            let diff_start = (target_time - clip.timeline_start).abs();
            if diff_start < min_diff {
                min_diff = diff_start;
                best_snap = clip.timeline_start;
            }
            let diff_end = (target_time - clip.timeline_end).abs();
            if diff_end < min_diff {
                min_diff = diff_end;
                best_snap = clip.timeline_end;
            }
        }
    };

    for track in &state.video_tracks {
        check_clips(&track.clips);
    }
    for track in &state.audio_tracks {
        check_clips(&track.clips);
    }

    best_snap
}

