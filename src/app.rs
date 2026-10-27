//! Графічний інтерфейс (egui) і зв'язування модулів: список ігор, копії, процеси, журнал.
//!
//! Усі повільні операції (мережа, `tasklist`) працюють у фонових потоках і повертають
//! результат через `mpsc`-канали; UI-потік лише опитує їх через `try_recv`.

use std::fs;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::Duration;

use eframe::egui::{self, Color32, RichText};

use crate::WORKER_FLAG;
use crate::detectable::{self, Game, GameList};
use crate::error::Result;
use crate::process::{self, Runner, Snapshot};
use crate::storage::{Manifest, ManifestEntry, Storage, manifest_key, write_atomic};
use crate::util::{format_datetime, format_hms, unix_now};

const MONITOR_INTERVAL: Duration = Duration::from_secs(2);
const MAX_LOG_LINES: usize = 500;

const GREEN: Color32 = Color32::from_rgb(110, 200, 120);
const YELLOW: Color32 = Color32::from_rgb(230, 185, 70);
const RED: Color32 = Color32::from_rgb(235, 95, 95);

#[derive(Clone, Copy)]
enum Level {
    Info,
    Warn,
    Error,
}

struct LogLine {
    time: u64,
    level: Level,
    text: String,
}

/// Стан копії, виведений зі знімка `tasklist`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    /// Перший знімок ще не отримано.
    Checking,
    /// Процес запущено нами, але в `tasklist` його ще не видно.
    Starting(u32),
    Running(u32),
    Stopped,
}

impl Status {
    fn is_active(self) -> bool {
        matches!(self, Self::Starting(_) | Self::Running(_))
    }
}

/// Дії, зібрані під час малювання й виконані після нього (щоб не боротися з borrow checker).
enum Action {
    Install {
        game_id: String,
        game_name: String,
        exe_name: String,
    },
    Start(String),
    Stop(String),
    Remove(String),
}

pub struct App {
    storage: Storage,
    manifest: Manifest,

    games: Vec<Game>,
    filtered: Vec<usize>,
    filter_dirty: bool,
    query: String,
    selected_game: Option<usize>,
    selected_exe: Option<String>,

    runner: Runner,
    snapshot: Option<Snapshot>,
    monitor_rx: Receiver<Result<Snapshot>>,
    monitor_error: Option<String>,
    fetch_rx: Option<Receiver<Result<GameList>>>,

    stop_on_exit: bool,
    log: Vec<LogLine>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Result<Self> {
        let storage = Storage::init()?;
        let ctx = cc.egui_ctx.clone();
        let monitor_rx = process::spawn_monitor(MONITOR_INTERVAL, move || ctx.request_repaint());

        let mut app = Self {
            storage,
            manifest: Manifest::default(),
            games: Vec::new(),
            filtered: Vec::new(),
            filter_dirty: true,
            query: String::new(),
            selected_game: None,
            selected_exe: None,
            runner: Runner::default(),
            snapshot: None,
            monitor_rx,
            monitor_error: None,
            fetch_rx: None,
            stop_on_exit: true,
            log: Vec::new(),
        };
        app.log(
            Level::Info,
            format!("Каталог копій: {}", app.storage.games_dir().display()),
        );
        app.load_manifest();
        app.load_cache();
        app.start_fetch(&cc.egui_ctx);
        Ok(app)
    }

    // ───────────────────────────── журнал ─────────────────────────────

    fn log(&mut self, level: Level, text: impl Into<String>) {
        self.log.push(LogLine {
            time: unix_now(),
            level,
            text: text.into(),
        });
        if self.log.len() > MAX_LOG_LINES {
            let excess = self.log.len() - MAX_LOG_LINES;
            self.log.drain(..excess);
        }
    }

    // ───────────────────────────── завантаження даних ─────────────────────────────

    fn load_manifest(&mut self) {
        let mut manifest = match self.storage.load_manifest() {
            Ok(m) => m,
            Err(e) => {
                self.log(Level::Error, format!("Маніфест не вдалося прочитати: {e}"));
                match self.storage.quarantine_manifest() {
                    Ok(p) => self.log(
                        Level::Warn,
                        format!("Пошкоджений маніфест відкладено: {}", p.display()),
                    ),
                    Err(e) => self.log(Level::Error, format!("Не вдалося відкласти маніфест: {e}")),
                }
                Manifest::default()
            }
        };
        let dropped = manifest.drop_invalid();
        if dropped > 0 {
            self.log(
                Level::Warn,
                format!("З маніфесту відкинуто записів із небезпечними іменами: {dropped}"),
            );
        }
        let count = manifest.entries.len();
        self.manifest = manifest;
        if dropped > 0 {
            self.save_manifest();
        }
        self.log(Level::Info, format!("Маніфест: копій {count}"));
    }

