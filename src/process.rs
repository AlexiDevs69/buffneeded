//! Керування процесами: запуск копій у фоні, моніторинг через `tasklist`, зупинка.
//!
//! Системні утиліти викликаються за абсолютним шляхом (`%SystemRoot%\System32\…`),
//! щоб підміна `PATH`/поточного каталогу не могла підсунути чужий `tasklist.exe`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use crate::WORKER_FLAG;
use crate::error::{AppError, Result};

/// PID → ім'я образу (`Image Name` з tasklist).
pub type Snapshot = HashMap<u32, String>;

// ───────────────────────────────── Режим «воркера» ─────────────────────────────────

/// Точка входу для копії, запущеної з [`WORKER_FLAG`]: нічого не робить і майже не їсть ресурсів.
pub fn run_worker() -> ! {
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}

// ───────────────────────────────── Допоміжні виклики ─────────────────────────────────

/// Без вікна консолі (інакше кожен виклик tasklist мигав би чорним вікном).
fn configure_background(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.stdin(Stdio::null())
}

fn system_tool(name: &str) -> PathBuf {
    if cfg!(windows) {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        Path::new(&root).join("System32").join(name)
    } else {
        PathBuf::from(name)
    }
}

// ───────────────────────────────────── tasklist ─────────────────────────────────────

/// Один виклик `tasklist /FO CSV /NH` → знімок усіх процесів.
pub fn snapshot() -> Result<Snapshot> {
    let mut cmd = Command::new(system_tool("tasklist.exe"));
    cmd.args(["/FO", "CSV", "/NH"])
        .stderr(Stdio::piped())
        .stdout(Stdio::piped());
    configure_background(&mut cmd);

    let output = cmd.output()?;
    if !output.status.success() {
        return Err(AppError::invalid(format!(
            "tasklist завершився з {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    // Кодування виводу — OEM-кодова сторінка; PID і ASCII-імена від цього не залежать.
    Ok(parse_tasklist_csv(&String::from_utf8_lossy(&output.stdout)))
}

/// Рядок виду: `"game.exe","1234","Console","1","12 345 K"`.
fn parse_tasklist_csv(text: &str) -> Snapshot {
    text.lines()
        .filter_map(|line| {
            let inner = line.trim().strip_prefix('"')?.strip_suffix('"')?;
            let mut fields = inner.split("\",\"");
            let image = fields.next()?;
            let pid = fields.next()?.parse::<u32>().ok()?;
            Some((pid, image.to_owned()))
        })
        .collect()
}

/// Фоновий потік, що кожні `interval` присилає свіжий знімок. Завершується, коли
/// приймач відкинуто. `wake` — щоб розбудити UI (наприклад, `ctx.request_repaint()`).
pub fn spawn_monitor(
    interval: Duration,
    wake: impl Fn() + Send + 'static,
) -> Receiver<Result<Snapshot>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        loop {
            if tx.send(snapshot()).is_err() {
                break;
            }
            wake();
            thread::sleep(interval);
        }
    });
    rx
}

/// Примусово завершує процес за PID (для копій, запущених у попередній сесії).
/// Без `/T`: дерево дочірніх процесів не чіпаємо.
pub fn taskkill(pid: u32) -> Result<()> {
    let mut cmd = Command::new(system_tool("taskkill.exe"));
    cmd.args(["/PID", &pid.to_string(), "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    configure_background(&mut cmd);
    let output = cmd.output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(AppError::invalid(format!(
            "taskkill: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

// ───────────────────────────────── Власні дочірні процеси ─────────────────────────────────

/// Реєстр процесів, запущених цим екземпляром застосунку (ключ — `manifest_key`).
#[derive(Default)]
pub struct Runner {
    children: HashMap<String, Child>,
}

impl Runner {
    pub fn pid(&self, key: &str) -> Option<u32> {
        self.children.get(key).map(Child::id)
    }

    /// Запускає копію як фоновий процес: без вікна, без stdio, із прапорцем [`WORKER_FLAG`],
    /// робочий каталог — каталог копії.
    pub fn start(&mut self, key: &str, exe: &Path) -> Result<u32> {
        if self.children.contains_key(key) {
            return Err(AppError::invalid("процес уже запущено"));
        }
        let mut cmd = Command::new(exe);
        cmd.arg(WORKER_FLAG)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(dir) = exe.parent() {
            cmd.current_dir(dir);
        }
        configure_background(&mut cmd);

        let child = cmd.spawn()?;
        let pid = child.id();
        self.children.insert(key.to_owned(), child);
        Ok(pid)
    }

    /// Зупиняє власний дочірній процес за дескриптором (без жодних зовнішніх команд).
    pub fn kill(&mut self, key: &str) -> Result<u32> {
        let mut child = self
            .children
            .remove(key)
            .ok_or_else(|| AppError::invalid("цей екземпляр не запускав такого процесу"))?;
        let pid = child.id();
        child.kill()?;
        child.wait()?;
        Ok(pid)
    }

    /// Прибирає завершені процеси; повертає `(ключ, опис)` для журналу.
    pub fn reap(&mut self) -> Vec<(String, String)> {
        let mut finished = Vec::new();
        self.children.retain(|key, child| match child.try_wait() {
            Ok(None) => true,
            Ok(Some(status)) => {
                finished.push((key.clone(), format!("завершився ({status})")));
                false
            }
            Err(e) => {
                finished.push((key.clone(), format!("не вдалося опитати стан: {e}")));
                false
            }
        });
        finished
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tasklist_csv() {
        let text = "\"System Idle Process\",\"0\",\"Services\",\"0\",\"8 K\"\r\n\
                    \"game one.exe\",\"4242\",\"Console\",\"1\",\"12 345 K\"\r\n\
                    garbage line\r\n\
                    \"bad.exe\",\"notapid\",\"Console\",\"1\",\"1 K\"\r\n";
        let snap = parse_tasklist_csv(text);
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[&4242], "game one.exe");
        assert_eq!(snap[&0], "System Idle Process");
    }
}
