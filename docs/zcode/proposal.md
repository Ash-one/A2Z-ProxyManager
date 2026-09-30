# 提案：zcode（Z.AI Coding Plan）订阅账号池接入

> **状态：Working Proposal（T1 已实现 → 见 `implementation.md`；T2/T3 未实施）**
> 决策类别：Feature（新增对外可见能力）
> 分支：`feat/zcode-subscription`（自 `origin/main` 分叉；远端当前不存在 `origin/beta`，实现阶段若 beta 通道恢复，按维护协议先落 beta）
> 前置调研：本仓库架构探索（`EXPLORATION_REPORT.md`）与 `zcode2api` 参考实现分析（2025-09-30）
> 通过后的归宿：T1 已实现并重写为稳定决策记录（`docs/zcode/implementation.md`）；T2/T3 实现完成后按同惯例各落独立决策记录，本提案保留未实施部分与否决/演进理由。

---

## 1. 问题（与解法无关）

A2Z-ProxyManager 的账号池目前只承载 antigravity（Google OAuth）账号；`proxy.zai` 仅支持**单个** API Key 的静态透传（`src-tauri/src/proxy/config.rs` `ZaiConfig`）。持有 Z.AI **Coding Plan / Start Plan 订阅**（OAuth JWT 额度）的用户，必须在 A2Z-ProxyManager 之外另行运行独立网关（如 zcode2api），导致：

- 两套网关、两份配置、两个端口、两套账号生命周期管理；
- 订阅额度无法与本仓库既有的配额仪表盘、模型路由、鉴权、headless/Docker 部署、多语言 UI 融合；
- 单 Key 透传没有池化：无轮询、无故障转移、无账号状态机，一个 Key 失效即断服。

该问题在移除"引入 zcode2api"这一具体方案后依然成立：**本仓库缺乏"订阅类（JWT）上游账号"的池化管理入口**。

## 2. 提案方向（分三阶段）

全部代码**净室重实现**：zcode2api 为 AGPL-3.0，本仓库为 CC-BY-NC-SA-4.0，代码零搬运；仅以协议事实（端点、头部、状态码语义）为规格来源。

### T1 — API Key 多账号池（✅ 已实现，决策记录：`implementation.md`）

- ~~接入现有 `TokenManager` 轮询/故障转移~~ **实现期修订**：`TokenManager` 端到端 Google 专属（OAuth refresh / accounts 文件 / quota / sticky session），嵌入即触碰本提案"Google 账号生命周期逻辑不动"不变量。落定为**独立 `ZaiKeyPool`**（`providers/zai_pool.rs`，进程级单例），调度与状态机语义对齐 `pipeline/policy.rs` 的 `UpstreamClassification`，代码路径与 Google 池完全隔离；
- `ZaiConfig` 从单 `api_key` 扩展为 `keys: ZaiKeyEntry[]`（保留旧字段做迁移兼容），支持 `zai`（`api.z.ai`）与 `bigmodel`（`open.bigmodel.cn`）两种 Anthropic 兼容上游（规范 base URL 单一真相源常量）；
- 账号状态机：429/529 → Retry-After 限速（不进冷却池）、5xx → 短冷却、401/403 → INVALID、402 → EXHAUSTED；请求级错误（404/签名污染）不触碰 Key 状态；转发循环内账号级失败原地换 Key 有界重试（≤4 次）；
- `pooled` 调度语义从"z.ai = 1 个槽位"扩展为"每个可用 Key = 1 个槽位"；
- Key 运行态仅内存（重启即清零）；headless parity 经 `ABV_ZAI_KEYS`/`ZAI_KEYS` env 覆盖；UI 于 z.ai 设置卡内多 Key 管理（Accounts 页留 T2）。

### T2 — OAuth 免密登录入池 + JWT 订阅账号

