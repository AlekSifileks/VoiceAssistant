//! Команды по имени. Слово «пуск» можно сказать, можно пропустить.
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
    Unclear,
    Maybe(String),
}

#[derive(Debug)]
pub enum Outcome {
    Ignored,
    Done(String),
    Failed(String),
    /// После «имя пуск» слово не совпало с командой. Его можно отдать модели.
    Unparsed { rest: String },
    /// После имени без «пуск» точной команды нет. Модель может подтвердить, иначе фраза уходит в чат.
    Maybe { rest: String },
}

/// Создаёт базу, если её ещё нет, и один раз читает таблицу команд.
pub fn load(settings: &Settings) -> Result<Catalog, String> {
    let connection = prepare_database(&settings.database, Some(Path::new(LEGACY_BOOKMARKS)))?;
    load_commands(&connection)
}

pub fn is_addressed(text: &str, settings: &Settings) -> bool {
    let spoken = normalize(text);
    classify(&spoken, &settings.name, &settings.wake).is_some()
}

pub fn handle(text: &str, settings: &Settings, catalog: &Catalog) -> Outcome {
    if !settings.enabled {
        return Outcome::Ignored;
    }
    match interpret(text, settings, catalog) {
        Request::Unknown(rest) => Outcome::Unparsed { rest },
        Request::Maybe(rest) => Outcome::Maybe { rest },
        request => execute(request, settings),
    }
}

