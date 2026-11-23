//! Команды по обращению «имя + пуск».
//!
//! Слова команд читаются из `data/assistant.db`. Модуль не слушает микрофон
//! и не ходит в сеть. Контракт — `docs/modules/commands.md`.

use std::path::{Path, PathBuf};
use std::process::Command;

use rusqlite::{params, Connection};

pub const DEFAULT_DATABASE: &str = "data/assistant.db";
const LEGACY_BOOKMARKS: &str = "data/bookmarks.db";

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
        let database = std::env::var("ASSISTANT_DB")
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

#[derive(Debug, Clone)]
struct StoredCommand {
    phrase: String,
    action: String,
    target: String,
    takes_argument: bool,
}

#[derive(Debug, Clone)]
pub struct Catalog {
    commands: Vec<StoredCommand>,
}

impl Catalog {
    pub fn empty() -> Self {
        Self {
            commands: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.commands.len()
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Request {
    Ignored,
    MissingCommand,
    Unknown(String),
    Unsupported { phrase: String, action: String },
    OpenApp { label: String, app: String },
    OpenBrowser,
    OpenBookmark(String),
}

#[derive(Debug)]
pub enum Outcome {
    Ignored,
    Done(String),
    Failed(String),
}

/// Создаёт базу, если её ещё нет, и один раз читает таблицу команд.
pub fn load(settings: &Settings) -> Result<Catalog, String> {
    let connection = prepare_database(&settings.database, Some(Path::new(LEGACY_BOOKMARKS)))?;
    load_commands(&connection)
}

pub fn handle(text: &str, settings: &Settings, catalog: &Catalog) -> Outcome {
    if !settings.enabled {
        return Outcome::Ignored;
    }
    match interpret(text, settings, catalog) {
        Request::Ignored => Outcome::Ignored,
        Request::MissingCommand => Outcome::Failed("после обращения нет команды".into()),
        Request::Unknown(command) => Outcome::Failed(format!("нет команды «{command}»")),
        Request::Unsupported { phrase, action } => {
            Outcome::Failed(format!("команда «{phrase}» не поддержана: {action}"))
        }
        Request::OpenApp { label, app } => launch_app(&label, &app),
        Request::OpenBrowser => launch_browser(),
        Request::OpenBookmark(name) => open_bookmark(&settings.database, &name),
    }
}

fn interpret(text: &str, settings: &Settings, catalog: &Catalog) -> Request {
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
    let Some(stored) = catalog.commands.iter().find(|item| item.phrase == command) else {
        return Request::Unknown(command.to_string());
    };
    match stored.action.as_str() {
        "open_app" if stored.target.is_empty() => Request::Unsupported {
            phrase: stored.phrase.clone(),
            action: stored.action.clone(),
        },
        "open_app" => Request::OpenApp {
            label: stored.target.clone(),
            app: stored.target.clone(),
        },
        "open_browser" if stored.takes_argument && !argument.is_empty() => {
            Request::OpenBookmark(argument)
        }
        "open_browser" => Request::OpenBrowser,
        _ => Request::Unsupported {
            phrase: stored.phrase.clone(),
            action: stored.action.clone(),
        },
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
    let connection = Connection::open(database).map_err(|err| err.to_string())?;
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

fn load_commands(connection: &Connection) -> Result<Catalog, String> {
    let mut statement = connection
        .prepare("SELECT phrase, action, target, takes_argument FROM commands")
        .map_err(|err| err.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .map_err(|err| err.to_string())?;
    let mut commands = Vec::new();
    for row in rows {
        let (phrase, action, target, takes_argument) = row.map_err(|err| err.to_string())?;
        let phrase = normalize(&phrase);
        if phrase.is_empty() {
            continue;
        }
        if commands.iter().any(|item: &StoredCommand| item.phrase == phrase) {
            return Err(format!("повторяется команда «{phrase}»"));
        }
        commands.push(StoredCommand {
            phrase,
            action,
            target: target.unwrap_or_default(),
            takes_argument: takes_argument != 0,
        });
    }
    Ok(Catalog { commands })
}

fn prepare_database(path: &Path, legacy: Option<&Path>) -> Result<Connection, String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
    }
    let fresh = !path.exists();
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS commands (
                id INTEGER PRIMARY KEY,
                phrase TEXT NOT NULL,
                action TEXT NOT NULL,
                target TEXT,
                takes_argument INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS bookmarks (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                url TEXT NOT NULL
            );",
        )
        .map_err(|err| err.to_string())?;
    if fresh {
        seed_commands(&connection)?;
        if let Some(legacy) = legacy {
            import_legacy_bookmarks(&connection, legacy)?;
        }
        seed_teambench_if_missing(&connection)?;
    }
    Ok(connection)
}

fn seed_commands(connection: &Connection) -> Result<(), String> {
    let rows = [
        ("курсор", "open_app", Some("Cursor"), 0),
        ("cursor", "open_app", Some("Cursor"), 0),
        ("телеграм", "open_app", Some("Telegram"), 0),
        ("телеграмм", "open_app", Some("Telegram"), 0),
        ("telegram", "open_app", Some("Telegram"), 0),
        ("браузер", "open_browser", None, 1),
        ("browser", "open_browser", None, 1),
    ];
    for (phrase, action, target, takes_argument) in rows {
        connection
            .execute(
                "INSERT INTO commands (phrase, action, target, takes_argument)
                 VALUES (?1, ?2, ?3, ?4)",
                params![phrase, action, target, takes_argument],
            )
            .map_err(|err| err.to_string())?;
    }
    Ok(())
}

fn import_legacy_bookmarks(connection: &Connection, legacy: &Path) -> Result<(), String> {
    if !legacy.is_file() {
        return Ok(());
    }
    let source = Connection::open(legacy).map_err(|err| err.to_string())?;
    let has_table: bool = source
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'bookmarks'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if !has_table {
        return Ok(());
    }
    let mut statement = source
        .prepare("SELECT name, url FROM bookmarks")
        .map_err(|err| err.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|err| err.to_string())?;
    for row in rows {
        let (name, url) = row.map_err(|err| err.to_string())?;
        connection
            .execute(
                "INSERT INTO bookmarks (name, url) VALUES (?1, ?2)",
                params![name, url],
            )
            .map_err(|err| err.to_string())?;
    }
    Ok(())
}

fn seed_teambench_if_missing(connection: &Connection) -> Result<(), String> {
    let mut statement = connection
        .prepare("SELECT name FROM bookmarks")
        .map_err(|err| err.to_string())?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|err| err.to_string())?;
    for row in rows {
        if normalize(&row.map_err(|err| err.to_string())?) == "тимбенч" {
            return Ok(());
        }
    }
    connection
        .execute(
            "INSERT INTO bookmarks (name, url) VALUES (?1, ?2)",
            params!["Тимбенч", "https://teambench.ru/"],
        )
        .map_err(|err| err.to_string())?;
    Ok(())
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

    fn settings(database: PathBuf) -> Settings {
        Settings {
            enabled: true,
            name: "Эни".into(),
            wake: "пуск".into(),
            database,
        }
    }

    fn fresh_catalog(name: &str) -> (Settings, Catalog) {
        let path = std::env::temp_dir().join(format!(
            "voice-assistant-{name}-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        let settings = settings(path);
        let connection = prepare_database(&settings.database, None).unwrap();
        let catalog = load_commands(&connection).unwrap();
        (settings, catalog)
    }

    #[test]
    fn address_selects_cursor_and_ignores_plain_speech() {
        let (settings, catalog) = fresh_catalog("cursor");
        assert!(matches!(
            interpret("Эни, пуск курсор.", &settings, &catalog),
            Request::OpenApp { app, .. } if app == "Cursor"
        ));
        assert_eq!(
            interpret("открой курсор", &settings, &catalog),
            Request::Ignored
        );
        assert_eq!(
            interpret("пуск курсор", &settings, &catalog),
            Request::Ignored
        );
        assert_eq!(
            interpret("Эни пуск", &settings, &catalog),
            Request::MissingCommand
        );
        assert!(matches!(
            interpret("Эни пуск курсор пожалуйста", &settings, &catalog),
            Request::OpenApp { app, .. } if app == "Cursor"
        ));
        let _ = std::fs::remove_file(&settings.database);
    }

    #[test]
    fn browser_argument_is_the_bookmark_name() {
        let (settings, catalog) = fresh_catalog("browser");
        assert_eq!(
            interpret("эни пуск браузер", &settings, &catalog),
            Request::OpenBrowser
        );
        assert_eq!(
            interpret("Эни пуск браузер Тимбенч", &settings, &catalog),
            Request::OpenBookmark("тимбенч".into())
        );
        assert!(matches!(
            interpret("эни пуск телеграмм", &settings, &catalog),
            Request::OpenApp { app, .. } if app == "Telegram"
        ));
        assert_eq!(
            find_bookmark(&settings.database, "Тимбенч").unwrap().as_deref(),
            Some("https://teambench.ru/")
        );
        let _ = std::fs::remove_file(&settings.database);
    }

    #[test]
    fn deleting_a_phrase_removes_only_that_phrase() {
        let (settings, _) = fresh_catalog("delete");
        let connection = Connection::open(&settings.database).unwrap();
        connection
            .execute("DELETE FROM commands WHERE phrase = 'курсор'", [])
            .unwrap();
        let catalog = load_commands(&connection).unwrap();
        drop(connection);
        assert_eq!(
            interpret("Эни пуск курсор", &settings, &catalog),
            Request::Unknown("курсор".into())
        );
        assert!(matches!(
            interpret("Эни пуск cursor", &settings, &catalog),
            Request::OpenApp { app, .. } if app == "Cursor"
        ));
        let _ = std::fs::remove_file(&settings.database);
    }

    #[test]
    fn existing_database_is_not_seeded_again() {
        let (settings, catalog) = fresh_catalog("once");
        assert_eq!(catalog.len(), 7);
        let connection = Connection::open(&settings.database).unwrap();
        connection.execute("DELETE FROM commands", []).unwrap();
        connection.execute("DELETE FROM bookmarks", []).unwrap();
        drop(connection);
        let connection = prepare_database(&settings.database, None).unwrap();
        let catalog = load_commands(&connection).unwrap();
        assert_eq!(catalog.len(), 0);
        assert_eq!(find_bookmark(&settings.database, "Тимбенч").unwrap(), None);
        let _ = std::fs::remove_file(&settings.database);
    }

    #[test]
    fn legacy_bookmarks_are_copied_into_a_new_database() {
        let legacy = std::env::temp_dir().join(format!(
            "voice-assistant-legacy-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&legacy);
        let source = Connection::open(&legacy).unwrap();
        source
            .execute(
                "CREATE TABLE bookmarks (
                    id INTEGER PRIMARY KEY,
                    name TEXT NOT NULL,
                    url TEXT NOT NULL
                )",
                [],
            )
            .unwrap();
        source
            .execute(
                "INSERT INTO bookmarks (name, url) VALUES ('Заметки', 'https://notes.example')",
                [],
            )
            .unwrap();
        drop(source);

        let path = std::env::temp_dir().join(format!(
            "voice-assistant-import-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        prepare_database(&path, Some(&legacy)).unwrap();
        assert_eq!(
            find_bookmark(&path, "Заметки").unwrap().as_deref(),
            Some("https://notes.example")
        );
        assert_eq!(
            find_bookmark(&path, "Тимбенч").unwrap().as_deref(),
            Some("https://teambench.ru/")
        );
        let _ = std::fs::remove_file(&legacy);
        let _ = std::fs::remove_file(&path);
    }
}
