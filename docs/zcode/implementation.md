# 决策记录：zcode T1 — z.ai / BigModel API Key 多账号池

> **状态：Stable Decision（已实现，T1 范围）**
> 取代：`proposal.md` §2 T1 的"接入现有 TokenManager"实现路线（提案保留否决/演进理由，T2/T3 仍为工作提案）
> 分支：`feat/zcode-subscription`
> 前置：`docs/zcode/proposal.md`（问题定义、备选方案、验收框架）
> 沿用惯例：`docs/zai/provider.md` 的 "Idea → Result" 结构

## Idea（问题与决策）

**问题**（承提案 §1，解法无关）：本仓库缺乏"API Key 类上游账号"的池化管理入口——单 Key 透传无轮询、无故障转移、无账号状态机，一个 Key 失效即断服。

**本决策落定的四项取舍**（实现期确认，2025-XX 提案回写 §2/§6.5）：

1. **独立 `ZaiKeyPool`，不嵌入 `TokenManager`**。提案原文写"接入现有 TokenManager"，但 `TokenManager`（约 5900 行）端到端是 Google 专属（OAuth refresh、`accounts/*.json`、quota 文件、sticky session、image scheduler），嵌入即触碰提案自身的不变量"Google 账号生命周期逻辑不动"，且 A5（Google 池零回退）回归风险高。Key 池作为平行模块挂在进程级单例（沿 `SignatureCache::global()` 先例），调度/状态机**语义**对齐 TokenManager 与 `UpstreamClassification`，代码路径完全隔离。T2 的 OAuth JWT 账号（`mode: jwt|apiKey`）也以此池为归宿。
2. **Key 运行态仅内存**（重启即清零）：Invalid/Exhausted 标记不持久化——重启后重试一次即可恢复标记，代价可忽略；避免为 T1 引入新的持久化 schema。Key 本体与启停随 `gui_config.json` 持久化（`proxy.zai.keys`）。
3. **UI 范围**：多 Key 管理放在既有 z.ai 设置卡内（`ZaiKeyPoolEditor`），Accounts 页留待 T2 OAuth 账号入池时一并接入。
4. **配置命名空间**（提案 §6.5 遗留决策）：复用 `proxy.zai`，新增 `keys: ZaiKeyEntry[]`；遗留单 `api_key` 字段保留做迁移兼容（`keys` 非空时忽略）。

## Result（当前行为）

### 配置模型（`src-tauri/src/proxy/config.rs`）

- `ZaiProvider`：`zai`（`https://api.z.ai/api/anthropic`）| `bigmodel`（`https://open.bigmodel.cn/api/anthropic`），规范 base URL 常量单一真相源（提案 §6.2 纪律）。
- `ZaiKeyEntry { key, provider, enabled, label? }`；`ZaiConfig.keys: Vec<ZaiKeyEntry>`。
- **迁移**（A2）：`resolved_keys()`——`keys` 非空原样生效；否则遗留 `api_key` 迁移为单条目，provider 由 `base_url` 推断（含 `open.bigmodel.cn` → BigModel）。
- **base URL 解析**：`effective_base_url()`——全局 `base_url` 缺省或为规范地址 → 按条目 provider 解析；自定义网关 → 全局覆盖（T1 前行为不变）。
- **headless parity**（A6）：`ABV_ZAI_KEYS` > `ZAI_KEYS` 环境变量，逗号/分号/换行分隔，条目可带 `zai:`/`bigmodel:` 前缀（缺省 z.ai）；启动时覆写并持久化进 `gui_config.json`（沿 `ABV_*` 惯例）。

### Key 池与状态机（`src-tauri/src/proxy/providers/zai_pool.rs`）

- 进程级单例，配置指纹惰性同步（配置热更新路径零侵入）；按 `(key, provider)` 身份保留运行态。
- 轮询选择：每个可用 Key = 1 槽位；disabled / 空 Key / 状态不放行均跳过。
- 状态机对齐 `pipeline/policy.rs` 的 `UpstreamClassification`（单一判定真理）：

  | 上游分类 | Key 状态 | 恢复条件 |
  |---|---|---|
  | `RateLimited`（429/529） | RateLimitedUntil（Retry-After，缺省 30s） | 到期自动 |
  | `TransientServerError`（5xx） | CooldownUntil（15s 短冷却） | 到期自动 |
  | `OtherClientError(401\|403)` | Invalid | 重启或配置变更 |
  | `OtherClientError(402)` | Exhausted | 重启或配置变更 |
  | 其余（404 模型不存在、400 签名污染等请求级错误） | 不变 | — |
  | 2xx | Active（清瞬时标记） | — |

### 转发路径（`src-tauri/src/proxy/providers/zai_anthropic.rs`）

- `/v1/messages` 与 `/v1/messages/count_tokens` 转发循环内每请求轮询取 Key；**账号级失败**（429/529、5xx、401/403、402）在响应未下发客户端前原地换下一个可用 Key 重试，尝试次数 `min(可用 Key 数, 4)` 有界；请求级错误/尝试耗尽时原样透传上游错误。网络级失败（连接/超时）与 Key 无关，不更新状态机。
- 池空语义：从未配置 → `400 z.ai api_key is not set`（原行为）；全部不可用 → `503 All z.ai API keys are unavailable`（网关内部消息，防自噬）。
- 不变量保持：保守头部转发；本地鉴权 Key 永不上游注入日志/回显（状态查询仅返回掩码）。

