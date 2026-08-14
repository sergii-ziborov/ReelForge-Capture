//! Build an ffmpeg CLI that writes segmented mkv (crash-safe closed files).

use reelforge_capture_core::{AudioMix, CaptureError, CaptureSpec, Result, VideoSource};
use std::path::Path;
use std::process::Command;

/// Planned ffmpeg invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FfmpegGrab {
    /// Program name (`ffmpeg` or `REELFORGE_FFMPEG`).
    pub program: String,
    /// Full argv (excluding the program).
    pub args: Vec<String>,
}

/// Build `ffmpeg … -f segment` for `spec` writing into `session_dir/segments/%06d.mkv`.
///
/// # Errors
///
/// Invalid fps / segment length.
pub fn grab_command(spec: &CaptureSpec, session_dir: &Path) -> Result<FfmpegGrab> {
    if !(spec.fps.is_finite() && spec.fps > 0.0) {
        return Err(CaptureError::message(format!("bad fps {}", spec.fps)));
    }
    if !(spec.segment_secs.is_finite() && spec.segment_secs > 0.0) {
        return Err(CaptureError::message(format!(
            "bad segment_secs {}",
            spec.segment_secs
        )));
    }
    let program = std::env::var("REELFORGE_FFMPEG").unwrap_or_else(|_| "ffmpeg".into());
    let mut args = vec![
        "-hide_banner".into(),
        "-y".into(),
        "-loglevel".into(),
        "error".into(),
    ];
    push_video(&mut args, spec);
    push_audio(&mut args, &spec.audio);
    let pattern = session_dir
        .join("segments")
        .join("%06d.mkv")
        .to_string_lossy()
        .into_owned();
    args.extend([
        "-c:v".into(),
        "libx264".into(),
        "-preset".into(),
        "ultrafast".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-f".into(),
        "segment".into(),
        "-segment_time".into(),
        format!("{}", spec.segment_secs),
        "-reset_timestamps".into(),
        "1".into(),
        pattern,
    ]);
    Ok(FfmpegGrab { program, args })
}

fn push_video(args: &mut Vec<String>, spec: &CaptureSpec) {
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
            args.extend([
                "-offset_x".into(),
                region.x.to_string(),
                "-offset_y".into(),
                region.y.to_string(),
                "-video_size".into(),
                format!("{}x{}", region.width, region.height),
                "-i".into(),
                "desktop".into(),
            ]);
        }
    }
}

fn push_audio(args: &mut Vec<String>, mix: &AudioMix) {
    for dev in [&mix.system, &mix.microphone].into_iter().flatten() {
        args.extend([
            "-f".into(),
            dev.backend.clone(),
            "-i".into(),
            format!("audio={}", dev.name),
        ]);
    }
}

/// Spawn the grab (caller owns the child).
///
/// # Errors
///
/// Spawn failure.
pub fn spawn_grab(grab: &FfmpegGrab) -> Result<std::process::Child> {
    Command::new(&grab.program)
        .args(&grab.args)
        .spawn()
        .map_err(|e| CaptureError::io(format!("ffmpeg spawn: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reelforge_capture_core::{AudioDevice, Region, VideoSource};
    use std::path::PathBuf;

    #[test]
    fn screen_segment_args() {
        let g = grab_command(&CaptureSpec::screen(), &PathBuf::from("sessions/s")).unwrap();
        assert!(g.args.iter().any(|a| a == "gdigrab"));
        assert!(g.args.iter().any(|a| a == "desktop"));
        assert!(g.args.iter().any(|a| a == "segment"));
    }

    #[test]
    fn region_and_mic() {
        let mut spec = CaptureSpec::screen();
        spec.video = VideoSource::Region {
            region: Region::new(10, 20, 640, 360),
        };
        spec.audio.microphone = Some(AudioDevice::dshow("Mic"));
        let g = grab_command(&spec, Path::new("s")).unwrap();
        assert!(g.args.iter().any(|a| a == "640x360"));
        assert!(g.args.iter().any(|a| a == "audio=Mic"));
    }
}