    fn save_manifest(&mut self) {
        if let Err(e) = self.storage.save_manifest(&self.manifest) {
            self.log(Level::Error, format!("Не вдалося зберегти маніфест: {e}"));
        }
    }

    fn load_cache(&mut self) {
        let Ok(bytes) = fs::read(self.storage.cache_path()) else {
            return;
        };
        match detectable::parse_games(&bytes) {
            Ok(list) => self.apply_game_list(list, "локальний кеш"),
            Err(e) => self.log(Level::Warn, format!("Кеш списку ігор пошкоджено: {e}")),
        }
    }

    fn start_fetch(&mut self, ctx: &egui::Context) {
        if self.fetch_rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let cache_path = self.storage.cache_path();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let result = detectable::fetch_raw().and_then(|bytes| {
                let list = detectable::parse_games(&bytes)?;
                // Кеш — лише оптимізація: помилку запису ігноруємо.
                let _ = write_atomic(&cache_path, &bytes);
                Ok(list)
            });
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        self.fetch_rx = Some(rx);
        self.log(
            Level::Info,
            format!("Завантажую список ігор: {}", detectable::ENDPOINT),
        );
    }

    fn apply_game_list(&mut self, list: GameList, source: &str) {
        let previous_id = self
            .selected_game
            .and_then(|i| self.games.get(i))
            .map(|g| g.id.clone());

        self.log(
            Level::Info,
            format!("Список ігор: {} ({source})", list.games.len()),
        );
        if list.skipped_executables > 0 {
            self.log(
                Level::Warn,
                format!(
                    "Пропущено небезпечних/некоректних імен exe: {}",
                    list.skipped_executables
                ),
            );
        }
        self.games = list.games;
        self.filter_dirty = true;
        self.selected_game = previous_id.and_then(|id| self.games.iter().position(|g| g.id == id));
        match self.selected_game {
            None => self.selected_exe = None,
            Some(i) => {
                let exes = &self.games[i].executables;
                let still_valid = self
                    .selected_exe
                    .as_ref()
                    .is_some_and(|sel| exes.iter().any(|e| &e.file_name == sel));
                if !still_valid {
                    self.selected_exe = exes.first().map(|e| e.file_name.clone());
                }
            }
        }
    }

    fn refilter(&mut self) {
        let q = self.query.trim().to_lowercase();
        self.filtered = self
            .games
            .iter()
            .enumerate()
            .filter(|(_, g)| q.is_empty() || g.search_key.contains(&q))
            .map(|(i, _)| i)
            .collect();
        self.filter_dirty = false;
    }

    fn select_game(&mut self, idx: usize) {
        self.selected_game = Some(idx);
        self.selected_exe = self.games[idx]
            .executables
            .first()
            .map(|e| e.file_name.clone());
    }

    // ───────────────────────────── фонові події ─────────────────────────────

    fn poll_background(&mut self) {
        // Завантаження списку.
        let fetched = self.fetch_rx.as_ref().map(Receiver::try_recv);
        match fetched {
            Some(Ok(Ok(list))) => {
                self.fetch_rx = None;
                self.apply_game_list(list, "Discord API");
            }
            Some(Ok(Err(e))) => {
                self.fetch_rx = None;
                self.log(Level::Error, format!("Не вдалося завантажити список: {e}"));
            }
            Some(Err(TryRecvError::Disconnected)) => {
                self.fetch_rx = None;
                self.log(Level::Error, "Потік завантаження завершився аварійно");
            }
            Some(Err(TryRecvError::Empty)) | None => {}
        }

        // Знімки tasklist: беремо найсвіжіший.
        let mut latest = None;
        while let Ok(result) = self.monitor_rx.try_recv() {
            latest = Some(result);
        }
        match latest {
            Some(Ok(snapshot)) => {
                self.snapshot = Some(snapshot);
                if self.monitor_error.take().is_some() {
                    self.log(Level::Info, "Моніторинг tasklist відновлено");
                }
            }
            Some(Err(e)) => {
                let message = e.to_string();
                if self.monitor_error.as_deref() != Some(message.as_str()) {
                    self.log(Level::Error, format!("Моніторинг недоступний: {message}"));
                    self.monitor_error = Some(message);
                }
            }
            None => {}
        }

        // Процеси, що завершилися самі.
        let finished = self.runner.reap();
        if !finished.is_empty() {
            for (key, description) in finished {
                let name = self
                    .manifest
                    .find(&key)
                    .map_or(key.clone(), |e| e.exe_name.clone());
                self.log(Level::Warn, format!("{name}: процес {description}"));
                if let Some(entry) = self.manifest.find_mut(&key) {
                    entry.last_pid = None;
                }
            }
            self.save_manifest();
        }
    }

