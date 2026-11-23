//! Команды по обращению «имя + пуск».
//!
//! Модуль не слушает микрофон и не ходит в сеть. Контракт — `docs/modules/commands.md`.

use std::path::{Path, PathBuf};
use std::process::Command;

use rusqlite::{params, Connection};

pub const DEFAULT_DATABASE: &str = "data/bookmarks.db";

#[derive(Debug, Clone)]
pub struct Settings {
    pub enabled: bool,
    pub name: String,
    pub wake: String,
    pub database: PathBuf,
}

impl Settings {
    pub fn from_env() -> Self {
        load_dotenv(Path::new(".env"));
        let enabled = env_flag("COMMANDS_ENABLED");
        let name = std::env::var("ASSISTANT_NAME").unwrap_or_default();
        let wake = std::env::var("COMMAND_WAKE").unwrap_or_default();
        let database = std::env::var("BOOKMARKS_DB")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_DATABASE));
        Self {
            enabled: enabled && !normalize(&name).is_empty() && !normalize(&wake).is_empty(),
            name,
            wake,
            database,
        }
    }

    pub fn address(&self) -> String {
        format!("{} {}", self.name.trim(), self.wake.trim())
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Request {
    Ignored,
    MissingCommand,
    Unknown(String),
    OpenApp { label: &'static str, app: &'static str },
    OpenBrowser,
    OpenBookmark(String),
}

#[derive(Debug)]
pub enum Outcome {
    Ignored,
    Done(String),
    Failed(String),
}

pub fn handle(text: &str, settings: &Settings) -> Outcome {
    if !settings.enabled {
        return Outcome::Ignored;
    }
    match interpret(text, settings) {
        Request::Ignored => Outcome::Ignored,
        Request::MissingCommand => Outcome::Failed("после обращения нет команды".into()),
        Request::Unknown(command) => Outcome::Failed(format!("нет команды «{command}»")),
        Request::OpenApp { label, app } => launch_app(label, app),
        Request::OpenBrowser => launch_browser(),
        Request::OpenBookmark(name) => open_bookmark(&settings.database, &name),
    }
}

fn interpret(text: &str, settings: &Settings) -> Request {
    let spoken = normalize(text);
    let name = normalize(&settings.name);
    let wake = normalize(&settings.wake);
    let Some(rest) = after_address(&spoken, &name, &wake) else {
        return Request::Ignored;
    };
    let mut words = rest.split_whitespace();
    let Some(command) = words.next() else {
        return Request::MissingCommand;
    };
    let argument = words.collect::<Vec<_>>().join(" ");
    match command {
        "курсор" | "cursor" => Request::OpenApp {
            label: "Cursor",
            app: "Cursor",
        },
        "телеграм" | "телеграмм" | "telegram" => Request::OpenApp {
            label: "Telegram",
            app: "Telegram",
        },
        "браузер" | "browser" if argument.is_empty() => Request::OpenBrowser,
        "браузер" | "browser" => Request::OpenBookmark(argument),
        other => Request::Unknown(other.to_string()),
    }
}

fn after_address<'a>(spoken: &'a str, name: &str, wake: &str) -> Option<&'a str> {
    let prefix = format!("{name} {wake}");
    if spoken == prefix {
        return Some("");
    }
    spoken
        .strip_prefix(&format!("{prefix} "))
        .map(str::trim_start)
}

fn launch_app(label: &str, app: &str) -> Outcome {
    match Command::new("open").args(["-a", app]).status() {
        Ok(status) if status.success() => Outcome::Done(format!("открыт {label}")),
        Ok(status) => Outcome::Failed(format!("не удалось открыть {label}: код {status}")),
        Err(err) => Outcome::Failed(format!("не удалось открыть {label}: {err}")),
    }
}

fn launch_browser() -> Outcome {
    match Command::new("open").arg("about:blank").status() {
        Ok(status) if status.success() => Outcome::Done("открыт браузер".into()),
        Ok(status) => Outcome::Failed(format!("не удалось открыть браузер: код {status}")),
        Err(err) => Outcome::Failed(format!("не удалось открыть браузер: {err}")),
    }
}

