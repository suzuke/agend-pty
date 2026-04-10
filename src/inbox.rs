//! Inbox — per-agent message queue for long messages.
//!
//! When a message is too long to inject directly into the PTY,
//! store it in the inbox and inject a short notification instead.
//! The agent can then use the `inbox` MCP tool to retrieve the full message.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const MAX_DIRECT_INJECT_LEN: usize = 500;

#[derive(Debug, Clone)]
pub struct InboxMessage {
    pub id: u64,
    pub sender: String,
    pub text: String,
    pub timestamp: u64,
}

/// Per-agent inbox.
struct AgentInbox {
    messages: Vec<InboxMessage>,
    next_id: u64,
}

impl AgentInbox {
    fn new() -> Self { Self { messages: Vec::new(), next_id: 1 } }

    fn push(&mut self, sender: String, text: String) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default().as_secs();
        self.messages.push(InboxMessage { id, sender, text, timestamp: ts });
        // Keep last 50 messages
        if self.messages.len() > 50 {
            self.messages.remove(0);
        }
        id
    }

    fn get(&self, id: u64) -> Option<&InboxMessage> {
        self.messages.iter().find(|m| m.id == id)
    }

    fn list(&self) -> &[InboxMessage] {
        &self.messages
    }

    fn clear(&mut self) {
        self.messages.clear();
    }
}

/// Global inbox store.
pub struct InboxStore {
    inboxes: Mutex<HashMap<String, AgentInbox>>,
}

impl InboxStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { inboxes: Mutex::new(HashMap::new()) })
    }

    /// Store a message. Returns (message_id, should_inject_directly).
    /// If the message is short enough, inject directly. Otherwise store and return notification text.
    pub fn store_or_inject(&self, agent: &str, sender: &str, message: &str) -> InjectAction {
        if message.len() <= MAX_DIRECT_INJECT_LEN {
            return InjectAction::Direct(format!(
                "[message from {sender} (reply via send_to_instance to \"{sender}\")] {message}\r"
            ));
        }
        // Long message → store in inbox, inject notification
        let mut inboxes = self.inboxes.lock().unwrap_or_else(|e| e.into_inner());
        let inbox = inboxes.entry(agent.to_owned()).or_insert_with(AgentInbox::new);
        let id = inbox.push(sender.to_owned(), message.to_owned());
        let preview: String = message.chars().take(100).collect();
        InjectAction::Notification(format!(
            "[message from {sender}] {preview}... (full message in inbox, use inbox tool with id={id})\r"
        ))
    }

    /// Get a specific message by ID.
    pub fn get(&self, agent: &str, id: u64) -> Option<InboxMessage> {
        let inboxes = self.inboxes.lock().unwrap_or_else(|e| e.into_inner());
        inboxes.get(agent).and_then(|i| i.get(id)).cloned()
    }

    /// List all messages for an agent.
    pub fn list(&self, agent: &str) -> Vec<InboxMessage> {
        let inboxes = self.inboxes.lock().unwrap_or_else(|e| e.into_inner());
        inboxes.get(agent).map(|i| i.list().to_vec()).unwrap_or_default()
    }

    /// Clear inbox for an agent.
    pub fn clear(&self, agent: &str) {
        let mut inboxes = self.inboxes.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(inbox) = inboxes.get_mut(agent) { inbox.clear(); }
    }
}

pub enum InjectAction {
    /// Message is short enough — inject directly into PTY.
    Direct(String),
    /// Message stored in inbox — inject this notification instead.
    Notification(String),
}
