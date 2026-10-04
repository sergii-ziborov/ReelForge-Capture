//! Host video input (`gdigrab` / `avfoundation` / `x11grab`).

use crate::host::HostOs;
use reelforge_capture_core::{CaptureSpec, Region, VideoSource};

/// Append `-f <grabber> … -i <src>`.
///
/// A macOS region crop is returned for a later output `-filter:v`. Pushing it
/// here would make ffmpeg treat the crop as an input option before the next `-i`.
pub(crate) fn push_video(args: &mut Vec<String>, spec: &CaptureSpec, os: HostOs) -> Option<String> {
    match os {
        HostOs::Windows => {
            push_gdigrab(args, spec);
            None
        }
        HostOs::Macos => push_avfoundation(args, spec),
        HostOs::Linux => {
            push_x11grab(args, spec);
            None
        }
    }
}

fn push_gdigrab(args: &mut Vec<String>, spec: &CaptureSpec) {
    args.extend([
        "-f".into(),
        "gdigrab".into(),
        "-framerate".into(),
        spec.fps.to_string(),
    ]);
    match &spec.video {
        VideoSource::Screen => args.extend(["-i".into(), "desktop".into()]),
        VideoSource::Window { title } => args.extend(["-i".into(), format!("title={title}")]),
        VideoSource::Region { region } => {
            args.extend(region_gdi(region));
            args.extend(["-i".into(), "desktop".into()]);
        }
    }
}

fn region_gdi(region: &Region) -> Vec<String> {
    vec![
        "-offset_x".into(),
        region.x.to_string(),
        "-offset_y".into(),
        region.y.to_string(),
        "-video_size".into(),
        format!("{}x{}", region.width, region.height),
    ]
}

fn push_avfoundation(args: &mut Vec<String>, spec: &CaptureSpec) -> Option<String> {
    args.extend([
        "-f".into(),
        "avfoundation".into(),
        "-framerate".into(),
        spec.fps.to_string(),
        "-capture_cursor".into(),
        "1".into(),
        "-i".into(),
        avfoundation_video(spec),
    ]);
    match &spec.video {
        VideoSource::Region { region } => Some(crop_filter(region)),
        VideoSource::Screen | VideoSource::Window { .. } => None,
    }
}

fn avfoundation_video(spec: &CaptureSpec) -> String {
    match &spec.video {
        VideoSource::Window { title } => title.clone(),
        VideoSource::Screen | VideoSource::Region { .. } => {
            std::env::var("REELFORGE_CAPTURE_SCREEN").unwrap_or_else(|_| "1".into())
        }
    }
}

fn push_x11grab(args: &mut Vec<String>, spec: &CaptureSpec) {
    let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0.0".into());
    args.extend([
        "-f".into(),
        "x11grab".into(),
        "-framerate".into(),
        spec.fps.to_string(),
        "-draw_mouse".into(),
        "1".into(),
    ]);
    match &spec.video {
        VideoSource::Screen => args.extend(["-i".into(), display]),
        VideoSource::Window { title }
            if title.starts_with("0x") || title.chars().all(|c| c.is_ascii_hexdigit()) =>
        {
            args.extend(["-window_id".into(), title.clone(), "-i".into(), display]);
        }
        VideoSource::Window { title } => args.extend(["-i".into(), format!("title={title}")]),
        VideoSource::Region { region } => {
            args.extend([
                "-video_size".into(),
                format!("{}x{}", region.width, region.height),
                "-i".into(),
                format!("{display}+{},{}", region.x, region.y),
            ]);
        }
    }
}

fn crop_filter(region: &Region) -> String {
    format!(
        "crop={}:{}:{}:{}",
        region.width, region.height, region.x, region.y
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_region_uses_display_offset() {
        let mut spec = CaptureSpec::screen();
        spec.video = VideoSource::Region {
            region: Region::new(8, 16, 320, 180),
        };
        let mut args = Vec::new();
        assert!(push_video(&mut args, &spec, HostOs::Linux).is_none());
        assert!(args.iter().any(|a| a == "x11grab"));
        assert!(args.iter().any(|a| a.contains("+8,16")));
        assert!(args.iter().any(|a| a == "320x180"));
    }

    #[test]
    fn mac_screen_uses_avfoundation() {
        let mut args = Vec::new();
        assert!(push_video(&mut args, &CaptureSpec::screen(), HostOs::Macos).is_none());
        assert!(args.iter().any(|a| a == "avfoundation"));
        assert!(args.iter().any(|a| a == "-capture_cursor"));
        assert!(!args.iter().any(|a| a == "-filter:v"));
    }

    #[test]
    fn mac_region_crop_is_not_pushed_between_inputs() {
        let mut spec = CaptureSpec::screen();
        spec.video = VideoSource::Region {
            region: Region::new(8, 16, 320, 180),
        };
        let mut args = Vec::new();
        assert_eq!(
            push_video(&mut args, &spec, HostOs::Macos).as_deref(),
            Some("crop=320:180:8:16")
        );
        assert!(!args.iter().any(|a| a == "-filter:v"));
    }
}
