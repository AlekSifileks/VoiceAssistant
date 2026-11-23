//! Голосовой чат после фразы «имя, поболтаем».
//!
//! Команды «имя пуск» сюда не входят и в сеть не отправляются. Контракт —
//! `docs/modules/chat.md`.

use std::path::Path;
use std::time::Duration;

use rusqlite::{params, Connection};

use crate::commands::{name_forms, normalize};

const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
const MAX_TURNS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatModel {
    pub phrase: String,
    pub label: String,
    pub model_id: String,
    pub is_default: bool,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub provider: String,
    pub api_key: String,
}

impl Config {
    pub fn from_env() -> Self {
        let provider = std::env::var("CHAT_PROVIDER").unwrap_or_else(|_| "openrouter".into());
        let provider = if provider.trim().is_empty() {
            "openrouter".into()
        } else {
            provider.trim().to_ascii_lowercase()
        };
        Self {
            provider,
            api_key: std::env::var("OPENROUTER_API_KEY").unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Task {
    Chat,
    Reason,
    Tools,
}

#[derive(Debug, Default)]
pub struct Dialogue {
    active: bool,
    model_id: String,
    label: String,
    history: Vec<(String, String)>,
}

impl Dialogue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    pub fn remember(&mut self, user: &str, reply: &str) {
        self.history.push((user.to_string(), reply.to_string()));
        let extra = self.history.len().saturating_sub(MAX_TURNS);
        if extra > 0 {
            self.history.drain(0..extra);
        }
    }

    fn request(&self, name: &str, user: &str) -> Vec<Message> {
        let mut messages = vec![Message {
            role: "system".into(),
            content: system_prompt(name),
        }];
        for (earlier_user, earlier_reply) in &self.history {
            messages.push(Message {
                role: "user".into(),
                content: earlier_user.clone(),
            });
            messages.push(Message {
                role: "assistant".into(),
                content: earlier_reply.clone(),
            });
        }
        messages.push(Message {
            role: "user".into(),
            content: user.to_string(),
        });
        messages
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Turn {
    Quiet,
    Notice(String),
    Ask {
        model_id: String,
        messages: Vec<Message>,
        user: String,
    },
}

pub fn load_models(path: &Path) -> Result<Vec<ChatModel>, String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
    }
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    let existed: bool = connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'chat_models'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS chat_models (
                id INTEGER PRIMARY KEY,
                phrase TEXT NOT NULL,
                model_id TEXT NOT NULL,
                is_default INTEGER NOT NULL DEFAULT 0
            );",
        )
        .map_err(|err| err.to_string())?;
    if !existed {
        seed_models(&connection)?;
    }
    read_models(&connection)
}

/// Решает, что делать с уже распознанной фразой. Сеть не открывает.
pub fn take(
    text: &str,
    name: &str,
    wake: &str,
    models: &[ChatModel],
    dialogue: &mut Dialogue,
) -> Turn {
    let spoken = normalize(text);
    if spoken.is_empty() || name.trim().is_empty() {
        return Turn::Quiet;
    }
    if is_command(spoken.as_str(), name, wake) {
        return Turn::Quiet;
    }
    let Some(rest) = after_name(spoken.as_str(), name) else {
        return if dialogue.active {
            ask(dialogue, name, text.trim())
        } else {
            Turn::Quiet
        };
    };
    if rest == "хватит" {
        if !dialogue.active {
            return Turn::Notice("разговор не идёт".into());
        }
        *dialogue = Dialogue::new();
        return Turn::Notice("разговор закончен".into());
    }
    if rest == "поболтаем" {
        return begin(dialogue, name, models, None);
    }
    if let Some(model_name) = rest.strip_prefix("поболтаем с ") {
        return begin(dialogue, name, models, Some(model_name.trim()));
    }
    if dialogue.active {
        ask(dialogue, name, text.trim())
    } else {
        Turn::Quiet
    }
}

pub fn complete(
    config: &Config,
    task: Task,
    model_id: &str,
    messages: &[Message],
) -> Result<String, String> {
    match task {
        Task::Reason | Task::Tools => Ok("эта задача пока не подключена".into()),
        Task::Chat if config.provider != "openrouter" => Ok("эта задача пока не подключена".into()),
        Task::Chat if config.api_key.trim().is_empty() => Err("нет ключа OpenRouter".into()),
        Task::Chat => post_openrouter(config, model_id, messages),
    }
}

pub fn speaker(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        "эни".into()
    } else {
        trimmed.to_lowercase()
    }
}

