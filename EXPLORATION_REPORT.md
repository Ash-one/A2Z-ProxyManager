# A2Z-ProxyManager（Antigravity Tools v4.8.4）仓库探索报告

> 由三个并行子智能体探索生成（Rust 后端 / 前端 / 构建发布体系），整合于本文档。
> 生成日期：2025-09-30 · 基线：`main @ d7438d6`（工作树干净，与 origin/main 同步）

---

## 一、项目定位

**Antigravity Tools v4.8.4** —— 专业级 AI 账号管理与协议代理系统（本地 AI 中转站）。

- 将 Google / Anthropic Web Session (OAuth) 转为标准 API
- 聚合 **OpenAI Responses / OpenAI Chat Completions / Anthropic Claude / Google Gemini** 四协议入站，统一输出 **Antigravity 风格 Gemini 协议**
- 核心能力：智能配额仪表盘与最佳账号推荐、OAuth 账号管家（批量导入 / V1 迁移 / 403 检测）、协议中继、模型路由（正则映射 / 分级路由 / 后台静默降级 Flash）、Imagen 3 多模态
- 许可证：CC-BY-NC-SA-4.0

## 二、总体规模

| 维度 | 数值 |
|---|---|
| 全仓源码 | 258 文件 / ~143,000 行 |
| Rust 后端 `src-tauri/src/` | 160 文件 / ~112,275 行 |
| 前端 `src/` | ~52,000 行（含 locales ~21,000） |
| Tauri 命令 | 142 个（`src-tauri/src/lib.rs:599` `generate_handler!` 注册） |
| 目标平台 | Linux（deb/rpm/AppImage）、macOS（dmg，arm64/x64/universal）、Windows（NSIS）、Docker headless |

## 三、Rust 后端架构（src-tauri/）

### 3.1 依赖概览（`src-tauri/Cargo.toml`）

包名 `antigravity-tools` v4.8.4，edition 2021，crate-type rlib。

- **Tauri 2.2.5**（tray-icon、image-png）+ 插件：dialog / fs / updater / process / window-state / autostart / single-instance(deep-link)
- **反代服务**：axum 0.7（multipart, ws）、hyper 1 + tower/tower-http（cors/trace/fs）、eventsource-stream、async-stream、dashmap、futures
- **上游客户端双栈**：reqwest 0.12（json/stream/socks/blocking/rustls-tls）+ rquest 5.1（**TLS 指纹模拟**）；另 reqwest_0_13 别名仅供 updater 走 SOCKS5
- **存储/安全**：rusqlite(bundled)、aes-gcm + machine-uid（设备指纹加密）、sha2
- **其他**：tokio(full)、serde_json(preserve_order)、tracing、yaml-rt、toml/toml_edit、plist、image

### 3.2 模块树

| 模块 | 规模 | 职责 |
|---|---|---|
| `lib.rs` | 849 行 | 入口聚合，注册 142 个 `#[tauri::command]` |
| `commands/` | 8 文件 3,301 行 | 账号管理、设备指纹绑定/恢复、配额刷新、config load/save、OAuth 流程、导入迁移、窗口/更新、代理服务与日志/限流、代理池绑定、user_token、security、cloudflared、patch、autostart |
| `modules/` | 27 文件 18,243 行 | gui_config.json 配置读写（`modules/config.rs:7`）、account.rs(2,579)、proxy_db.rs(2,474)、process.rs(1,759)、oauth、tray、scheduler |
| `proxy/` | ~75,000 行 | 网关核心（见下） |
| `models/` `utils/` | 977 / 1,394 行 | 数据模型与工具 |

### 3.3 proxy/ 核心

- **HTTP 层**：`proxy/server.rs`（4,885 行）axum 路由
  - 代理路由 `server.rs:615-691`：`/v1beta` generateContent、`/v1/messages`、`/v1/responses`、`/responses`、`/v1/chat/completions`、`/v1/models`、`/mcp/web_reader`、`/internal/warmup`
  - 管理路由 `nest("/api")` `server.rs:716-1033`：accounts / stats / config / proxy / logs / security / user-tokens / oauth
- **四协议适配器**：位于 `proxy/mappers/{openai,claude,gemini}`（注意 `proxy/adapters/` 不是协议适配器，是 apply_patch_preflight.rs 2,578 行 + artifact_store.rs）
  - 入口：`openai/request.rs:158` transform_openai_request、`claude/request.rs:378` transform_claude_request_in、`gemini/wrapper.rs:6` wrap_request_v2（cloudcode 信封包装）
  - 各协议 `response.rs` / `streaming.rs` / `collector.rs` 负责反向转协议与流式回流
