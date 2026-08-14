//! Host audio inputs (`dshow` / `avfoundation` / `pulse`).

use crate::host::HostOs;
use reelforge_capture_core::{AudioDevice, AudioMix};

/// Append one `-f <backend> -i <src>` per mix leg.
pub(crate) fn push_audio(args: &mut Vec<String>, mix: &AudioMix, os: HostOs) {
    for dev in [&mix.system, &mix.microphone].into_iter().flatten() {
        let backend = resolve_backend(dev, os);
        args.extend([
            "-f".into(),
            backend.clone(),
            "-i".into(),
            audio_input(&backend, &dev.name),
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
}
