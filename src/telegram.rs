//! Telegram adapter — bot polling + message routing.
//! Uses isahc (sync HTTP) for Telegram Bot API. No tokio needed.

use crate::api;
use isahc::config::Configurable;
use isahc::ReadResponseExt;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

const POLL_TIMEOUT: u64 = 30;

pub struct TelegramConfig {
    pub bot_token: String,
    pub group_id: i64,
}

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

    fn get_updates(&self, offset: i64) -> Result<Vec<Value>, String> {
        let result = self.call("getUpdates", &json!({
            "offset": offset, "timeout": POLL_TIMEOUT, "allowed_updates": ["message"]
        }))?;
        Ok(result.as_array().cloned().unwrap_or_default())
    }
}

/// Start Telegram polling in a new thread.
/// Routes incoming messages to agents via the API writers.
pub fn start(config: TelegramConfig, writers: api::AgentWriters) {
    std::thread::Builder::new()
        .name("telegram".into())
        .spawn(move || run_poll_loop(config, writers))
        .unwrap();
}

fn run_poll_loop(config: TelegramConfig, writers: api::AgentWriters) {
    let bot = BotApi::new(&config.bot_token);
    let mut offset: i64 = 0;
    eprintln!("[telegram] polling started (group: {})", config.group_id);

    loop {
        let updates = match bot.get_updates(offset) {
            Ok(u) => u,
            Err(e) => {
                eprintln!("[telegram] poll error: {e}");
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
        };

        for update in &updates {
            if let Some(uid) = update["update_id"].as_i64() {
                offset = uid + 1;
            }
            let msg = &update["message"];
            let text = msg["text"].as_str().unwrap_or("");
            let chat_id = msg["chat"]["id"].as_i64().unwrap_or(0);
            let username = msg["from"]["username"].as_str().unwrap_or("unknown");
            let thread_id = msg["message_thread_id"].as_i64();

            if text.is_empty() || chat_id != config.group_id { continue; }

            // Route: find target agent by thread topic name or use first agent
            let target = find_target_agent(&writers, thread_id);
            if let Some(target) = target {
                let formatted = format!("[user:{username} via telegram] {text}\r");
                let w = writers.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(pw) = w.get(&target) {
                    let _ = pw.lock().unwrap_or_else(|e| e.into_inner()).write_all(formatted.as_bytes());
                    eprintln!("[telegram] {username} → {target}: {}", text.chars().take(60).collect::<String>());
                }
            }
        }
    }
}

/// Simple routing: for now, use the first available agent.
/// TODO: topic-based routing (thread_id → agent name mapping)
fn find_target_agent(writers: &api::AgentWriters, _thread_id: Option<i64>) -> Option<String> {
    writers.lock().unwrap_or_else(|e| e.into_inner())
        .keys().next().cloned()
}

/// Send a notification to Telegram (called from daemon/MCP).
pub fn notify(token: &str, chat_id: i64, text: &str) {
    let bot = BotApi::new(token);
    if let Err(e) = bot.send_message(chat_id, text, None) {
        eprintln!("[telegram] notify error: {e}");
    }
}
