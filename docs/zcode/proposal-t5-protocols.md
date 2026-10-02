# 提案：T5 — z.ai 通道多协议形态支持（OpenAI / Anthropic 跟随入站协议）

> **状态：Working Proposal（待用户确认方向后进入实现）**
> 决策类别：Feature（通道能力扩展，含一处对既有记录的事实勘误）
> 分支：`feat/zcode-subscription`
> 归属：`docs/zcode/proposal.md` 第五阶段；**闭案 `docs/zai/notes.md` §8 开放问题 #1**（"Anthropic passthrough only, or also OpenAI-like chat/completions"），落地其 §2.3 预留的 phase-2 端点事实
> 前置：T4 修订 #5（分发模式收敛为 GLM 确定性路由）已实现

---

## 1. 问题（与解法无关）

z.ai/zcode 通道当前只讲 Anthropic 格式，且 GLM 确定性路由只存在于 Anthropic 入站处理器（`handlers/claude.rs`）。OpenAI 协议入站（`/v1/chat/completions`、`/v1/completions`、`/v1/responses`）的 GLM 请求没有任何通道判定，全部落入 Google 池。该问题在移除"OpenAI 处理器"这一具体实现后依然成立：**通道协议形态与入站协议解耦缺失——非 Anthropic 协议的 GLM 请求永远无法进入订阅额度通道。** 实际后果（2026-10-02 调试日志实证）：Codex CLI 经 `/v1/responses` 请求 `GLM-5.3-Flash` 被发往 Google 上游，撞上 Google 账号 RESOURCE_EXHAUSTED 429，而订阅额度闲置。

## 2. 事实基础（实现前核查）

1. **「多协议支持」UI 区块不是配置**：它是三张信息卡（OpenAI `/v1/chat/completions|/v1/completions|/v1/responses`、Anthropic `/v1/messages`、Gemini `/v1beta/models`），三个协议族常开、无落盘开关；`selectedProtocol` 仅为前端高亮状态。用户提案中"如果这里配置的是 OpenAI 协议"的前提需要勘误（见 §3 层 3）。
2. **上游端点协议事实**（`docs/zai/notes.md` §2.3，官方文档级）：z.ai 提供 OpenAI 兼容端点 `POST https://api.z.ai/api/paas/v4/chat/completions`，并有 **Coding Plan 专用 OpenAI 兼容端点 `https://api.z.ai/api/coding/paas/v4`**（官方注明 coding plan 场景使用）；BigModel 对应 `open.bigmodel.cn/api/paas/v4`。
3. **未验证边界**：订阅 API Key（OAuth 开通的 `zcode-api-key`）在 `paas/v4` / `coding/paas/v4` 端点上的可用性**未经真机确认**——T2 协议事实只覆盖 `/api/anthropic` 端点（订阅 Key 免验证码、直耗订阅额度）。
4. `/v1/responses`（Codex）是独立 Responses schema，z.ai 任何上游端点均不原生支持——**无论选哪个上游端点，Responses 都必须经网关转换**。
5. 现有管道：openai.rs 入站 → Gemini IR → Google 上游；出站侧已有 IR→各协议 SSE 发射器（Google 路径复用中）；z.ai 通道为 Anthropic 直传（`docs/zai/provider.md` 既定例外）。

## 3. 提案方向（三层）

### 层 1 — 路由判定（与 T4 修订 #5 同语义，扩展入站点）

在 `handle_chat_completions` 与 `handle_completions` 的上游选择点加入与 `claude.rs` 完全一致的固定判定：`zai.enabled && Key 池非空 && is_glm_model` → 进 z.ai 通道，否则走 Google。Gemini 入站（`/v1beta`）按同语义评估是否纳入（GLM 模型经 Gemini 协议请求属边缘场景，首版可不做并在提案记录为不做）。

### 层 2 — 通道 OpenAI 形态（有界实验落定 S2a/S1b）

- **S2a 原生直传（优先验证）**：OpenAI 入站 → 原样转发 **`api.z.ai/api/coding/paas/v4/chat/completions`**（对照 `paas/v4`），模型名走既有 `model_mapping`/规范化；完整复用 `ZaiKeyPool` 轮询、状态机、有界失败转移。零格式转换成本。
- **S1b 网关转换（兜底）**：若实验证明订阅 Key 在 OpenAI 端点不可用，则 OpenAI 入站 → 转换为 Anthropic messages → 既有 `/api/anthropic` 通道；需新增 OpenAI↔Anthropic 请求/响应/SSE 双向转换器（工程量显著更高）。
- **`/v1/responses`（Codex）**：两条策略下都需要 Responses→目标格式转换。推荐 **Responses→chat/completions 请求转换 + chat SSE→Responses SSE 事件转换 + S2a 端点**（比 Responses→Anthropic 少一层；Codex 重度依赖 tool calls，转换器必须覆盖 tool_call 双向映射——本设计最大工程风险点）。备选（管道化）：Responses→IR→(新)IR→Anthropic + (新)Anthropic SSE→IR + 复用既有 IR→Responses 发射器；工程量最大但最符合 Pipeline-First 长期架构。
- **响应侧复用**：S2a 下 chat/completions 响应即 OpenAI 形态，chat/completions 入站零转换；仅 /v1/responses 需要事件层转换。

