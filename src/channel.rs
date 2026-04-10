//! Channel — abstract interface for messaging platforms (Telegram, Discord, Slack, etc.)
//!
//! Each adapter implements the ChannelAdapter trait. The daemon calls lifecycle
//! hooks (on_agent_created/removed) and the adapter handles platform-specific
//! actions (create topic, send notification, etc.)

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Incoming message from a channel.
#[derive(Debug, Clone)]
pub struct IncomingMessage {
    pub agent_target: String,
    pub sender: String,
    pub text: String,
}

/// Abstract channel adapter. Implementations handle platform-specific logic.
pub trait ChannelAdapter: Send + Sync {
    /// Called when a new agent is created. Create a topic/thread/channel.
    fn on_agent_created(&self, name: &str);
    /// Called when an agent is removed. Optionally archive the topic.
    fn on_agent_removed(&self, name: &str);
    /// Send a message to a specific agent's topic/thread.
    fn send_to_agent(&self, agent: &str, text: &str);
    /// Send a notification to the general/default topic.
    fn notify(&self, text: &str);
    /// Poll for incoming messages (blocking, with timeout).
    fn poll(&self) -> Vec<IncomingMessage>;
    /// Get the adapter name (e.g. "telegram", "discord").
    fn name(&self) -> &str;
}

/// Manages all channel adapters and agent→topic routing.
pub struct ChannelManager {
    adapters: Vec<Box<dyn ChannelAdapter>>,
}

impl ChannelManager {
    pub fn new() -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self { adapters: Vec::new() }))
    }

    pub fn add_adapter(&mut self, adapter: Box<dyn ChannelAdapter>) {
        eprintln!("[channel] registered adapter: {}", adapter.name());
        self.adapters.push(adapter);
    }

    pub fn on_agent_created(&self, name: &str) {
        for adapter in &self.adapters {
            adapter.on_agent_created(name);
        }
    }

    pub fn on_agent_removed(&self, name: &str) {
        for adapter in &self.adapters {
            adapter.on_agent_removed(name);
        }
    }

    pub fn send_to_agent(&self, agent: &str, text: &str) {
        for adapter in &self.adapters {
            adapter.send_to_agent(agent, text);
        }
    }

    pub fn notify(&self, text: &str) {
        for adapter in &self.adapters {
            adapter.notify(text);
        }
    }

    pub fn poll_all(&self) -> Vec<IncomingMessage> {
        let mut msgs = Vec::new();
        for adapter in &self.adapters {
            msgs.extend(adapter.poll());
        }
        msgs
    }
}
