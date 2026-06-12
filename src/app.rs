use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use eframe::egui;

use crate::file_name::build_search_input;
use crate::lookup::ThemeCandidate;
use crate::processor::{
    ProcessStatus, SaveResult, SearchResult, WorkerEvent, save_selected_comment, search_file,
};

pub struct ContentRenamerApp {
    entries: Vec<FileEntry>,
    next_id: u64,
    worker_tx: Sender<WorkerEvent>,
    worker_rx: Receiver<WorkerEvent>,
}

impl Default for ContentRenamerApp {
    fn default() -> Self {
        let (worker_tx, worker_rx) = mpsc::channel();

        Self {
            entries: Vec::new(),
            next_id: 1,
            worker_tx,
            worker_rx,
        }
    }
}

impl eframe::App for ContentRenamerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.collect_results();
        self.handle_dropped_files(ctx);

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Theme Comment Writer");
            ui.label("Finderからファイルをドラッグ&ドロップすると、MP3 / M4A のタイトルタグをもとに AnimeThemes API で曲名を検索し、日本語タイトルが取れる場合はそれを優先して、関連しそうなアニメ作品と OP / ED 候補を `作品名 [OP]` 形式でコメントへ保存できます。");
            ui.add_space(8.0);

            if !cfg!(target_os = "macos") {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    "このビルドでは音声ファイルのコメント更新に対応していません。macOSで実行してください。",
                );
                ui.add_space(8.0);
            }

            if ui.button("ファイルを選択").clicked() {
                if let Some(paths) = rfd::FileDialog::new().pick_files() {
                    self.enqueue_files(paths);
                }
            }

            ui.add_space(12.0);
            self.render_drop_zone(ui, ctx);
            ui.add_space(12.0);
            self.render_entries(ui);
        });
    }
}

impl ContentRenamerApp {
    fn collect_results(&mut self) {
        while let Ok(event) = self.worker_rx.try_recv() {
            match event {
                WorkerEvent::SearchCompleted(result) => self.apply_search_result(result),
                WorkerEvent::SaveCompleted(result) => self.apply_save_result(result),
            }
        }
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped_files = ctx.input(|input| input.raw.dropped_files.clone());
        if dropped_files.is_empty() {
            return;
        }

        let paths = dropped_files
            .into_iter()
            .filter_map(|file| file.path)
            .collect::<Vec<_>>();

        self.enqueue_files(paths);
    }

