use std::path::PathBuf;

use crate::file_name::build_search_input;
#[cfg(target_os = "macos")]
use crate::finder_comment::{merge_theme_comment, read_comment, write_comment};
use crate::lookup::{ThemeCandidate, ThemeLookupService};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessStatus {
    Searching,
    Ready,
    Saving,
    Saved,
    Skipped,
    Error,
}

impl ProcessStatus {
    pub fn label(self) -> &'static str {
        match self {
            ProcessStatus::Searching => "検索中",
            ProcessStatus::Ready => "候補あり",
            ProcessStatus::Saving => "保存中",
            ProcessStatus::Saved => "保存済み",
            ProcessStatus::Skipped => "候補なし",
            ProcessStatus::Error => "エラー",
        }
    }
}

pub enum WorkerEvent {
    SearchCompleted(SearchResult),
    SaveCompleted(SaveResult),
}

pub struct SearchResult {
    pub id: u64,
    pub status: ProcessStatus,
    pub detail: String,
    pub candidates: Vec<ThemeCandidate>,
    pub comment: Option<String>,
}

pub struct SaveResult {
    pub id: u64,
    pub status: ProcessStatus,
    pub detail: String,
    pub comment: Option<String>,
    pub saved_candidate_value: Option<String>,
}

pub fn search_file(id: u64, path: PathBuf) -> WorkerEvent {
    let search_input = match build_search_input(&path) {
        Ok(search_input) => search_input,
        Err(error) => {
            return WorkerEvent::SearchCompleted(SearchResult {
                id,
                status: ProcessStatus::Error,
                detail: format!("検索元にするタイトルタグを読めませんでした: {error}"),
                candidates: Vec::new(),
                comment: current_comment(&path),
            });
        }
    };
    let service = match ThemeLookupService::new() {
        Ok(service) => service,
        Err(error) => {
            return WorkerEvent::SearchCompleted(SearchResult {
                id,
                status: ProcessStatus::Error,
                detail: format!("検索クライアントの初期化に失敗しました: {error}"),
                candidates: Vec::new(),
                comment: current_comment(&path),
            });
        }
    };

    match service.lookup_candidates(&search_input) {
        Ok(candidates) if candidates.is_empty() => WorkerEvent::SearchCompleted(SearchResult {
            id,
            status: ProcessStatus::Skipped,
            detail: "AnimeThemes で関連しそうなアニメ作品を見つけられませんでした".to_string(),
            candidates,
            comment: current_comment(&path),
        }),
        Ok(candidates) => WorkerEvent::SearchCompleted(SearchResult {
            id,
            status: ProcessStatus::Ready,
            detail: format!(
                "関連候補を {} 件見つけました。プルダウンから選んでコメントへ保存してください",
                candidates.len()
            ),
            candidates,
            comment: current_comment(&path),
        }),
        Err(error) => WorkerEvent::SearchCompleted(SearchResult {
            id,
            status: ProcessStatus::Error,
            detail: format!("検索に失敗しました: {error}"),
            candidates: Vec::new(),
            comment: current_comment(&path),
        }),
    }
}

pub fn save_selected_comment(id: u64, path: PathBuf, candidate_value: String) -> WorkerEvent {
    apply_selected_comment(id, &path, &candidate_value)
}

fn apply_selected_comment(id: u64, path: &PathBuf, candidate_value: &str) -> WorkerEvent {
    #[cfg(target_os = "macos")]
    {
        let existing = match read_comment(path) {
            Ok(comment) => comment,
            Err(error) => {
                return WorkerEvent::SaveCompleted(error_result(
                    id,
                    format!("既存のコメントを読めませんでした: {error}"),
                ));
            }
        };

        let merged_comment = merge_theme_comment(&existing, candidate_value);
        if let Err(error) = write_comment(path, &merged_comment) {
            return WorkerEvent::SaveCompleted(error_result(
                id,
                format!("コメントを書き込めませんでした: {error}"),
            ));
        }

        return WorkerEvent::SaveCompleted(SaveResult {
            id,
            status: ProcessStatus::Saved,
            detail: format!("Commentタグに「{candidate_value}」を保存しました"),
            comment: Some(merged_comment),
            saved_candidate_value: Some(candidate_value.to_string()),
        });
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;

        WorkerEvent::SaveCompleted(SaveResult {
            id,
            status: ProcessStatus::Skipped,
            detail: "このOSではコメントを書き込めません".to_string(),
            comment: None,
            saved_candidate_value: None,
        })
    }
}

fn current_comment(path: &PathBuf) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        match read_comment(path) {
            Ok(comment) if comment.is_empty() => None,
            Ok(comment) => Some(comment),
            Err(_) => None,
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        None
    }
}

fn error_result(id: u64, detail: String) -> SaveResult {
    SaveResult {
        id,
        status: ProcessStatus::Error,
        detail,
        comment: None,
        saved_candidate_value: None,
    }
}
