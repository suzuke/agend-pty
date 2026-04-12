//! Channel — abstract interface for messaging platforms (Telegram, Discord, Slack, etc.)
//!
//! Each adapter implements the ChannelAdapter trait. The daemon calls lifecycle
//! hooks (on_agent_created/removed) and the adapter handles platform-specific
//! actions (create topic, send notification, etc.)

use std::sync::{Arc, Mutex};

/// Incoming message from a channel.
#[derive(Debug, Clone)]
pub struct IncomingMessage {
    pub agent_target: String,
    pub sender: String,
    pub text: String,
    pub message_id: Option<String>,
}

/// Abstract channel adapter. Implementations handle platform-specific logic.
pub trait ChannelAdapter: Send + Sync {
    /// Called when a new agent is created. Create a topic/thread/channel.
    fn on_agent_created(&self, name: &str);
    /// Called when an agent is removed. Optionally archive the topic.
    fn on_agent_removed(&self, name: &str);
    /// Send a message to a specific agent's topic/thread. Returns message ID if available.
    fn send_to_agent(&self, agent: &str, text: &str) -> Option<String>;
    fn send_to_agent_ext(
        &self,
        agent: &str,
        text: &str,
        _format: &str,
        _reply_to: Option<&str>,
    ) -> Option<String> {
        self.send_to_agent(agent, text)
    }
    fn react(&self, _agent: &str, _message_id: &str, _emoji: &str) -> Result<(), String> {
        Ok(())
    }
    fn edit_message(&self, _agent: &str, _message_id: &str, _text: &str) -> Result<(), String> {
        Ok(())
    }
    /// Send a notification to the general/default topic.
    fn notify(&self, text: &str);
    /// Poll for incoming messages (blocking, with timeout).
    fn poll(&self) -> Vec<IncomingMessage>;
    /// Get the adapter name (e.g. "telegram", "discord").
    fn name(&self) -> &str;
}

/// Manages all channel adapters and agent→topic routing.
pub struct ChannelManager {
    adapters: Vec<Arc<dyn ChannelAdapter>>,
}

impl ChannelManager {
    pub fn new() -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            adapters: Vec::new(),
        }))
    }

    pub fn add_adapter(&mut self, adapter: Box<dyn ChannelAdapter>) {
        tracing::debug!(adapter = %adapter.name(), "registered adapter");
        self.adapters.push(Arc::from(adapter));
    }

    /// Clone adapter refs for polling outside the lock.
    pub fn adapters_clone(&self) -> Vec<Arc<dyn ChannelAdapter>> {
        self.adapters.clone()
    }

    pub fn has_adapters(&self) -> bool {
        !self.adapters.is_empty()
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

    pub fn send_to_agent(&self, agent: &str, text: &str) -> Option<String> {
        self.send_to_agent_ext(agent, text, "text", None)
    }

    pub fn send_to_agent_ext(
        &self,
        agent: &str,
        text: &str,
        format: &str,
        reply_to: Option<&str>,
    ) -> Option<String> {
        let mut last_id = None;
        for adapter in &self.adapters {
            last_id = adapter
                .send_to_agent_ext(agent, text, format, reply_to)
                .or(last_id);
        }
        last_id
    }

    pub fn notify(&self, text: &str) {
        for adapter in &self.adapters {
            adapter.notify(text);
        }
    }

    /// Poll all adapters for incoming messages.
    /// Polls sequentially — each adapter should return within its timeout.
    /// Caller should not hold the Mutex lock longer than necessary.
    pub fn poll_all(&self) -> Vec<IncomingMessage> {
        self.adapters.iter().flat_map(|a| a.poll()).collect()
    }
    pub fn react(&self, agent: &str, message_id: &str, emoji: &str) -> Result<(), String> {
        for a in &self.adapters {
            a.react(agent, message_id, emoji)?;
        }
        Ok(())
    }
    pub fn edit_message(&self, agent: &str, message_id: &str, text: &str) -> Result<(), String> {
        for a in &self.adapters {
            a.edit_message(agent, message_id, text)?;
        }
        Ok(())
    }
}

/// Null adapter — no-op for local dev/testing without external channels.
pub struct NullAdapter;

impl ChannelAdapter for NullAdapter {
    fn name(&self) -> &str {
        "null"
    }
    fn on_agent_created(&self, _name: &str) {}
    fn on_agent_removed(&self, _name: &str) {}
    fn send_to_agent(&self, _agent: &str, _text: &str) -> Option<String> {
        None
    }
    fn notify(&self, _text: &str) {}
    fn poll(&self) -> Vec<IncomingMessage> {
        vec![]
    }
}