    fn enqueue_files(&mut self, paths: Vec<PathBuf>) {
        for path in paths {
            if self.entries.iter().any(|entry| entry.path == path) {
                continue;
            }

            let id = self.next_id;
            self.next_id += 1;

            let file_name = path
                .file_name()
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());

            match build_search_input(&path) {
                Ok(search_input) => {
                    self.entries.push(FileEntry {
                        id,
                        path: path.clone(),
                        file_name,
                        source_title: search_input.source_title,
                        source_artist: search_input.source_artist,
                        search_queries: search_input.search_queries,
                        status: ProcessStatus::Searching,
                        detail: "タイトルタグをもとに AnimeThemes から関連候補を検索しています"
                            .to_string(),
                        candidates: Vec::new(),
                        selected_candidate: 0,
                        comment: None,
                    });

                    let sender = self.worker_tx.clone();
                    thread::spawn(move || {
                        let result = search_file(id, path);
                        let _ = sender.send(result);
                    });
                }
                Err(error) => {
                    self.entries.push(FileEntry {
                        id,
                        path,
                        file_name,
                        source_title: String::new(),
                        source_artist: None,
                        search_queries: Vec::new(),
                        status: ProcessStatus::Error,
                        detail: format!("検索元にするタイトルタグを読めませんでした: {error}"),
                        candidates: Vec::new(),
                        selected_candidate: 0,
                        comment: None,
                    });
                }
            }
        }
    }

    fn render_drop_zone(&self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let hovering = ctx.input(|input| !input.raw.hovered_files.is_empty());
        let fill = if hovering {
            ui.visuals().selection.bg_fill
        } else {
            ui.visuals().faint_bg_color
        };

        let frame = egui::Frame::group(ui.style())
            .fill(fill)
            .inner_margin(egui::Margin::same(20.0));

        frame.show(ui, |ui| {
            ui.set_min_size(egui::vec2(ui.available_width(), 120.0));
            ui.vertical_centered(|ui| {
                ui.heading("ここへファイルをドロップ");
                ui.label(
                    "候補を集めて、選んだ「作品名 [OP]」形式の値を MP3 / M4A のコメント欄へ保存します。",
                );
            });
        });
    }

    fn render_entries(&mut self, ui: &mut egui::Ui) {
        if self.entries.is_empty() {
            ui.label("まだ処理したファイルはありません。");
            return;
        }

        let mut save_requested = None;

        egui::ScrollArea::vertical().show(ui, |ui| {
            for entry in &mut self.entries {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.strong(&entry.file_name);
                        ui.separator();
                        ui.label(entry.status.label());
                    });
                    if !entry.source_title.is_empty() {
                        ui.label(format!("参照タイトル: {}", entry.source_title));
                    }
                    if let Some(source_artist) = &entry.source_artist {
                        ui.label(format!("参照アーティスト: {}", source_artist));
                    }
                    if !entry.search_queries.is_empty() {
                        ui.label(format!("検索候補: {}", entry.search_queries.join(" / ")));
                    }
                    ui.label(&entry.detail);

                    if !entry.candidates.is_empty() {
                        if entry.selected_candidate >= entry.candidates.len() {
                            entry.selected_candidate = 0;
                        }

                        let selected = &entry.candidates[entry.selected_candidate];
                        egui::ComboBox::from_id_salt(("candidate", entry.id))
                            .selected_text(format!(
                                "{} (スコア {})",
                                selected.formatted_label(),
                                selected.score
                            ))
                            .show_ui(ui, |ui| {
                                for (index, candidate) in entry.candidates.iter().enumerate() {
                                    ui.selectable_value(
                                        &mut entry.selected_candidate,
                                        index,
                                        format!(
                                            "{} (スコア {})",
                                            candidate.formatted_label(),
                                            candidate.score
                                        ),
                                    );
                                }
                            });

                        let selected = &entry.candidates[entry.selected_candidate];
                        ui.label(format!("候補数: {} 件", entry.candidates.len()));

                        for source in selected.sources.iter().take(3) {
                            ui.horizontal_wrapped(|ui| {
                                ui.label("出典:");
                                ui.hyperlink_to(&source.page_title, &source.source_url);
                                ui.label(format!("検索語: {}", source.matched_query));
                            });
                        }

                        if entry.status != ProcessStatus::Saving
                            && ui.button("この候補をコメントへ保存").clicked()
                        {
                            save_requested = Some(entry.id);
                        }
                    }

                    if let Some(comment) = &entry.comment {
                        ui.label(format!("現在のコメント: {comment}"));
                    }
                });
                ui.add_space(8.0);
            }
        });

        if let Some(entry_id) = save_requested {
            self.start_save(entry_id);
        }
    }

    fn apply_search_result(&mut self, result: SearchResult) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == result.id) {
            entry.status = result.status;
            entry.detail = result.detail;
            entry.candidates = result.candidates;
            entry.comment = result.comment;
            entry.selected_candidate = 0;
        }
    }

    fn apply_save_result(&mut self, result: SaveResult) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == result.id) {
            entry.status = result.status;
            entry.detail = result.detail;
            entry.comment = result.comment;

            if let Some(saved_candidate_value) = result.saved_candidate_value {
                if let Some(index) = entry
                    .candidates
                    .iter()
                    .position(|candidate| candidate.formatted_label() == saved_candidate_value)
                {
                    entry.selected_candidate = index;
                }
            }
        }
    }

    fn start_save(&mut self, entry_id: u64) {
        let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == entry_id) else {
            return;
        };

        let Some(selected) = entry.candidates.get(entry.selected_candidate).cloned() else {
            return;
        };

        entry.status = ProcessStatus::Saving;
        let selected_value = selected.formatted_label();
        entry.detail = format!("「{selected_value}」をコメントへ保存しています");

        let sender = self.worker_tx.clone();
        let path = entry.path.clone();
        thread::spawn(move || {
            let result = save_selected_comment(entry_id, path, selected_value);
            let _ = sender.send(result);
        });
    }
}

struct FileEntry {
    id: u64,
    path: PathBuf,
    file_name: String,
    source_title: String,
    source_artist: Option<String>,
    search_queries: Vec<String>,
    status: ProcessStatus,
    detail: String,
    candidates: Vec<ThemeCandidate>,
    selected_candidate: usize,
    comment: Option<String>,
}
