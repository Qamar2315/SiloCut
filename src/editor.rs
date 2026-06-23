use crate::media::MediaAsset;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Clip {
    pub id: usize,
    pub asset_id: usize,
    pub timeline_start: f64,     // in seconds
    pub timeline_end: f64,       // in seconds
    pub source_trim_start: f64,  // in seconds from start of source asset
    pub source_trim_end: f64,    // in seconds from start of source asset
    pub fade_in_duration: f64,   // in seconds
    pub fade_out_duration: f64,  // in seconds
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Track {
    pub id: usize,
    pub name: String,
    pub is_video: bool,
    pub is_muted: bool,
    pub is_hidden: bool,
    pub is_solo: bool,
    pub volume: f32, // 0.0 to 1.0
    pub clips: Vec<Clip>,
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum EditTool {
    Select,
    Razor,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct EditorState {
    pub assets: Vec<MediaAsset>,
    pub video_tracks: Vec<Track>,
    pub audio_tracks: Vec<Track>,
    pub playhead_secs: f64,
    pub is_playing: bool,
    pub selected_tool: EditTool,
    pub selected_clip: Option<(bool, usize, usize)>, // (is_video, track_idx, clip_idx)
    pub zoom_factor: f32, // pixels per second
    pub scroll_offset: f32,
    pub next_clip_id: usize,
    pub next_track_id: usize,
}

impl EditorState {
    pub fn new() -> Self {
        Self {
            assets: Vec::new(),
            video_tracks: vec![
                Track { id: 0, name: "Video 1".to_string(), is_video: true, is_muted: false, is_hidden: false, is_solo: false, volume: 1.0, clips: Vec::new() },
                Track { id: 1, name: "Video 2".to_string(), is_video: true, is_muted: false, is_hidden: false, is_solo: false, volume: 1.0, clips: Vec::new() },
            ],
            audio_tracks: vec![
                Track { id: 2, name: "Audio 1".to_string(), is_video: false, is_muted: false, is_hidden: false, is_solo: false, volume: 1.0, clips: Vec::new() },
                Track { id: 3, name: "Audio 2".to_string(), is_video: false, is_muted: false, is_hidden: false, is_solo: false, volume: 1.0, clips: Vec::new() },
            ],
            playhead_secs: 0.0,
            is_playing: false,
            selected_tool: EditTool::Select,
            selected_clip: None,
            zoom_factor: 15.0, // 15 pixels per second default
            scroll_offset: 0.0,
            next_clip_id: 1,
            next_track_id: 4,
        }
    }

    // Split clip at the playhead
    pub fn razor_at(&mut self, is_video: bool, track_idx: usize, clip_idx: usize, time_secs: f64) {
        let tracks = if is_video { &mut self.video_tracks } else { &mut self.audio_tracks };
        if track_idx >= tracks.len() { return; }
        let track = &mut tracks[track_idx];
        if clip_idx >= track.clips.len() { return; }
        
        let clip = &track.clips[clip_idx];
        if time_secs <= clip.timeline_start || time_secs >= clip.timeline_end { return; }

        let split_offset = time_secs - clip.timeline_start;
        let mut first_clip = clip.clone();
        first_clip.timeline_end = time_secs;
        first_clip.source_trim_end = clip.source_trim_start + split_offset;
        first_clip.fade_out_duration = 0.0; // Clear fade-out at split point

        let mut second_clip = clip.clone();
        second_clip.id = self.next_clip_id;
        self.next_clip_id += 1;
        second_clip.timeline_start = time_secs;
        second_clip.source_trim_start = clip.source_trim_start + split_offset;
        second_clip.fade_in_duration = 0.0; // Clear fade-in at split point

        track.clips[clip_idx] = first_clip;
        track.clips.insert(clip_idx + 1, second_clip);
        self.selected_clip = None;
    }

    /// Remove a clip and slide later clips on the same track left to close the gap.
    pub fn ripple_delete(&mut self, is_video: bool, track_idx: usize, clip_idx: usize) {
        let tracks = if is_video { &mut self.video_tracks } else { &mut self.audio_tracks };
        if track_idx >= tracks.len() || clip_idx >= tracks[track_idx].clips.len() {
            return;
        }
        let removed = tracks[track_idx].clips.remove(clip_idx);
        let gap = removed.timeline_end - removed.timeline_start;
        for clip in tracks[track_idx].clips.iter_mut() {
            if clip.timeline_start >= removed.timeline_end - 1e-9 {
                clip.timeline_start -= gap;
                clip.timeline_end -= gap;
            }
        }
        self.selected_clip = None;
    }

    /// Place a copy of a clip after it on the same track, nudged to the first free
    /// slot so it never overlaps an existing clip.
    pub fn duplicate_clip(&mut self, is_video: bool, track_idx: usize, clip_idx: usize) {
        let new_id = self.next_clip_id;
        let tracks = if is_video { &mut self.video_tracks } else { &mut self.audio_tracks };
        if track_idx >= tracks.len() || clip_idx >= tracks[track_idx].clips.len() {
            return;
        }
        let mut copy = tracks[track_idx].clips[clip_idx].clone();
        let dur = copy.timeline_end - copy.timeline_start;
        let start = earliest_free_start(&tracks[track_idx].clips, copy.timeline_end, dur, usize::MAX);
        copy.id = new_id;
        copy.timeline_start = start;
        copy.timeline_end = start + dur;
        tracks[track_idx].clips.push(copy);
        tracks[track_idx].clips.sort_by(|a, b| a.timeline_start.partial_cmp(&b.timeline_start).unwrap());
        self.next_clip_id += 1;
        self.selected_clip = None;
    }
}

/// Find the earliest start time >= `desired` at which a clip of length `dur` fits
/// without overlapping any existing clip (ignoring the clip with id `exclude`).
fn earliest_free_start(clips: &[Clip], desired: f64, dur: f64, exclude: usize) -> f64 {
    let mut start = desired.max(0.0);
    for _ in 0..clips.len() + 1 {
        let mut moved = false;
        for c in clips {
            if c.id == exclude {
                continue;
            }
            if start < c.timeline_end && start + dur > c.timeline_start {
                start = c.timeline_end;
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }
    start
}
