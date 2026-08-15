//! Host audio inputs (`dshow` / `avfoundation` / `pulse`).

use crate::host::HostOs;
use reelforge_capture_core::{AudioDevice, AudioMix};

/// How many extra `-i` legs were appended (system then mic).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AudioPlan {
    pub legs: u8,
    pub has_system: bool,
    pub has_mic: bool,
}

impl AudioPlan {
    pub(crate) const fn empty() -> Self {
        Self {
            legs: 0,
            has_system: false,
            has_mic: false,
        }
    }
}

/// Append one `-f <backend> -i <src>` per mix leg. Video is assumed to be input 0.
pub(crate) fn push_audio(args: &mut Vec<String>, mix: &AudioMix, os: HostOs) -> AudioPlan {
    let mut plan = AudioPlan::empty();
    if mix.system.is_some() {
        plan.has_system = true;
        plan.legs = plan.legs.saturating_add(1);
    }
    if mix.microphone.is_some() {
        plan.has_mic = true;
        plan.legs = plan.legs.saturating_add(1);
    }
    for dev in mix.legs() {
        let backend = resolve_backend(dev, os);
        args.extend([
            "-f".into(),
            backend.clone(),
            "-i".into(),
            audio_input(&backend, &dev.name),
        ]);
    }
    plan
}

/// Map video + each audio stream separately; resample with bounded async.
///
/// System and mic stay as distinct streams (not `amix`). Default ffmpeg
/// stream selection would otherwise keep only one audio input.
pub(crate) fn push_audio_maps(args: &mut Vec<String>, plan: AudioPlan) {
    args.extend(["-map".into(), "0:v".into()]);
    match plan.legs {
        0 => {}
        1 => {
            args.extend([
                "-filter_complex".into(),
                "[1:a]aresample=async=1:first_pts=0[a0]".into(),
                "-map".into(),
                "[a0]".into(),
            ]);
        }
        _ => {
            args.extend([
                "-filter_complex".into(),
                "[1:a]aresample=async=1:first_pts=0[asys];[2:a]aresample=async=1:first_pts=0[amic]"
                    .into(),
                "-map".into(),
                "[asys]".into(),
                "-map".into(),
                "[amic]".into(),
            ]);
        }
    }
    if plan.legs > 0 {
        args.extend([
            "-c:a".into(),
            "aac".into(),
            "-ar".into(),
            "48000".into(),
            "-ac".into(),
            "2".into(),
        ]);
    }
}

fn resolve_backend(dev: &AudioDevice, os: HostOs) -> String {
    if dev.backend.is_empty() {
        os.default_audio_backend().into()
    } else {
        dev.backend.clone()
    }
}

fn audio_input(backend: &str, name: &str) -> String {
    match backend {
        "dshow" | "wasapi" => format!("audio={name}"),
        "avfoundation" => avfoundation_audio(name),
        _ => name.into(),
    }
}

fn avfoundation_audio(name: &str) -> String {
    if name.starts_with(':') {
        name.into()
    } else {
        format!(":{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_mic_follows_host() {
        let mix = AudioMix {
            system: None,
            microphone: Some(AudioDevice::named("0")),
        };
        let mut mac = Vec::new();
        push_audio(&mut mac, &mix, HostOs::Macos);
        assert!(mac.iter().any(|a| a == "avfoundation"));
        assert!(mac.iter().any(|a| a == ":0"));

        let mut lin = Vec::new();
        push_audio(&mut lin, &mix, HostOs::Linux);
        assert!(lin.iter().any(|a| a == "pulse"));
        assert!(lin.iter().any(|a| a == "0"));

        let mut win = Vec::new();
        push_audio(&mut win, &mix, HostOs::Windows);
        assert!(win.iter().any(|a| a == "dshow"));
        assert!(win.iter().any(|a| a == "audio=0"));
    }

    #[test]
    fn two_legs_map_as_separate_streams() {
        let mix = AudioMix {
            system: Some(AudioDevice::named("loop")),
            microphone: Some(AudioDevice::named("mic")),
        };
        let mut args = Vec::new();
        let plan = push_audio(&mut args, &mix, HostOs::Windows);
        assert_eq!(plan.legs, 2);
        push_audio_maps(&mut args, plan);
        assert!(args.iter().any(|a| a.contains("[asys]")));
        assert!(args.iter().any(|a| a.contains("[amic]")));
        assert!(!args.iter().any(|a| a.contains("amix")));
        assert!(args.windows(2).any(|w| w == ["-c:a", "aac"]));
    }
}
