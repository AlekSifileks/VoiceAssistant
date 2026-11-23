//! Задачи, бэклог, время и комментарии Тимбенча. Контракт — `docs/modules/teambench.md`.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::Value;

use crate::commands;

const API: &str = "https://teambench.ru/api";
const OPEN_STATUSES: &[&str] = &[
    "todo",
    "analysis",
    "development",
    "review",
    "testing",
    "production",
    "blocked",
];

pub struct Desk {
    token: String,
    agent: ureq::Agent,
    projects: Vec<Project>,
    projects_loaded: bool,
    last: Option<IssueRef>,
}

struct Project {
    id: u64,
    key: String,
    name: String,
}

struct IssueRef {
    id: u64,
    key: String,
}

struct Issue {
    id: u64,
    key: String,
    title: String,
    status: String,
    project: String,
    assignee: String,
    estimate: Option<u32>,
    spent: u32,
    description: String,
}

pub struct Answer {
    pub terminal: String,
    pub aloud: String,
    pub ready: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum Phrase {
    List { status: Option<&'static str> },
    Backlog { project: String },
    Card { task: Mention },
    Comments { task: Mention },
    Timesheet { range: Range },
    Spent { task: Mention },
    Log { minutes: u32, when: When, task: Mention, comment: String },
    Comment { task: Mention, body: String },
    Later,
    Reject(String),
}

#[derive(Debug, PartialEq, Eq)]
enum Mention {
    Last,
    Key { letters: String, number: u32 },
    Title(String),
}

#[derive(Debug, PartialEq, Eq)]
enum Range {
    Today,
    ThisWeek,
    LastWeek,
}

#[derive(Debug, PartialEq, Eq)]
enum When {
    Today,
    Yesterday,
    DayBefore,
}

impl Desk {
    pub fn from_env() -> Self {
        let token = std::env::var("TEAMBENCH_API_KEY").unwrap_or_default();
        Self {
            token: token.trim().to_string(),
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(20))
                .build(),
            projects: Vec::new(),
            projects_loaded: false,
            last: None,
        }
    }

    pub fn enabled(&self) -> bool {
        !self.token.is_empty()
    }

    /// `None` — это не фраза Тимбенча. Иначе ответ или ошибка, уже без записи при неясной задаче.
    pub fn perform(&mut self, rest: &str) -> Option<Result<Answer, String>> {
        let phrase = parse(rest)?;
        if self.token.is_empty() {
            return Some(Err("нет ключа тимбенча".into()));
        }
        Some(self.perform_phrase(phrase))
    }

    fn perform_phrase(&mut self, phrase: Phrase) -> Result<Answer, String> {
        match phrase {
            Phrase::Later => Ok(Answer {
                terminal: "так ещё не умею".into(),
                aloud: "Так ещё не умею".into(),
                ready: false,
            }),
            Phrase::Reject(message) => Err(message),
            Phrase::List { status } => self.list(status),
            Phrase::Backlog { project } => self.backlog(&project),
            Phrase::Card { task } => self.card(task),
            Phrase::Comments { task } => self.comments(task),
            Phrase::Timesheet { range } => self.timesheet(range),
            Phrase::Spent { task } => self.spent(task),
            Phrase::Log {
                minutes,
                when,
                task,
                comment,
            } => self.log(minutes, when, task, comment),
            Phrase::Comment { task, body } => self.comment(task, body),
        }
    }

    fn list(&mut self, status: Option<&str>) -> Result<Answer, String> {
        let statuses: Vec<&str> = match status {
            Some(status) => vec![status],
            None => OPEN_STATUSES.to_vec(),
        };
        let mut found = BTreeMap::new();
        let mut truncated = false;
        for status in statuses {
            let query = format!("assignee:me status:{status}");
            let batch = self.search(&query)?;
            if batch.len() >= 20 {
                truncated = true;
            }
            for issue in batch {
                found.insert(issue.id, issue);
            }
        }
        let mut issues: Vec<_> = found.into_values().collect();
        issues.sort_by(|left, right| left.key.cmp(&right.key));
        if issues.len() == 1 {
            self.remember(&issues[0]);
        }
        let title = match status {
            Some(status) => format!("Задач в статусе «{}»", status_ru(status)),
            None => "Открытых задач".into(),
        };
        Ok(list_answer(&title, &issues, truncated))
    }

    fn backlog(&mut self, project: &str) -> Result<Answer, String> {
        if project.is_empty() {
            return Err("нужно имя проекта".into());
        }
        let project = self.project(project)?.clone_ref();
        let payload = self.get(&format!(
            "/projects/{}/issues?backlog_only=1&per_page=20&page=1",
            project.id
        ))?;
        let mut issues: Vec<_> = data_array(&payload)
            .iter()
            .filter_map(parse_issue)
            .collect();
        issues.sort_by(|left, right| left.key.cmp(&right.key));
        let total = payload.get("total").and_then(|value| value.as_u64()).unwrap_or(issues.len() as u64);
        let truncated = total > issues.len() as u64;
        if issues.len() == 1 {
            self.remember(&issues[0]);
        }
        Ok(list_answer(
            &format!("В бэклоге {}", project.name),
            &issues,
            truncated,
        ))
    }

    fn card(&mut self, task: Mention) -> Result<Answer, String> {
        let issue = self.resolve(task)?;
        let payload = self.get(&format!("/issues/{}", issue.id))?;
        let full = parse_issue(&unwrap_data(&payload)).unwrap_or(issue);
        self.remember(&full);
        let mut lines = vec![format!(
            "{} · {} · {}",
            full.key,
            status_ru(&full.status),
            full.title
        )];
        if !full.assignee.is_empty() {
            lines.push(format!("исполнитель: {}", full.assignee));
        }
        lines.push(format!(
            "оценка: {}, факт: {}",
            full.estimate.map(minutes_words).unwrap_or_else(|| "нет".into()),
            minutes_words(full.spent)
        ));
        let description = full.description.trim();
        if !description.is_empty() {
            lines.push(clip(description, 400).to_string());
        }
        let aloud = format!(
            "{}, {}. {}",
            full.key,
            status_ru(&full.status),
            clip(&full.title, 80)
        );
        Ok(Answer {
            terminal: lines.join("\n"),
            aloud,
            ready: false,
        })
    }

    fn comments(&mut self, task: Mention) -> Result<Answer, String> {
        let issue = self.resolve(task)?;
        let payload = self.get(&format!("/issues/{}/comments", issue.id))?;
        self.remember(&issue);
        let notes: Vec<_> = data_array(&payload);
        if notes.is_empty() {
            return Ok(Answer {
                terminal: format!("В {} комментариев нет", issue.key),
                aloud: format!("В {} комментариев нет", issue.key),
                ready: false,
            });
        }
        let mut lines = vec![format!("Комментарии {} ({})", issue.key, notes.len())];
        for note in &notes {
            let author = note
                .pointer("/user/name")
                .and_then(|value| value.as_str())
                .unwrap_or("кто-то");
            let body = note.get("body").and_then(|value| value.as_str()).unwrap_or("");
            lines.push(format!("{author}: {body}"));
        }
        let last = notes.last().unwrap();
        let last_body = last.get("body").and_then(|value| value.as_str()).unwrap_or("");
        let aloud = if last_body.chars().count() <= 120 {
            format!("В {} последний комментарий: {}", issue.key, last_body)
        } else {
            format!("В {} {} комментариев", issue.key, notes.len())
        };
        Ok(Answer {
            terminal: lines.join("\n"),
            aloud,
            ready: false,
        })
    }

    fn timesheet(&mut self, range: Range) -> Result<Answer, String> {
        let today = local_today()?;
        let (from, to) = match range {
            Range::Today => (today, today),
            Range::ThisWeek => week_range(today, false),
            Range::LastWeek => week_range(today, true),
        };
        let payload = self.get(&format!(
            "/timesheet?from={}&to={}",
            from.iso(),
            to.iso()
        ))?;
        let body = unwrap_data(&payload);
        let planned = body.get("planned_minutes").and_then(|value| value.as_u64()).unwrap_or(0) as u32;
        let spent = body.get("spent_minutes").and_then(|value| value.as_u64()).unwrap_or(0) as u32;
        let mut lines = vec![format!(
            "Табель {} — {}: план {}, факт {}",
            from.iso(),
            to.iso(),
            minutes_words(planned),
            minutes_words(spent)
        )];
        if let Some(days) = body.get("days").and_then(|value| value.as_array()) {
            for day in days {
                let date = day.get("date").and_then(|value| value.as_str()).unwrap_or("");
                let day_spent = day.get("spent_minutes").and_then(|value| value.as_u64()).unwrap_or(0);
                if day_spent == 0 {
                    continue;
                }
                let mut bits = Vec::new();
                if let Some(entries) = day.get("entries").and_then(|value| value.as_array()) {
                    for entry in entries {
                        let minutes = entry.get("minutes").and_then(|value| value.as_u64()).unwrap_or(0);
                        let key = entry.pointer("/issue/key").and_then(|value| value.as_str()).unwrap_or("без задачи");
                        bits.push(format!("{key} {}", minutes_words(minutes as u32)));
                    }
                }
                lines.push(format!("{date}: {}", bits.join(", ")));
            }
        }
        Ok(Answer {
            terminal: lines.join("\n"),
            aloud: format!(
                "План {}, факт {}",
                minutes_words(planned),
                minutes_words(spent)
            ),
            ready: false,
        })
    }

    fn spent(&mut self, task: Mention) -> Result<Answer, String> {
        let issue = self.resolve(task)?;
        let payload = self.get(&format!("/issues/{}/time-entries", issue.id))?;
        self.remember(&issue);
        let unwrapped = unwrap_data(&payload);
        let total = payload
            .get("time_spent_minutes")
            .or_else(|| unwrapped.get("time_spent_minutes"))
            .and_then(|value| value.as_u64())
            .unwrap_or(0) as u32;
        let mut lines = vec![format!("На {} списано {}", issue.key, minutes_words(total))];
        for entry in data_array(&payload) {
            let minutes = entry.get("minutes").and_then(|value| value.as_u64()).unwrap_or(0) as u32;
            let day = entry.get("spent_on").and_then(|value| value.as_str()).unwrap_or("");
            let comment = entry.get("comment").and_then(|value| value.as_str()).unwrap_or("");
            if comment.is_empty() {
                lines.push(format!("{day}: {}", minutes_words(minutes)));
            } else {
                lines.push(format!("{day}: {} — {comment}", minutes_words(minutes)));
            }
        }
        Ok(Answer {
            terminal: lines.join("\n"),
            aloud: format!("На {} списано {}", issue.key, minutes_words(total)),
            ready: false,
        })
    }

    fn log(&mut self, minutes: u32, when: When, task: Mention, comment: String) -> Result<Answer, String> {
        if minutes == 0 {
            return Err("не разобрал сколько списать".into());
        }
        let issue = self.resolve(task)?;
        let day = shift_date(local_today()?, when)?.iso();
        let mut body = serde_json::json!({
            "minutes": minutes,
            "spent_on": day,
        });
        if !comment.is_empty() {
            body["comment"] = Value::String(comment);
        }
        self.post(&format!("/issues/{}/time-entries", issue.id), body)?;
        self.remember(&issue);
        Ok(Answer {
            terminal: format!(
                "списано {} на {} за {day}",
                minutes_words(minutes),
                issue.key
            ),
            aloud: format!("{} на {}", minutes_words(minutes), issue.key),
            ready: true,
        })
    }

    fn comment(&mut self, task: Mention, body: String) -> Result<Answer, String> {
        let body = body.trim().to_string();
        if body.is_empty() {
            return Err("нет текста комментария".into());
        }
        let issue = self.resolve(task)?;
        self.post(
            &format!("/issues/{}/comments", issue.id),
            serde_json::json!({ "body": body }),
        )?;
        self.remember(&issue);
        Ok(Answer {
            terminal: format!("комментарий в {}: {body}", issue.key),
            aloud: format!("Комментарий в {}", issue.key),
            ready: true,
        })
    }

    fn resolve(&mut self, task: Mention) -> Result<Issue, String> {
        match task {
            Mention::Last => {
                let last = self.last.as_ref().ok_or("нужен ключ задачи")?;
                Ok(Issue {
                    id: last.id,
                    key: last.key.clone(),
                    title: String::new(),
                    status: String::new(),
                    project: String::new(),
                    assignee: String::new(),
                    estimate: None,
                    spent: 0,
                    description: String::new(),
                })
            }
            Mention::Key { letters, number } => {
                let project = self.project_by_letters(&letters)?;
                let key = format!("{}-{number}", project.key);
                let mut hits: Vec<_> = self
                    .search(&key)?
                    .into_iter()
                    .filter(|issue| issue.key.eq_ignore_ascii_case(&key))
                    .collect();
                match hits.len() {
                    0 => Err(format!("не нашёл {key}")),
                    1 => Ok(hits.remove(0)),
                    _ => Err(ambiguous(&hits)),
                }
            }
            Mention::Title(title) => {
                let hits = self.search(&title)?;
                match hits.len() {
                    0 => Err(format!("не нашёл «{title}»")),
                    1 => Ok(hits.into_iter().next().unwrap()),
                    _ => Err(ambiguous(&hits)),
                }
            }
        }
    }

    fn remember(&mut self, issue: &Issue) {
        self.last = Some(IssueRef {
            id: issue.id,
            key: issue.key.clone(),
        });
    }

    fn project(&mut self, query: &str) -> Result<Project, String> {
        self.ensure_projects()?;
        match_project(query, &self.projects).cloned_project()
    }

    fn project_by_letters(&mut self, letters: &str) -> Result<Project, String> {
        self.ensure_projects()?;
        let wanted = sound_key(letters);
        let mut hits: Vec<_> = self
            .projects
            .iter()
            .filter(|project| sound_key(&project.key) == wanted || sound_key(&project.name) == wanted)
            .collect();
        hits.sort_by_key(|project| sound_key(&project.key) != wanted);
        hits.dedup_by(|left, right| left.id == right.id);
        let keys: Vec<_> = hits
            .iter()
            .filter(|project| sound_key(&project.key) == wanted)
            .collect();
        let chosen = if keys.len() == 1 {
            keys[0]
        } else if keys.is_empty() && hits.len() == 1 {
            hits[0]
        } else if keys.len() > 1 {
            return Err(format!(
                "несколько проектов: {}",
                keys.iter().map(|project| project.key.as_str()).collect::<Vec<_>>().join(", ")
            ));
        } else {
            return Err(format!("нет проекта для «{letters}»"));
        };
        Ok(chosen.clone_ref())
    }

    fn ensure_projects(&mut self) -> Result<(), String> {
        if self.projects_loaded {
            return Ok(());
        }
        let payload = self.get("/projects")?;
        self.projects = data_array(&payload).iter().filter_map(parse_project).collect();
        self.projects_loaded = true;
        if self.projects.is_empty() {
            return Err("проектов нет".into());
        }
        Ok(())
    }

    fn search(&self, query: &str) -> Result<Vec<Issue>, String> {
        let payload = self.get(&format!("/search?limit=20&q={}", encode(query)))?;
        let issues = payload
            .pointer("/data/issues")
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(issues.iter().filter_map(parse_issue).collect())
    }

    fn get(&self, path: &str) -> Result<Value, String> {
        self.send("GET", path, None)
    }

    fn post(&self, path: &str, body: Value) -> Result<Value, String> {
        self.send("POST", path, Some(body))
    }

    fn send(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value, String> {
        let url = format!("{API}{path}");
        let request = match method {
            "POST" => self.agent.post(&url),
            _ => self.agent.get(&url),
        };
        let request = request
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("Accept", "application/json");
        let result = if let Some(body) = body {
            request
                .set("Content-Type", "application/json")
                .send_json(body)
        } else {
            request.call()
        };
        match result {
            Ok(response) => response
                .into_json()
                .map_err(|err| format!("тимбенч прислал не json: {err}")),
            Err(ureq::Error::Status(401, _)) => Err("ключ тимбенча не принят".into()),
            Err(ureq::Error::Status(code, response)) => {
                let text = response.into_string().unwrap_or_default();
                Err(format!("тимбенч {code}: {}", clip(&text, 180)))
            }
            Err(err) => Err(format!("нет связи с тимбенчем: {err}")),
        }
    }
}

impl Project {
    fn clone_ref(&self) -> Project {
        Project {
            id: self.id,
            key: self.key.clone(),
            name: self.name.clone(),
        }
    }
}

fn list_answer(title: &str, issues: &[Issue], truncated: bool) -> Answer {
    let mut lines = vec![format!("{title}: {}", issues.len())];
    for issue in issues {
        lines.push(format!(
            "{} · {} · {} · {}",
            issue.key,
            status_ru(&issue.status),
            issue.project,
            issue.title
        ));
    }
    if truncated {
        lines.push("показаны первые двадцать".into());
    }
    let mut aloud = format!("{title}: {}", issues.len());
    if truncated {
        aloud.push_str(", показаны первые двадцать");
    }
    Answer {
        terminal: lines.join("\n"),
        aloud,
        ready: false,
    }
}

fn ambiguous(issues: &[Issue]) -> String {
    let shown: Vec<_> = issues
        .iter()
        .take(5)
        .map(|issue| format!("{} {}", issue.key, issue.title))
        .collect();
    format!("нашёл несколько: {}", shown.join("; "))
}

fn parse(rest: &str) -> Option<Phrase> {
    let rest = commands::normalize(rest);
    let words: Vec<&str> = rest.split_whitespace().collect();
    if words.is_empty() {
        return None;
    }
    let all = words.join(" ");
    if all == "мои задачи" || all == "мои открытые задачи" {
        return Some(Phrase::List { status: None });
    }
    if all == "что в работе" || all == "что в разработке" {
        return Some(Phrase::List { status: Some("development") });
    }
    if all == "что на проверке" || all == "что на ревью" {
        return Some(Phrase::List { status: Some("review") });
    }
    if all == "что на тесте" || all == "что в тесте" || all == "что на тестировании" {
        return Some(Phrase::List { status: Some("testing") });
    }
    if words[0] == "бэклог" {
        return Some(Phrase::Backlog {
            project: words[1..].join(" "),
        });
    }
    if words.first() == Some(&"что") && words.get(1) == Some(&"в") && words.get(2) == Some(&"бэклоге") {
        return Some(Phrase::Backlog {
            project: words[3..].join(" "),
        });
    }
    if words[0] == "задача" || words[0] == "задачу" {
        return Some(task_phrase(PhraseKind::Card, &words[1..]));
    }
    if all.starts_with("что писали") {
        let tail = if words.get(1) == Some(&"в") || words.get(1) == Some(&"по") {
            &words[2..]
        } else {
            &words[2..]
        };
        let tail = if words.get(1) == Some(&"писали") {
            if words.get(2) == Some(&"в") || words.get(2) == Some(&"по") {
                &words[3..]
            } else {
                &words[2..]
            }
        } else {
            tail
        };
        return Some(task_phrase(PhraseKind::Comments, tail));
    }
    if words[0] == "комментарии" || words[0] == "комментарий" && words.get(1) != Some(&"к") && words.get(1) != Some(&"на") {
        let tail = strip_preposition(&words[1..]);
        if words[0] == "комментарии" {
            return Some(task_phrase(PhraseKind::Comments, tail));
        }
    }
    if words[0] == "комментарий" {
        let tail = strip_preposition(&words[1..]);
        return Some(split_write(PhraseKind::Comment, tail, 0));
    }
    if words[0] == "время" && words.get(1) == Some(&"за") {
        return Some(match words[2..].join(" ").as_str() {
            "сегодня" => Phrase::Timesheet { range: Range::Today },
            "неделю" | "эту неделю" => Phrase::Timesheet { range: Range::ThisWeek },
            "прошлую неделю" => Phrase::Timesheet { range: Range::LastWeek },
            _ => Phrase::Reject("не разобрал период".into()),
        });
    }
    if words[0] == "сколько" {
        let mut index = 1;
        if words.get(1) == Some(&"времени") || words.get(1) == Some(&"списано") {
            index = 2;
        }
        let tail = strip_preposition(&words[index..]);
        return Some(task_phrase(PhraseKind::Spent, tail));
    }
    if words[0] == "спиши" || words[0] == "спиши" || words[0] == "залогируй" || words[0] == "залогируй" {
        return Some(parse_log(&words[1..]));
    }
    if words[0] == "переведи" || words[0] == "оценка" || words[0] == "оцени" || words[0] == "создай" {
        return Some(Phrase::Later);
    }
    None
}

enum PhraseKind {
    Card,
    Comments,
    Spent,
    Comment,
}

fn task_phrase(kind: PhraseKind, words: &[&str]) -> Phrase {
    let Some(task) = mention(words) else {
        return Phrase::Reject("нужен ключ задачи".into());
    };
    if !matches!(task, Mention::Title(_)) && mention_len(words).is_some_and(|len| len < words.len()) {
        return Phrase::Reject("лишние слова после задачи".into());
    }
    match kind {
        PhraseKind::Card => Phrase::Card { task },
        PhraseKind::Comments => Phrase::Comments { task },
        PhraseKind::Spent => Phrase::Spent { task },
        PhraseKind::Comment => Phrase::Comment { task, body: String::new() },
    }
}

fn split_write(kind: PhraseKind, words: &[&str], _unused: usize) -> Phrase {
    let Some(len) = mention_len(words) else {
        return Phrase::Reject("нужен ключ задачи".into());
    };
    let task = mention(&words[..len]).unwrap();
    let body = words[len..].join(" ");
    match kind {
        PhraseKind::Comment => {
            if body.is_empty() {
                Phrase::Reject("нет текста комментария".into())
            } else {
                Phrase::Comment { task, body }
            }
        }
        _ => Phrase::Reject("не разобрал запись".into()),
    }
}

fn parse_log(words: &[&str]) -> Phrase {
    let (when, words) = take_when(words);
    let Some((minutes, consumed)) = take_duration(words) else {
        return Phrase::Reject("не разобрал сколько списать".into());
    };
    let tail = strip_preposition(&words[consumed..]);
    let Some(len) = mention_len(tail) else {
        return Phrase::Reject("нужен ключ задачи".into());
    };
    let task = mention(&tail[..len]).unwrap();
    Phrase::Log {
        minutes,
        when,
        task,
        comment: tail[len..].join(" "),
    }
}

fn mention(words: &[&str]) -> Option<Mention> {
    let len = mention_len(words)?;
    if is_last(&words[..len]) {
        return Some(Mention::Last);
    }
    if let Some((letters, number, consumed)) = split_key(words) {
        if consumed == len {
            return Some(Mention::Key { letters, number });
        }
    }
    let title = words[..len].join(" ");
    if title.is_empty() {
        None
    } else {
        Some(Mention::Title(title))
    }
}

fn mention_len(words: &[&str]) -> Option<usize> {
    if words.is_empty() {
        return None;
    }
    if let Some(len) = last_len(words) {
        return Some(len);
    }
    if let Some((_, _, len)) = split_key(words) {
        return Some(len);
    }
    Some(words.len())
}

fn is_last(words: &[&str]) -> bool {
    last_len(words).is_some_and(|len| len == words.len())
}

fn last_len(words: &[&str]) -> Option<usize> {
    let first = *words.first()?;
    if first == "сюда" {
        return Some(1);
    }
    const PRONOUN: &[&str] = &["нее", "неё", "ее", "её", "эту", "эта", "этой", "ней"];
    if PRONOUN.contains(&first) {
        if words.get(1) == Some(&"задачу") || words.get(1) == Some(&"задача") {
            return Some(2);
        }
        return Some(1);
    }
    None
}

fn split_key(words: &[&str]) -> Option<(String, u32, usize)> {
    let mut index = 0;
    let mut letters = Vec::new();
    while index < words.len() {
        if take_number(&words[index..]).is_some() {
            break;
        }
        let word = words[index];
        if word.chars().all(|ch| ch.is_alphabetic()) && word.chars().count() <= 4 {
            letters.push(word);
            index += 1;
            continue;
        }
        return None;
    }
    if letters.is_empty() {
        return None;
    }
    let (number, consumed) = take_number(&words[index..])?;
    if number == 0 {
        return None;
    }
    Some((letters.join(" "), number, index + consumed))
}

fn strip_preposition<'a>(words: &'a [&'a str]) -> &'a [&'a str] {
    match words.first().copied() {
        Some("на" | "к" | "по" | "в") => &words[1..],
        _ => words,
    }
}

fn take_when<'a>(words: &'a [&'a str]) -> (When, &'a [&'a str]) {
    match words.first().copied() {
        Some("вчера") => (When::Yesterday, &words[1..]),
        Some("позавчера") => (When::DayBefore, &words[1..]),
        Some("сегодня") => (When::Today, &words[1..]),
        _ => (When::Today, words),
    }
}

fn take_duration(words: &[&str]) -> Option<(u32, usize)> {
    if words.is_empty() {
        return None;
    }
    if words[0] == "полчаса" {
        return Some((30, 1));
    }
    if words[0] == "полтора" && matches!(words.get(1), Some(&"часа") | Some(&"час")) {
        return Some((90, 2));
    }
    if matches!(words[0], "час" | "часа" | "часов") {
        return Some((60, 1));
    }
    let (number, consumed) = take_number(words)?;
    let unit = *words.get(consumed)?;
    let minutes = if matches!(unit, "час" | "часа" | "часов") {
        number.checked_mul(60)?
    } else if matches!(unit, "минуту" | "минуты" | "минут") {
        number
    } else {
        return None;
    };
    Some((minutes, consumed + 1))
}

fn take_number(words: &[&str]) -> Option<(u32, usize)> {
    if words.is_empty() {
        return None;
    }
    if words[0].chars().all(|ch| ch.is_ascii_digit()) {
        return words[0].parse::<u32>().ok().map(|value| (value, 1));
    }
    let mut total = 0u32;
    let mut consumed = 0;
    while consumed < words.len() {
        let Some(value) = number_word(words[consumed]) else {
            break;
        };
        if (11..20).contains(&value) {
            if total % 100 != 0 {
                break;
            }
            total += value;
            consumed += 1;
            break;
        }
        if value < 10 {
            if total % 10 != 0 {
                break;
            }
            total += value;
            consumed += 1;
            break;
        }
        if value < 100 && total % 100 >= 20 {
            break;
        }
        if value >= 100 && total % 1000 >= 100 {
            break;
        }
        total += value;
        consumed += 1;
    }
    if consumed == 0 {
        None
    } else {
        Some((total, consumed))
    }
}

fn number_word(word: &str) -> Option<u32> {
    Some(match word {
        "ноль" => 0,
        "один" | "одна" | "одну" | "одного" => 1,
        "два" | "две" => 2,
        "три" => 3,
        "четыре" => 4,
        "пять" => 5,
        "шесть" => 6,
        "семь" => 7,
        "восемь" => 8,
        "девять" => 9,
        "десять" => 10,
        "одиннадцать" => 11,
        "двенадцать" => 12,
        "тринадцать" => 13,
        "четырнадцать" => 14,
        "пятнадцать" => 15,
        "шестнадцать" => 16,
        "семнадцать" => 17,
        "восемнадцать" => 18,
        "девятнадцать" => 19,
        "двадцать" => 20,
        "тридцать" => 30,
        "сорок" => 40,
        "пятьдесят" => 50,
        "шестьдесят" => 60,
        "семьдесят" => 70,
        "восемьдесят" => 80,
        "девяносто" => 90,
        "сто" => 100,
        "двести" => 200,
        "триста" => 300,
        "четыреста" => 400,
        "пятьсот" => 500,
        "шестьсот" => 600,
        "семьсот" => 700,
        "восемьсот" => 800,
        "девятьсот" => 900,
        _ => return None,
    })
}

fn match_project<'a>(query: &str, projects: &'a [Project]) -> Result<&'a Project, String> {
    let query = commands::normalize(query);
    if query.is_empty() {
        return Err("нужно имя проекта".into());
    }
    if let Some(project) = projects.iter().find(|project| commands::normalize(&project.key) == query) {
        return Ok(project);
    }
    if let Some(project) = projects.iter().find(|project| commands::normalize(&project.name) == query) {
        return Ok(project);
    }
    let contained: Vec<_> = projects
        .iter()
        .filter(|project| {
            query.chars().count() >= 4 && commands::normalize(&project.name).contains(&query)
        })
        .collect();
    if contained.len() == 1 {
        return Ok(contained[0]);
    }
    let wanted = sound_key(&query);
    let sounded: Vec<_> = projects
        .iter()
        .filter(|project| sound_key(&project.name) == wanted || sound_key(&project.key) == wanted)
        .collect();
    if sounded.len() == 1 {
        return Ok(sounded[0]);
    }
    Err(format!("не нашёл проект «{query}»"))
}

fn parse_project(value: &Value) -> Option<Project> {
    Some(Project {
        id: value.get("id")?.as_u64()?,
        key: value.get("key")?.as_str()?.to_string(),
        name: value.get("name")?.as_str()?.to_string(),
    })
}

fn parse_issue(value: &Value) -> Option<Issue> {
    let project = value
        .pointer("/project/key")
        .and_then(|item| item.as_str())
        .or_else(|| value.pointer("/project/name").and_then(|item| item.as_str()))
        .unwrap_or("")
        .to_string();
    let assignee = value
        .get("assignee_name")
        .and_then(|item| item.as_str())
        .or_else(|| value.pointer("/assignee/name").and_then(|item| item.as_str()))
        .unwrap_or("")
        .to_string();
    Some(Issue {
        id: value.get("id")?.as_u64()?,
        key: value.get("key")?.as_str()?.to_string(),
        title: value.get("title").and_then(|item| item.as_str()).unwrap_or("").to_string(),
        status: value.get("status").and_then(|item| item.as_str()).unwrap_or("").to_string(),
        project,
        assignee,
        estimate: value.get("estimate_minutes").and_then(|item| item.as_u64()).map(|item| item as u32),
        spent: value
            .get("time_spent_minutes")
            .and_then(|item| item.as_u64())
            .unwrap_or(0) as u32,
        description: value.get("description").and_then(|item| item.as_str()).unwrap_or("").to_string(),
    })
}

fn unwrap_data(value: &Value) -> Value {
    value.get("data").cloned().unwrap_or_else(|| value.clone())
}

fn data_array(value: &Value) -> Vec<Value> {
    if let Some(items) = value.as_array() {
        return items.clone();
    }
    if let Some(items) = value.get("data").and_then(|item| item.as_array()) {
        return items.clone();
    }
    Vec::new()
}

fn status_ru(status: &str) -> &'static str {
    match status {
        "backlog" => "бэклог",
        "todo" => "к работе",
        "analysis" => "анализ",
        "development" => "в работе",
        "review" => "на проверке",
        "testing" => "на тесте",
        "production" => "на проде",
        "done" => "сделано",
        "blocked" => "блок",
        _ => "без статуса",
    }
}

fn minutes_words(minutes: u32) -> String {
    if minutes == 0 {
        return "0 минут".into();
    }
    let hours = minutes / 60;
    let rest = minutes % 60;
    match (hours, rest) {
        (0, minutes) => format!("{minutes} {}", minute_word(minutes)),
        (hours, 0) => format!("{hours} {}", hour_word(hours)),
        (hours, minutes) => format!(
            "{hours} {} {minutes} {}",
            hour_word(hours),
            minute_word(minutes)
        ),
    }
}

fn hour_word(value: u32) -> &'static str {
    let value = value % 100;
    if (11..15).contains(&value) {
        return "часов";
    }
    match value % 10 {
        1 => "час",
        2..=4 => "часа",
        _ => "часов",
    }
}