### 层 3 — "跟随多协议配置"的落地解释（按请求跟随）

多协议区块无落盘配置（§2.1 勘误）。本设计采用**按请求跟随**：客户端以何种协议入站，z.ai 通道即以对应格式承接并转发到匹配形态的上游端点（OpenAI 入 → OpenAI 形态；Anthropic 入 → Anthropic 形态）。"如果配置的是 OpenAI 协议，通道就只支持 OpenAI 协议"在按请求跟随下自然成立——每条请求的通道形态与其入站协议严格一致。多协议信息卡的文案同步修正，明示"z.ai 通道跟随入站协议"。

**明确不做（另案）**：若产品上需要"全局锁定单一入站协议"的真配置（限制哪些入站端点可用），这会影响 Google 路径的入站面，属独立决策，不混入本提案。

## 4. 备选方案与落选原因

| 备选 | 落选原因 |
|---|---|
| A. 仅做 S1b（全部转换到 Anthropic 端点，单一上游格式） | 可行但放弃零转换直传；在订阅 Key 于 OpenAI 端点可用性未证伪前，先背最重的转换工程不经济 |
| B. 仅支持 /v1/responses（只修 Codex 痛点） | 治标；chat/completions 的 GLM 缺口仍在，且 /v1/responses 恰是转换工作量最大的入站 |
| C. 引入全局协议选择配置并闸门所有入站端点 | 影响 Google 路径入站面（三协议常开是既有契约），范围爆炸；按请求跟随已满足"通道形态=入站协议"诉求 |
| D. 维持现状（GLM 走 Google 池 + 用户改用 Anthropic 客户端） | 不解决问题；Codex 只讲 Responses 协议，无 Anthropic 形态可选 |

## 5. 有界实验（S2a 可行性，实现 PR 内最先执行）

- **观察**：真机用订阅 API Key 分别 curl `api.z.ai/api/coding/paas/v4/chat/completions` 与 `api.z.ai/api/paas/v4/chat/completions`，GLM 模型、小 `max_tokens`；以 `/api/anthropic` 同 Key 请求作对照组。
- **接受边界**：HTTP 200 且返回 `chat.completions` 结构、billing 可见订阅额度消耗 → S2a 落定（两端点择优：优先 coding 专用端点）；401/403/404 或计费不入订阅 → 回落 S1b。
- **停止条件**：三组对照完成即停，不扩展探索面；结果回写本节并升格为稳定事实。

## 6. 验收标准与证据映射（草案，实现 PR 回写）

| # | 验收 | 失败面 | 直接证据 |
|---|---|---|---|
| A1 | Codex `/v1/responses` 请求 `GLM-5.3-Flash` 返回 200，debug_exchanges 显示走 z.ai 通道 | 路由 + 转换 | 真机（需订阅账号）+ 交换日志 |
| A2 | `/v1/chat/completions` GLM 模型流式/非流式均成功且形态为 OpenAI | 通道形态 | curl + 上游日志 |
| A3 | `/v1/messages` Anthropic 路径零回退 | 回归 | 既有金指标 curl |
| A4 | 非 GLM 模型在全部协议入站下仍走 Google | 负面保证 | 日志核验 |
| A5 | OpenAI 形态转发复用池轮询/状态机/有界失败转移 | 池语义 | `cargo test zai` 扩展用例 |
| A6 | 提供商开关关闭时所有协议入站的 GLM 请求不进 z.ai 通道 | 开关语义 | curl + 日志 |

## 7. 风险与主动放弃

1. **订阅 Key 对 OpenAI 端点可用性未知**——以 §5 有界实验闭案，不预设结果。
2. **Responses↔chat 转换的语义损耗**（tool calls、reasoning 事件、`web_search` 内部请求等）——Codex 工具调用为最高优先适配面；转换器以真实 Codex 会话抓包为规格来源。
3. **coding/paas/v4 与 paas/v4 的计费/限流差异未知**——实验对照覆盖。
4. **主动放弃**：全局协议闸门（§3 层 3）；Gemini 协议入站的 GLM 路由（首版不做，记录于层 1）；IR 管道化转换（备选保留，除非 S2a/S1b 均失败才重估）。
