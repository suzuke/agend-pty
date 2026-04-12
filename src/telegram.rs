use crate::channel::{ChannelAdapter, IncomingMessage};
use isahc::config::Configurable;
use isahc::ReadResponseExt;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

const POLL_TIMEOUT: u64 = 30;

pub struct TelegramConfig {
    pub bot_token: String,
    pub group_id: i64,
}

struct TopicMap {
    by_name: HashMap<String, i64>,
    by_id: HashMap<i64, String>,
}

pub struct TelegramAdapter {
    bot: BotApi,
    group_id: i64,
    topics: Mutex<TopicMap>,
    offset: Mutex<i64>,
}

impl TelegramAdapter {
    pub fn new(config: TelegramConfig) -> Self {
        let adapter = Self {
            bot: BotApi::new(&config.bot_token),
            group_id: config.group_id,
            topics: Mutex::new(TopicMap {
                by_name: HashMap::new(),
                by_id: HashMap::new(),
            }),
            offset: Mutex::new(0),
        };
        adapter.load_topics();
        adapter
    }

    fn register_topic(&self, agent: &str, thread_id: i64) {
        let mut t = self.topics.lock().unwrap_or_else(|e| e.into_inner());
        t.by_name.insert(agent.to_owned(), thread_id);
        t.by_id.insert(thread_id, agent.to_owned());
        let _ = std::fs::write(
            Self::topics_path(),
            serde_json::to_string_pretty(&t.by_name).unwrap_or_default(),
        );
    }

    fn topics_path() -> std::path::PathBuf {
        crate::paths::home().join("topics.json")
    }

    fn load_topics(&self) {
        let path = Self::topics_path();
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(map) = serde_json::from_str::<HashMap<String, i64>>(&content) {
                let mut t = self.topics.lock().unwrap_or_else(|e| e.into_inner());
                for (name, tid) in &map {
                    t.by_name.insert(name.clone(), *tid);
                    t.by_id.insert(*tid, name.clone());
                }
                eprintln!("[telegram] loaded {} persisted topics", map.len());
            }
        }
    }
}

impl ChannelAdapter for TelegramAdapter {
    fn name(&self) -> &str {
        "telegram"
    }

    fn on_agent_created(&self, name: &str) {
        let t = self.topics.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(&tid) = t.by_name.get(name) {
            drop(t);
            eprintln!("[telegram] reusing existing topic '{name}' (thread_id: {tid})");
            self.bot
                .send_message(
                    self.group_id,
                    &format!("🟢 Agent '{name}' reconnected"),
                    Some(tid),
                )
                .ok();
            return;
        }
        drop(t);
        match self.bot.create_forum_topic(self.group_id, name) {
            Ok(thread_id) => {
                self.register_topic(name, thread_id);
                eprintln!("[telegram] created topic '{name}' (thread_id: {thread_id})");
                self.bot
                    .send_message(
                        self.group_id,
                        &format!("🟢 Agent '{name}' started"),
                        Some(thread_id),
                    )
                    .ok();
            }
            Err(e) => eprintln!("[telegram] failed to create topic for '{name}': {e}"),
        }
    }

    fn on_agent_removed(&self, name: &str) {
        let mut t = self.topics.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tid) = t.by_name.remove(name) {
            t.by_id.remove(&tid);
            drop(t);
            self.bot
                .send_message(
                    self.group_id,
                    &format!("🔴 Agent '{name}' stopped"),
                    Some(tid),
                )
                .ok();
        }
    }

    fn send_to_agent(&self, agent: &str, text: &str) -> Option<String> {
        self.send_to_agent_ext(agent, text, "text", None)
    }

    fn send_to_agent_ext(
        &self,
        agent: &str,
        text: &str,
        format: &str,
        reply_to: Option<&str>,
    ) -> Option<String> {
        let tid = self
            .topics
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .by_name
            .get(agent)
            .copied()?;
        let parse_mode = match format {
            "markdown" => Some("Markdown"),
            "html" => Some("HTML"),
            _ => None,
        };
        let reply_to_id = reply_to.and_then(|s| s.parse::<i64>().ok());
        match self
            .bot
            .send_message_ext(self.group_id, text, Some(tid), parse_mode, reply_to_id)
        {
            Ok(val) => val["message_id"].as_i64().map(|id| id.to_string()),
            Err(e) => {
                eprintln!("[telegram] send to '{agent}' failed: {e}");
                None
            }
        }
    }

    fn notify(&self, text: &str) {
        self.bot.send_message(self.group_id, text, None).ok();
    }

    fn poll(&self) -> Vec<IncomingMessage> {
        let current_offset = *self.offset.lock().unwrap_or_else(|e| e.into_inner());
        let updates = match self.bot.get_updates(current_offset) {
            Ok(u) => u,
            Err(_) => return vec![],
        };

        let mut messages = Vec::new();
        let mut new_offset = current_offset;
        for update in &updates {
            if let Some(uid) = update["update_id"].as_i64() {
                new_offset = uid + 1;
            }
            let msg = &update["message"];
            let text = msg["text"].as_str().unwrap_or("");
            let chat_id = msg["chat"]["id"].as_i64().unwrap_or(0);
            let username = msg["from"]["username"].as_str().unwrap_or("unknown");
            let thread_id = msg["message_thread_id"].as_i64();

            if text.is_empty() || chat_id != self.group_id {
                continue;
            }

            let target = thread_id.and_then(|tid| {
                self.topics
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .by_id
                    .get(&tid)
                    .cloned()
            });

            if let Some(agent) = target {
                messages.push(IncomingMessage {
                    agent_target: agent,
                    sender: username.to_owned(),
                    text: text.to_owned(),
                    message_id: msg["message_id"].as_i64().map(|id| id.to_string()),
                });
            }
        }
        *self.offset.lock().unwrap_or_else(|e| e.into_inner()) = new_offset;
        messages
    }
}

