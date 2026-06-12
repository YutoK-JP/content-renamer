use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::blocking::{Client, Response};
use reqwest::header::RETRY_AFTER;
use serde::{Deserialize, Serialize};

static MULTI_SPACE: Lazy<Regex> = Lazy::new(|| Regex::new(r#"\s+"#).expect("valid regex"));
static NORMALIZE_SEPARATORS: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"[\s_\-／/・:;!?,\(\)\[\]【】"'`\.]+"#).expect("valid regex"));
static TITLE_CACHE: Lazy<Mutex<TitleCacheStore>> =
    Lazy::new(|| Mutex::new(TitleCacheStore::load()));

const ANILIST_ENDPOINT: &str = "https://graphql.anilist.co";
const MAX_RETRY_ATTEMPTS: usize = 3;
const DEFAULT_RETRY_DELAY_SECS: u64 = 3;
const MAX_RESPONSE_BODY_IN_ERROR: usize = 160;

const SEARCH_QUERY: &str = r#"
query ($search: String) {
  page: Page(page: 1, perPage: 5) {
    media(search: $search, type: ANIME) {
      siteUrl
      title {
        native
        romaji
        english
      }
      synonyms
    }
  }
}
"#;

#[derive(Clone)]
pub struct TitleLocalizer {
    client: Client,
}

impl TitleLocalizer {
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    pub fn localize_title(&self, title: &str) -> Result<Option<String>> {
        if !should_localize(title) {
            return Ok(None);
        }

        let cache_key = normalized_lookup_text(title);
        if let Some(cached) = cache_get(&cache_key) {
            return Ok(cached);
        }

        let request = GraphQlRequest {
            query: SEARCH_QUERY,
            variables: GraphQlVariables { search: title },
        };
        let response = self.request_json::<GraphQlResponse>(ANILIST_ENDPOINT, &request)?;
        let localized = select_localized_title(title, &response);

        cache_put(cache_key, localized.clone());
        Ok(localized)
    }

    fn request_json<T>(&self, url: &str, request: &GraphQlRequest<'_>) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        for attempt in 0..MAX_RETRY_ATTEMPTS {
            let response = self
                .client
                .post(url)
                .json(request)
                .send()
                .context("failed to call AniList API")?;

            let status = response.status();
            if status.is_success() {
                let parsed = response
                    .json::<T>()
                    .context("failed to parse AniList API response")?;
                return Ok(parsed);
            }

            if status.as_u16() == 429 || status.as_u16() == 503 {
                if attempt + 1 < MAX_RETRY_ATTEMPTS {
                    let wait_seconds =
                        retry_delay_seconds(&response).unwrap_or(DEFAULT_RETRY_DELAY_SECS);
                    thread::sleep(Duration::from_secs(wait_seconds));
                    continue;
                }

                let detail = compact_response_body(response);
                bail!(
                    "AniList API のレート制限または一時的な混雑で失敗しました (HTTP {})。{}",
                    status.as_u16(),
                    detail
                );
            }

            let detail = compact_response_body(response);
            bail!("AniList API returned HTTP {}. {}", status.as_u16(), detail);
        }

        bail!("AniList API request failed after retries")
    }
}

fn select_localized_title(original_title: &str, response: &GraphQlResponse) -> Option<String> {
    let media = response.data.as_ref()?.page.as_ref()?.media.as_slice();

    let mut best_match = None::<(usize, &AniListMedia)>;
    for candidate in media {
        let score = media_match_score(original_title, candidate);
        match best_match {
            Some((best_score, _)) if best_score >= score => {}
            _ => best_match = Some((score, candidate)),
        }
    }

    let (score, media) = best_match?;
    if score < 50 {
        return None;
    }

    let native = media.title.native.as_deref()?.trim();
    if native.is_empty() || !contains_japanese(native) {
        return None;
    }

    if normalized_lookup_text(native) == normalized_lookup_text(original_title) {
        return None;
    }

    Some(native.to_string())
}

fn media_match_score(original_title: &str, media: &AniListMedia) -> usize {
    let mut best = 0;

    for candidate in media
        .title
        .romaji
        .iter()
        .chain(media.title.english.iter())
        .chain(media.title.native.iter())
        .chain(media.synonyms.iter())
    {
        best = best.max(title_similarity_score(original_title, candidate));
    }

    if contains_japanese(media.title.native.as_deref().unwrap_or_default()) {
        best += 5;
    }

    best
}

fn title_similarity_score(left: &str, right: &str) -> usize {
    let left = normalized_lookup_text(left);
    let right = normalized_lookup_text(right);

    if left.is_empty() || right.is_empty() {
        return 0;
    }

    if left == right {
        return 100;
    }

    if left.contains(&right) || right.contains(&left) {
        return 65;
    }

    let left_tokens = token_set(&left);
    let right_tokens = token_set(&right);
    let overlap = left_tokens.intersection(&right_tokens).count();
    overlap.saturating_mul(14)
}

fn should_localize(title: &str) -> bool {
    !contains_japanese(title) && title.chars().any(|character| character.is_alphabetic())
}

fn contains_japanese(value: &str) -> bool {
    value.chars().any(is_japanese_character)
}

fn is_japanese_character(character: char) -> bool {
    matches!(
        character as u32,
        0x3040..=0x309F
            | 0x30A0..=0x30FF
            | 0x31F0..=0x31FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xFF66..=0xFF9D
    )
}

fn token_set(value: &str) -> std::collections::HashSet<String> {
    value
        .split(' ')
        .filter(|token| !token.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn normalized_lookup_text(value: &str) -> String {
    let lowered = value.to_lowercase();
    MULTI_SPACE
        .replace_all(&NORMALIZE_SEPARATORS.replace_all(&lowered, " "), " ")
        .trim()
        .to_string()
}

fn retry_delay_seconds(response: &Response) -> Option<u64> {
    response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
}

fn compact_response_body(response: Response) -> String {
    let body = response.text().unwrap_or_default();
    let normalized = MULTI_SPACE.replace_all(body.trim(), " ");
    let trimmed = normalized
        .chars()
        .take(MAX_RESPONSE_BODY_IN_ERROR)
        .collect::<String>();

    if trimmed.is_empty() {
        "レスポンス本文は空でした。".to_string()
    } else {
        format!("詳細: {trimmed}")
    }
}

fn cache_get(key: &str) -> Option<Option<String>> {
    TITLE_CACHE
        .lock()
        .ok()
        .and_then(|cache| cache.entries.get(key).cloned())
}

fn cache_put(key: String, value: Option<String>) {
    if let Ok(mut cache) = TITLE_CACHE.lock() {
        cache.entries.insert(key, value);
        let _ = cache.persist();
    }
}

#[derive(Default)]
struct TitleCacheStore {
    path: Option<PathBuf>,
    entries: HashMap<String, Option<String>>,
}

impl TitleCacheStore {
    fn load() -> Self {
        let path = cache_file_path();
        let entries = path
            .as_ref()
            .and_then(|path| fs::read_to_string(path).ok())
            .and_then(|content| serde_json::from_str::<CacheFile>(&content).ok())
            .map(|file| file.entries)
            .unwrap_or_default();

        Self { path, entries }
    }

    fn persist(&self) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).context("failed to create cache directory")?;
        }

        let content = serde_json::to_string_pretty(&CacheFile {
            entries: self.entries.clone(),
        })
        .context("failed to serialize title cache")?;
        fs::write(path, content).context("failed to write title cache")?;

        Ok(())
    }
}

