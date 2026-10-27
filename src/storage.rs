//! Робота з файловою системою: каталоги даних, безпечне копіювання власного
//! бінарника, JSON-маніфест.
//!
//! Принципи безпеки:
//! * імена `.exe` приходять із мережі, тому вважаються недовіреними — вони проходять
//!   [`sanitize_exe_name`] і *щоразу* перевіряються заново (в т.ч. значення з маніфесту);
//! * шлях до копії ніколи не зберігається як «істина» — він завжди збирається як
//!   `games_dir.join(<перевірене ім'я>)`;
//! * запис — атомарний (тимчасовий файл → `rename`), без перезапису символічних посилань;
//! * копія ніколи не затирає працюючий бінарник.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{AppError, Result};
use crate::util::unix_now;

const MAX_NAME_CHARS: usize = 100;
const APP_DIR_NAME: &str = "GameLauncher";

// ───────────────────────── Санітизація імені виконуваного файлу ─────────────────────────

/// Перетворює «сире» ім'я з Discord API на безпечне ім'я файлу для Windows або повертає помилку.
///
/// * префікс `>` — службова мітка в даних Discord, не частина імені файлу;
/// * якщо ім'я містить підкаталоги (`bin/win64/game.exe`), береться лише останній сегмент;
/// * дозволені лише імена `*.exe` без заборонених символів, керуючих символів
///   та зарезервованих імен пристроїв (`CON`, `NUL`, `COM1`, …).
pub fn sanitize_exe_name(raw: &str) -> Result<String> {
    let trimmed = raw.trim().trim_start_matches('>');
    let name = trimmed.rsplit(['/', '\\']).next().unwrap_or("").trim();

    if name.is_empty() {
        return Err(AppError::invalid("порожнє ім'я файлу"));
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(AppError::invalid(format!(
            "ім'я файлу довше за {MAX_NAME_CHARS} символів"
        )));
    }
    if let Some(bad) = name.chars().find(|c| {
        c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
    }) {
        return Err(AppError::invalid(format!(
            "ім'я «{name}» містить заборонений символ {bad:?}"
        )));
    }

    let lower = name.to_lowercase();
    let Some(stem_len) = lower.strip_suffix(".exe").map(str::len) else {
        return Err(AppError::invalid(format!("«{name}» не є .exe файлом")));
    };
    // `to_lowercase` може змінити довжину в байтах, тому стем беремо за кількістю символів.
    let stem: String = name
        .chars()
        .take(lower[..stem_len].chars().count())
        .collect();

    if stem.is_empty() || stem.ends_with([' ', '.']) {
        return Err(AppError::invalid(format!("некоректне ім'я «{name}»")));
    }
    if is_reserved_device_name(&stem) {
        return Err(AppError::invalid(format!(
            "«{name}» — зарезервоване ім'я пристрою Windows"
        )));
    }
    Ok(name.to_owned())
}

fn is_reserved_device_name(stem: &str) -> bool {
    // Windows ігнорує все після першої крапки та кінцеві пробіли: `con.txt` теж зарезервоване.
    let base = stem
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    match base.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" => true,
        _ => {
            (base.starts_with("COM") || base.starts_with("LPT"))
                && base.len() == 4
                && base.as_bytes()[3].is_ascii_digit()
        }
    }
}

/// Ключ запису в маніфесті: Windows нечутлива до регістру, тож `Game.EXE` ≡ `game.exe`.
pub fn manifest_key(exe_name: &str) -> String {
    exe_name.to_lowercase()
}

// ───────────────────────────────────── Маніфест ─────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub game_id: String,
    pub game_name: String,
    /// Ім'я файлу копії (вже перевірене). Повний шлях виводиться з нього, а не зберігається.
    pub exe_name: String,
    pub size: u64,
    pub created_at: u64,
    #[serde(default)]
    pub last_pid: Option<u32>,
    #[serde(default)]
    pub last_started_at: Option<u64>,
}

impl ManifestEntry {
    pub fn key(&self) -> String {
        manifest_key(&self.exe_name)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub entries: Vec<ManifestEntry>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            version: 1,
            entries: Vec::new(),
        }
    }
}

impl Manifest {
    pub fn find(&self, key: &str) -> Option<&ManifestEntry> {
        self.entries.iter().find(|e| e.key() == key)
    }

    pub fn find_mut(&mut self, key: &str) -> Option<&mut ManifestEntry> {
        self.entries.iter_mut().find(|e| e.key() == key)
    }

    /// Додає запис або оновлює наявний (зберігаючи `last_pid`).
    pub fn upsert(&mut self, new: ManifestEntry) {
        let key = new.key();
        match self.find_mut(&key) {
            Some(existing) => {
                existing.game_id = new.game_id;
                existing.game_name = new.game_name;
                existing.exe_name = new.exe_name;
                existing.size = new.size;
                existing.created_at = new.created_at;
            }
            None => self.entries.push(new),
        }
        self.entries.sort_by_key(|e| e.key());
    }

