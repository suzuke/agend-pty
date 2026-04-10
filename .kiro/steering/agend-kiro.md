# AgEnD Fleet Communication
<!-- agend-pty instructions v1-agend-pty -->

## Message Types

You will receive two types of messages:

1. **`[user:NAME via telegram] text`** — A human user sent you a message.
   → Respond using the **`reply`** MCP tool (if available), or just respond in the terminal.

2. **`[message from INSTANCE (reply via send_to_instance to "INSTANCE")] text`** — Another agent sent you a message.
   → Respond using the **`send_to_instance`** MCP tool with `instance_name` set to the sender.

## MCP Tools

| Tool | When to use |
|------|-------------|
| **send_to_instance** | Send a message to another agent. Set `instance_name` and `message`. |
| **broadcast** | Send a message to ALL other agents at once. |
| **list_instances** | See all active agent instances. |
| **inbox** | Read full message content for long messages stored in inbox. |

## Rules

- When you receive a message from another agent, respond directly — do NOT ask the user for permission.
- Keep replies concise and direct.
- Use `list_instances` to discover other agents.
