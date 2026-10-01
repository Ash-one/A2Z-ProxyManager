# Project Maintenance Guidelines & Agent Operational Guide

> 本文档是面向 AI 智能体（Agent）及人类维护者的核心工程开发规范、架构规范与操作基准。修改前请严格通读并遵守。

---

## 1. 架构总览（Architecture Overview）

本项目是一个高性能 AI 协议网关与桌面管理工具（A2Z-ProxyManager / Antigravity Tools），集成了两大核心引擎：

```
                    ┌────────────────────────────┐
                    │  Inbound Clients / SDKs    │
                    │ (Claude / OpenAI / Gemini) │
                    └─────────────┬──────────────┘
                                  │ :8045
                    ┌─────────────▼──────────────┐
                    │  Axum Proxy Router & Auth  │
                    └──────┬──────────────┬──────┘
                           │              │
       [Antigravity Models]│              │[GLM / ZCode Models]
                           │              │
             ┌─────────────▼────┐    ┌────▼─────────────────┐
             │ TokenManager Pool │    │   ZaiKeyPool (T1-T3)  │
             │ (Google OAuth)   │    │ (API Key + Plan JWT) │
             └─────────────┬────┘    └────┬─────────────────┘
                           │              │
             ┌─────────────▼────┐    ┌────▼─────────────────┐
             │ Pipeline Engine  │    │ Zcode Plan Gateway   │
             │ (Thinking/Sanit) │    │ (Captcha + 3012 WAF) │
             └─────────────┬────┘    └────┬─────────────────┘
                           │              │
                     Google Upstream    Z.AI / ZCode Upstream
```

1. **Antigravity 引擎（Google Gemini）**：
   - 将入站的四种主流协议（OpenAI Responses、OpenAI Chat Completions、Anthropic Claude、Google Gemini）适配转换为 Antigravity-style Gemini 协议并输出；
   - 核心组件包含多账号 OAuth 轮询调度器（`TokenManager`）、配额保护（Quota Protection）、统一思考预算过滤与补全（Thinking Auto-Heal）、提示词清洗与上下文对齐。
2. **Z.AI / ZCode 引擎（GLM & Coding Plan）**：
   - 统一承载 Z.AI API Key 池与 ZCode Coding Plan / Start Plan（OAuth JWT 订阅账号池）；
   - **Plan 通道仿真**：转发至 `https://zcode.z.ai/api/v1/zcode-plan/anthropic/v1/messages`，携带 Bearer JWT + 14 项客户端指纹头（`X-ZCode-*`, `X-Device-Mid` 等）；
   - **验证码缓冲池（Captcha Buffer Pool）**：双槽代际预热（Staggered Pre-warm），单账号常驻最多 3 个新鲜验证码（90s TTL），上游 3007 拒绝时单 Token 精准作废（`invalidate_param`）；
   - **无浏览器后台求解器（Headless Node Solver）**：内置 `captcha_node/solver.js`（happy-dom 模拟环境），脱离前端 WebView，实现 500ms 级别无感秒级自愈求解；
   - **反风控内容审查合规（3012 / 405 WAF Bypass）**：自动前置 ZCode 官方智能体 system 身份块、动态当前模型块、实时从 JWT payload 解析注入 `metadata.user_id`，并在最后一条消息末尾追加 `cache_control: {"type": "ephemeral"}`。

---

## 2. 核心架构纪律（Core Principles）

- **Pipeline First（管道优先）**：
  - 核心处理管道与具体协议严格解耦；协议适配器（Claude/OpenAI 等）仅做参数归一化、载荷格式适配及管道阶段无法消解的协议分歧。
  - 思考块回填、思考预算裁剪、上下文结构对齐、前缀稳定性保障及风控提示词过滤统一由管道阶段处理。
  - 修复策略**优先在通用管道内进行协议无关的修复**，严禁针对特定适配器做局部硬编码修补，除非通用管道方案不可行。
- **确定性模型路由（Deterministic Model Routing）**：
  - 遇到 `glm-*`、`zai:*`、`zcode:*` 等模型请求时，确定性路由至 `zai_anthropic` / `zai_pool` 处理，避免误入 Google 账号池导致 404/503。
  - 模型规范化遵循通配规则（如 `canonicalize_plan_model`），通配分段处理大小写，禁止脆弱的全字面量硬匹配。
- **极简主义 UI 与去广告化（Clean UI & Minimalist Design）**：
  - 界面设计注重轻量、不打扰用户的交互体验，严格禁止在菜单栏或设置页内散落与核心代理/管理功能无关的商业广告或引流外链。
  - 菜单项显示与隐藏需在「常规设置」->「菜单设置」中提供受控开关，并持久化于 `gui_config.json`。
