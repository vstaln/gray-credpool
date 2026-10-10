# credpool × gray core — integration note

What this plugin can intercept today, what it cannot, and the one core
change that would make failover transparent for `api_key`-mode providers.

## The interception point that exists: `provider/chat` relay

Gray's protocol-1.2 provider flow (verified in
`crates/gray-plugin/src/provider.rs`, `crates/gray/src/auth/broker.rs`,
`crates/gray-provider/src/openai.rs`):

1. A plugin's manifest declares `providers[]` with `auth_methods[]`
   carrying `"operations": ["models","chat"]`.
2. The operator connects the provider (`auth_ref` =
   `plugin:credpool:credpool:pool`, envelope stored in `auth.json`).
3. **Before every chat request** gray calls the sidecar's
   `provider/chat` RPC (30 s TTL). The result `{relay_url, relay_token}`
   becomes a `CredentialLease`: `secrets["relay_token"]` rides in the
   `Authorization: Bearer` header; `metadata["relay_url"]` is POSTed to
   **verbatim** (openai.rs: `the full /responses URL, used verbatim`).
4. credpool's `provider/chat` mints a per-turn loopback relay on
   `127.0.0.1` and returns `http://127.0.0.1:PORT/relay/<bearer>/responses`
   + the bearer as `relay_token`.
5. Inside the relay, the pool does the real work: `select()` a cold key,
   resolve its `key_ref` (`env:`/`file:`/`cmd:`/`auth:` — material never
   stored), forward to the pool's `base_url`, and on a rotatable status
   (401/402/403/429/5xx) mark the key hot and resend with the next cold
   key — **inside the one admitted request**. Gray sees one ordinary
   response; rotation is invisible underneath it.
6. `provider/models` gets the same treatment for the model catalog
   (static list, mock file, or live `GET {base}/models` through the pool).

This is the same shape `devin-sub` uses for its subscription relay, so it
is a supported integration — no core patch needed for relay-mode
failover.

## What is NOT possible for `api_key`-mode providers

For providers configured with `credential_source != "plugin"`
(`config.api_key` / `auth.json["<provider>"]` entries):

- The key is read **once** when the provider is constructed
  (`crates/gray/src/providers/runtime.rs` builds the provider from
  `config.api_key` / `CredentialStore`). There is no per-request hook
  that can swap it mid-stream.
- Provider HTTP errors never surface to plugins as events. `CoreEvent`
  is `pre_tool` / `post_tool` / `turn_end` only; `tool/after` sees tool
  outputs, not the provider's HTTP status. The provider layer classifies
  and retries 429s internally, but a plugin cannot observe or redirect
  that.
- credpool's `apply` shim writes the *currently selected* key into
  `auth.json[<provider>]` (and optionally `config.json.api_key`), and
  `env` emits `export VAR='…'` for wrappers. Both are **lossy by
  design**: they take effect on the next agent start / provider rebuild,
  not on the request that just failed. A 429 against a baked-in key does
  not rotate anything until the operator (or a wrapper) re-applies.
- `tool/after` marking (`watch_tool_errors`) is a heuristic for tools
  that *proxy* an upstream and leak its status into their content — it
  cannot see the provider's own calls.

## The core change that would fix it

Gray already leases credentials per request via
`CredentialSource::acquire()`. The missing piece is only in the
`api_key` path:

```
Provider attempt fails with a rotatable status
  → core calls acquire() again for the retry
  → the plugin-backed CredentialSource returns a *fresh* lease
```

Concretely, two options:

**A. Re-acquire on retry.** In the provider retry loop
(`gray-provider`/`providers/runtime.rs`), call `credential_source
.acquire()` per attempt instead of once per request. For plugin-backed
sources this re-enters `provider/chat`; credpool then hands out the next
cold key. This is the smallest change and benefits every plugin provider.

**B. Generic `provider/credential` RPC.** Add an auth-method operation
that returns `{secret}` (not just `{relay_url, relay_token}`) so a
plugin can inject a rotating key directly into the transport's bearer
slot — i.e. pool rotation without a relay hop. Then `api_key`-mode
providers could be backed by a `credential_source: "plugin"` that swaps
the secret per acquire.

Either way, per-request rotation for api_key-mode providers is a core
change; the pool cannot reach it from a plugin today.

## Signals credpool already consumes

| Surface | What it does |
|---|---|
| `provider/chat` (relay) | per-request select → forward → mark-on-failover; the real rotation |
| `provider/models` | catalog through the same walk |
| `tool/after` | opt-in (`watch_tool_errors`): content carrying an upstream-looking 401/429 marker benches the last-selected key |
| `credpool_mark` / `gray credpool mark` | operator/agent benching — sole-key pools cap the cool-off at 60 s (hermes' sole-credential mercy) |
| `Retry-After` | parsed per attempt → entry `reset_at`; provider-declared resets beat the status-derived TTL |
