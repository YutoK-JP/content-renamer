use std::path::Path;

use anyhow::{Context, Result};
use id3::frame::{Comment, Content, Frame};
use id3::{Tag, TagLike, Version};
use mp4ameta::Tag as Mp4Tag;
use once_cell::sync::Lazy;
use regex::Regex;

use crate::audio_file::{AudioFileKind, detect_audio_file_kind};

const THEME_PREFIX: &str = "主題歌作品:";
static THEME_LINE_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"(?i)^.+\s\[(OP|ED|etc\.|その他)\]$"#).expect("valid regex"));

pub fn read_comment(path: &Path) -> Result<String> {
    match detect_audio_file_kind(path)? {
        AudioFileKind::Mp3 => read_mp3_comment(path),
        AudioFileKind::M4a => read_m4a_comment(path),
    }
}

pub fn write_comment(path: &Path, comment: &str) -> Result<()> {
    match detect_audio_file_kind(path)? {
        AudioFileKind::Mp3 => write_mp3_comment(path, comment),
        AudioFileKind::M4a => write_m4a_comment(path, comment),
    }
}

pub fn merge_theme_comment(existing: &str, theme_value: &str) -> String {
    let theme_line = theme_value.to_string();
    let mut lines = existing
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();

    if let Some(index) = lines.iter().position(|line| is_theme_line(line)) {
        lines[index] = theme_line;
    } else {
        lines.push(theme_line);
    }

    lines.join("\n")
}

fn is_theme_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with(THEME_PREFIX) || THEME_LINE_PATTERN.is_match(trimmed)
}

fn read_mp3_comment(path: &Path) -> Result<String> {
    let tag = match Tag::read_from_path(path) {
        Ok(tag) => tag,
        Err(_) => return Ok(String::new()),
    };

    Ok(tag
        .comments()
        .next()
        .map(|comment| comment.text.clone())
        .unwrap_or_default())
}

fn read_m4a_comment(path: &Path) -> Result<String> {
    let tag = match Mp4Tag::read_from_path(path) {
        Ok(tag) => tag,
        Err(_) => return Ok(String::new()),
    };

    Ok(tag.comment().unwrap_or_default().to_string())
}

fn write_mp3_comment(path: &Path, comment: &str) -> Result<()> {
    let mut tag = Tag::read_from_path(path).unwrap_or_else(|_| Tag::new());
    let (lang, description) = tag
        .comments()
        .next()
        .map(|existing| (existing.lang.clone(), existing.description.clone()))
        .unwrap_or_else(|| ("jpn".to_string(), String::new()));

    tag.remove("COMM");

    if !comment.trim().is_empty() {
        tag.add_frame(Frame::with_content(
            "COMM",
            Content::Comment(Comment {
                lang,
                description,
                text: comment.to_string(),
            }),
        ));
    }

    tag.write_to_path(path, Version::Id3v24)
        .context("failed to write mp3 comment tag")?;

    Ok(())
}

fn write_m4a_comment(path: &Path, comment: &str) -> Result<()> {
    let mut tag = Mp4Tag::read_from_path(path).context("failed to read m4a metadata")?;
    tag.remove_comments();

    if !comment.trim().is_empty() {
        tag.set_comment(comment.to_string());
    }

    tag.write_to_path(path)
        .context("failed to write m4a comment tag")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{merge_theme_comment, read_comment, write_comment};

    #[test]
    fn appends_theme_line() {
        let updated = merge_theme_comment("既存コメント", "君の名は。 [OP]");
        assert_eq!(updated, "既存コメント\n君の名は。 [OP]");
    }

    #[test]
    fn replaces_existing_theme_line() {
        let updated = merge_theme_comment("メモ\n主題歌作品: 古い作品", "SPY×FAMILY [ED]");
        assert_eq!(updated, "メモ\nSPY×FAMILY [ED]");
    }

    #[test]
    fn detailed_comment_roundtrip() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("content_renamer_comment_{unique}.mp3"));

        fs::write(&path, b"").expect("create temp file");
        write_comment(&path, "君の名は。 [OP]").expect("write detailed comment");

        let comment = read_comment(&path).expect("read detailed comment");
        assert_eq!(comment, "君の名は。 [OP]");

        let _ = fs::remove_file(path);
    }

    #[test]
    fn merges_theme_line_for_m4a_comment_too() {
        let updated = merge_theme_comment("既存コメント", "超かぐや姫 [etc.]");
        assert_eq!(updated, "既存コメント\n超かぐや姫 [etc.]");
    }

    #[test]
    fn replaces_new_style_theme_line() {
        let updated = merge_theme_comment("メモ\nドラえもん [OP]", "ドラえもん [ED]");
        assert_eq!(updated, "メモ\nドラえもん [ED]");
    }
}