- **pipeline/**（4 文件 2,108 行）——协议无关的通用处理层：
  - `inbound.rs` InboundThinkingPipeline：`process_contents`(:82)、`normalize_tool_call_ids`(:439)、`configure_inbound_thinking`(:606，thinking budget 过滤/回填)、`align_google_request_prefix_topology`(:776，prefix 稳定)
  - `policy.rs:7` ProxyProtocol 四协议枚举 + UpstreamClassification（429/404/5xx/签名失效/网关自产错误统一分类）
  - `usage.rs:9` CanonicalUsage 统一用量收拢/散开
- **thinking_store.rs**（4,807 行）：服务端思考块全量仓库，内容匹配重注入签名，隔离键 `{tenant}:{session}`(:1-11)
- **配套清洗**：`mappers/context_manager.rs:159` 上下文结构对齐；`mappers/prompt_sanitizer.rs:422` sanitize_gemini_payload 提示词清洗；请求头清洗 `upstream/client.rs:443,576`（剔除 x-goog-user-project）
- **转发与调度**：`upstream/client.rs` call_v1_internal(:313) / call_v1_internal_with_headers(:334)（rquest 客户端）+ retry.rs；`token_manager.rs`(5,884) 账号调度、proxy_pool.rs 代理池、rate_limit.rs(1,272)、middleware/（auth/cors/ip_filter/monitor 等 7 文件）

### 3.4 请求全链路

```
axum 路由（server.rs:618-691）
  → 中间件（auth / ip_filter / monitor）
  → handler（handlers/openai.rs:3066 handle_completions · claude.rs:428 handle_messages · gemini.rs:70 handle_generate）
  → mappers transform_* 转 Gemini canonical IR
  → InboundThinkingPipeline（inbound.rs:82）对齐 / budget 过滤
  → thinking_store 回填签名
  → upstream/client.rs:313 转发上游
  → collector / streaming 反向转换 + CanonicalUsage 回流客户端
```

与 AGENTS.md 的 **Pipeline First** 原则吻合：协议逻辑收敛在 mappers，通用逻辑全部在 pipeline 层；出站不设统一 pipeline 以保流式稳定（`pipeline/mod.rs:1-8`）。

## 四、前端架构（src/）

### 4.1 技术栈（package.json）

- React 19.1 + TypeScript 5.8 + Vite 7
- 状态管理：**zustand 5**；UI：antd 5 + @lobehub/ui 4 + daisyui / Tailwind 3.4
- 路由 react-router-dom 7；i18next 25；@tauri-apps/api 2
- 辅助：recharts、framer-motion、@tanstack/react-virtual、dnd-kit
- scripts：`dev` / `build`(tsc && vite build) / `preview` / `bump` / `tauri` / `tauri:debug`

### 4.2 模块树

| 目录 | 规模 | 内容 |
|---|---|---|
| `pages/` | 9 页 ~9.5k 行 | Dashboard(610)、Accounts(1,259)、**ApiProxy(3,023，代理核心页)**、Monitor（薄包装→ProxyMonitor）、TokenStats(822)、UserToken(677)、ApiKeyFun(1,028，外部 Key 查询并同步 CLI)、Security(IP 安全)、Settings(1,976) |
| `components/` | 62 文件 ~17.7k 行 | 按域分目录：accounts(10)/common(13)/settings(8+proxy/4)/proxy(8)/security(5)/navbar(6)/dashboard(3)/layout/debug。最大：`proxy/ProxyMonitor.tsx` 2,312、`settings/ThinkingBudget.tsx` 1,309、`accounts/AccountTable.tsx` 1,018、`proxy/VirtualizedPayloadViewer.tsx` 978、`proxy/CliSyncCard.tsx` 746 |
| `stores/` | zustand×5 共 687 行 | useConfigStore(97：配置读写/主题/语言/菜单显隐)、useAccountStore(324：账号+配额)、useDebugConsole(208)、networkMonitorStore(45)、useViewStore(13) |
| `services/` | 239 行 | configService.ts / accountService.ts 薄封装 |
| `utils/` | 11 文件 1,008 行 | format、modelCategory、opencodeProfiles、liveLimit、windowManager 等 |
| `config/` `types/` `hooks/` | 837 / 400 / 153 行 | modelConfig.ts(526，模型元数据+图标，含 __tests__)；types/config.ts(260)、account.ts(140)；useProxyModels.tsx(153) |

### 4.3 三端同构的关键：`src/utils/request.ts`（295 行）

双通道通信抽象 —— **GUI = Tauri invoke，Headless/Web = REST**：

- Tauri 端直接 `invoke`
- Web 端按 `COMMAND_MAPPING`（150+ 命令）映射为 `/api/*` fetch，含 `:param` 路径替换与 Bearer / x-api-key（sessionStorage `abv_admin_api_key`）

对应 AGENTS.md 的 headless parity 要求，GUI / Headless / CLI 行为一致。

### 4.4 配置流（gui_config.json）

```
useConfigStore.loadConfig() → services/configService.loadConfig() → request('load_config')
  ├─ Tauri 端：invoke → src-tauri/src/modules/config.rs:7（数据目录 gui_config.json，含迁移逻辑）
  └─ Web 端：GET/POST /api/config
渲染：theme → components/common/ThemeManager.tsx:29-54（data-theme + set_window_theme）
      language → i18n；hidden_menu_items → Navbar 显隐
保存：统一全量 saveConfig（src/stores/useConfigStore.ts:39）
```

### 4.5 i18n

12 语言：zh / zh-TW / en / ja / tr / vi / pt / ko / ru / ar / es / my（`i18n.ts`、`components/navbar/constants.ts:19-30`），每份 locale 约 1,800 行。

## 五、构建 / 发布 / 部署 / 文档体系

### 5.1 CI/CD（`.github/workflows/`）

- **ci.yml**：push/PR → main/master/beta；前端 tsc+build（ubuntu），Rust check 三平台矩阵（ubuntu / windows-2025 / macos）
- **release.yml**（529 行，核心）：tag push `v*` + 手动 dispatch
  - **Release gate 强制分支对齐**（release.yml:70-88）：带 `-` 的预发 tag（如 v4.8.4-beta.1）只准 beta 分支，纯数字正式 tag 只准 main，交叉即拦截
  - 构建矩阵 6 目标：mac arm64/x64/universal、ubuntu-22.04、ubuntu-24.04-arm、windows-2025；预发版跳过 MSI（WiX 不支持 prerelease 串，仅 NSIS）
  - 预发版 `prerelease:true, makeLatest:false` —— beta 完全隔离、不打 Latest、不污染正式更新通道；Docker Hub 推 AMD64+ARM64，正式打 `latest`，预发独立 tag
- **deploy-pages.yml**：main push → 发布 `web_site/` 到 GitHub Pages
- ⚠️ 仓库根目录 `workflows/` 是**旧版精简副本**（release.yml 仅 330 行，无 gate），GitHub 不识别，疑似遗留备份

### 5.2 脚本与安装器

- `scripts/bump-version.mjs`（`npm run bump`：原子同步 12+ 清单 + 生成 CHANGELOG 骨架）
- `scripts/close_integrated_prs.sh` + MANUAL_PR_CLOSE_GUIDE.md（批量关闭已集成 PR）
- `scripts/fix_app.sh` / `Fix_Damaged.command`（修 macOS Gatekeeper "已损坏"）、`package_dmg.sh`（手动打 dmg）
- `scripts/test-opencode-profiles.mjs`（node:test 前端单测）、`test_sqlite_sliding_and_thinking.py`（SQLite 思考块/日志滑动窗口测试）
- 根目录 `install.sh`：Linux/macOS 一键安装（GitHub API 取 release，按 OS/架构选包，支持 `--version`、dry-run）；`install.ps1` 为 Windows 等价（NSIS exe）
- `deploy/arch/`：Arch Linux PKGBUILD 模板 + 自更新 install.sh

### 5.3 Docker 部署（docker/）

**原生 Headless**（无需 VNC/桌面）：完整 Web 管理界面 + API 反代 + 数据持久化，单端口 **8045**，`API_KEY` / `WEB_PASSWORD` 分离鉴权。

变体：`Dockerfile`（全量）、`Dockerfile.backend`（后端-only，可复用前端镜像或本地 dist）、`Dockerfile.backend.localdist`；对应 compose：`docker-compose.yml` / `backend.yml` / `localdist.yml` / `fork.yml`。镜像 `lbjlaq/antigravity-manager`。

### 5.4 文档（docs/）

- `docs/README.md`：索引
- `docs/RELEASE_GUIDE.md`：发版 SOP —— 通道 A 正式版走 main、通道 B beta 预发布完全隔离；bump / 回溯贡献者署名 / README 更新日志同步 / tag 三者一致
- `docs/API_REFERENCE.md`：8045 统一端口 —— AI 协议 `/v1*` 走 Bearer、管理 `/api/*` 走 x-admin-token；账号/配额/切换/代理管理接口
- `docs/proxy/`：鉴权模式与账号池生命周期
- `docs/zai/`：z.ai GLM 集成（实现 / MCP Search-Reader-Vision / Anthropic 兼容 provider）
- `docs/testing/`：上下文压缩 / IP 安全 / opencode 同步测试；其余为专项排障记录（503、invalid_grant、opus 优先级等）

### 5.5 其他

- `web_site/`：项目官网静态页（index.html + qa.html + 素材），Pages 流水线部署
- `request_transform.md`：Codex `/v1/responses` → Gemini v1internal 转换 ASCII 图 —— `instructions`→sanitize+Antigravity 身份→`systemInstruction`（**~17.5K token 稳定前缀核心**）；`input[]` 按 item 映射 message/function_call/function_call_output→contents 的 text/inlineData/functionCall/functionResponse；`tools[]`→flatten 展平+按名排序+clean schema（**~5K 稳定前缀**）；采样参数→generationConfig；`sessionId`←FNV-1a(account_id)；固定 `userAgent: "antigravity"`

## 六、亮点 ✅

1. **Gemini canonical IR 统一四协议**，mappers 只做协议适配，通用逻辑全在 pipeline 层（与 AGENTS.md 架构约束自洽）
2. **UpstreamClassification 统一错误分类**，防"自噬锁定"（`proxy/pipeline/policy.rs:14-33`）
3. **thinking 签名跨协议内容匹配回填**（thinking_store.rs，4,807 行）
4. **request.ts 双通道抽象**实现 GUI / Headless / CLI 三端同构
5. VirtualizedPayloadViewer 虚拟滚动抗大日志；i18n 12 语言齐全；config 带单测
6. **双通道发版由 gate 强制隔离**、6 平台矩阵构建、Docker headless 完整支持、安装脚本覆盖三平台 + Arch
7. 全库几乎无 TODO/FIXME 残留（后端仅 1 处：`commands/user_token.rs:106`）

## 七、风险清单 ⚠️

| # | 类型 | 位置 | 说明 |
|---|---|---|---|
| 1 | 超大文件 | `proxy/handlers/openai.rs` 7,583 行 | 102 个函数，含 chat/responses/images/ws 多职责 |
| 2 | 超大文件 | `proxy/token_manager.rs` 5,884 / `server.rs` 4,885 / `thinking_store.rs` 4,807 / `opencode_sync.rs` 4,356 | 同类维护成本问题 |
| 3 | 超大组件 | `src/pages/ApiProxy.tsx` 3,023 行 | 约 39 处 useState/useEffect |
| 4 | 超大组件 | `ProxyMonitor.tsx` 2,312 / `Settings.tsx` 1,976 | 同上 |
| 5 | 重复逻辑 | `UpstreamClassification::classify` 6 处调用点 | openai.rs:2832,4816,5363,5854、claude.rs:1718、gemini.rs:921，可收敛 |
| 6 | 手工双份同步 | 前端 `COMMAND_MAPPING` ↔ Rust 142 命令 | 新增命令易漏映射 |
| 7 | 并发覆盖 | 全量 `saveConfig` 散落 4 个调用点 | Settings、ApiProxy、Navbar、SuggestionDeleteThinkingModal，last-write-wins |
| 8 | i18n 漂移 | 12 份 locale json 各约 1,800 行 | 键靠人工同步 |
| 9 | 测试混入源码树 | `mappers/claude/serde_leak_test.rs`、`common_utils_test_probe.rs` | 应移入 tests/ |
| 10 | 遗留物 | 仓库根目录 `workflows/` | GitHub 不识别的旧版流水线副本 |

## 八、近期开发脉络（git log 摘要）

最近提交集中在 **proxy 的 thinking block 签名链路**：

- `5321c49` fix(proxy): 消除占位 thinking block，首个非 thought part 强制锚点签名
- `639209b` fix(proxy): 消除破坏性 base64 解码，支持 raw protobuf 签名，最小签名长度放宽至 32
- `441510d` fix(proxy): proxy_db 损坏签名的数据库回写自愈
- `34dcacf` feat(updater): tauri updater 支持自定义 endpoint 与原生更新检查
- 当前版本线：4.8.4（beta.1 已发，main 已合并 beta）

---

*本报告基于三个并行子智能体的只读探索（后端 / 前端 / 构建发布体系），所有结论均标注了相对路径与行号，可直接跳转验证。*
