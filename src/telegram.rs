#![allow(dead_code, unused_imports)]
//! Telegram adapter — implements ChannelAdapter for Telegram Bot API.
//! Creates forum topics per agent, routes messages by topic.

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

pub struct TelegramAdapter {
    bot: BotApi,
    group_id: i64,
    /// agent_name → topic thread_id
    topics: Mutex<HashMap<String, i64>>,
    /// thread_id → agent_name (reverse lookup)
    routing: Mutex<HashMap<i64, String>>,
    offset: Mutex<i64>,
}

impl TelegramAdapter {
    pub fn new(config: TelegramConfig) -> Self {
        let adapter = Self {
            bot: BotApi::new(&config.bot_token),
            group_id: config.group_id,
            topics: Mutex::new(HashMap::new()),
            routing: Mutex::new(HashMap::new()),
            offset: Mutex::new(0),
        };
        // Load persisted topic mappings
        adapter.load_topics();
        adapter
    }

    fn register_topic(&self, agent: &str, thread_id: i64) {
        self.topics.lock().unwrap_or_else(|e| e.into_inner())
            .insert(agent.to_owned(), thread_id);
        self.routing.lock().unwrap_or_else(|e| e.into_inner())
            .insert(thread_id, agent.to_owned());
        self.save_topics();
    }

    fn topics_path() -> std::path::PathBuf {
        crate::paths::home().join("topics.json")
    }

    fn save_topics(&self) {
        let topics = self.topics.lock().unwrap_or_else(|e| e.into_inner());
        let _ = std::fs::write(Self::topics_path(), serde_json::to_string_pretty(&*topics).unwrap_or_default());
    }

    fn load_topics(&self) {
        let path = Self::topics_path();
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(map) = serde_json::from_str::<HashMap<String, i64>>(&content) {
                let mut topics = self.topics.lock().unwrap_or_else(|e| e.into_inner());
                let mut routing = self.routing.lock().unwrap_or_else(|e| e.into_inner());
                for (name, tid) in &map {
                    topics.insert(name.clone(), *tid);
                    routing.insert(*tid, name.clone());
                }
                eprintln!("[telegram] loaded {} persisted topics", map.len());
            }
        }
    }
}

impl ChannelAdapter for TelegramAdapter {
    fn name(&self) -> &str { "telegram" }

    fn on_agent_created(&self, name: &str) {
        // Check if topic already exists (persisted from previous run)
        if self.topics.lock().unwrap_or_else(|e| e.into_inner()).contains_key(name) {
            let tid = self.topics.lock().unwrap_or_else(|e| e.into_inner())[name];
            eprintln!("[telegram] reusing existing topic '{name}' (thread_id: {tid})");
            self.bot.send_message(self.group_id, &format!("🟢 Agent '{name}' reconnected"), Some(tid)).ok();
            return;
        }
        // Create new topic
        match self.bot.create_forum_topic(self.group_id, name) {
            Ok(thread_id) => {
                self.register_topic(name, thread_id);
                eprintln!("[telegram] created topic '{name}' (thread_id: {thread_id})");
                self.bot.send_message(self.group_id, &format!("🟢 Agent '{name}' started"), Some(thread_id)).ok();
            }
            Err(e) => eprintln!("[telegram] failed to create topic for '{name}': {e}"),
        }
    }

    fn on_agent_removed(&self, name: &str) {
        let thread_id = self.topics.lock().unwrap_or_else(|e| e.into_inner()).remove(name);
        if let Some(tid) = thread_id {
            self.routing.lock().unwrap_or_else(|e| e.into_inner()).remove(&tid);
            self.bot.send_message(self.group_id, &format!("🔴 Agent '{name}' stopped"), Some(tid)).ok();
        }
    }

    fn send_to_agent(&self, agent: &str, text: &str) {
        let thread_id = self.topics.lock().unwrap_or_else(|e| e.into_inner()).get(agent).copied();
        if let Some(tid) = thread_id {
            if let Err(e) = self.bot.send_message(self.group_id, text, Some(tid)) {
                eprintln!("[telegram] send to '{agent}' failed: {e}");
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

            if text.is_empty() || chat_id != self.group_id { continue; }

            // Route by thread_id → agent name
            let target = thread_id
                .and_then(|tid| self.routing.lock().unwrap_or_else(|e| e.into_inner()).get(&tid).cloned());

            if let Some(agent) = target {
                messages.push(IncomingMessage {
                    agent_target: agent,
                    sender: username.to_owned(),
                    text: text.to_owned(),
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
        Self { base_url: format!("https://api.telegram.org/bot{token}") }
    }

    fn call(&self, method: &str, body: &Value) -> Result<Value, String> {
        let url = format!("{}/{method}", self.base_url);
        let timeout = if method == "getUpdates" {
            Duration::from_secs(POLL_TIMEOUT + 10)
        } else {
            Duration::from_secs(30)
        };
        let mut resp = isahc::Request::post(&url)
            .timeout(timeout)
            .header("Content-Type", "application/json")
            .body(serde_json::to_string(body).unwrap())
            .map_err(|e| format!("build: {e}"))
            .and_then(|r| isahc::send(r).map_err(|e| format!("send: {e}")))?;
        let text = resp.text().map_err(|e| format!("read: {e}"))?;
        let parsed: Value = serde_json::from_str(&text).map_err(|e| format!("parse: {e}"))?;
        if parsed["ok"].as_bool() == Some(true) {
            Ok(parsed["result"].clone())
        } else {
            Err(format!("API error: {}", parsed["description"].as_str().unwrap_or("unknown")))
        }
    }

    fn send_message(&self, chat_id: i64, text: &str, thread_id: Option<i64>) -> Result<Value, String> {
        let mut body = json!({"chat_id": chat_id, "text": text});
        if let Some(tid) = thread_id { body["message_thread_id"] = json!(tid); }
        self.call("sendMessage", &body)
    }

    fn create_forum_topic(&self, chat_id: i64, name: &str) -> Result<i64, String> {
        let result = self.call("createForumTopic", &json!({"chat_id": chat_id, "name": name}))?;
        result["message_thread_id"].as_i64()
            .ok_or_else(|| "no message_thread_id in response".into())
    }

    fn get_updates(&self, offset: i64) -> Result<Vec<Value>, String> {
        let result = self.call("getUpdates", &json!({
            "offset": offset, "timeout": POLL_TIMEOUT, "allowed_updates": ["message"]
        }))?;
        Ok(result.as_array().cloned().unwrap_or_default())
    }
}
