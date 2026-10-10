# gray-credpool

Multi-key credential pool for gray providers — a port of the hermes-agent
credential pool (`agent/credential_pool.py`, `fallback_cooldown.py`,
`rate_limit_credits.py`, `credential_sources.py`).

One pool per provider holds API-key **references** (never raw key material
on disk), cools keys off on 401/402/403/429/5xx, honors `Retry-After`, and
rotates to the next cold key. Rotation happens inside a per-request
loopback relay, so failover is invisible to the agent.

## Install

```sh
cargo build --release
gray plugin install ./target/release/gray-credpool   # or: check it first
gray plugin check ./target/release/gray-credpool
```

Register the provider it serves:

```sh
gray credpool add <provider> <key-ref> [--label L] [--base-url URL]
gray credpool use <provider>          # default pool for the relay
```

Then connect `credpool` in gray (`auth_ref = plugin:credpool:credpool:pool`)
and pick a model. Each chat request mints a fresh `127.0.0.1` relay that
injects the selected key and fails over mid-request on rate limits.

## Key references

The pool file (`~/.gray/credpool.json`) stores references only:

| Form | Resolves to |
|---|---|
| `env:VAR` | the environment variable |
| `file:PATH` | first line of the file (`~` expands) |
| `cmd:PROG ARGS…` | stdout of the command (trimmed) |
| `auth:NAME` | an `AuthEntry::Key` row in gray's own `auth.json` |

Raw keys are rejected at `add` — they can never land in the pool file.

## Tools (model-facing)

`credpool_status` · `credpool_add` · `credpool_remove` · `credpool_mark` ·
`credpool_rotate` · `credpool_clear` — see `--help`-style descriptions in
the manifest; `/credpool <verb>` maps to the same ops.

## CLI

```sh
gray credpool status [provider]       # cold/hot, cooldowns, next-available
gray credpool add demo env:OPENAI_API_KEY --base-url https://api.openai.com/v1
gray credpool mark demo --status 429  # bench last-picked key (60m default;
                                      # sole-key pools cap at 60s)
gray credpool rotate demo             # demote current pick, take next cold
gray credpool clear demo              # lift cooldowns (--id for one entry)
gray credpool apply demo --activate   # api_key shim: write selected key to
                                      # auth.json + config.json (next start)
gray credpool request demo            # one failover walk, verbose
```

## Mock endpoint (verification, no live keys)

A `mock:` base_url serves canned responses from a JSON file, keyed by the
*resolved* key material:

```json
{"status": 200, "body": {"ok": true},
 "models": [{"id": "mock-1", "name": "Mock One"}],
 "keys": {"test-key-1": {"status": 429, "retry_after": 30},
          "test-key-2": {"status": 429}}}
```

`gray credpool request demo` then walks key1 → 429 (hot) → key2 → 429
(hot) → key3 → 200, printing each attempt. `tests/failover.rs` automates
this, including a real loopback-relay POST.

## Wire methods

Claims: `tools`, `commands: ["/credpool"]`, `hooks: ["prompt/context",
"tool/after"]`, `capabilities: ["provider.credentials"]`,
`providers: [credpool]` at protocol `1.2`.

Handles `plugin/manifest`, `provider/chat`, `provider/models`,
`tool/call`, `command/run`, `prompt/context`, `tool/after`,
`plugin/shutdown`.

## Limits

`tool/after` cannot see provider HTTP failures — only tool results that
embed an upstream-looking 401/429 marker (opt-in via
`config <pool> --watch-tool-errors`). For `api_key`-mode providers the
`apply`/`env` shim only reaches the *next* provider construction; true
per-request failover for those needs a core change — see
`docs/INTEGRATION.md`.
