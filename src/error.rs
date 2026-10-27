//! Єдиний тип помилки застосунку (без сторонніх крейтів на кшталт `thiserror`).

use std::{fmt, io};

#[derive(Debug)]
pub enum AppError {
    Io(io::Error),
    Http(reqwest::Error),
    Json(serde_json::Error),
    /// Некоректні/небезпечні дані або порушення інваріанта (людиночитне повідомлення).
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, AppError>;

impl AppError {
    pub fn invalid(msg: impl Into<String>) -> Self {
        Self::Invalid(msg.into())
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "помилка файлової системи/процесу: {e}"),
            Self::Http(e) => write!(f, "мережева помилка: {e}"),
            Self::Json(e) => write!(f, "помилка JSON: {e}"),
            Self::Invalid(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for AppError {}

impl From<io::Error> for AppError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<reqwest::Error> for AppError {
    fn from(e: reqwest::Error) -> Self {
        Self::Http(e)
    }
}
impl From<serde_json::Error> for AppError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}
