# Changelog

All notable changes to SiloCut are documented here.

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