fn execute(request: Request, settings: &Settings) -> Outcome {
    match request {
        Request::Ignored => Outcome::Ignored,
        Request::MissingCommand => Outcome::Failed("после обращения нет команды".into()),
        Request::Unknown(command) => Outcome::Failed(format!("нет команды «{command}»")),
        Request::Unclear | Request::Maybe(_) => Outcome::Failed("не понял".into()),
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
    let Some(address) = classify(&spoken, &name, &wake) else {
        return Request::Ignored;
    };
    let (rest, optional) = match address {
        Address::Wake("") => return Request::MissingCommand,
        Address::Name("") => return Request::Ignored,
        Address::Name(rest) if is_chat_control(rest) => return Request::Ignored,
        Address::Wake(rest) => (rest, false),
        Address::Name(rest) => (rest, true),
    };
    let body = strip_leading_verb(rest);
    if body.is_empty() {
        return if optional {
            Request::Ignored
        } else {
            Request::MissingCommand
        };
    }
    if let Some(request) = exact_command(body, catalog) {
        return request;
    }
    if let Ok(Some(_)) = find_bookmark(&settings.database, body) {
        return Request::OpenBookmark(body.to_string());
    }
    if let Some(request) = heard_request(body, settings, catalog) {
        return request;
    }
    if optional {
        Request::Maybe(body.to_string())
    } else {
        Request::Unknown(body.to_string())
    }
}

fn heard_request(body: &str, settings: &Settings, catalog: &Catalog) -> Option<Request> {
    let (rest, saw_browser) = without_fillers(body);
    let names = bookmark_names(&settings.database).unwrap_or_default();
    let bookmark = nearest_sound(&rest, &names);
    let phrases: Vec<String> = catalog.commands.iter().map(|item| item.phrase.clone()).collect();
    let command = nearest_sound(&rest, &phrases);
    match (bookmark, command) {
        (Some((name, bookmark_distance)), Some((_, command_distance)))
            if bookmark_distance < command_distance
                || (saw_browser && bookmark_distance == command_distance) =>
        {
            Some(Request::OpenBookmark(name))
        }
        (_, Some((phrase, _))) => exact_command(&phrase, catalog),
        (Some((name, _)), None) => Some(Request::OpenBookmark(name)),
        (None, None) if saw_browser && rest.is_empty() => Some(Request::OpenBrowser),
        _ => None,
    }
}

fn without_fillers(body: &str) -> (String, bool) {
    const FILLER: &[&str] = &[
        "в",
        "во",
        "на",
        "через",
        "мне",
        "пожалуйста",
        "браузере",
        "браузера",
        "браузером",
        "сайте",
        "сайт",
        "закладке",
        "закладку",
        "закладка",
        "закладки",
    ];
    let mut saw_browser = false;
    let mut words = Vec::new();
    for word in body.split_whitespace() {
        if FILLER.contains(&word) {
            if word.starts_with("брауз") || word.starts_with("сайт") || word.starts_with("заклад") {
                saw_browser = true;
            }
            continue;
        }
        words.push(word);
    }
    (words.join(" "), saw_browser)
}

fn nearest_sound(query: &str, names: &[String]) -> Option<(String, usize)> {
    let wanted = sound_key(query);
    if wanted.len() < 4 {
        return None;
    }
    let mut best: Option<(String, usize)> = None;
    for name in names {
        let key = sound_key(name);
        if key.len() < 4 {
            continue;
        }
        let distance = edit_distance(&wanted, &key);
        let limit = if wanted.len().min(key.len()) >= 5 { 1 } else { 0 };
        if distance > limit {
            continue;
        }
        match &best {
            None => best = Some((name.clone(), distance)),
            Some((_, best_distance)) if distance < *best_distance => {
                best = Some((name.clone(), distance));
            }
            Some((_, best_distance)) if distance > *best_distance => {}
            Some((best_name, _)) if sound_key(best_name) != key => return None,
            Some(_) => {}
        }
    }
    best
}

fn sound_key(text: &str) -> String {
    let folded = latin_to_cyrillic(&normalize(text));
    folded
        .chars()
        .filter(|ch| {
            matches!(
                ch,
                'б' | 'в'
                    | 'г'
                    | 'д'
                    | 'ж'
                    | 'з'
                    | 'й'
                    | 'к'
                    | 'л'
                    | 'м'
                    | 'н'
                    | 'п'
                    | 'р'
                    | 'с'
                    | 'т'
                    | 'ф'
                    | 'х'
                    | 'ц'
                    | 'ч'
                    | 'ш'
                    | 'щ'
            )
        })
        .collect()
}

fn latin_to_cyrillic(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut index = 0;
    while index < chars.len() {
        let rest: String = chars[index..].iter().collect();
        let digraphs = [
            ("sch", "щ"),
            ("sh", "ш"),
            ("ch", "ч"),
            ("zh", "ж"),
            ("ts", "ц"),
            ("kh", "х"),
            ("ya", "я"),
            ("yu", "ю"),
            ("yo", "ё"),
        ];
        if let Some((latin, cyrillic)) = digraphs.iter().find(|(latin, _)| rest.starts_with(latin))
        {
            out.push_str(cyrillic);
            index += latin.chars().count();
            continue;
        }
        let mapped = match chars[index] {
            'a' => "а",
            'b' => "б",
            'c' => "к",
            'd' => "д",
            'e' => "е",
            'f' => "ф",
            'g' => "г",
            'h' => "х",
            'i' => "и",
            'j' => "й",
            'k' => "к",
            'l' => "л",
            'm' => "м",
            'n' => "н",
            'o' => "о",
            'p' => "п",
            'q' => "к",
            'r' => "р",
            's' => "с",
            't' => "т",
            'u' => "у",
            'v' => "в",
            'w' => "в",
            'x' => "кс",
            'y' => "и",
            'z' => "з",
            other => {
                out.push(other);
                index += 1;
                continue;
            }
        };
        out.push_str(mapped);
        index += 1;
    }
    out
}

fn edit_distance(left: &str, right: &str) -> usize {
    let left: Vec<char> = left.chars().collect();
    let right: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0; right.len() + 1];
    for (i, left_char) in left.iter().enumerate() {
        current[0] = i + 1;
        for (j, right_char) in right.iter().enumerate() {
            let cost = if left_char == right_char { 0 } else { 1 };
            current[j + 1] = (previous[j + 1] + 1).min(current[j] + 1).min(previous[j] + cost);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

fn bookmark_names(database: &Path) -> Result<Vec<String>, String> {
    let connection = Connection::open(database).map_err(|err| err.to_string())?;
    let mut statement = connection
        .prepare("SELECT name FROM bookmarks")
        .map_err(|err| err.to_string())?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|err| err.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())
}

fn is_chat_control(rest: &str) -> bool {
    rest == "хватит" || rest == "поболтаем" || rest.starts_with("поболтаем с ")
}

const INTENT_MIN_SCORE: f32 = 0.93;
const INTENT_MIN_MARGIN: f32 = 0.08;

pub struct Example {
    pub phrase: String,
    pub text: String,
}

pub struct IntentScore {
    pub label: String,
    pub score: f32,
}

/// Подписи, с которыми модель сравнивает хвост фразы.
pub fn examples(catalog: &Catalog) -> Vec<Example> {
    const EXTRA: &[(&str, &str)] = &[("курсор", "редактор")];
    let mut out = Vec::new();
    for command in &catalog.commands {
        out.push(Example {
            phrase: command.phrase.clone(),
            text: command.phrase.clone(),
        });
    }
    for (phrase, text) in EXTRA {
        if catalog.commands.iter().any(|item| item.phrase == *phrase)
            && !out.iter().any(|item| item.text == *text)
        {
            out.push(Example {
                phrase: (*phrase).into(),
                text: (*text).into(),
            });
        }
    }
    out
}

/// Выбирает команду по оценкам модели и выполняет её, если похожесть достаточна.
pub fn apply_intent(
    rest: &str,
    settings: &Settings,
    catalog: &Catalog,
    scores: &[IntentScore],
) -> Outcome {
    match decide_intent(rest, catalog, scores) {
        Request::Unclear
        | Request::Unknown(_)
        | Request::Maybe(_)
        | Request::Ignored
        | Request::MissingCommand => {
            Outcome::Failed("не понял".into())
        }
        request => execute(request, settings),
    }
}

fn decide_intent(rest: &str, catalog: &Catalog, scores: &[IntentScore]) -> Request {
    let Some(phrase) = select_phrase(catalog, scores) else {
        return Request::Unclear;
    };
    let Some(stored) = catalog.commands.iter().find(|item| item.phrase == phrase) else {
        return Request::Unclear;
    };
    let example = scores
        .iter()
        .max_by(|left, right| left.score.total_cmp(&right.score))
        .map(|item| item.label.as_str())
        .unwrap_or("");
    stored_request(stored, &leftover(rest, example))
}

fn select_phrase(catalog: &Catalog, scores: &[IntentScore]) -> Option<String> {
    let examples = examples(catalog);
    let mut best: Vec<(String, f32)> = Vec::new();
    for score in scores {
        let Some(example) = examples.iter().find(|item| item.text == score.label) else {
            continue;
        };
        if let Some(slot) = best.iter_mut().find(|(phrase, _)| phrase == &example.phrase) {
            slot.1 = slot.1.max(score.score);
        } else {
            best.push((example.phrase.clone(), score.score));
        }
    }
    best.sort_by(|left, right| right.1.total_cmp(&left.1));
    let (phrase, score) = best.first()?;
    if *score < INTENT_MIN_SCORE {
        return None;
    }
    if let Some((_, other)) = best.get(1) {
        if score - other < INTENT_MIN_MARGIN {
            return None;
        }
    }
    Some(phrase.clone())
}

fn leftover(rest: &str, example: &str) -> String {
    let used: std::collections::HashSet<&str> = example.split_whitespace().collect();
    rest.split_whitespace()
        .filter(|word| !used.contains(word))
        .collect::<Vec<_>>()
        .join(" ")
}

fn strip_leading_verb(rest: &str) -> &str {
    const LEADING: &[&str] = &[
        "открой",
        "запусти",
        "включи",
        "покажи",
        "открыть",
        "запустить",
        "включить",
        "open",
    ];
    let Some(first) = rest.split_whitespace().next() else {
        return rest;
    };
    if LEADING.contains(&first) {
        rest.split_once(' ')
            .map(|(_, tail)| tail.trim())
            .unwrap_or("")
    } else {
        rest
    }
}

fn exact_command(body: &str, catalog: &Catalog) -> Option<Request> {
    let mut words = body.split_whitespace();
    let command = words.next()?;
    let argument = words.collect::<Vec<_>>().join(" ");
    let stored = catalog
        .commands
        .iter()
        .find(|item| item.phrase == command)?;
    Some(stored_request(stored, &argument))
}

fn stored_request(stored: &StoredCommand, argument: &str) -> Request {
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
            Request::OpenBookmark(argument.to_string())
        }
        "open_browser" => Request::OpenBrowser,
        _ => Request::Unsupported {
            phrase: stored.phrase.clone(),
            action: stored.action.clone(),
        },
    }
}