    /// Стан копії: збіг PID *і* імені образу в знімку `tasklist` (захист від повторного
    /// використання PID іншим процесом).
    fn status_of(&self, entry: &ManifestEntry) -> Status {
        let Some(snapshot) = &self.snapshot else {
            return Status::Checking;
        };
        let key = entry.key();
        let tracked = self.runner.pid(&key);
        for pid in [tracked, entry.last_pid].into_iter().flatten() {
            if snapshot
                .get(&pid)
                .is_some_and(|image| image.eq_ignore_ascii_case(&entry.exe_name))
            {
                return Status::Running(pid);
            }
        }
        tracked.map_or(Status::Stopped, Status::Starting)
    }

    // ───────────────────────────── дії ─────────────────────────────

    fn apply(&mut self, action: Action) {
        match action {
            Action::Install {
                game_id,
                game_name,
                exe_name,
            } => self.install(game_id, game_name, exe_name),
            Action::Start(key) => self.start(&key),
            Action::Stop(key) => self.stop(&key),
            Action::Remove(key) => self.remove(&key),
        }
    }

    fn install(&mut self, game_id: String, game_name: String, exe_name: String) {
        let key = manifest_key(&exe_name);
        if let Some(entry) = self.manifest.find(&key)
            && self.status_of(entry).is_active()
        {
            self.log(
                Level::Warn,
                format!("{exe_name}: спершу зупиніть процес, потім оновлюйте копію"),
            );
            return;
        }
        match self.storage.install_copy(&exe_name) {
            Ok(info) => {
                self.manifest.upsert(ManifestEntry {
                    game_id,
                    game_name: game_name.clone(),
                    exe_name: exe_name.clone(),
                    size: info.size,
                    created_at: unix_now(),
                    last_pid: None,
                    last_started_at: None,
                });
                self.save_manifest();
                self.log(
                    Level::Info,
                    format!(
                        "Створено копію {exe_name} для «{game_name}» ({} КБ)",
                        info.size / 1024
                    ),
                );
            }
            Err(e) => self.log(
                Level::Error,
                format!("{exe_name}: не вдалося створити копію: {e}"),
            ),
        }
    }

    fn start(&mut self, key: &str) {
        let Some(entry) = self.manifest.find(key).cloned() else {
            return;
        };
        match self.status_of(&entry) {
            Status::Checking => {
                self.log(
                    Level::Warn,
                    "Зачекайте на першу перевірку tasklist і повторіть",
                );
                return;
            }
            s if s.is_active() => {
                self.log(Level::Warn, format!("{}: уже запущено", entry.exe_name));
                return;
            }
            _ => {}
        }
        let path = match self.storage.existing_copy(&entry.exe_name) {
            Ok(path) => path,
            Err(e) => {
                self.log(Level::Error, format!("{}: {e}", entry.exe_name));
                return;
            }
        };
        match self.runner.start(key, &path) {
            Ok(pid) => {
                if let Some(e) = self.manifest.find_mut(key) {
                    e.last_pid = Some(pid);
                    e.last_started_at = Some(unix_now());
                }
                self.save_manifest();
                self.log(
                    Level::Info,
                    format!(
                        "Запущено {} (PID {pid}, аргумент {WORKER_FLAG})",
                        entry.exe_name
                    ),
                );
            }
            Err(e) => self.log(
                Level::Error,
                format!("{}: не вдалося запустити: {e}", entry.exe_name),
            ),
        }
    }

