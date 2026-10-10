//! The plugin manifest and the `credpool` provider declaration.
//!
//! `gray plugin install` reads `<bin> manifest`; the sidecar answers the
//! same object on `plugin/manifest`. The provider row is what makes
//! `provider/chat` per-request rotation reachable: gray's host calls it
//! before every chat request and POSTs to the relay URL it returns.

use serde_json::{Value, json};

/// Slash command routed through `command/run`.
pub const COMMAND: &str = "/credpool";

/// CLI verbs the host forwards as `gray credpool <verb>` (completion).
pub const COMPLETION: &[&str] = &[
    "add", "remove", "list", "status", "mark", "rotate", "clear", "use", "resolve", "env",
    "apply", "config", "request", "models", "setup",
];

pub fn manifest() -> Value {
    json!({
        "name": crate::PLUGIN_NAME,
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": crate::PROTOCOL,
        "sidecar_args": ["sidecar"],
        "commands": [COMMAND],
        "completion": COMPLETION,
        "hooks": ["prompt/context", "tool/after"],
        "capabilities": ["provider.credentials"],
        "tools": [
            {
                "name": "credpool_status",
                "label": "Credpool Status",
                "description": "Show credential pool state: cold/hot keys, cooldown expiries, next-available time. Pass provider to scope to one pool.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "provider": {"type": "string", "description": "Pool/provider name; omit for all pools"}
                    }
                }
            },
            {
                "name": "credpool_add",
                "label": "Credpool Add Key",
                "description": "Add a key reference to a provider pool. key_ref is env:VAR, file:PATH, cmd:PROG…, or auth:NAME — never a raw key.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "provider": {"type": "string"},
                        "key_ref": {"type": "string", "description": "env:VAR | file:PATH | cmd:PROG ARGS | auth:NAME"},
                        "label": {"type": "string", "description": "Optional display label (defaults to the ref)"},
                        "base_url": {"type": "string", "description": "Upstream base URL the keys authenticate (sets it when the pool is new)"}
                    },
                    "required": ["provider", "key_ref"]
                }
            },
            {
                "name": "credpool_remove",
                "label": "Credpool Remove Key",
                "description": "Remove a pool entry by id or label.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "provider": {"type": "string"},
                        "id": {"type": "string", "description": "Entry id or label"}
                    },
                    "required": ["provider", "id"]
                }
            },
            {
                "name": "credpool_mark",
                "label": "Credpool Mark Hot",
                "description": "Bench a key: mark it exhausted until now+TTL (401→5m, 429/other→60m, sole-key pools capped at 60s) or a given reset instant. Rotation then picks the next cold key.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "provider": {"type": "string"},
                        "id": {"type": "string", "description": "Entry id/label; default: the last-selected key"},
                        "status": {"type": "integer", "description": "HTTP status that failed (401, 402, 429, …)"},
                        "reason": {"type": "string", "description": "billing | rate_limit | auth | error"},
                        "seconds": {"type": "number", "description": "Cooldown override in seconds"},
                        "dead": {"type": "boolean", "description": "Mark permanently dead until credpool_clear"}
                    },
                    "required": ["provider"]
                }
            },
            {
                "name": "credpool_rotate",
                "label": "Credpool Rotate",
                "description": "Advance to the next cold key without benching anything (demotes the current pick to the back of the order).",
                "parameters": {
                    "type": "object",
                    "properties": {"provider": {"type": "string"}},
                    "required": ["provider"]
                }
            },
            {
                "name": "credpool_clear",
                "label": "Credpool Clear",
                "description": "Lift cooldowns: `id` clears one entry, omit it to clear the whole pool (dead entries too).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "provider": {"type": "string"},
                        "id": {"type": "string"}
                    },
                    "required": ["provider"]
                }
            }
        ],
        "providers": [{
            "id": crate::PROVIDER_ID,
            "name": "Credential pool",
            "transport": {
                "kind": "openai-responses",
                // Placeholder: the host always POSTs to the per-request
                // relay_url this plugin returns, never this base.
                "base_url": "https://127.0.0.1:1/",
                "authorization": {"kind": "bearer", "secret_name": "relay_token"},
                "request": {
                    "prompt_cache_key": false,
                    "store": false,
                    "include_reasoning_encrypted": true,
                    "previous_response_id": false,
                    "tool_choice": "auto",
                    "parallel_tool_calls": true
                }
            },
            "auth_methods": [{
                "id": crate::AUTH_METHOD_ID,
                "name": "Key pool",
                "kind": "api_key",
                "operations": ["models", "chat"]
            }]
        }]
    })
}