### pooled 槽位语义（`src-tauri/src/proxy/handlers/claude.rs`）

- 从"整个 z.ai = 1 槽"扩展为"**每个可用 Key = 1 槽**"：`total = google_accounts + zai_available_keys`，`slot < zai_slots` 走 Key 池。全部 Key 不可用时槽位为 0，请求自然落到 Google 槽。

### 辅助通道

- MCP（web_search/web_reader）、Vision MCP、模型拉取（Tauri 命令 + `/api/zai/models/fetch`）跟随池内**首个可用 Key**（`primary_api_key()`）：这些端点为 z.ai 域名专属，不参与轮询。已知边界：辅助通道不做 Key 级轮询/转移（低流量，T2 再评估）。

### UI（`src/components/proxy/ZaiKeyPoolEditor.tsx`）

- z.ai 设置卡内多 Key 列表：启停 pill + 上游家族选择 + Key 输入 + 运行状态徽标（可用/已失效/已耗尽/限速中/冷却中，含剩余秒数）；保存时同步遗留 `api_key` = 首个可用 Key（向后兼容）。
- 状态查询双通道：Tauri 命令 `get_zai_key_pool_status` + Web 模式 `GET /api/zai/keys/status`（`COMMAND_MAPPING` 登记），12 语言键完整。

## 验收证据（提案 §5 对应行）

| # | 验收 | 直接证据 | 结果 |
|---|---|---|---|
| A1 | ≥2 Key 轮询；401 自动跳过并标记 INVALID | 选择/跳过/状态机纯逻辑单测（`zai_pool.rs` 内嵌 9 例 + `tests/zai_key_pool_tests.rs` 5 例）；真实端到端 | **单测通过**（`cargo test zai`，见下方命令记录）；**未验证边界**：真实 z.ai/bigmodel Key 的起服 curl 序列未执行（环境无凭证），转发循环的失败转移序列未在真实上游观察 |
| A2 | 旧单 `api_key` 配置迁移不回退 | `legacy_single_api_key_migrates_to_pool` / `legacy_bigmodel_base_url_infers_provider` / `keys_list_takes_precedence_over_legacy_api_key`（含旧格式 JSON 回放反序列化） | **单测通过** |
| A5 | Google 池零回退 | Google 路径零改动（`git diff` 仅触及 zai 消费点）；`cargo clippy --all-targets --all-features` + 既有测试 | clippy **通过**；全量测试套件按 AGENTS.md 留给 CI，本地未跑全矩阵 |
| A6 | headless parity：config 字段 + env 覆盖 | `parse_zai_keys_env` 单测 + `ABV_ZAI_KEYS` 启动覆写路径；Docker 容器内冒烟 | **env 解析单测通过**；**未验证边界**：`docker/Dockerfile.backend` 构建冒烟未执行（本地无 Docker 环境） |
| A7 | UI 状态可见；12 语言键完整 | `npm run build`（tsc）+ locale 键一致性脚本校验（12 文件 `proxy.config.zai.keys` 键集全等） | **通过** |

命令记录（本地实际执行，提交前复核）：

```text
cd src-tauri && cargo fmt -- --check                          # 通过
cd src-tauri && cargo clippy --all-targets --all-features     # 0 error；新增文件 0 warning
cd src-tauri && cargo test zai                                # 14 passed / 0 failed（9 池状态机 + 5 迁移/env）
cd src-tauri && cargo test --lib proxy::tests::               # 53 passed / 33 failed
npm run build                                                 # tsc + vite 构建通过（chunk 体积警告为预存）
```

**33 个失败的归因（基线对照）**：`git stash` 后在基线提交 `e462fb8`（无本变更）复跑同命令，结果 48 passed / **33 failed** —— 失败集与本变更工作树完全一致（均为 `security_db_tests`/`security_integration_tests` 的落盘类用例与 `retry_strategy_tests` 四例，属分支预存的测试漂移/环境依赖）；本变更后通过数 48→53，恰为新加 5 例，**无任何既有用例由通过转为失败**。全量测试矩阵按 AGENTS.md 留给 CI。

## 风险与有意放弃的能力

- **Key 运行态不持久化**（决策 2）：重启后 Invalid/Exhausted Key 会被重试一次再标记。接受：代价一次请求，换取零新增持久化 schema。
- **辅助通道不轮询**：MCP/Vision/模型拉取固定跟随首个可用 Key；若首 Key 失效，这些通道降级但主通道（/v1/messages）不受影响。
- **状态机按状态码字面映射**：403 一律 Invalid（不区分"配额型 403"）；上游若以非 402 状态表达余额耗尽，会落入限速/冷却而非 Exhausted。T2 引入 billing 查询时再细化。
- **双通道状态查询**：Tauri 与 HTTP 端点共用同一池单例，无一致性风险；但池为进程单例意味着单测与运行态天然隔离（测试用 `new()`）。

## 对后续阶段的影响

- **T2（OAuth JWT 入池）**：`ZaiKeyEntry` 扩展 `mode: jwt|apiKey`；OAuth CLI 流程、billing 轮询、JWT↔Key 回退都挂在本池，Accounts 页 UI 届时接入。
- **T3（Plan 通道仿真）**：verify_param 注入、指纹/领取配置的命名空间拆分决策仍按提案 §6.5 在实现 PR 落定。
