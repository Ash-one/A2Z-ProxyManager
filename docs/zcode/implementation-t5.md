# 决策记录：T5 — z.ai 通道多协议形态支持（OpenAI / Anthropic 跟随入站协议，S1b 双向网关转换）

> 状态：已实现（Idea → Result 收敛，沿 `docs/zcode/` 惯例）  
> 归属提案：`docs/zcode/proposal-t5-protocols.md`、`docs/zcode/proposal.md` §2 T5  
> 分支：`feat/zcode-subscription`  
> 闭案：`docs/zai/notes.md` §8 开放问题 #1（"Anthropic passthrough only, or also OpenAI-like chat/completions"）

---

## 1. 交付结果（现在为真的事实）

- **GLM 确定性路由全面覆盖 OpenAI 协议入站**：
  在 `/v1/chat/completions`、`/v1/completions` 与 `/v1/responses`（Codex 入口）的上游分流点统一挂载 `zai_openai_bridge::should_divert` 判定（与 `claude.rs` 固定分发语义保持一致：提供商开启 + Key 池非空 + GLM 系模型）。彻底修复 Codex CLI 请求 `GLM-5.3-Flash` 误入 Google 账号池导致 429 报错的核心痛点；非 GLM 流量严格维持原路径转发至 Google 账号池。
- **S1b 网关双向转换器（`src-tauri/src/proxy/providers/zai_openai_bridge.rs`）**：
  - **请求端适配**：
    - `convert_chat_request`：解析 `messages[]` 与 `instructions` 提取 `system` 提示词；将 OpenAI `tools`（含 `parameters`）规范化为 Anthropic `tools`（`input_schema`）；将 `tool` 角色消息（工具结果）对齐并入对应 `user` 消息（`tool_result` 块）；将 `assistant` 工具调用还原为 `tool_use` 块；保留 `temperature`、`top_p`、`stream`、`stop_sequences` 与 `tool_choice`。
    - `convert_responses_request`：解析 Codex 的 `instructions` 与 `input` 结构（扁平字符串或包含 `message`、`function_call`、`function_call_output` 的条目数组），平滑映射至 Anthropic messages。
  - **通道内核全量复用**：
    转换后的请求复用既有成熟的 `forward_anthropic_json` 转发内核，完整享受 `ZaiKeyPool` 多账号轮询、429/402 状态机退避、有界故障转移、Plan 通道 14 项客户端指纹头仿真与 90 秒阿里云验证码注入能力，免除任何上游新鉴权风险。
  - **响应端反向映射**：
    - `convert_anthropic_json_to_chat`：输出合规的 `chat.completion` 结构，转换 content 文本、`tool_calls` 与 `finish_reason`（stop / tool_calls / length）；
    - `convert_anthropic_json_to_responses`：输出合规的 Codex `response` 结构，构建 `output` 消息与 `function_call` 条目及 token usage。
- **SSE 流式双向转换状态机（`SseBridgeState`）**：
  - 流式消费 Anthropic 原生 SSE 帧（`message_start`、`content_block_start`、`content_block_delta`、`content_block_stop`、`message_delta`、`message_stop`）；
  - Chat Completions 模式：实时转换为 OpenAI `chat.completion.chunk` 增量事件，准确下发文本 delta、工具调用参数碎片及带有 `finish_reason` 的结束帧与 `[DONE]` 终止符；
  - Responses 模式：实时转换为 Codex 协议事件序列（`response.created`、`response.in_progress`、`response.output_item.added`、`response.text.delta`、`response.function_call_arguments.delta`、`response.output_item.done`、`response.completed`、`response.done`）。
- **形态跟随入站协议**：
  客户端以何种协议调用，通道即输出对应协议，无需全局切换配置；多协议信息展示卡文案语义对齐。

---

## 2. 有界实验与实现期决策

| # | 决策 | 理由与实证数据 | 授权 |
|---|---|---|---|
| 1 | **落定 S1b 网关转换，否决 S2a 原生直传** | **实验证伪 S2a**：使用订阅 JWT / 订阅 Key 分别请求 `api.z.ai/api/coding/paas/v4` 与 `api.z.ai/api/paas/v4` 两个 OpenAI 兼容端点均返回 `401 Unauthorized`（上游 PaaS 端点只认开发者 API Key，不兼容订阅体系凭证）。S1b 转换到既有 `/api/anthropic` 与 Plan 通道无鉴权壁垒，复用完整 Key 池与验证码机制 | 提案 §5 有界实验标准驱动 |
| 2 | **按请求动态跟随入站协议** | 多协议卡片非落盘配置；客户端入站协议即为响应协议形态，兼顾所有客户端生态，无全局协议互斥锁定副作用 | 用户提案核准 |
| 3 | **Gemini 协议入站首版暂不纳入** | GLM 模型经 Gemini 协议请求属边缘冷门场景，保持实现聚焦 Codex 与 OpenAI 核心路径 | 提案 §3 层 1 约定 |

---

## 3. 验收证据（实际执行）

| # | 验收项 | 验证手段 | 执行结果 |
|---|---|---|---|
| A1 | 路由分流：GLM 系列模型进入 bridge，非 GLM 走 Google 池，提供商关闭或池空时不分流 | 单元测试 `test_should_divert` + `test_is_glm_model_name` | **通过**：多模型前缀与配置开关判定均符合预期 |
| A2 | Chat Completions 基础参数与消息体转换为 Anthropic messages | 单元测试 `test_convert_chat_request_basic` | **通过**：system 提取、messages、max_tokens、stream 等字段正确转换 |
| A3 | Chat 复杂工具调用与工具结果层序合并 | 单元测试 `test_convert_chat_request_tools_and_tool_results` | **通过**：assistant tool_calls 与 user tool_result 正确关联合并，tool_choice any 映射正确 |
| A4 | Codex Responses 基础与工具流转换 | 单元测试 `test_convert_responses_request_*` | **通过**：instructions、input 数组、function_call/output 正确映射 |
| A5 | Anthropic JSON 响应反向转换为 Chat 与 Responses 对象 | 单元测试 `test_convert_anthropic_json_to_*` | **通过**：对象结构、字段、tool_calls 与 usage 映射无误 |
| A6 | SSE 流式转换状态机完整性（Chat 块流与 Responses 事件流） | 单元测试 `test_sse_bridge_*_streaming` | **通过**：全量 SSE 生命周期事件顺序与格式完全符合规范 |
| A7 | 既有模块回归与前端全量构建 | `cargo test zai` + `cargo test zcode` + `npm run build` | **通过**：zai 27 例全绿，zcode 31 例全绿，前端 build 耗时 15s 零错误 |

---

## 4. 边界与已知限制

1. **历史推理条目（Reasoning）**：Codex 历史请求中的 `reasoning` 块在 T5 v1 首版中予以跳过，不反灌入上下文；
2. **服务端历史恢复（previous_response_id）**：Codex 客户端默认采用 `store=false` 发送完整对话历史，因此服务端历史缓存未走本桥接，不影响正常使用；
3. **Gemini 入站（/v1beta）**：暂不进行 GLM 桥接，留待后续若有需求另案评估。