- 净室实现 zcode OAuth CLI 流程：`/api/v1/oauth/cli/init` + `/poll/{flow_id}`（服务端下发 `poll_token` 优先，未下发时本地生成兼容旧协议；`3004` = 会话过期）；
- JWT 与同账号 API Key 并存：订阅额度（JWT 通道）耗尽或 429 耗尽时自动切同账号 API Key 回退通道；
- 额度查询走 billing 端点，错峰轮询（billing/* 有 WAF 拦截风险）；
- 手动导入：粘贴 JWT（3 段点分判定）或 API Key，复用现有账号导入 UI。

### T3 — Plan 通道完整仿真（订阅核心价值，第二期）

- 每请求 `X-Aliyun-Captcha-Verify-Param`（Plan 通道必需，token TTL ~2 分钟）；
- 每账号独立指纹（`device_mid` 全新 UUIDv4 + 桌面 SKU）、首启安装序仿真（`client/configs` + `app_launch`/`app_daily_active` 事件上报、`install_id` 持久化）；
- 限时套餐自动/手动领取（`preview`/`claim`、`server_time` 回执、`1005` 名额用完按 `next_at` 退避）；
- 模型名大小写敏感映射（`glm-5.3-flash → GLM-5.3-Flash` 等），按 AGENTS.md 惯例优先通配规则；
- **验证码求解路线为未决项**（见 §6），手动过码（用户浏览器滑块产生 verify_param → 端内转发）为默认起步路线。

## 3. 受影响的所有权边界

| 表面 | 变更 | 不变量 |
|---|---|---|
| `src-tauri/src/proxy/providers/` | ✅ T1 已落地：新增 `zai_pool.rs`（独立 Key 池）+ `zai_anthropic.rs` 多 Key 转发与有界失败转移；T2 新增 `zcode` Plan 通道 provider | 保守头部转发、本地鉴权 Key 永不上游注入 |
| `src-tauri/src/proxy/token_manager.rs` | **T1 实现期修订**：不改动（Google 专属全链路）；T2 的 `zai`/`zcode` provider 族（`mode: jwt|apiKey`）以 `ZaiKeyPool` 为归宿 | Google 账号生命周期逻辑零改动（T1 已保持） |
| `src-tauri/src/proxy/config.rs` | ✅ T1 已落地：`ZaiProvider`/`ZaiKeyEntry`/`keys` + 迁移解析 + env 覆盖；T3 配置另议 | 旧单 `api_key` 字段迁移兼容；headless parity（config 字段 + env 覆盖，`ABV_ZAI_KEYS`） |
| `src-tauri/src/proxy/pipeline/policy.rs` | ✅ T1：`UpstreamClassification` 作为 Key 状态机唯一判定源（未改动分类器本身）；T3 增加 zcode 专属判定（`3004/3006/3007/3012/1005`、`405 unusual activity`） | 现有 Google 错误分类行为不变 |
| 前端 `Accounts`/`AccountTable`/`useAccountStore` | T2：新 provider 类型的展示与操作（T1 仅 z.ai 设置卡内多 Key 管理） | 全量 `saveConfig` 语义不变（其并发风险另案） |
| `src/locales/*` ×12 | ✅ T1 已落地：Key 池管理文案 | — |
| `docs/zcode/`（本提案 + `implementation.md`）、`docs/README.md`、`docs/zai/provider.md` | ✅ T1 已同步 | — |
| **明确不触碰** | Gemini IR pipeline（`pipeline/inbound.rs` 等）、四协议 mappers、Google 池 | Pipeline-First：zcode 走 Anthropic 兼容透传，与 `docs/zai/provider.md` 既定例外同构 |

## 4. 备选方案与落选原因

| 备选 | 落选原因 |
|---|---|
| A. 与 zcode2api 并行运行、不做集成 | 两套运维面、重复账号录入、AGPL 依赖边界仍需隔离；不解决"单一管理面"问题 |
| B. 直接搬运 zcode2api 代码 | **AGPL-3.0 与 CC-BY-NC-SA-4.0 不兼容**，法律上不可行；只能协议事实级净室重实现 |
| C. zcode 上游也走 Gemini IR（pipeline 化） | 上游本就是 Anthropic 兼容端点，IR 转换是无谓损耗且破坏响应流透传；现有 z.ai 透传先例已确立该类上游的例外地位（`docs/zai/provider.md`） |
| D. 仅做 T1（API Key 池） | API Key 是计费通道，不承载订阅额度；用户核心诉求是订阅池化 |
| E. 等官方开放订阅 API | 无时间表；且本提案 T1/T2 不依赖任何仿真行为，先行落地可独立成立 |

## 5. 验收标准与证据映射

> T1 行的结果列由实现 PR 回写；完整证据与未验证边界见 `implementation.md`。

| # | 验收（可观察行为） | 失败面 | 直接证据 | 结果 |
|---|---|---|---|---|
| A1 | T1：配置 ≥2 个 zai/bigmodel Key 后，`/v1/messages` 连续请求在 Key 间轮询；人为置某 Key 401 后请求自动跳过并标记 INVALID | provider 组合 + 调度 | 本地起服 + `curl /v1/messages` 序列观察 + 账号状态查询；聚焦单测覆盖选择/跳过纯逻辑 | **部分执行**：选择/跳过/状态机单测 14 例通过（`cargo test zai`）；真实 Key 端到端 curl 序列**未执行**（环境无凭证，见 implementation.md 未验证边界） |
| A2 | T1：旧配置（单 `api_key`）升级后行为不回退，Key 迁入列表 | 配置迁移/持久化 | 迁移单测 + 旧 `gui_config.json` 启动回放 | **单测通过**（迁移 + 旧格式 JSON 回放反序列化） |
| A3 | T2：OAuth 登录后账号入池并以 JWT 发起请求成功；JWT 失效（3004）时标记并按回退通道续服 | OAuth 流程 + 回退组合 | 真实账号端到端（需凭证）；无凭证时明确标注"未验证边界"，仅单测覆盖 poll 状态机 | 未执行（T2 未实施） |
| A4 | T3：Plan 通道请求携带有效 verify_param 成功；挑战失效（3007）原地换码重试 ≤3 后上抛 | 验证码注入 + 重试语义 | 真实账号端到端；单元层以注入桩验证头部与重试序列 | 未执行（T3 未实施） |
| A5 | 全阶段：Google 池行为零回退（exclusive/pooled/fallback 三模式下 antigravity 路径回归） | 既有消费路径回归 | 现有测试全绿 + `cargo clippy --all-targets --all-features` + `npm run build`（AGENTS.md pre-flight） | **执行**：clippy 0 error（新代码 0 warning）、`npm run build` 通过、`cargo test --lib proxy::` 既有套件全绿；全量测试矩阵留 CI |
| A6 | headless parity：全部新配置可通过 config 文件 + env 覆盖，Docker 容器内等价可用 | 部署组合 | `docker/Dockerfile.backend` 构建冒烟 | **部分执行**：`ABV_ZAI_KEYS`/`ZAI_KEYS` env 解析单测通过 + 启动覆写路径落地；Docker 构建冒烟**未执行**（本地无 Docker） |
| A7 | UI：Accounts 页可见 zcode 账号状态/配额；12 语言键完整 | 前端消费路径 | `npm run build`（tsc）+ locale 键完整性检查 | **T1 范围通过**：z.ai 卡内 Key 池管理 + 状态徽标 + 12 语言键全等校验通过；Accounts 页按实现决策留 T2 |

## 6. 风险、权衡与主动放弃的能力

1. **验证码求解器路线（未决，最大工程摩擦）**：zcode2api 用 Node 子进程跑阿里云 SDK（21MB 依赖）。候选：①手动过码起步（默认，零依赖）；②Tauri sidecar/外置求解服务（桌面打包跨平台摩擦大，Docker 无压力）；③纯 Rust 移植（不可行：混淆 JS SDK 依赖浏览器环境）。**主动放弃**：首版不做全自动求解。
2. **上游协议漂移**：客户端版本（当前 3.11.2）、头部形态是移动靶，T3 需长期跟随；T1/T2 面较小。**主动放弃**：不做版本自适应探测，采用单一真相源常量模块（对齐 zcode2api 的 constants 收口纪律）。
3. **账号风控面扩大**：`3012/405` 真风控需立即 DISABLED 熔断保护资产；billing 轮询须错峰。接受此运维责任为功能代价。
4. **AGPL 净室约束**：贡献者若接触过 zcode2api 源码，提交需声明仅依据协议事实文档重写；PR 模板的"问题分类"栏注明。
5. **配置命名空间**：~~倾向复用 `proxy.zai`……留给实现期决策~~ **已落定（T1 实现期）**：复用 `proxy.zai`，新增 `keys: ZaiKeyEntry[]`，遗留 `api_key` 保留迁移兼容；T3 引入领取/指纹配置时再评估是否拆分 `proxy.zcode`。

## 7. 发版策略

按 AGENTS.md 维护者分阶协议：实现于 beta 分支先行验证，稳定后并入 main；T1/T2 与 T3 分独立 PR（单一问题域原则），各自可独立回退。
