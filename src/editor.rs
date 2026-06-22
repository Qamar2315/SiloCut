use crate::media::MediaAsset;

#[derive(Clone, Debug)]
pub struct Clip {
    pub id: usize,
    pub asset_id: usize,
    pub timeline_start: f64,     // in seconds
    pub timeline_end: f64,       // in seconds
    pub source_trim_start: f64,  // in seconds from start of source asset
    pub source_trim_end: f64,    // in seconds from start of source asset
}

#[derive(Clone, Debug)]
pub struct Track {
    pub id: usize,
    pub name: String,
    pub is_video: bool,
    pub is_muted: bool,
    pub is_hidden: bool,
    pub clips: Vec<Clip>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EditTool {
    Select,
    Razor,
}

#[derive(Clone, Debug)]
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
}

impl EditorState {
    pub fn new() -> Self {
        Self {
            assets: Vec::new(),
            video_tracks: vec![
                Track { id: 0, name: "Video 1".to_string(), is_video: true, is_muted: false, is_hidden: false, clips: Vec::new() },
                Track { id: 1, name: "Video 2".to_string(), is_video: true, is_muted: false, is_hidden: false, clips: Vec::new() },
            ],
            audio_tracks: vec![
                Track { id: 2, name: "Audio 1".to_string(), is_video: false, is_muted: false, is_hidden: false, clips: Vec::new() },
                Track { id: 3, name: "Audio 2".to_string(), is_video: false, is_muted: false, is_hidden: false, clips: Vec::new() },
            ],
            playhead_secs: 0.0,
            is_playing: false,
            selected_tool: EditTool::Select,
            selected_clip: None,
            zoom_factor: 15.0, // 15 pixels per second default
            scroll_offset: 0.0,
            next_clip_id: 1,
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

        let mut second_clip = clip.clone();
        second_clip.id = self.next_clip_id;
        self.next_clip_id += 1;
        second_clip.timeline_start = time_secs;
        second_clip.source_trim_start = clip.source_trim_start + split_offset;

        track.clips[clip_idx] = first_clip;
        track.clips.insert(clip_idx + 1, second_clip);
        self.selected_clip = None;
    }
}