fn begin(
    dialogue: &mut Dialogue,
    name: &str,
    models: &[ChatModel],
    requested: Option<&str>,
) -> Turn {
    let chosen = match requested {
        None => match models.iter().find(|model| model.is_default) {
            Some(model) => model,
            None if models.is_empty() => {
                return Turn::Notice("в базе нет моделей чата".into());
            }
            None => return Turn::Notice("нет модели по умолчанию".into()),
        },
        Some(phrase) => {
            let wanted = normalize(phrase);
            match models.iter().find(|model| model.phrase == wanted) {
                Some(model) => model,
                None => return Turn::Notice(format!("нет модели «{phrase}»")),
            }
        }
    };
    dialogue.active = true;
    dialogue.model_id = chosen.model_id.clone();
    dialogue.label = chosen.label.clone();
    dialogue.history.clear();
    let shown = name.trim();
    Turn::Notice(format!(
        "модель «{}» ({}). Можно говорить. «{shown}, хватит» заканчивает",
        chosen.label, chosen.model_id
    ))
}

fn ask(dialogue: &Dialogue, name: &str, user: &str) -> Turn {
    if user.is_empty() {
        return Turn::Quiet;
    }
    Turn::Ask {
        model_id: dialogue.model_id.clone(),
        messages: dialogue.request(name, user),
        user: user.to_string(),
    }
}

fn is_command(spoken: &str, name: &str, wake: &str) -> bool {
    let wake = normalize(wake);
    if wake.is_empty() {
        return false;
    }
    name_forms(name).into_iter().any(|form| {
        let prefix = format!("{form} {wake}");
        spoken == prefix || spoken.starts_with(&format!("{prefix} "))
    })
}

fn after_name<'a>(spoken: &'a str, name: &str) -> Option<&'a str> {
    name_forms(name).into_iter().find_map(|form| {
        if spoken == form {
            return Some("");
        }
        spoken
            .strip_prefix(&form)
            .and_then(|rest| rest.strip_prefix(' '))
            .map(str::trim)
    })
}

fn system_prompt(name: &str) -> String {
    let shown = name.trim();
    let shown = if shown.is_empty() { "Эни" } else { shown };
    format!(
        "Тебя зовут {shown}. Отвечай по-русски, коротко, одной или двумя фразами, как в устном разговоре. Программы и сайты сам не открывай."
    )
}

fn seed_models(connection: &Connection) -> Result<(), String> {
    let rows = [
        ("быстрая", "google/gemini-3.8-flash", 1),
        ("сонет", "anthropic/claude-sonnet-5.5", 0),
    ];
    for (phrase, model_id, is_default) in rows {
        connection
            .execute(
                "INSERT INTO chat_models (phrase, model_id, is_default) VALUES (?1, ?2, ?3)",
                params![phrase, model_id, is_default],
            )
            .map_err(|err| err.to_string())?;
    }
    Ok(())
}

fn read_models(connection: &Connection) -> Result<Vec<ChatModel>, String> {
    let mut statement = connection
        .prepare("SELECT phrase, model_id, is_default FROM chat_models ORDER BY id")
        .map_err(|err| err.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .map_err(|err| err.to_string())?;
    let mut models = Vec::new();
    for row in rows {
        let (label, model_id, is_default) = row.map_err(|err| err.to_string())?;
        let phrase = normalize(&label);
        if phrase.is_empty() || model_id.trim().is_empty() {
            continue;
        }
        if models.iter().any(|model: &ChatModel| model.phrase == phrase) {
            return Err(format!("повторяется модель чата «{phrase}»"));
        }
        models.push(ChatModel {
            phrase,
            label,
            model_id,
            is_default: is_default != 0,
        });
    }
    Ok(models)
}

fn post_openrouter(config: &Config, model_id: &str, messages: &[Message]) -> Result<String, String> {
    let payload = chat_request(model_id, messages);
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(45))
        .build();
    let response = agent
        .post(OPENROUTER_URL)
        .set(
            "Authorization",
            &format!("Bearer {}", config.api_key.trim()),
        )
        .set("Content-Type", "application/json")
        .set("X-Title", "Voice Assistant")
        .send_json(payload);
    match response {
        Ok(response) => {
            let body = response
                .into_string()
                .map_err(|err| format!("OpenRouter прислал не текст: {err}"))?;
            read_reply(&body)
        }
        Err(ureq::Error::Status(code, response)) => {
            let body = response.into_string().unwrap_or_default();
            Err(format!("OpenRouter {code}: {}", error_text(&body)))
        }
        Err(err) => Err(format!("нет связи с OpenRouter: {err}")),
    }
}