fn cache_file_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME")?;
        Some(
            PathBuf::from(home)
                .join("Library")
                .join("Caches")
                .join("ThemeCommentWriter")
                .join("anilist_title_cache.json"),
        )
    }

    #[cfg(not(target_os = "macos"))]
    {
        Some(
            std::env::temp_dir()
                .join("ThemeCommentWriter")
                .join("anilist_title_cache.json"),
        )
    }
}

#[derive(Serialize, Deserialize)]
struct CacheFile {
    entries: HashMap<String, Option<String>>,
}

#[derive(Serialize)]
struct GraphQlRequest<'a> {
    query: &'a str,
    variables: GraphQlVariables<'a>,
}

#[derive(Serialize)]
struct GraphQlVariables<'a> {
    search: &'a str,
}

#[derive(Deserialize)]
struct GraphQlResponse {
    data: Option<GraphQlData>,
}

#[derive(Deserialize)]
struct GraphQlData {
    page: Option<GraphQlPage>,
}

#[derive(Deserialize)]
struct GraphQlPage {
    #[serde(default)]
    media: Vec<AniListMedia>,
}

#[derive(Deserialize)]
struct AniListMedia {
    title: AniListTitle,
    #[serde(default)]
    synonyms: Vec<String>,
}

#[derive(Deserialize)]
struct AniListTitle {
    native: Option<String>,
    romaji: Option<String>,
    english: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::{
        AniListMedia, AniListTitle, GraphQlData, GraphQlPage, GraphQlResponse, contains_japanese,
        media_match_score, normalized_lookup_text, select_localized_title, should_localize,
        title_similarity_score,
    };

    #[test]
    fn localizes_romaji_title_to_native() {
        let response = GraphQlResponse {
            data: Some(GraphQlData {
                page: Some(GraphQlPage {
                    media: vec![AniListMedia {
                        title: AniListTitle {
                            native: Some("ブルーロック".to_string()),
                            romaji: Some("Blue Lock".to_string()),
                            english: Some("BLUE LOCK".to_string()),
                        },
                        synonyms: vec![],
                    }],
                }),
            }),
        };

        let localized = select_localized_title("Blue Lock", &response);
        assert_eq!(localized.as_deref(), Some("ブルーロック"));
    }

    #[test]
    fn skips_titles_that_are_already_japanese() {
        assert!(!should_localize("ぼっち・ざ・ろっく！"));
        assert!(should_localize("Bocchi the Rock!"));
    }

    #[test]
    fn similarity_prefers_exact_alias_match() {
        assert!(
            title_similarity_score("Blue Lock", "Blue Lock")
                > title_similarity_score("Blue Lock", "Blue Dragon")
        );
    }

    #[test]
    fn detects_japanese_characters() {
        assert!(contains_japanese("ブルーロック"));
        assert!(!contains_japanese("Blue Lock"));
    }

    #[test]
    fn normalizes_titles_for_cache_key() {
        assert_eq!(normalized_lookup_text("Blue-Lock!"), "blue lock");
    }

    #[test]
    fn media_score_uses_synonyms_too() {
        let media = AniListMedia {
            title: AniListTitle {
                native: Some("ブルーロック".to_string()),
                romaji: Some("Blue Lock".to_string()),
                english: Some("BLUE LOCK".to_string()),
            },
            synonyms: vec![
                "ブルーロック第2期".to_string(),
                "Blue Lock 2nd Season".to_string(),
            ],
        };

        assert!(media_match_score("Blue Lock", &media) >= 100);
    }
}
