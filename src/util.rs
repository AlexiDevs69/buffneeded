//! Дрібні допоміжні функції часу (у std немає форматування дат, а chrono тягти не хочемо).
//! Увесь час відображається в UTC.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// "ГГ:ХХ:СС" (UTC) — для рядків журналу.
pub fn format_hms(unix_secs: u64) -> String {
    let s = unix_secs % 86_400;
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// "РРРР-ММ-ДД ГГ:ХХ" (UTC) — для маніфесту в UI.
pub fn format_datetime(unix_secs: u64) -> String {
    let (y, m, d) = civil_from_days((unix_secs / 86_400) as i64);
    let s = unix_secs % 86_400;
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        s / 3600,
        (s % 3600) / 60
    )
}

/// Алгоритм Говарда Гіннанта: кількість днів від 1970-01-01 → (рік, місяць, день).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_known_timestamps() {
        assert_eq!(format_datetime(0), "1970-01-01 00:00");
        // 2024-02-29 12:34:56 UTC
        assert_eq!(format_datetime(1_709_210_096), "2024-02-29 12:34");
        assert_eq!(format_hms(1_709_210_096), "12:34:56");
    }
}