    pub fn remove(&mut self, key: &str) {
        self.entries.retain(|e| e.key() != key);
    }

    /// Відкидає записи з небезпечними іменами (маніфест — звичайний файл, його можуть змінити).
    /// Повертає кількість відкинутих записів.
    pub fn drop_invalid(&mut self) -> usize {
        let before = self.entries.len();
        self.entries
            .retain(|e| sanitize_exe_name(&e.exe_name).is_ok_and(|n| n == e.exe_name));
        before - self.entries.len()
    }
}

// ───────────────────────────────────── Сховище ─────────────────────────────────────

pub struct CopyInfo {
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct Storage {
    root: PathBuf,
    games_dir: PathBuf,
}

impl Storage {
    /// Каталог даних: `$GAME_LAUNCHER_HOME`, інакше `%LOCALAPPDATA%\GameLauncher`,
    /// інакше `<каталог exe>/data`. Копії лежать у підкаталозі `games`.
    pub fn init() -> Result<Self> {
        let root = if let Some(custom) = std::env::var_os("GAME_LAUNCHER_HOME") {
            PathBuf::from(custom)
        } else if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            PathBuf::from(local).join(APP_DIR_NAME)
        } else {
            let exe = std::env::current_exe()?;
            exe.parent().unwrap_or(Path::new(".")).join("data")
        };
        let games_dir = root.join("games");
        fs::create_dir_all(&games_dir)?;
        Ok(Self { root, games_dir })
    }

    pub fn games_dir(&self) -> &Path {
        &self.games_dir
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.root.join("manifest.json")
    }

    pub fn cache_path(&self) -> PathBuf {
        self.root.join("detectable_cache.json")
    }

    // ── шляхи копій ──

    /// Шлях до копії для вже *перевіреного* імені. Єдине місце, де збирається шлях.
    pub fn copy_path(&self, exe_name: &str) -> Result<PathBuf> {
        let name = sanitize_exe_name(exe_name)?;
        let path = self.games_dir.join(&name);
        // Страховка: після join батьківський каталог обов'язково має лишитися games_dir.
        if path.parent() != Some(self.games_dir.as_path()) {
            return Err(AppError::invalid("шлях виходить за межі каталогу games"));
        }
        Ok(path)
    }

