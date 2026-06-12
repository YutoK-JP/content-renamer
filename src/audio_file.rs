use std::path::Path;

use anyhow::{Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFileKind {
    Mp3,
    M4a,
}

pub fn detect_audio_file_kind(path: &Path) -> Result<AudioFileKind> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase());

    match extension.as_deref() {
        Some("mp3") => Ok(AudioFileKind::Mp3),
        Some("m4a") => Ok(AudioFileKind::M4a),
        _ => bail!("mp3 / m4a ファイルのみ対応しています"),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{AudioFileKind, detect_audio_file_kind};

    #[test]
    fn detects_supported_extensions() {
        assert_eq!(
            detect_audio_file_kind(Path::new("song.mp3")).expect("mp3"),
            AudioFileKind::Mp3
        );
        assert_eq!(
            detect_audio_file_kind(Path::new("song.m4a")).expect("m4a"),
            AudioFileKind::M4a
        );
    }

    #[test]
    fn rejects_unsupported_extensions() {
        let error = detect_audio_file_kind(Path::new("song.wav")).expect_err("wav rejected");
        assert!(error.to_string().contains("mp3 / m4a"));
    }
}