fn open_bookmark(database: &Path, query: &str) -> Outcome {
    match find_bookmark(database, query) {
        Ok(Some(url)) if is_http_url(&url) => match Command::new("open").arg(&url).status() {
            Ok(status) if status.success() => Outcome::Done(format!("открыт {url}")),
            Ok(status) => Outcome::Failed(format!("не удалось открыть {url}: код {status}")),
            Err(err) => Outcome::Failed(format!("не удалось открыть {url}: {err}")),
        },
        Ok(Some(url)) => Outcome::Failed(format!("в закладке не http-адрес: {url}")),
        Ok(None) => Outcome::Failed(format!("нет закладки «{query}»")),
        Err(err) => Outcome::Failed(err),
    }
}

fn find_bookmark(database: &Path, query: &str) -> Result<Option<String>, String> {
    let wanted = normalize(query);
    if wanted.is_empty() {
        return Ok(None);
    }
    let connection = open_database(database)?;
    let mut statement = connection
        .prepare("SELECT name, url FROM bookmarks")
        .map_err(|err| err.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|err| err.to_string())?;
    for row in rows {
        let (name, url) = row.map_err(|err| err.to_string())?;
        if normalize(&name) == wanted {
            return Ok(Some(url));
        }
    }
    Ok(None)
}

fn open_database(path: &Path) -> Result<Connection, String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
    }
    let fresh = !path.exists();
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    connection
        .execute(
            "CREATE TABLE IF NOT EXISTS bookmarks (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                url TEXT NOT NULL
            )",
            [],
        )
        .map_err(|err| err.to_string())?;
    if fresh {
        connection
            .execute(
                "INSERT INTO bookmarks (name, url) VALUES (?1, ?2)",
                params!["Тимбенч", "https://teambench.ru/"],
            )
            .map_err(|err| err.to_string())?;
    }
    Ok(connection)
}

fn is_http_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.starts_with("https://") || lower.starts_with("http://")
}

fn normalize(text: &str) -> String {
    let mut out = String::new();
    let mut spaced = false;
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            for lower in ch.to_lowercase() {
                out.push(lower);
            }
            spaced = false;
        } else if ch.is_whitespace() && !out.is_empty() && !spaced {
            out.push(' ');
            spaced = true;
        }
    }
    out.trim().to_string()
}

fn env_flag(key: &str) -> bool {
    matches!(
        std::env::var(key)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn load_dotenv(path: &Path) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || std::env::var(key).is_ok() {
            continue;
        }
        let value = value.trim().trim_matches('"').trim_matches('\'');
        std::env::set_var(key, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings {
            enabled: true,
            name: "Эни".into(),
            wake: "пуск".into(),
            database: PathBuf::from("data/bookmarks.db"),
        }
    }

    #[test]
    fn address_selects_cursor_and_ignores_plain_speech() {
        let settings = settings();
        assert!(matches!(
            interpret("Эни, пуск курсор.", &settings),
            Request::OpenApp { app: "Cursor", .. }
        ));
        assert_eq!(interpret("открой курсор", &settings), Request::Ignored);
        assert_eq!(
            interpret("пуск курсор", &settings),
            Request::Ignored
        );
        assert_eq!(interpret("Эни пуск", &settings), Request::MissingCommand);
    }

    #[test]
    fn browser_argument_is_the_bookmark_name() {
        let settings = settings();
        assert_eq!(interpret("эни пуск браузер", &settings), Request::OpenBrowser);
        assert_eq!(
            interpret("Эни пуск браузер Тимбенч", &settings),
            Request::OpenBookmark("тимбенч".into())
        );
        assert!(matches!(
            interpret("эни пуск телеграмм", &settings),
            Request::OpenApp { app: "Telegram", .. }
        ));
    }

    #[test]
    fn fresh_database_finds_teambench_once() {
        let path = std::env::temp_dir().join(format!(
            "voice-assistant-bookmarks-{}-{}.db",
            std::process::id(),
            normalize("тимбенч")
        ));
        let _ = std::fs::remove_file(&path);
        let url = find_bookmark(&path, "Тимбенч").unwrap();
        assert_eq!(url.as_deref(), Some("https://teambench.ru/"));
        let connection = Connection::open(&path).unwrap();
        connection.execute("DELETE FROM bookmarks", []).unwrap();
        drop(connection);
        assert_eq!(find_bookmark(&path, "Тимбенч").unwrap(), None);
        let _ = std::fs::remove_file(&path);
    }
}