// ── Telegram Bot API ────────────────────────────────────────────────────

struct BotApi {
    base_url: String,
}

impl BotApi {
    fn new(token: &str) -> Self {
        Self {
            base_url: format!("https://api.telegram.org/bot{token}"),
        }
    }

    fn call(&self, method: &str, body: &Value) -> Result<Value, String> {
        let url = format!("{}/{method}", self.base_url);
        let is_poll = method == "getUpdates";
        let timeout = if is_poll {
            Duration::from_secs(POLL_TIMEOUT + 10)
        } else {
            Duration::from_secs(30)
        };
        let max_retries = if is_poll { 1 } else { 3 };
        let mut last_err = String::new();
        for attempt in 0..max_retries {
            if attempt > 0 {
                std::thread::sleep(Duration::from_secs(1 << (attempt - 1)));
            }
            let result = isahc::Request::post(&url)
                .timeout(timeout)
                .header("Content-Type", "application/json")
                .body(serde_json::to_string(body).unwrap_or_default())
                .map_err(|e| format!("build: {e}"))
                .and_then(|r| isahc::send(r).map_err(|e| format!("send: {e}")));
            let mut resp = match result {
                Ok(r) => r,
                Err(e) => {
                    last_err = e;
                    continue;
                }
            };
            let text = match resp.text() {
                Ok(t) => t,
                Err(e) => {
                    last_err = format!("read: {e}");
                    continue;
                }
            };
            let parsed: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => {
                    last_err = format!("parse: {e}");
                    continue;
                }
            };
            if parsed["ok"].as_bool() == Some(true) {
                return Ok(parsed["result"].clone());
            }
            let desc = parsed["description"].as_str().unwrap_or("unknown");
            if parsed["error_code"].as_i64() == Some(429) {
                let retry_after = parsed["parameters"]["retry_after"].as_u64().unwrap_or(2);
                eprintln!("[telegram] rate limited, waiting {retry_after}s");
                std::thread::sleep(Duration::from_secs(retry_after));
                last_err = format!("rate limited: {desc}");
                continue;
            }
            return Err(format!("API error: {desc}"));
        }
        Err(last_err)
    }

    fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        thread_id: Option<i64>,
    ) -> Result<Value, String> {
        self.send_message_ext(chat_id, text, thread_id, None, None)
    }

    fn send_message_ext(
        &self,
        chat_id: i64,
        text: &str,
        thread_id: Option<i64>,
        parse_mode: Option<&str>,
        reply_to: Option<i64>,
    ) -> Result<Value, String> {
        let mut body = json!({"chat_id": chat_id, "text": text});
        if let Some(tid) = thread_id {
            body["message_thread_id"] = json!(tid);
        }
        if let Some(pm) = parse_mode {
            body["parse_mode"] = json!(pm);
        }
        if let Some(rt) = reply_to {
            body["reply_to_message_id"] = json!(rt);
        }
        self.call("sendMessage", &body)
    }

    fn create_forum_topic(&self, chat_id: i64, name: &str) -> Result<i64, String> {
        let result = self.call(
            "createForumTopic",
            &json!({"chat_id": chat_id, "name": name}),
        )?;
        result["message_thread_id"]
            .as_i64()
            .ok_or_else(|| "no message_thread_id in response".into())
    }

    fn get_updates(&self, offset: i64) -> Result<Vec<Value>, String> {
        let result = self.call(
            "getUpdates",
            &json!({
                "offset": offset, "timeout": POLL_TIMEOUT, "allowed_updates": ["message"]
            }),
        )?;
        Ok(result.as_array().cloned().unwrap_or_default())
    }
}
