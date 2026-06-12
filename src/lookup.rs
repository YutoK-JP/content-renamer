use std::collections::{HashMap, HashSet};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::blocking::{Client, Response};
use reqwest::header::RETRY_AFTER;
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::file_name::SearchInput;
use crate::title_localizer::TitleLocalizer;

static MULTI_SPACE: Lazy<Regex> = Lazy::new(|| Regex::new(r#"\s+"#).expect("valid regex"));
static NORMALIZE_SEPARATORS: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"[\s_\-／/・:;!?,\(\)\[\]【】"'`\.]+"#).expect("valid regex"));

const MAX_EXACT_QUERIES: usize = 6;
const MAX_EXACT_RESULTS_PER_QUERY: usize = 8;
const MAX_FUZZY_QUERIES: usize = 4;
const MAX_FUZZY_RESULTS_PER_QUERY: usize = 6;
const MAX_FUZZY_DETAIL_REQUESTS: usize = 8;
const DESIRED_CANDIDATE_COUNT: usize = 8;
const MAX_RETRY_ATTEMPTS: usize = 3;
const DEFAULT_RETRY_DELAY_SECS: u64 = 3;
const MAX_RESPONSE_BODY_IN_ERROR: usize = 160;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThemeKind {
    Opening,
    Ending,
    Other,
}

impl ThemeKind {
    pub fn label(self) -> &'static str {
        match self {
            ThemeKind::Opening => "OP",
            ThemeKind::Ending => "ED",
            ThemeKind::Other => "etc.",
        }
    }

    fn priority(self) -> usize {
        match self {
            ThemeKind::Opening | ThemeKind::Ending => 1,
            ThemeKind::Other => 0,
        }
    }

    fn from_api_type(value: &str) -> Self {
        match value {
            "OP" => ThemeKind::Opening,
            "ED" => ThemeKind::Ending,
            _ => ThemeKind::Other,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ThemeCandidate {
    pub work_title: String,
    pub kind: ThemeKind,
    pub score: usize,
    pub sources: Vec<CandidateSource>,
}

impl ThemeCandidate {
    pub fn formatted_label(&self) -> String {
        format!("{} [{}]", self.work_title, self.kind.label())
    }
}

#[derive(Debug, Clone)]
pub struct CandidateSource {
    pub page_title: String,
    pub source_url: String,
    pub matched_query: String,
}

pub struct ThemeLookupService {
    client: Client,
    title_localizer: TitleLocalizer,
}

impl ThemeLookupService {
    pub fn new() -> Result<Self> {
        let client = Client::builder()
            .user_agent(build_user_agent())
            .build()
            .context("failed to build HTTP client")?;

        Ok(Self {
            title_localizer: TitleLocalizer::new(client.clone()),
            client,
        })
    }

    pub fn lookup_candidates(&self, search_input: &SearchInput) -> Result<Vec<ThemeCandidate>> {
        let mut songs = HashMap::<u64, SongMatch>::new();

        for (query_index, query) in build_exact_queries(search_input)
            .into_iter()
            .take(MAX_EXACT_QUERIES)
            .enumerate()
        {
            for song in self.lookup_exact_songs(&query)? {
                merge_exact_song(&mut songs, song, &query, query_index, search_input);
            }

            if build_candidates(&songs).len() >= DESIRED_CANDIDATE_COUNT {
                break;
            }
        }

        if songs.is_empty() {
            let fuzzy_hits = self.collect_fuzzy_hits(search_input)?;

            for hit in fuzzy_hits.into_iter().take(MAX_FUZZY_DETAIL_REQUESTS) {
                let song = self.fetch_song_detail(hit.id)?;
                merge_fuzzy_song(&mut songs, song, hit, search_input);
            }
        }

        let mut candidates = build_candidates(&songs);
        self.localize_candidate_titles(&mut candidates);
        Ok(candidates)
    }

    fn lookup_exact_songs(&self, query: &ExactQuery) -> Result<Vec<SongRecord>> {
        let params = vec![
            ("filter[title]", query.title.clone()),
            ("include", "animethemes.anime,artists".to_string()),
            ("page[size]", MAX_EXACT_RESULTS_PER_QUERY.to_string()),
        ];
        let response = self
            .request_json::<SongCollectionResponse>("https://api.animethemes.moe/song", &params)
            .with_context(|| {
                format!("failed to search AnimeThemes songs for \"{}\"", query.title)
            })?;
        Ok(prefer_artist_matches(
            response.songs,
            query.artist.as_deref(),
        ))
    }

    fn collect_fuzzy_hits(
        &self,
        search_input: &SearchInput,
    ) -> Result<Vec<SearchSongHitAccumulator>> {
        let mut hits = HashMap::<u64, SearchSongHitAccumulator>::new();

        for (query_index, query) in build_fuzzy_queries(search_input)
            .into_iter()
            .take(MAX_FUZZY_QUERIES)
            .enumerate()
        {
            for (position, song) in self.search_song_hits(&query)?.into_iter().enumerate() {
                if position >= MAX_FUZZY_RESULTS_PER_QUERY {
                    break;
                }

                let score = fuzzy_hit_score(&song.title, &query, position, query_index);
                let entry = hits
                    .entry(song.id)
                    .or_insert_with(|| SearchSongHitAccumulator {
                        id: song.id,
                        title: song.title.clone(),
                        score: 0,
                        matched_queries: HashSet::new(),
                    });
                entry.title = song.title;
                entry.score += score;
                entry.matched_queries.insert(query.clone());
            }
        }

        let mut values = hits.into_values().collect::<Vec<_>>();
        values.sort_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| left.title.cmp(&right.title))
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(values)
    }

    fn search_song_hits(&self, query: &str) -> Result<Vec<SearchSongHit>> {
        let params = vec![("q", query.to_string())];
        let response = self
            .request_json::<SearchResponse>("https://api.animethemes.moe/search", &params)
            .with_context(|| format!("failed to search AnimeThemes for \"{query}\""))?;
        Ok(response.search.songs)
    }

    fn fetch_song_detail(&self, song_id: u64) -> Result<SongRecord> {
        let url = format!("https://api.animethemes.moe/song/{song_id}");
        let params = vec![("include", "animethemes.anime,artists".to_string())];
        let response = self
            .request_json::<SongResourceResponse>(&url, &params)
            .with_context(|| format!("failed to fetch AnimeThemes song detail for id {song_id}"))?;
        Ok(response.song)
    }

    fn request_json<T>(&self, url: &str, params: &[(&str, String)]) -> Result<T>
    where
        T: DeserializeOwned,
    {
        for attempt in 0..MAX_RETRY_ATTEMPTS {
            let response = self
                .client
                .get(url)
                .query(params)
                .send()
                .context("failed to call AnimeThemes API")?;

            let status = response.status();
            if status.is_success() {
                return response
                    .json::<T>()
                    .context("failed to parse AnimeThemes API response");
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
                    "AnimeThemes API のレート制限または一時的な混雑で失敗しました (HTTP {})。少し待ってから再試行してください。{}",
                    status.as_u16(),
                    detail
                );
            }

            let detail = compact_response_body(response);
            bail!(
                "AnimeThemes API returned HTTP {}. {}",
                status.as_u16(),
                detail
            );
        }

        bail!("AnimeThemes API request failed after retries")
    }

    fn localize_candidate_titles(&self, candidates: &mut [ThemeCandidate]) {
        for candidate in candidates {
            if let Ok(Some(localized_title)) =
                self.title_localizer.localize_title(&candidate.work_title)
            {
                candidate.work_title = localized_title;
            }
        }
    }
}

fn build_user_agent() -> String {
    format!("ThemeCommentWriter/{}", env!("CARGO_PKG_VERSION"))
}

fn build_exact_queries(search_input: &SearchInput) -> Vec<ExactQuery> {
    let mut queries = Vec::new();

    for title in &search_input.title_queries {
        queries.push(ExactQuery {
            title: title.clone(),
            matched_query: format_exact_matched_query(title, search_input.source_artist.as_deref()),
            artist: search_input.source_artist.clone(),
        });
    }

    if search_input.source_artist.is_some() {
        for title in &search_input.title_queries {
            queries.push(ExactQuery {
                title: title.clone(),
                matched_query: title.clone(),
                artist: None,
            });
        }
    }

    queries
}

fn format_exact_matched_query(title: &str, artist: Option<&str>) -> String {
    match artist {
        Some(artist) => format!("{title} {artist}"),
        None => title.to_string(),
    }
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
    compact_body_text(&body)
}

fn compact_body_text(body: &str) -> String {
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

fn build_fuzzy_queries(search_input: &SearchInput) -> Vec<String> {
    let mut queries = Vec::new();
    let mut seen = HashSet::new();

    for query in &search_input.search_queries {
        if !query.trim().is_empty() && seen.insert(normalized_lookup_text(query)) {
            queries.push(query.clone());
        }
    }

    queries
}

fn prefer_artist_matches(songs: Vec<SongRecord>, artist: Option<&str>) -> Vec<SongRecord> {
    let Some(artist) = artist else {
        return songs;
    };

    let matched = songs
        .iter()
        .filter(|song| artist_match_score(&song.artists, Some(artist)) > 0)
        .cloned()
        .collect::<Vec<_>>();

    if matched.is_empty() { songs } else { matched }
}

fn merge_exact_song(
    songs: &mut HashMap<u64, SongMatch>,
    song: SongRecord,
    query: &ExactQuery,
    query_index: usize,
    search_input: &SearchInput,
) {
    let score = exact_song_score(&song, query, query_index, search_input);
    let entry = songs.entry(song.id).or_insert_with(|| SongMatch {
        song: song.clone(),
        score: 0,
        matched_queries: HashSet::new(),
    });

    entry.song = song;
    entry.score += score;
    entry.matched_queries.insert(query.matched_query.clone());
}

fn merge_fuzzy_song(
    songs: &mut HashMap<u64, SongMatch>,
    song: SongRecord,
    hit: SearchSongHitAccumulator,
    search_input: &SearchInput,
) {
    let mut score = hit.score;
    score += title_match_score(&song.title, &search_input.source_title);
    score += artist_match_score(&song.artists, search_input.source_artist.as_deref());

    let entry = songs.entry(song.id).or_insert_with(|| SongMatch {
        song: song.clone(),
        score: 0,
        matched_queries: HashSet::new(),
    });

    entry.song = song;
    entry.score += score;
    entry.matched_queries.extend(hit.matched_queries);
}

fn exact_song_score(
    song: &SongRecord,
    query: &ExactQuery,
    query_index: usize,
    search_input: &SearchInput,
) -> usize {
    90 + query_rank_bonus(query_index)
        + title_match_score(&song.title, &query.title)
        + title_match_score(&song.title, &search_input.source_title)
        + artist_match_score(&song.artists, search_input.source_artist.as_deref())
}

fn fuzzy_hit_score(song_title: &str, query: &str, position: usize, query_index: usize) -> usize {
    20 + query_rank_bonus(query_index)
        + position_rank_bonus(position)
        + title_match_score(song_title, query)
}

fn query_rank_bonus(query_index: usize) -> usize {
    MAX_EXACT_QUERIES
        .saturating_sub(query_index)
        .saturating_mul(4)
}

fn position_rank_bonus(position: usize) -> usize {
    MAX_FUZZY_RESULTS_PER_QUERY
        .saturating_sub(position)
        .saturating_mul(3)
}

fn title_match_score(song_title: &str, candidate: &str) -> usize {
    let song = normalized_lookup_text(song_title);
    let needle = normalized_lookup_text(candidate);

    if song.is_empty() || needle.is_empty() {
        return 0;
    }

    if song == needle {
        return 45;
    }

    if song.contains(&needle) || needle.contains(&song) {
        return 20;
    }

    let song_tokens = token_set(&song);
    let needle_tokens = token_set(&needle);
    let overlap = song_tokens.intersection(&needle_tokens).count();

    overlap.saturating_mul(6)
}

fn artist_match_score(artists: &[ArtistRecord], source_artist: Option<&str>) -> usize {
    let Some(source_artist) = source_artist else {
        return 0;
    };

    let needle = normalized_lookup_text(source_artist);
    if needle.is_empty() {
        return 0;
    }

    for artist in artists {
        let candidate = normalized_lookup_text(&artist.name);
        if candidate.is_empty() {
            continue;
        }

        if candidate == needle {
            return 30;
        }

        if candidate.contains(&needle) || needle.contains(&candidate) {
            return 15;
        }
    }

    0
}

fn build_candidates(songs: &HashMap<u64, SongMatch>) -> Vec<ThemeCandidate> {
    let mut aggregated = HashMap::<CandidateKey, CandidateAccumulator>::new();

    for song_match in songs.values() {
        for animetheme in &song_match.song.animethemes {
            let Some(anime) = &animetheme.anime else {
                continue;
            };

            let kind = ThemeKind::from_api_type(&animetheme.theme_type);
            let key = CandidateKey {
                work_title: anime.name.clone(),
                kind,
            };
            let entry = aggregated
                .entry(key)
                .or_insert_with(|| CandidateAccumulator::new(anime.name.clone(), kind));
            let source_key = format!("{}::{}", song_match.song.id, animetheme.id);

            if entry.source_keys.insert(source_key) {
                entry.score += song_match.score;
                entry.sources.push(CandidateSource {
                    page_title: format_song_source(&song_match.song),
                    source_url: anime_url(anime),
                    matched_query: summarize_queries(&song_match.matched_queries),
                });
            }
        }
    }

    let mut candidates = aggregated
        .into_values()
        .map(|candidate| ThemeCandidate {
            work_title: candidate.work_title,
            kind: candidate.kind,
            score: candidate.score,
            sources: candidate.sources,
        })
        .collect::<Vec<_>>();

    candidates.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| right.kind.priority().cmp(&left.kind.priority()))
            .then_with(|| right.sources.len().cmp(&left.sources.len()))
            .then_with(|| left.work_title.cmp(&right.work_title))
            .then_with(|| left.kind.label().cmp(right.kind.label()))
    });

    candidates
}