fn chat_request(model_id: &str, messages: &[Message]) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "model": model_id,
        "messages": messages
            .iter()
            .map(|message| serde_json::json!({
                "role": message.role,
                "content": message.content,
            }))
            .collect::<Vec<_>>(),
        // Лимит общий для скрытого рассуждения и видимой фразы.
        // 400 токенов Gemini тратила на рассуждение и обрывала слово.
        "max_tokens": 2048,
    });
    if model_id.starts_with("google/") {
        payload["reasoning"] = serde_json::json!({ "effort": "low" });
    }
    payload
}

fn read_reply(body: &str) -> Result<String, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("OpenRouter прислал не json: {err}"))?;
    let content = value
        .pointer("/choices/0/message/content")
        .and_then(|item| item.as_str())
        .map(str::trim)
        .filter(|text| !text.is_empty());
    match content {
        Some(text) => Ok(text.to_string()),
        None => Err(error_text(body)),
    }
}

fn error_text(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .and_then(|item| item.as_str())
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| {
            if body.trim().is_empty() {
                "пустой ответ".into()
            } else {
                "в ответе нет текста".into()
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn models() -> Vec<ChatModel> {
        vec![
            ChatModel {
                phrase: "быстрая".into(),
                label: "быстрая".into(),
                model_id: "google/gemini-3.8-flash".into(),
                is_default: true,
            },
            ChatModel {
                phrase: "сонет".into(),
                label: "сонет".into(),
                model_id: "anthropic/claude-sonnet-5.5".into(),
                is_default: false,
            },
        ]
    }

    fn temp_db(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "voice-assistant-chat-{name}-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn chat_starts_only_on_the_entry_phrase_and_keeps_context() {
        let models = models();
        let mut dialogue = Dialogue::new();
        assert_eq!(
            take("Энни, привет", "Энни", "пуск", &models, &mut dialogue),
            Turn::Quiet
        );
        assert!(!dialogue.is_active());
        assert!(matches!(
            take("Эни, поболтаем", "Энни", "пуск", &models, &mut dialogue),
            Turn::Notice(text) if text.contains("быстрая") && text.contains("gemini-3.8-flash")
        ));
        let first = take("как дела?", "Энни", "пуск", &models, &mut dialogue);
        let Turn::Ask { messages, user, model_id } = first else {
            panic!("ожидался запрос, {first:?}");
        };
        assert_eq!(model_id, "google/gemini-3.8-flash");
        assert_eq!(user, "как дела?");
        assert_eq!(messages.len(), 2);
        dialogue.remember(&user, "нормально");
        let second = take("а ты кто?", "Энни", "пуск", &models, &mut dialogue);
        let Turn::Ask { messages, .. } = second else {
            panic!("ожидался второй запрос");
        };
        assert_eq!(messages[1].content, "как дела?");
        assert_eq!(messages[2].content, "нормально");
        assert_eq!(messages[3].content, "а ты кто?");
    }

    #[test]
    fn named_model_lasts_one_conversation_and_stop_clears_it() {
        let models = models();
        let mut dialogue = Dialogue::new();
        take("Энни, поболтаем", "Энни", "пуск", &models, &mut dialogue);
        dialogue.remember("первый", "ответ");
        assert!(matches!(
            take(
                "Энни, поболтаем с сонет",
                "Энни",
                "пуск",
                &models,
                &mut dialogue
            ),
            Turn::Notice(text) if text.contains("сонет")
        ));
        assert!(dialogue.history.is_empty());
        assert_eq!(dialogue.model_id(), "anthropic/claude-sonnet-5.5");
        assert!(models.iter().find(|model| model.phrase == "быстрая").unwrap().is_default);
        let turn = take("привет", "Энни", "пуск", &models, &mut dialogue);
        let Turn::Ask { model_id, .. } = turn else {
            panic!("ожидался запрос");
        };
        assert_eq!(model_id, "anthropic/claude-sonnet-5.5");
        assert_eq!(
            take("Энни, хватит", "Энни", "пуск", &models, &mut dialogue),
            Turn::Notice("разговор закончен".into())
        );
        assert!(!dialogue.is_active());
        assert_eq!(
            take("привет", "Энни", "пуск", &models, &mut dialogue),
            Turn::Quiet
        );
        assert_eq!(
            take("Эни, хватит", "Энни", "пуск", &models, &mut dialogue),
            Turn::Notice("разговор не идёт".into())
        );
    }

    #[test]
    fn unknown_model_does_not_drop_the_current_chat() {
        let models = models();
        let mut dialogue = Dialogue::new();
        take("Энни, поболтаем", "Энни", "пуск", &models, &mut dialogue);
        assert_eq!(
            take(
                "Энни, поболтаем с несуществующей",
                "Энни",
                "пуск",
                &models,
                &mut dialogue
            ),
            Turn::Notice("нет модели «несуществующей»".into())
        );
        assert!(dialogue.is_active());
        assert_eq!(dialogue.model_id(), "google/gemini-3.8-flash");
    }

    #[test]
    fn command_phrase_is_not_sent_while_chat_is_open() {
        let models = models();
        let mut dialogue = Dialogue::new();
        take("Энни, поболтаем", "Энни", "пуск", &models, &mut dialogue);
        assert_eq!(
            take(
                "Энни пуск курсор",
                "Энни",
                "пуск",
                &models,
                &mut dialogue
            ),
            Turn::Quiet
        );
        assert!(dialogue.history.is_empty());
    }

    #[test]
    fn other_tasks_do_not_call_openrouter() {
        let config = Config {
            provider: "subscription".into(),
            api_key: String::new(),
        };
        assert_eq!(
            complete(&config, Task::Reason, "kimi", &[]).unwrap(),
            "эта задача пока не подключена"
        );
        assert_eq!(
            complete(&config, Task::Tools, "grok", &[]).unwrap(),
            "эта задача пока не подключена"
        );
        assert_eq!(
            complete(&config, Task::Chat, "google/gemini-3.8-flash", &[]).unwrap(),
            "эта задача пока не подключена"
        );
        let openrouter = Config {
            provider: "openrouter".into(),
            api_key: String::new(),
        };
        assert_eq!(
            complete(&openrouter, Task::Chat, "google/gemini-3.8-flash", &[]).unwrap_err(),
            "нет ключа OpenRouter"
        );
    }

    #[test]
    fn gemini_keeps_a_short_reply_inside_a_larger_token_budget() {
        let request = chat_request("google/gemini-3.8-flash", &[]);
        assert_eq!(request["max_tokens"], 2048);
        assert_eq!(request["reasoning"]["effort"], "low");
        let other = chat_request("anthropic/claude-sonnet-5.5", &[]);
        assert_eq!(other["max_tokens"], 2048);
        assert!(other.get("reasoning").is_none());
    }

    #[test]
    fn reply_text_is_read_from_the_choice() {
        assert_eq!(
            read_reply(r#"{"choices":[{"message":{"content":" привет "}}]}"#).unwrap(),
            "привет"
        );
        assert_eq!(
            error_text(r#"{"error":{"message":"нет денег"}}"#),
            "нет денег"
        );
    }

    #[test]
    fn empty_table_is_not_seeded_again() {
        let path = temp_db("seed");
        let models = load_models(&path).unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].phrase, "быстрая");
        assert!(models[0].is_default);
        assert_eq!(models[1].phrase, "сонет");
        let connection = Connection::open(&path).unwrap();
        connection.execute("DELETE FROM chat_models", []).unwrap();
        drop(connection);
        let models = load_models(&path).unwrap();
        assert!(models.is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
