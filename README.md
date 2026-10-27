# Game Launcher

Windows-утиліта (Rust, eframe/egui): завантажує список ігор з Discord Detectable API,
копіює власний `.exe` у каталог копій під іменем гри, веде JSON-маніфест і запускає копії
як фонові процеси з прапорцем `--background-worker`, моніторячи їх через `tasklist`.

## Збірка (Windows)

    cargo build --release        # потрібен Rust 1.95+
    target\release\game-launcher.exe

Залежності: лише `eframe`, `serde` (+`serde_json`) і `reqwest` (blocking + native-tls/SChannel).

## Де лежать дані

`%LOCALAPPDATA%\GameLauncher\` — `games\` (копії), `manifest.json`, `detectable_cache.json`.
Змінна середовища `GAME_LAUNCHER_HOME` перевизначає каталог.

## Розробка на Linux/macOS

    cargo test --features eframe/x11

Моніторинг через `tasklist` на інших ОС недоступний (у журналі буде повідомлення).
