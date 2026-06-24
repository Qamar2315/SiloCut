# Changelog

All notable changes to SiloCut are documented here.

## [1.5.0] - 2026-06-23

A large feature release building on the 1.4.0 fixes: project persistence, richer
editing, a more capable recorder, and a more informative timeline.

### Added

- **Project save/load** to `.silocut` files (File menu + Ctrl+S / Ctrl+O). Media is
  referenced by path; decoded audio is regenerated on load.
- **Still-image clips** (PNG/JPEG), default 5s and freely extendable, rendered in
  both the preview and the exporter.
- **Audio waveforms** on audio clips and **poster thumbnails** on video/image clips
  (decoded on a background thread).
- **Export quality control** (Low / Medium / High) that scales the encoder bitrate.
- **Ripple delete** (Shift+Delete) and **clip duplicate** (Ctrl+D).
- **Zoom-to-fit** and **follow-playhead** auto-scroll during playback.
- **Recorder options**: FPS selector (30/60), a 3-2-1 countdown, a live recording
  timer, pause/resume, and **monitor selection** on multi-display setups.

### Changed

- The preview now uses the **source aspect ratio** and fits the available area
  (previously locked to 16:9).
- Fades animate smoothly while paused and on stills (the frame re-uploads when the
  fade alpha changes).
- Dropped `panic = "abort"` so a panic in a background decode/record/export worker
  terminates only that thread, not the whole application.

### Tests

- Unit tests for razor / ripple-delete / duplicate and still-image metadata.
- A fuzz test asserting the `avcC` / Annex-B parsers never panic on garbage input.

### Not yet included

These were scoped but deferred as larger follow-ups: picture-in-picture / clip
transforms, cross-dissolve transitions, text/title overlays (needs a CPU font
rasterizer), and live audio capture muxed into the MP4 (needs an AAC/Opus encoder,
which has no production pure-Rust implementation).

## [1.4.0] - 2026-06-23

This release fixes the core issues that prevented both the editor and the screen
recorder from producing usable output, and adds several quality-of-life
improvements.

### Fixed

- **Screen recordings are now watchable.** The recorder used OpenH264's default
  encoder settings — a 120 kbps bitrate with frame-skipping enabled and no
  periodic keyframes — which produced an unwatchable, unseekable stream for
  full-screen capture. The encoder is now configured with a resolution-scaled
  bitrate, no frame dropping, the screen-content tuning profile, and a keyframe
  roughly once per second.
- **Exports are now watchable.** Video export used the same broken 120 kbps
  default; it now uses the same properly-scaled encoder.
- **The preview and export now actually show video.** MP4 stores the H.264
  SPS/PPS parameter sets in the `avcC` box rather than inline with the frames, so
  the decoder was never given them and produced no frames (blank preview, black
  export). The decoder is now primed with the SPS/PPS parsed from the container,
  and re-primed after every seek.
- **Recorded clips no longer have zero duration.** Clip duration was derived from
  a track field that symphonia leaves unset for video-only files (such as screen
  recordings), so recordings imported with a length of 0 and could not be edited,
  played, or exported. Duration is now read from the correct field.
- **Undo/redo is correct and sensible.** The first undo used to be a no-op (it
  recorded the post-change state instead of the pre-change state), and a single
  drag generated dozens of history entries. Undo now records the pre-change state,
  and a continuous drag collapses into a single undoable step.

### Added

- **Mouse cursor capture** in screen recordings, so pointer movement is visible in
  tutorials and demos.
- **Wall-clock-accurate recording.** Each captured frame is tagged with the real
  elapsed time, so recordings play back at the correct speed even when capture or
  encoding can't sustain the target frame rate (previously such recordings played
  back sped up).
- **Export progress percentage** in the status bar (the UI also now repaints
  during export so the indicator updates live).

### Changed

- Migrated the UI to egui 0.34's current panel/menu API (`Panel::*` +
  `show_inside`, `MenuBar`), eliminating all build warnings.
- Centralised the H.264 encode/decode helpers into a shared `codec` module with an
  end-to-end encode → mux → demux → decode test.

### Notes

- Exported audio is written as a companion `.wav` file next to the `.mp4` (the
  zero-dependency build does not bundle an AAC encoder, so audio is not muxed into
  the MP4 container).
