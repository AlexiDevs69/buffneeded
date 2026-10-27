//! Завантаження та парсинг публічного списку Discord Detectable API.
//!
//! Нас цікавлять лише назва гри та її win32-виконувані файли. Усі імена з мережі
//! проходять [`sanitize_exe_name`], тому далі по коду вони вважаються безпечними.

use std::io::Read;
use std::time::Duration;

use serde::{Deserialize, Deserializer};

use crate::error::{AppError, Result};
use crate::storage::{manifest_key, sanitize_exe_name};

pub const ENDPOINT: &str = "https://discord.com/api/v9/applications/detectable";
/// Верхня межа розміру відповіді (реальний список — кілька мегабайт).
const MAX_BODY_BYTES: u64 = 32 * 1024 * 1024;

// ─────────────────────────────── «Сирі» структури JSON ───────────────────────────────
// Невідомі поля ігноруються; відсутні/`null` значення не ламають розбір усього списку.

#[derive(Debug, Deserialize)]
struct RawGame {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default, deserialize_with = "null_as_default")]
    executables: Vec<RawExecutable>,
}

#[derive(Debug, Deserialize)]
struct RawExecutable {
    #[serde(default)]
    name: String,
    #[serde(default)]
    os: String,
    #[serde(default)]
    is_launcher: bool,
}

fn null_as_default<'de, D, T>(de: D) -> std::result::Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(de)?.unwrap_or_default())
}

// ─────────────────────────────── Модель для застосунку ───────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct GameExe {
    /// Перевірене ім'я файлу (без шляху та службових префіксів).
    pub file_name: String,
    pub is_launcher: bool,
}

#[derive(Debug, Clone)]
pub struct Game {
    pub id: String,
    pub name: String,
    pub executables: Vec<GameExe>,
    /// Нижній регістр «назва + усі exe» — для швидкого пошуку.
    pub search_key: String,
}

#[derive(Debug, Default)]
pub struct GameList {
    pub games: Vec<Game>,
    /// Скільки win32-імен було відкинуто як небезпечні/некоректні.
    pub skipped_executables: usize,
}

/// Розбирає JSON (масив об'єктів) у список ігор, залишаючи лише win32 `.exe`.
pub fn parse_games(bytes: &[u8]) -> Result<GameList> {
    let raw: Vec<RawGame> = serde_json::from_slice(bytes)?;
    let mut list = GameList::default();

    for game in raw {
        let name = game.name.trim().to_owned();
        if name.is_empty() {
            continue;
        }
        let mut executables: Vec<GameExe> = Vec::new();
        for exe in game.executables.iter().filter(|e| e.os == "win32") {
            match sanitize_exe_name(&exe.name) {
                Ok(file_name) => {
                    let key = manifest_key(&file_name);
                    if !executables
                        .iter()
                        .any(|e| manifest_key(&e.file_name) == key)
                    {
                        executables.push(GameExe {
                            file_name,
                            is_launcher: exe.is_launcher,
                        });
                    }
                }
                Err(_) => list.skipped_executables += 1,
            }
        }
        if executables.is_empty() {
            continue;
        }
        // Спочатку «справжні» exe, лаунчери — в кінець.
        executables.sort_by_key(|e| (e.is_launcher, manifest_key(&e.file_name)));

        let mut search_key = name.to_lowercase();
        for e in &executables {
            search_key.push('\n');
            search_key.push_str(&e.file_name.to_lowercase());
        }
        list.games.push(Game {
            id: game.id,
            name,
            executables,
            search_key,
        });
    }

    list.games.sort_by_key(|g| g.name.to_lowercase());
    Ok(list)
}

/// Завантажує «сирий» JSON. Блокуючий виклик — запускати у фоновому потоці.
pub fn fetch_raw() -> Result<Vec<u8>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("game-launcher/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let response = client.get(ENDPOINT).send()?.error_for_status()?;

    let mut body = Vec::new();
    response.take(MAX_BODY_BYTES + 1).read_to_end(&mut body)?;
    if body.len() as u64 > MAX_BODY_BYTES {
        return Err(AppError::invalid("відповідь сервера завелика — відхилено"));
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"[
      {"id":"1","name":"  Alpha ","executables":[
        {"name":"alpha.exe","os":"win32","is_launcher":false},
        {"name":"Alpha.EXE","os":"win32","is_launcher":false},
        {"name":">alpha_launcher.exe","os":"win32","is_launcher":true},
        {"name":"alpha","os":"linux","is_launcher":false},
        {"name":"..\\..\\bad:name.exe","os":"win32","is_launcher":false}]},
      {"id":"2","name":"NoExe","executables":null},
      {"id":"3","name":"MacOnly","executables":[{"name":"m.app","os":"darwin"}]},
      {"id":"4","name":"beta","executables":[{"name":"bin/beta64.exe","os":"win32"}],"extra":{"x":1}}
    ]"#;

    #[test]
    fn parses_only_valid_win32_executables() {
        let list = parse_games(SAMPLE.as_bytes()).unwrap();
        let names: Vec<_> = list.games.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, ["Alpha", "beta"]);

        let alpha = &list.games[0];
        let exes: Vec<_> = alpha
            .executables
            .iter()
            .map(|e| e.file_name.as_str())
            .collect();
        assert_eq!(exes, ["alpha.exe", "alpha_launcher.exe"]); // дубль за регістром відкинуто
        assert!(alpha.executables[1].is_launcher);
        assert_eq!(list.skipped_executables, 1);
        assert_eq!(list.games[1].executables[0].file_name, "beta64.exe");
    }

    #[test]
    fn rejects_non_array_json() {
        assert!(parse_games(b"{}").is_err());
    }
}