enum Address<'a> {
    Wake(&'a str),
    Name(&'a str),
}

fn classify<'a>(spoken: &'a str, name: &str, wake: &str) -> Option<Address<'a>> {
    let wake = normalize(wake);
    name_forms(name).into_iter().find_map(|form| {
        if !wake.is_empty() {
            let prefix = format!("{form} {wake}");
            if spoken == prefix {
                return Some(Address::Wake(""));
            }
            if let Some(rest) = spoken
                .strip_prefix(&format!("{prefix} "))
                .map(str::trim_start)
            {
                return Some(Address::Wake(rest));
            }
        }
        if spoken == form {
            return Some(Address::Name(""));
        }
        spoken
            .strip_prefix(&format!("{form} "))
            .map(str::trim_start)
            .map(Address::Name)
    })
}

pub(crate) fn name_forms(name: &str) -> Vec<String> {
    let name = normalize(name);
    if name == "эни" || name == "энни" {
        vec!["эни".into(), "энни".into()]
    } else if name.is_empty() {
        Vec::new()
    } else {
        vec![name]
    }
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

pub(crate) fn tune_connection(connection: &Connection) -> Result<(), String> {
    connection
        .busy_timeout(std::time::Duration::from_secs(3))
        .map_err(|err| err.to_string())?;
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .map_err(|err| err.to_string())?;
    Ok(())
}

fn prepare_database(path: &Path, legacy: Option<&Path>) -> Result<Connection, String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
    }
    let fresh = !path.exists();
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    tune_connection(&connection)?;
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