fn minute_word(value: u32) -> &'static str {
    let value = value % 100;
    if (11..15).contains(&value) {
        return "минут";
    }
    match value % 10 {
        1 => "минута",
        2..=4 => "минуты",
        _ => "минут",
    }
}

fn clip(text: &str, limit: usize) -> &str {
    let mut end = text.len();
    for (index, (byte, _)) in text.char_indices().enumerate() {
        if index == limit {
            end = byte;
            break;
        }
    }
    text[..end].trim()
}

fn sound_key(text: &str) -> String {
    let folded = latin_to_cyrillic(&commands::normalize(text));
    folded
        .chars()
        .filter(|ch| {
            matches!(
                ch,
                'б' | 'в' | 'г' | 'д' | 'ж' | 'з' | 'й' | 'к' | 'л' | 'м' | 'н' | 'п' | 'р' | 'с'
                    | 'т' | 'ф' | 'х' | 'ц' | 'ч' | 'ш' | 'щ'
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
        if let Some((latin, cyrillic)) = digraphs.iter().find(|(latin, _)| rest.starts_with(latin)) {
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

fn encode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Date {
    year: i32,
    month: u32,
    day: u32,
}

impl Date {
    fn iso(self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

fn local_today() -> Result<Date, String> {
    let output = std::process::Command::new("date")
        .arg("+%Y-%m-%d")
        .output()
        .map_err(|err| format!("не узнал сегодняшнюю дату: {err}"))?;
    if !output.status.success() {
        return Err("не узнал сегодняшнюю дату".into());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_iso(text.trim()).ok_or_else(|| "не узнал сегодняшнюю дату".into())
}

fn parse_iso(text: &str) -> Option<Date> {
    let mut parts = text.split('-');
    Some(Date {
        year: parts.next()?.parse().ok()?,
        month: parts.next()?.parse().ok()?,
        day: parts.next()?.parse().ok()?,
    })
}

fn shift_date(today: Date, when: When) -> Result<Date, String> {
    let shift = match when {
        When::Today => 0,
        When::Yesterday => -1,
        When::DayBefore => -2,
    };
    Ok(add_days(today, shift))
}

fn week_range(today: Date, previous: bool) -> (Date, Date) {
    let days = days_from_civil(today.year, today.month, today.day);
    let monday_index = (days + 3).rem_euclid(7);
    let this_monday = days - monday_index;
    if previous {
        (civil_from_days(this_monday - 7), civil_from_days(this_monday - 1))
    } else {
        (civil_from_days(this_monday), today)
    }
}

fn add_days(date: Date, shift: i64) -> Date {
    civil_from_days(days_from_civil(date.year, date.month, date.day) + shift)
}

fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let year = year - i32::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = (year - era * 400) as u64;
    let month_prime = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year as u64;
    era as i64 * 146097 + day_of_era as i64 - 719468
}

fn civil_from_days(days: i64) -> Date {
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let day_of_era = (z - era * 146097) as u64;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36524 - day_of_era / 146096) / 365;
    let year = year_of_era as i32 + era as i32 * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let year = year + i32::from(month <= 2);
    Date {
        year,
        month: month as u32,
        day: day as u32,
    }
}

trait ProjectMatch {
    fn cloned_project(&self) -> Result<Project, String>;
}

impl ProjectMatch for Result<&Project, String> {
    fn cloned_project(&self) -> Result<Project, String> {
        self.as_ref().map(|project| project.clone_ref()).map_err(|err| err.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phrases_cover_the_workday() {
        assert!(matches!(parse("мои задачи"), Some(Phrase::List { status: None })));
        assert!(matches!(
            parse("что в работе"),
            Some(Phrase::List { status: Some("development") })
        ));
        assert!(matches!(
            parse("что на проверке"),
            Some(Phrase::List { status: Some("review") })
        ));
        assert!(matches!(
            parse("что на тесте"),
            Some(Phrase::List { status: Some("testing") })
        ));
        assert!(matches!(parse("бэклог"), Some(Phrase::Backlog { project }) if project.is_empty()));
        assert!(matches!(parse("бэклог асилан"), Some(Phrase::Backlog { project }) if project == "асилан"));
        assert!(matches!(
            parse("задача тэ бэ шестьдесят два"),
            Some(Phrase::Card { task: Mention::Key { letters, number } }) if letters == "тэ бэ" && number == 62
        ));
        assert!(matches!(parse("как дела"), None));
        assert!(matches!(parse("переведи тб 62 в тест"), Some(Phrase::Later)));
        assert!(matches!(parse("создай в асилане задачу проверить отчёт"), Some(Phrase::Later)));
    }

    #[test]
    fn log_and_comment_keep_the_tail() {
        match parse("спиши вчера полчаса на неё разбирал падение").unwrap() {
            Phrase::Log { minutes, when, task, comment } => {
                assert_eq!(minutes, 30);
                assert_eq!(when, When::Yesterday);
                assert_eq!(task, Mention::Last);
                assert_eq!(comment, "разбирал падение");
            }
            other => panic!("{other:?}"),
        }
        match parse("спиши два часа на тб 62").unwrap() {
            Phrase::Log { minutes, task, comment, .. } => {
                assert_eq!(minutes, 120);
                assert!(matches!(task, Mention::Key { number: 62, .. }));
                assert!(comment.is_empty());
            }
            other => panic!("{other:?}"),
        }
        match parse("комментарий к тб 62 смотрел логи").unwrap() {
            Phrase::Comment { task, body } => {
                assert!(matches!(task, Mention::Key { number: 62, .. }));
                assert_eq!(body, "смотрел логи");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(parse("спиши на тб 62"), Some(Phrase::Reject(_))));
        assert!(matches!(parse("комментарий к тб 62"), Some(Phrase::Reject(_))));
    }

    #[test]
    fn week_of_known_monday() {
        let monday = Date { year: 2026, month: 11, day: 23 };
        assert_eq!(week_range(monday, false), (monday, monday));
        let (from, to) = week_range(monday, true);
        assert_eq!(from.iso(), "2026-11-16");
        assert_eq!(to.iso(), "2026-11-22");
        let wednesday = Date { year: 2026, month: 11, day: 25 };
        let (from, to) = week_range(wednesday, false);
        assert_eq!(from.iso(), "2026-11-23");
        assert_eq!(to, wednesday);
    }

    #[test]
    fn project_letters_match_key() {
        let projects = vec![
            Project { id: 2, key: "TB".into(), name: "TeamBench".into() },
            Project { id: 3, key: "ASN".into(), name: "Асилан".into() },
        ];
        assert_eq!(sound_key("тэ бэ"), sound_key("TB"));
        assert_eq!(match_project("асилан", &projects).unwrap().key, "ASN");
        assert_eq!(match_project("tb", &projects).unwrap().id, 2);
    }
}
