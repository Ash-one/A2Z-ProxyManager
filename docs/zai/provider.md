# z.ai provider (Anthropic-compatible passthrough)

## Idea
Support z.ai (GLM) as an optional upstream for **Anthropic-compatible requests** (`/v1/messages`), without applying any Google/Gemini-specific transformations when z.ai is selected.

This keeps compatibility high (request/response shapes stay Anthropic-like) and avoids coupling z.ai traffic to the Google account pool.

## Result
We added an optional “z.ai provider” that:
- Is configured in proxy settings (`proxy.zai.*`).
- Can be enabled/disabled and used via dispatch modes.
- Forwards `/v1/messages` and `/v1/messages/count_tokens` to a z.ai Anthropic-compatible base URL.
- Streams responses back without parsing SSE.
- **[zcode T1]** Pools multiple API keys with round-robin scheduling, per-key runtime state aligned to `UpstreamClassification`, and bounded account-level failover inside a single request.
- **[zcode T2]** OAuth password-free login (ZCode CLI flow) provisions the subscription API key automatically and adds it to the pool; the Coding-Plan JWT is stored alongside (paired by `account_id`) for quota queries and the future T3 Plan channel, and is excluded from message forwarding until then.

## Configuration
Schema: `src-tauri/src/proxy/config.rs`
- `ZaiConfig` in `src-tauri/src/proxy/config.rs`
- `ZaiDispatchMode` in `src-tauri/src/proxy/config.rs`
- `ZaiProvider` / `ZaiKeyEntry` / `ZaiKeyMode` in `src-tauri/src/proxy/config.rs` ([zcode T1/T2])

Key fields:
- `proxy.zai.enabled`
- `proxy.zai.base_url` (default `https://api.z.ai/api/anthropic`; a custom gateway overrides per-provider canonical URLs, canonical values resolve per key; [zcode T2] the `zcode_plan` provider is a separate subscription domain and ignores the global override)
- `proxy.zai.keys` — [zcode T1] API key pool entries `{ key, provider: zai|bigmodel, enabled, label? }`; canonical upstreams: `zai` → `https://api.z.ai/api/anthropic`, `bigmodel` → `https://open.bigmodel.cn/api/anthropic`; [zcode T2] entries additionally carry `mode: api_key|jwt`, `account_id`, `user_email`, `business_jwt` (management token for billing queries, never used for message forwarding); `provider: zcode_plan` → `https://zcode.z.ai`
- `proxy.zai.api_key` — legacy single-key field, kept for migration compat only (`resolved_keys()` migrates it into a single pool entry; ignored when `keys` is non-empty)
- `proxy.zai.dispatch_mode`:
  - `off`
  - `exclusive`
  - `pooled` (each **available key** = one pool slot, next to the Google accounts)
  - `fallback`
- `proxy.zai.models` default mapping for `claude-*` request models:
  - `opus`, `sonnet`, `haiku`

Headless parity ([zcode T1]): `ABV_ZAI_KEYS` > `ZAI_KEYS` env var overrides the key pool at startup (comma/semicolon/newline separated entries, optional `zai:`/`bigmodel:` provider prefix) and persists into `gui_config.json`.

## Routing logic
Entry point: [`src-tauri/src/proxy/handlers/claude.rs`](../../src-tauri/src/proxy/handlers/claude.rs)
- `handle_messages(...)` decides whether to route the request to z.ai or to the existing Google-backed flow.
- `pooled` mode uses round-robin across `(google_accounts + available_zai_keys)` slots; zai-side slots each resolve to a key via the key pool.

## Upstream implementation
Provider implementation: [`src-tauri/src/proxy/providers/zai_anthropic.rs`](../../src-tauri/src/proxy/providers/zai_anthropic.rs)
- Forwarding is conservative about headers (does not forward the proxy’s own auth key).
- Injects z.ai auth (`Authorization` / `x-api-key`) and forwards the request body as-is.
- Uses the global upstream proxy config when configured.
- **[zcode T1]** Key pool & state machine: [`src-tauri/src/proxy/providers/zai_pool.rs`](../../src-tauri/src/proxy/providers/zai_pool.rs) — round-robin over available keys; account-level failures (429/529 → Retry-After rate limit, 5xx → short cooldown, 401/403 → invalid, 402 → exhausted) are marked and the request fails over to the next available key (bounded, ≤4 attempts) before any bytes reach the client. Request-level errors (404 model-not-found, 400 signature errors) pass through untouched. See `docs/zcode/implementation.md` for the owning decision.
- **[zcode T2]** OAuth & subscription chain: [`src-tauri/src/proxy/providers/zcode_oauth.rs`](../../src-tauri/src/proxy/providers/zcode_oauth.rs) — CLI login (`zcode.z.ai/api/v1/oauth/cli/init` + `poll/{flow_id}`, `3004` = session expired), business JWT derivation (`/api/auth/z/login`), API-key provisioning chain (reuses/creates `zcode-api-key`), and on-demand subscription queries (`/api/biz/subscription/list`). Tauri commands `zcode_oauth_start`/`zcode_oauth_poll`/`zcode_query_quota` with Web-route parity (`/api/zcode/*`). See `docs/zcode/implementation-t2.md` for the owning decision.
- MCP / Vision / model-list helpers follow the first available **API-key** pool entry (`primary_api_key()` skips `mode=jwt` entries); they target z.ai-domain endpoints only and do not rotate keys.

## Validation
1) Enable z.ai in the UI (`src/pages/ApiProxy.tsx`) and set `dispatch_mode=exclusive`.
   - UI: [`src/pages/ApiProxy.tsx`](../../src/pages/ApiProxy.tsx)
2) Start the proxy.
3) Send a normal Anthropic request to `POST /v1/messages`.
4) Verify the request is served by z.ai (and Google accounts are not involved for this endpoint in exclusive mode).
5) [zcode T1] With ≥2 keys configured, send consecutive requests and verify round-robin key selection; mark a key invalid (401) and verify it is skipped on subsequent requests and reported by the key status view (`get_zai_key_pool_status` / `GET /api/zai/keys/status`).