fn format_song_source(song: &SongRecord) -> String {
    let artists = song
        .artists
        .iter()
        .map(|artist| artist.name.as_str())
        .collect::<Vec<_>>();

    if artists.is_empty() {
        format!("AnimeThemes: {}", song.title)
    } else {
        format!("AnimeThemes: {} / {}", song.title, artists.join(", "))
    }
}

fn anime_url(anime: &AnimeRecord) -> String {
    format!("https://animethemes.moe/anime/{}", anime.slug)
}

fn summarize_queries(queries: &HashSet<String>) -> String {
    let mut values = queries.iter().cloned().collect::<Vec<_>>();
    values.sort();

    values.into_iter().take(3).collect::<Vec<_>>().join(" / ")
}

fn normalized_lookup_text(value: &str) -> String {
    let lowered = value.to_lowercase();
    MULTI_SPACE
        .replace_all(&NORMALIZE_SEPARATORS.replace_all(&lowered, " "), " ")
        .trim()
        .to_string()
}

fn token_set(value: &str) -> HashSet<String> {
    value
        .split(' ')
        .filter(|token| !token.is_empty())
        .map(ToOwned::to_owned)
        .collect::<HashSet<_>>()
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CandidateKey {
    work_title: String,
    kind: ThemeKind,
}

#[derive(Debug, Clone)]
struct CandidateAccumulator {
    work_title: String,
    kind: ThemeKind,
    score: usize,
    sources: Vec<CandidateSource>,
    source_keys: HashSet<String>,
}

impl CandidateAccumulator {
    fn new(work_title: String, kind: ThemeKind) -> Self {
        Self {
            work_title,
            kind,
            score: 0,
            sources: Vec::new(),
            source_keys: HashSet::new(),
        }
    }
}

#[derive(Debug, Clone)]
struct SongMatch {
    song: SongRecord,
    score: usize,
    matched_queries: HashSet<String>,
}

#[derive(Debug, Clone)]
struct ExactQuery {
    title: String,
    matched_query: String,
    artist: Option<String>,
}

#[derive(Debug, Clone)]
struct SearchSongHitAccumulator {
    id: u64,
    title: String,
    score: usize,
    matched_queries: HashSet<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct SongCollectionResponse {
    #[serde(default)]
    songs: Vec<SongRecord>,
}

#[derive(Debug, Clone, Deserialize)]
struct SongResourceResponse {
    song: SongRecord,
}

#[derive(Debug, Clone, Deserialize)]
struct SearchResponse {
    search: SearchPayload,
}

#[derive(Debug, Clone, Deserialize)]
struct SearchPayload {
    #[serde(default)]
    songs: Vec<SearchSongHit>,
}

#[derive(Debug, Clone, Deserialize)]
struct SearchSongHit {
    id: u64,
    title: String,
}

#[derive(Debug, Clone, Deserialize)]
struct SongRecord {
    id: u64,
    title: String,
    #[serde(default)]
    animethemes: Vec<AnimeThemeRecord>,
    #[serde(default)]
    artists: Vec<ArtistRecord>,
}

#[derive(Debug, Clone, Deserialize)]
struct AnimeThemeRecord {
    id: u64,
    #[serde(rename = "type")]
    theme_type: String,
    anime: Option<AnimeRecord>,
}

#[derive(Debug, Clone, Deserialize)]
struct AnimeRecord {
    name: String,
    slug: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ArtistRecord {
    name: String,
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::{
        AnimeRecord, AnimeThemeRecord, ArtistRecord, SearchInput, SongMatch, SongRecord, ThemeKind,
        build_candidates, build_exact_queries, build_fuzzy_queries, build_user_agent,
        compact_body_text, title_match_score,
    };

    #[test]
    fn maps_api_theme_types() {
        assert_eq!(ThemeKind::from_api_type("OP"), ThemeKind::Opening);
        assert_eq!(ThemeKind::from_api_type("ED"), ThemeKind::Ending);
        assert_eq!(ThemeKind::from_api_type("IN"), ThemeKind::Other);
    }

    #[test]
    fn builds_candidates_from_song_matches() {
        let mut songs = HashMap::new();
        songs.insert(
            1,
            SongMatch {
                song: SongRecord {
                    id: 1,
                    title: "KICK BACK".to_string(),
                    animethemes: vec![AnimeThemeRecord {
                        id: 11,
                        theme_type: "OP".to_string(),
                        anime: Some(AnimeRecord {
                            name: "チェンソーマン".to_string(),
                            slug: "chainsaw_man".to_string(),
                        }),
                    }],
                    artists: vec![ArtistRecord {
                        name: "米津玄師".to_string(),
                    }],
                },
                score: 120,
                matched_queries: HashSet::from([String::from("KICK BACK")]),
            },
        );
        songs.insert(
            2,
            SongMatch {
                song: SongRecord {
                    id: 2,
                    title: "KICK BACK".to_string(),
                    animethemes: vec![AnimeThemeRecord {
                        id: 12,
                        theme_type: "OP".to_string(),
                        anime: Some(AnimeRecord {
                            name: "チェンソーマン".to_string(),
                            slug: "chainsaw_man".to_string(),
                        }),
                    }],
                    artists: vec![ArtistRecord {
                        name: "米津玄師".to_string(),
                    }],
                },
                score: 80,
                matched_queries: HashSet::from([String::from("KICK BACK")]),
            },
        );

        let candidates = build_candidates(&songs);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].formatted_label(), "チェンソーマン [OP]");
        assert_eq!(candidates[0].score, 200);
        assert_eq!(candidates[0].sources.len(), 2);
    }

    #[test]
    fn includes_artist_in_fuzzy_queries() {
        let search_input = SearchInput {
            source_title: "Blue".to_string(),
            source_artist: Some("Fujifabric".to_string()),
            title_queries: vec!["Blue".to_string()],
            search_queries: vec![
                "Blue Fujifabric".to_string(),
                "Fujifabric Blue".to_string(),
                "Blue".to_string(),
            ],
        };

        let queries = build_fuzzy_queries(&search_input);
        assert_eq!(queries[0], "Blue Fujifabric");
        assert_eq!(queries[1], "Fujifabric Blue");
        assert_eq!(queries[2], "Blue");
        assert_eq!(queries.len(), 3);
    }

    #[test]
    fn builds_artist_aware_exact_queries_first() {
        let search_input = SearchInput {
            source_title: "Blue".to_string(),
            source_artist: Some("Fujifabric".to_string()),
            title_queries: vec!["Blue".to_string()],
            search_queries: vec!["Blue Fujifabric".to_string(), "Blue".to_string()],
        };

        let queries = build_exact_queries(&search_input);
        assert_eq!(queries[0].title, "Blue");
        assert_eq!(queries[0].matched_query, "Blue Fujifabric");
        assert_eq!(queries[0].artist.as_deref(), Some("Fujifabric"));
        assert_eq!(queries[1].matched_query, "Blue");
        assert!(queries[1].artist.is_none());
    }

    #[test]
    fn scores_exact_title_highly() {
        assert!(title_match_score("Blue", "Blue") > title_match_score("Blue", "Reply"));
    }

    #[test]
    fn builds_descriptive_user_agent() {
        let user_agent = build_user_agent();
        assert!(user_agent.starts_with("ThemeCommentWriter/"));
    }

    #[test]
    fn compacts_response_body() {
        let detail = compact_body_text("  too   many\nspaces   here  ");
        assert_eq!(detail, "詳細: too many spaces here");
    }
}