    /// Шлях до копії, яка існує й є звичайним файлом (не посиланням і не каталогом).
    pub fn existing_copy(&self, exe_name: &str) -> Result<PathBuf> {
        let path = self.copy_path(exe_name)?;
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_file() => Ok(path),
            Ok(_) => Err(AppError::invalid(format!(
                "«{}» не є звичайним файлом — відмова",
                path.display()
            ))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                Err(AppError::invalid("копію не знайдено — створіть її заново"))
            }
            Err(e) => Err(e.into()),
        }
    }

    // ── копіювання ──

    /// Копіює поточний бінарник у `games/<exe_name>`.
    ///
    /// Кроки: перевірка цілі → запис у `.<name>.tmp` (`create_new`, тобто без слідування
    /// за наявними файлами/посиланнями) → `sync_all` → звірка розміру → атомарний `rename`.
    pub fn install_copy(&self, exe_name: &str) -> Result<CopyInfo> {
        let dest = self.copy_path(exe_name)?;
        let src = std::env::current_exe()?;

        match fs::symlink_metadata(&dest) {
            Ok(meta) => {
                if !meta.file_type().is_file() {
                    return Err(AppError::invalid(format!(
                        "ціль «{}» існує і не є звичайним файлом",
                        dest.display()
                    )));
                }
                if same_file(&src, &dest) {
                    return Err(AppError::invalid(
                        "не можна перезаписати бінарник, що зараз виконується",
                    ));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }

        let tmp = self
            .games_dir
            .join(format!(".{}.tmp", manifest_key(exe_name)));
        // Залишок від збою: remove_file на символічному посиланні видаляє саме посилання.
        let _ = fs::remove_file(&tmp);

        let result = (|| -> Result<u64> {
            let mut input = File::open(&src)?;
            let expected = input.metadata()?.len();
            let mut out = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
            // Лише для розробки на unix: зберегти біт виконання. На Windows не потрібно
            // (і копіювання атрибута «лише читання» тільки заважало б оновленню копії).
            #[cfg(unix)]
            out.set_permissions(input.metadata()?.permissions())?;
            let copied = io::copy(&mut input, &mut out)?;
            out.flush()?;
            out.sync_all()?;
            if copied != expected {
                return Err(AppError::invalid(format!(
                    "розмір копії ({copied}) не збігається з оригіналом ({expected})"
                )));
            }
            Ok(copied)
        })();

        let size = match result {
            Ok(size) => size,
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
        };

        if let Err(e) = fs::rename(&tmp, &dest) {
            let _ = fs::remove_file(&tmp);
            return Err(AppError::invalid(format!(
                "не вдалося замінити «{}»: {e} (можливо, процес ще запущено)",
                dest.display()
            )));
        }
        Ok(CopyInfo { size })
    }

    /// Видаляє копію. Відсутній файл — не помилка.
    pub fn remove_copy(&self, exe_name: &str) -> Result<()> {
        let path = self.copy_path(exe_name)?;
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_file() => fs::remove_file(&path).map_err(Into::into),
            Ok(_) => Err(AppError::invalid(format!(
                "«{}» не є звичайним файлом — не видаляю",
                path.display()
            ))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    // ── маніфест ──

    pub fn load_manifest(&self) -> Result<Manifest> {
        match fs::read(self.manifest_path()) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Manifest::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save_manifest(&self, manifest: &Manifest) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(manifest)?;
        write_atomic(&self.manifest_path(), &bytes)
    }

    /// Відкладає пошкоджений маніфест убік, щоб наступне збереження його не затерло.
    pub fn quarantine_manifest(&self) -> Result<PathBuf> {
        let target = self
            .root
            .join(format!("manifest.corrupt-{}.json", unix_now()));
        fs::rename(self.manifest_path(), &target)?;
        Ok(target)
    }
}

/// Атомарний запис: тимчасовий файл поруч + `rename`.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    let _ = fs::remove_file(&tmp);

    let write = || -> io::Result<()> {
        let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    };
    write().map_err(|e| {
        let _ = fs::remove_file(&tmp);
        e.into()
    })
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_regular_names() {
        assert_eq!(sanitize_exe_name("Game.exe").unwrap(), "Game.exe");
        assert_eq!(sanitize_exe_name(">hl2.exe").unwrap(), "hl2.exe");
        assert_eq!(
            sanitize_exe_name("bin/win64/Game Client.EXE").unwrap(),
            "Game Client.EXE"
        );
        assert_eq!(sanitize_exe_name("гра.exe").unwrap(), "гра.exe");
    }

    #[test]
    fn rejects_dangerous_names() {
        for bad in [
            "",
            ">",
            "game",
            "game.dll",
            ".exe",
            "a:b.exe",
            "a?.exe",
            "a*.exe",
            "a\"b.exe",
            "con.exe",
            "NUL.exe",
            "com1.exe",
            "LPT9.exe",
            "aux.txt.exe",
            "name .exe",
            "name..exe",
            "evil\n.exe",
            "../",
            "dir/",
            "x/..",
            "game.exe.",
        ] {
            assert!(sanitize_exe_name(bad).is_err(), "має відхилятися: {bad:?}");
        }
        assert!(sanitize_exe_name(&format!("{}.exe", "a".repeat(200))).is_err());
    }

    #[test]
    fn traversal_collapses_to_file_name() {
        // Сегменти шляху відкидаються, тож вийти за межі games/ неможливо.
        assert_eq!(
            sanitize_exe_name("..\\..\\Windows\\evil.exe").unwrap(),
            "evil.exe"
        );
        assert_eq!(sanitize_exe_name("/etc/x.exe").unwrap(), "x.exe");
    }

    #[test]
    fn manifest_drops_tampered_entries() {
        let entry = |name: &str| ManifestEntry {
            game_id: "1".into(),
            game_name: "G".into(),
            exe_name: name.into(),
            size: 1,
            created_at: 0,
            last_pid: None,
            last_started_at: None,
        };
        let mut m = Manifest {
            entries: vec![entry("ok.exe"), entry("..\\evil.exe"), entry("con.exe")],
            ..Manifest::default()
        };
        assert_eq!(m.drop_invalid(), 2);
        assert_eq!(m.entries.len(), 1);
    }

    #[test]
    fn install_copy_is_atomic_and_confined() {
        let dir = std::env::temp_dir().join(format!("gl-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let games = dir.join("games");
        fs::create_dir_all(&games).unwrap();
        let st = Storage {
            root: dir.clone(),
            games_dir: games.clone(),
        };

        let info = st.install_copy("Test Game.exe").unwrap();
        assert!(info.size > 0);
        assert!(games.join("Test Game.exe").is_file());
        // Повторне копіювання (оновлення) працює, тимчасових файлів не лишається.
        st.install_copy("Test Game.exe").unwrap();
        assert!(
            fs::read_dir(&games).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp"))
        );

        assert!(st.install_copy("..\\..\\x.exe").is_ok()); // зводиться до x.exe всередині games
        assert!(games.join("x.exe").is_file());
        assert!(st.copy_path("con.exe").is_err());

        st.remove_copy("Test Game.exe").unwrap();
        assert!(st.existing_copy("Test Game.exe").is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
