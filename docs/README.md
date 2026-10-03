# Documentation index

This folder contains developer-focused documentation (architecture, implementation details, and validation steps).

## Proxy
- [`docs/proxy/auth.md`](proxy/auth.md) — proxy authorization modes, expected client behavior, and implementation pointers.
- [`docs/proxy/accounts.md`](proxy/accounts.md) — account lifecycle in the proxy pool (including auto-disable on `invalid_grant`) and UI behavior.

## z.ai (GLM) integration
- [`docs/zai/implementation.md`](zai/implementation.md) — end-to-end “what’s implemented” and how to validate it.
- [`docs/zai/mcp.md`](zai/mcp.md) — MCP endpoints exposed by the proxy (Search / Reader / Vision) and upstream behavior.
- [`docs/zai/provider.md`](zai/provider.md) — Anthropic-compatible passthrough provider details and dispatch modes.
- [`docs/zai/vision-mcp.md`](zai/vision-mcp.md) — built-in Vision MCP server protocol and tool implementations.
- [`docs/zai/notes.md`](zai/notes.md) — research notes, constraints, and future follow-ups (budget/usage, additional endpoints).

## zcode (Z.AI Coding Plan) integration
- [`docs/zcode/proposal.md`](zcode/proposal.md) — **proposal (T1~T5 implemented)**: pooling zcode subscription (OAuth JWT) and API-key accounts alongside antigravity; phased plan, alternatives, acceptance criteria, and open decisions.
- [`docs/zcode/implementation.md`](zcode/implementation.md) — **stable decision record (T1)**: z.ai/BigModel API key pool — independent `ZaiKeyPool`, classification-aligned state machine, pooled slot semantics, migration & headless parity, acceptance evidence.
- [`docs/zcode/implementation-t2.md`](zcode/implementation-t2.md) — **stable decision record (T2)**: OAuth password-free login (ZCode CLI flow) → auto-provisioned subscription API key + paired Plan JWT into the pool, manual import detection, on-demand quota query, clean-room protocol-facts declaration.
- [`docs/zcode/implementation-t3.md`](zcode/implementation-t3.md) — **stable decision record (T3)**: Plan-channel emulation — per-request captcha verify_param injection, per-account device profiles, claim/quota panels, case-sensitive model canonicalization.
- [`docs/zcode/implementation-t4.md`](zcode/implementation-t4.md) — **stable decision record (T4)**: promoting subscription account management to a first-class standalone page (`/zcode-accounts`) and collapsing dispatch modes into fixed deterministic routing, superseding T2 decision #3.
- [`docs/zcode/proposal-t4-page.md`](zcode/proposal-t4-page.md) — **proposal (T4)**: standalone page design record and original acceptance mapping (superseded by `implementation-t4.md`).
- [`docs/zcode/implementation-t5.md`](zcode/implementation-t5.md) — **stable decision record (T5)**: multi-protocol z.ai channel — GLM deterministic routing extended to OpenAI inbound paths (`/v1/responses`, `/v1/chat/completions`), S1b bidirectional gateway converter, channel format follows inbound protocol; closes open question in `docs/zai/notes.md` §8.
- [`docs/zcode/proposal-t5-protocols.md`](zcode/proposal-t5-protocols.md) — **proposal (T5)**: multi-protocol channel design record and bounded experiment validation (superseded by `implementation-t5.md`).

## Agent Integrations
- [`docs/jeikcode_integration.md`](jeikcode_integration.md) — JeikCode integration guide, one-click synchronization, configuration options, and KV-cache optimization.
