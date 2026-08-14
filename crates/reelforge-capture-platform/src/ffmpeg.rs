//! Build an ffmpeg CLI that writes segmented mkv (crash-safe closed files).

use crate::audio::push_audio;
use crate::host::HostOs;
use crate::video::push_video;
use reelforge_capture_core::{CaptureError, CaptureSpec, Result};
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

/// Build `ffmpeg … -f segment` for this host.
///
/// # Errors
///
/// Invalid fps / segment length.
pub fn grab_command(spec: &CaptureSpec, session_dir: &Path) -> Result<FfmpegGrab> {
    grab_command_on(HostOs::current(), spec, session_dir)
}

/// Same as [`grab_command`] with an explicit OS (tests + cross-compile checks).
///
/// # Errors
///
/// Invalid fps / segment length.
pub fn grab_command_on(os: HostOs, spec: &CaptureSpec, session_dir: &Path) -> Result<FfmpegGrab> {
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
    push_video(&mut args, spec, os);
    push_audio(&mut args, &spec.audio, os);
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
    fn windows_screen_is_gdigrab() {
        let g = grab_command_on(
            HostOs::Windows,
            &CaptureSpec::screen(),
            &PathBuf::from("sessions/s"),
        )
        .unwrap();
        assert!(g.args.iter().any(|a| a == "gdigrab"));
        assert!(g.args.iter().any(|a| a == "desktop"));
        assert!(g.args.iter().any(|a| a == "segment"));
    }

    #[test]
    fn each_host_gets_a_segment_mux() {
        for os in [HostOs::Windows, HostOs::Macos, HostOs::Linux] {
            let g = grab_command_on(os, &CaptureSpec::screen(), Path::new("s")).unwrap();
            assert!(g.args.iter().any(|a| a == os.video_format()), "{os:?}");
            assert!(g.args.iter().any(|a| a == "segment"), "{os:?}");
        }
    }

    #[test]
    fn windows_region_and_dshow_mic() {
        let mut spec = CaptureSpec::screen();
        spec.video = VideoSource::Region {
            region: Region::new(10, 20, 640, 360),
        };
        spec.audio.microphone = Some(AudioDevice::dshow("Mic"));
        let g = grab_command_on(HostOs::Windows, &spec, Path::new("s")).unwrap();
        assert!(g.args.iter().any(|a| a == "640x360"));
        assert!(g.args.iter().any(|a| a == "audio=Mic"));
    }
}