    fn stop(&mut self, key: &str) {
        let Some(entry) = self.manifest.find(key).cloned() else {
            return;
        };
        let result = if self.runner.pid(key).is_some() {
            // Власний дочірній процес — зупиняємо за дескриптором.
            self.runner.kill(key)
        } else if let Status::Running(pid) = self.status_of(&entry) {
            // Процес із попередньої сесії: PID уже звірено з іменем образу в tasklist.
            process::taskkill(pid).map(|()| pid)
        } else {
            self.log(Level::Info, format!("{}: не запущено", entry.exe_name));
            return;
        };
        match result {
            Ok(pid) => {
                if let Some(snapshot) = &mut self.snapshot {
                    snapshot.remove(&pid);
                }
                if let Some(e) = self.manifest.find_mut(key) {
                    e.last_pid = None;
                }
                self.save_manifest();
                self.log(
                    Level::Info,
                    format!("Зупинено {} (PID {pid})", entry.exe_name),
                );
            }
            Err(e) => self.log(
                Level::Error,
                format!("{}: не вдалося зупинити: {e}", entry.exe_name),
            ),
        }
    }

    fn remove(&mut self, key: &str) {
        let Some(entry) = self.manifest.find(key).cloned() else {
            return;
        };
        if self.status_of(&entry).is_active() {
            self.log(
                Level::Warn,
                format!("{}: спершу зупиніть процес", entry.exe_name),
            );
            return;
        }
        match self.storage.remove_copy(&entry.exe_name) {
            Ok(()) => {
                self.manifest.remove(key);
                self.save_manifest();
                self.log(Level::Info, format!("Видалено копію {}", entry.exe_name));
            }
            Err(e) => self.log(
                Level::Error,
                format!("{}: не вдалося видалити: {e}", entry.exe_name),
            ),
        }
    }

    fn shutdown(&mut self) {
        if !self.stop_on_exit {
            return;
        }
        let keys: Vec<String> = self
            .manifest
            .entries
            .iter()
            .map(ManifestEntry::key)
            .collect();
        let mut changed = false;
        for key in keys {
            if self.runner.kill(&key).is_ok()
                && let Some(e) = self.manifest.find_mut(&key)
            {
                e.last_pid = None;
                changed = true;
            }
        }
        if changed {
            let _ = self.storage.save_manifest(&self.manifest);
        }
    }

    // ───────────────────────────── малювання ─────────────────────────────