pub(crate) fn normalize(text: &str) -> String {
    let mut out = String::new();
    let mut spaced = false;
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            for lower in ch.to_lowercase() {
                out.push(lower);
            }
            spaced = false;
        } else if (ch.is_whitespace() || ch == '-' || ch == '—' || ch == '–')
            && !out.is_empty()
            && !spaced
        {
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
        assert!(matches!(
            interpret("Энни пуск курсор", &settings, &catalog),
            Request::OpenApp { app, .. } if app == "Cursor"
        ));
        assert!(matches!(
            interpret("Эни пуск открой курсор", &settings, &catalog),
            Request::OpenApp { app, .. } if app == "Cursor"
        ));
        assert!(matches!(
            interpret("Эни пуск запусти редактор", &settings, &catalog),
            Request::Unknown(rest) if rest == "редактор"
        ));
        assert!(matches!(
            decide_intent(
                "редактор",
                &catalog,
                &[
                    IntentScore { label: "редактор".into(), score: 1.0 },
                    IntentScore { label: "телеграм".into(), score: 0.84 },
                ],
            ),
            Request::OpenApp { app, .. } if app == "Cursor"
        ));
        assert!(matches!(
            decide_intent(
                "напиши маме",
                &catalog,
                &[
                    IntentScore { label: "телеграм".into(), score: 0.71 },
                    IntentScore { label: "курсор".into(), score: 0.67 },
                ],
            ),
            Request::Unclear
        ));
        assert!(matches!(
            interpret("Эни пуск открой тимбенч", &settings, &catalog),
            Request::OpenBookmark(name) if name == "тимбенч"
        ));
        assert!(matches!(
            interpret("Эни пуск-курсор.", &settings, &catalog),
            Request::OpenApp { app, .. } if app == "Cursor"
        ));
        assert!(is_addressed("Энни, пуск курсор", &settings));
        assert!(!is_addressed("просто речь", &settings));
        assert!(matches!(
            interpret("Эни открой курсор", &settings, &catalog),
            Request::OpenApp { app, .. } if app == "Cursor"
        ));
        assert!(matches!(
            interpret("Эни курсор", &settings, &catalog),
            Request::OpenApp { app, .. } if app == "Cursor"
        ));
        assert!(matches!(
            interpret("Эни поболтаем", &settings, &catalog),
            Request::Ignored
        ));
        assert!(matches!(
            interpret("Эни, хватит", &settings, &catalog),
            Request::Ignored
        ));
        assert!(matches!(
            interpret("Эни, как дела", &settings, &catalog),
            Request::Maybe(rest) if rest == "как дела"
        ));
        for heard in ["team banch", "tim banch", "team bach"] {
            assert!(
                matches!(
                    interpret(&format!("Эни, открой в браузере {heard}"), &settings, &catalog),
                    Request::OpenBookmark(name) if name == "Тимбенч"
                ),
                "{heard}"
            );
        }
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
        let mode: String = connection
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
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