- **端与 CLI 行为对齐（Headless & CLI Parity）**：
  - 任何 GUI 上具备的功能（如验证码求解、密钥管理、模型代理），均需支持无头服务器、终端 `curl` 或外部 Agent（Claude Code / Codex / DSH）调用；
  - 禁止将关键网关流程（如验证码补给）硬锁死在前端 React 组件挂载或浏览器窗口前台生命周期上。

---

## 3. 工程环境与沙盒操作规范（Environment & Sandbox Rules）

在 DSH 或受限沙盒环境中执行终端命令时，必须遵守以下环境事实：

1. **Cargo 注册表沙盒保护**：
   - 沙盒模式下无法直接写入全局 `~/.cargo/registry`，执行任何 `cargo` 命令时**必须显式指定本地 `CARGO_HOME`**：
     ```bash
     CARGO_HOME="$(pwd)/src-tauri/.cargo-home" cargo test --manifest-path src-tauri/Cargo.toml <module>
     CARGO_HOME="$(pwd)/src-tauri/.cargo-home" cargo build --manifest-path src-tauri/Cargo.toml
     ```
2. **Tauri 桌面应用静态资源嵌入规律**：
   - Tauri v2 在编译 Rust 二进制文件（`antigravity-tools`）时，会将前端 `dist/` 静态产物直接编译内嵌到二进制中；
   - **如果修改了 `src/` 前端代码**：
     1. 首先执行 `npm run build` 生成全新 `dist/`；
     2. 接着必须执行 `cargo build` 重新内嵌编译 Rust 二进制；
     3. 将编译好的二进制同步到 App bundle 并重启应用进程：
        ```bash
        cp src-tauri/target/debug/antigravity-tools "src-tauri/target/debug/bundle/macos/Antigravity Tools.app/Contents/MacOS/antigravity-tools"
        ```
3. **Node 求解器依赖保障**：
   - `captcha_node/` 目录维护轻量 `happy-dom` 依赖，解算脚本为 `captcha_node/solver.js`；
   - 依赖被 Git 忽略，如在新环境中运行，需确保执行 `cd captcha_node && npm install`。

---

## 4. 核心验证门禁与基准测试（Verification Gates）

在交付任何修改前，必须执行以下测试与质量门禁：

### 4.1 核心连通性金指标（Golden Path Verification）
网关在本地 `8045` 端口运行时，以下测试命令**必须百分之百正确返回 200 OK 内容**：

```bash
curl -i -X POST http://127.0.0.1:8045/v1/messages \
  -H "Content-Type: application/json" \
  -H "x-api-key: test" \
  -H "anthropic-version: 2023-06-01" \
  -d '{
    "model": "glm-5.3-flash",
    "max_tokens": 100,
    "messages": [{"role": "user", "content": "Hello"}]
  }'
```
流式验证（SSE Chunked Transfer）：
```bash
curl -i -X POST http://127.0.0.1:8045/v1/messages \
  -H "Content-Type: application/json" \
  -H "x-api-key: test" \
  -H "anthropic-version: 2023-06-01" \
  -d '{
    "model": "glm-5.3-flash",
    "max_tokens": 100,
    "stream": true,
    "messages": [{"role": "user", "content": "Hello"}]
  }'
```

### 4.2 本地提交前预检（Pre-flight Checks）
```bash
# 1. Rust 代码格式检查
cd src-tauri && cargo fmt -- --check

# 2. 针对受影响模块的单元测试
CARGO_HOME="$(pwd)/src-tauri/.cargo-home" cargo test --manifest-path src-tauri/Cargo.toml zcode

# 3. 前端编译与类型检查（当修改了 src/ 或前端配置时）
npm run build
```

---

## 5. 分支与发版纪律（Release & PR Protocol）

1. **分支规范**：
   - `main`：正式稳定版发布分支，仅部署官方生产 Release 与 Docker latest；
   - `beta`：功能演进与预览版（Pre-release）分支。所有新特性、破坏性重构与非紧急修复，必须先在 `beta` 或特性分支测试验证。
2. **PR 范围单一原则（Single Problem Scope）**：
   - 每个 PR 仅解决一个清晰的问题域，坚决禁止将功能开发、工程治理、依赖升级或文档调整混杂在同一个 PR 中；
   - 提交历史在合并前应保持整洁（通过 rebase/squash 收敛探索期产生的冗余 commit）。
3. **版本同步与更新日志（Changelog Synchronization）**：
   - 版本升级使用 `npm run bump <patch|minor|beta|version>`；
   - 正式发版必须同步更新 `CHANGELOG.md`、`CHANGELOG_EN.md`，以及 `README.md` 与 `README_ZH.md` 首页中的更新日志段落。

---

Maintained by @jeikl