    fn show_top(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("Game Launcher");
                ui.separator();
                ui.label("Пошук:");
                let search = ui.add(
                    egui::TextEdit::singleline(&mut self.query)
                        .hint_text("назва гри або .exe")
                        .desired_width(260.0),
                );
                if search.changed() {
                    self.filter_dirty = true;
                }
                if ui
                    .add_enabled(!self.query.is_empty(), egui::Button::new("Очистити"))
                    .clicked()
                {
                    self.query.clear();
                    self.filter_dirty = true;
                }
                ui.separator();
                let fetching = self.fetch_rx.is_some();
                if ui
                    .add_enabled(!fetching, egui::Button::new("Оновити список"))
                    .clicked()
                {
                    let ctx = ui.ctx().clone();
                    self.start_fetch(&ctx);
                }
                if fetching {
                    ui.spinner();
                }
                ui.label(RichText::new(format!("Ігор: {}", self.games.len())).weak());
            });
            ui.add_space(4.0);
        });
    }

    fn show_log(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("log")
            .resizable(true)
            .default_size(170.0)
            .min_size(80.0)
            .show(ui, |ui| {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.strong("Журнал (UTC)");
                    ui.separator();
                    ui.checkbox(
                        &mut self.stop_on_exit,
                        "Зупиняти запущені мною процеси при виході",
                    );
                    if ui.button("Очистити журнал").clicked() {
                        self.log.clear();
                    }
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .auto_shrink(false)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for line in &self.log {
                            let color = match line.level {
                                Level::Info => ui.visuals().text_color(),
                                Level::Warn => YELLOW,
                                Level::Error => RED,
                            };
                            ui.label(
                                RichText::new(format!("[{}] {}", format_hms(line.time), line.text))
                                    .monospace()
                                    .color(color),
                            );
                        }
                    });
            });
    }

    fn show_game_list(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("games")
            .resizable(true)
            .default_size(340.0)
            .min_size(220.0)
            .show(ui, |ui| {
                ui.add_space(4.0);
                ui.label(RichText::new(format!("Знайдено: {}", self.filtered.len())).weak());
                ui.separator();

                let row_height = ui.spacing().interact_size.y;
                let mut clicked = None;
                egui::ScrollArea::vertical().auto_shrink(false).show_rows(
                    ui,
                    row_height,
                    self.filtered.len(),
                    |ui, rows| {
                        ui.with_layout(egui::Layout::top_down_justified(egui::Align::LEFT), |ui| {
                            for row in rows {
                                let idx = self.filtered[row];
                                let selected = self.selected_game == Some(idx);
                                let label =
                                    egui::Button::selectable(selected, &self.games[idx].name)
                                        .truncate();
                                if ui.add(label).clicked() {
                                    clicked = Some(idx);
                                }
                            }
                        });
                    },
                );
                if let Some(idx) = clicked {
                    self.select_game(idx);
                }
            });
    }

    fn show_selected_game(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let Some(idx) = self.selected_game else {
            ui.label(RichText::new("Оберіть гру у списку ліворуч.").weak());
            return;
        };
        let game = self.games[idx].clone();

        ui.heading(&game.name);
        ui.label(
            RichText::new(format!("Discord ID: {}", game.id))
                .weak()
                .small(),
        );
        ui.add_space(6.0);
        ui.label("Виконувані файли (win32):");
        for exe in &game.executables {
            ui.horizontal(|ui| {
                let selected = self.selected_exe.as_deref() == Some(exe.file_name.as_str());
                if ui.radio(selected, &exe.file_name).clicked() {
                    self.selected_exe = Some(exe.file_name.clone());
                }
                if exe.is_launcher {
                    ui.label(RichText::new("лаунчер").weak().small());
                }
                if self.manifest.find(&manifest_key(&exe.file_name)).is_some() {
                    ui.label(RichText::new("копію створено").color(GREEN).small());
                }
            });
        }

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let selected = self.selected_exe.clone();
            let has_copy = selected
                .as_deref()
                .is_some_and(|n| self.manifest.find(&manifest_key(n)).is_some());
            let caption = if has_copy {
                "Оновити копію"
            } else {
                "Створити копію"
            };
            if ui
                .add_enabled(selected.is_some(), egui::Button::new(caption))
                .clicked()
                && let Some(exe_name) = selected
            {
                actions.push(Action::Install {
                    game_id: game.id.clone(),
                    game_name: game.name.clone(),
                    exe_name,
                });
            }
            ui.label(
                RichText::new("Копіює поточний .exe у каталог копій під обраним іменем")
                    .weak()
                    .small(),
            );
        });
    }

    fn show_copies(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.heading("Створені копії");
        if self.manifest.entries.is_empty() {
            ui.label(RichText::new("Копій ще немає.").weak());
            return;
        }
        let entries = self.manifest.entries.clone();
        egui::Grid::new("copies")
            .striped(true)
            .num_columns(5)
            .spacing([16.0, 6.0])
            .show(ui, |ui| {
                for title in ["Файл", "Гра", "Створено (UTC)", "Стан", ""] {
                    ui.strong(title);
                }
                ui.end_row();

                for entry in &entries {
                    let key = entry.key();
                    let status = self.status_of(entry);

                    let path = self.storage.games_dir().join(&entry.exe_name);
                    ui.label(&entry.exe_name)
                        .on_hover_text(path.display().to_string());
                    ui.label(&entry.game_name);
                    ui.label(format_datetime(entry.created_at));
                    match status {
                        Status::Checking => ui.label(RichText::new("перевірка…").weak()),
                        Status::Starting(pid) => {
                            ui.label(RichText::new(format!("запуск (PID {pid})")).color(YELLOW))
                        }
                        Status::Running(pid) => {
                            ui.label(RichText::new(format!("працює (PID {pid})")).color(GREEN))
                        }
                        Status::Stopped => ui.label(RichText::new("зупинено").weak()),
                    };
                    ui.horizontal(|ui| {
                        if status.is_active() {
                            if ui.button("Зупинити").clicked() {
                                actions.push(Action::Stop(key.clone()));
                            }
                        } else if ui
                            .add_enabled(status != Status::Checking, egui::Button::new("Запустити"))
                            .clicked()
                        {
                            actions.push(Action::Start(key.clone()));
                        }
                        if ui
                            .add_enabled(!status.is_active(), egui::Button::new("Видалити"))
                            .clicked()
                        {
                            actions.push(Action::Remove(key.clone()));
                        }
                    });
                    ui.end_row();
                }
            });
    }
}

impl eframe::App for App {
    fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_background();
        if self.filter_dirty {
            self.refilter();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let mut actions = Vec::new();

        self.show_top(ui);
        self.show_log(ui);
        self.show_game_list(ui);
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink(false)
                .show(ui, |ui| {
                    self.show_selected_game(ui, &mut actions);
                    ui.add_space(14.0);
                    ui.separator();
                    self.show_copies(ui, &mut actions);
                });
        });

        for action in actions {
            self.apply(action);
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.shutdown();
    }
}
