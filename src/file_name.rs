use std::collections::{BTreeSet, HashSet};
use std::path::Path;

use anyhow::{Result, bail};
use id3::{Tag, TagLike};
use mp4ameta::Tag as Mp4Tag;
use once_cell::sync::Lazy;
use regex::Regex;

use crate::audio_file::{AudioFileKind, detect_audio_file_kind};

static BRACKETED_TEXT: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"[\(\[【][^)\]】]{0,80}[\)\]】]"#).expect("valid regex"));
static LEADING_TRACK_NUMBER: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"^\s*\d{1,3}[\s._-]+"#).expect("valid regex"));
static TITLE_NOISE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"(?ix)
        \b(
            inst(?:rumental)? |
            off[\s-]?vocal |
            no[\s-]?limiter |
            tv[\s-]?size |
            anime[\s-]?size |
            short[\s-]?(?:ver|version) |
            long[\s-]?(?:ver|version) |
            edit[\s-]?(?:ver|version)
        )\b
    "#,
    )
    .expect("valid regex")
});
static MULTI_SPACE: Lazy<Regex> = Lazy::new(|| Regex::new(r#"\s+"#).expect("valid regex"));

pub struct SearchInput {
    pub source_title: String,
    pub source_artist: Option<String>,
    pub title_queries: Vec<String>,
    pub search_queries: Vec<String>,
}

pub fn build_search_input(path: &Path) -> Result<SearchInput> {
    let (source_title, source_artist) = read_metadata(path)?;
    let title_queries = build_search_queries_from_title(&source_title);
    let search_queries = build_search_queries(&title_queries, source_artist.as_deref());

    if title_queries.is_empty() {
        bail!("タイトルタグから検索語を作れませんでした");
    }

    Ok(SearchInput {
        source_title,
        source_artist,
        title_queries,
        search_queries,
    })
}

fn read_metadata(path: &Path) -> Result<(String, Option<String>)> {
    match detect_audio_file_kind(path)? {
        AudioFileKind::Mp3 => read_mp3_metadata(path),
        AudioFileKind::M4a => read_m4a_metadata(path),
    }
}

fn read_mp3_metadata(path: &Path) -> Result<(String, Option<String>)> {
    let tag = Tag::read_from_path(path)?;
    let Some(title) = tag.title() else {
        bail!("タイトルタグが見つかりません");
    };

    let source_title = normalize_text(title);
    if source_title.is_empty() {
        bail!("タイトルタグが空です");
    }

    let source_artist = tag
        .artist()
        .map(normalize_text)
        .filter(|value| !value.is_empty());

    Ok((source_title, source_artist))
}

fn read_m4a_metadata(path: &Path) -> Result<(String, Option<String>)> {
    let tag = Mp4Tag::read_from_path(path)?;
    let Some(title) = tag.title() else {
        bail!("タイトルタグが見つかりません");
    };

    let source_title = normalize_text(title);
    if source_title.is_empty() {
        bail!("タイトルタグが空です");
    }

    let source_artist = tag
        .artist()
        .or_else(|| tag.album_artist())
        .map(normalize_text)
        .filter(|value| !value.is_empty());

    Ok((source_title, source_artist))
}

fn build_search_queries_from_title(title: &str) -> Vec<String> {
    let title_without_track = strip_leading_track_number(title);

    let mut queries = BTreeSet::new();
    push_query(&mut queries, &title_without_track);
    push_query(&mut queries, &clean_title(title));
    push_query(&mut queries, &strip_title_noise(&title_without_track));
    push_query(&mut queries, &strip_title_noise(&clean_title(title)));

    for separator in [" - ", " – ", " — ", "_", "／", "/"] {
        if title_without_track.contains(separator) {
            for piece in title_without_track.split(separator) {
                push_query(&mut queries, piece);
                push_query(&mut queries, &clean_title(piece));
                push_query(&mut queries, &strip_title_noise(piece));
                push_query(&mut queries, &strip_title_noise(&clean_title(piece)));
            }
        }
    }

    queries
        .into_iter()
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
}

fn build_search_queries(title_queries: &[String], artist: Option<&str>) -> Vec<String> {
    let mut queries = Vec::new();
    let mut seen = HashSet::new();

    if let Some(artist) = artist {
        let normalized_artist = normalize_text(artist);
        if normalized_artist.len() >= 2 {
            for title_query in title_queries {
                push_unique_query(
                    &mut queries,
                    &mut seen,
                    &format!("{title_query} {normalized_artist}"),
                );
                push_unique_query(
                    &mut queries,
                    &mut seen,
                    &format!("{normalized_artist} {title_query}"),
                );
            }
        }
    }

    for title_query in title_queries {
        push_unique_query(&mut queries, &mut seen, title_query);
    }

    queries
}

fn clean_title(value: &str) -> String {
    let without_track = strip_leading_track_number(value);
    let without_brackets = BRACKETED_TEXT.replace_all(&without_track, " ");
    normalize_text(&without_brackets)
}

fn strip_leading_track_number(value: &str) -> String {
    LEADING_TRACK_NUMBER.replace(value, "").to_string()
}

fn strip_title_noise(value: &str) -> String {
    let stripped = TITLE_NOISE.replace_all(value, " ");
    normalize_text(&stripped)
}

fn push_query(queries: &mut BTreeSet<String>, value: &str) {
    let normalized = normalize_text(value);
    if normalized.len() >= 2 {
        queries.insert(normalized);
    }
}

fn push_unique_query(queries: &mut Vec<String>, seen: &mut HashSet<String>, value: &str) {
    let normalized = normalize_text(value);
    if normalized.len() < 2 {
        return;
    }

    if seen.insert(normalized.clone()) {
        queries.push(normalized);
    }
}

fn normalize_text(value: &str) -> String {
    MULTI_SPACE
        .replace_all(
            &value.replace('_', " ").replace('　', " ").replace('.', " "),
            " ",
        )
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::{build_search_queries, build_search_queries_from_title};

    #[test]
    fn extracts_multiple_query_shapes_from_title() {
        let queries = build_search_queries_from_title("01. Artist - Song Title [Official]");
        assert!(
            queries
                .iter()
                .any(|value| value == "Artist - Song Title [Official]")
        );
        assert!(queries.iter().any(|value| value == "Artist - Song Title"));
        assert!(queries.iter().any(|value| value == "Artist"));
        assert!(queries.iter().any(|value| value == "Song Title [Official]"));
        assert!(queries.iter().any(|value| value == "Song Title"));
    }

    #[test]
    fn strips_common_mix_suffixes() {
        let queries = build_search_queries_from_title("Reply Inst(NoLimiter)");
        assert!(queries.iter().any(|value| value == "Reply"));
        assert!(queries.iter().all(|value| value != "Inst"));
    }

    #[test]
    fn prepends_title_and_artist_search_queries() {
        let title_queries = build_search_queries_from_title("Blue");
        let queries = build_search_queries(&title_queries, Some("Fujifabric"));

        assert_eq!(queries[0], "Blue Fujifabric");
        assert_eq!(queries[1], "Fujifabric Blue");
        assert!(queries.iter().any(|value| value == "Blue"));
    }
}
