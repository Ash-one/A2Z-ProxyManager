# 决策记录：T3 — Plan 通道完整仿真、无痕过码与套餐领取（zcode 提案第三阶段）

> 状态：已实现（Idea → Result 收敛，沿 `docs/zcode/` 惯例）  
> 归属提案：`docs/zcode/proposal.md` §2 T3  
> 分支：`feat/zcode-subscription`  

## 1. 交付结果（现在为真的事实）

- **Plan 通道消息转发**：Key 池内的 `mode=jwt`（Plan 通道 JWT）条目正式参与调度。在其所属账号的验证码参数处于新鲜窗内时，作为可用槽位参与轮询与故障转移；请求路由至 `https://zcode.z.ai/api/v1/zcode-plan/anthropic/v1/messages`，附带 Bearer JWT、官方客户端身份头全集（User-Agent, X-ZCode-App-Version, X-ZCode-Agent, X-Platform, X-Os-Category, X-Release-Channel, X-Client-Language, X-Client-Timezone, X-Title, HTTP-Referer, X-Device-Mid）以及验证码头（`X-Aliyun-Captcha-Verify-Param`, `X-Aliyun-Captcha-Region`）；
- **端内无痕验证码自动求解**：前端组件按需/自动加载阿里云官方验证码 SDK（`https://o.alicdn.com/captcha-frontend/aliyunCaptcha/AliyunCaptcha.js`），动态获取 `client/configs` 场景配置（默认 `11xygtvd/cn/no8xfe`），默认通过无痕验证（`popup` 模式免弹窗）自动产出 `verifyParam` 并提交至后端 `ZcodeCaptchaStore`；若上游升级风险挑战则弹窗由用户滑块人工兜底；
- **状态机与失败分类（协议事实 §被拒信号）**：
  - 上游返回 `400` + `code: 3007` 或 `403` + captcha 响应头判为 `CaptchaChallenge`：立即失效对应账号的验证码缓存，条目置为 `CaptchaNeeded`（需要过码），立即触发池内账号级失败转移，并驱动前端自动重试过码；新验证码提交后自动重置为 `Active`；
  - `402` 或 body 含余额耗尽关键字判为 `Exhausted`，设置 30 分钟到期自动恢复窗口；
  - `429` 判为 `RateLimited`，遵循 Retry-After 头或默认 300 秒冷却；
  - `401` 或非验证码 `403` 判为凭证失效（`Invalid`）；
- **模型名大小写规范化**：Plan 通道对模型名大小写敏感，实现通用通配算法（`canonicalize_plan_model`）：按 `-` 分段，`glm` 转为全大写 `GLM`，纯数字段保留，包含视觉/变体后缀的大写（如 `4.6v` → `4.6V`），其余段首字母大写（`flash` → `Flash`，`highspeed` → `Highspeed`）；幂等且覆盖现有全系模型；
- **每账号设备档案（成套桌面 SKU）**：官方桌面端高可信 SKU 表（平台、架构、系统版本、分辨率严格绑定，禁止笛卡尔积）；每个 OAuth 账号生成独立的 UUIDv4 `device_mid` 与真实地区语言/时区对，随 `ZaiKeyEntry.device_profile` 字段持久化，实现“一号一台设备”且跨重启稳定；手动导入条目由运行时进程内稳定缓存兜底；
- **首启安装序仿真**：服务启动及新账号就绪时，按官方客户端顺序仿真：免鉴权拉取 `client/configs?app_version=3.14.3`，随后上报激活事件（`app_launch`、`app_daily_active`），事件体包含官方固定 16 字段；全程后台 best-effort，失败仅记录日志，不阻断主流程；
- **Plan 额度与限时套餐领取（preview + claim）**：
  - Plan 额度：JWT 行提供 `billing/balance` 专属查询，返回 PlanSlot 各模型剩余/总额度（如 `GLM-5.3 · 3000000 / 3000000`）；
  - 限时套餐领取：JWT 行提供活动套餐面板，调用 `billing/preview` 获取当前活动（404 正常静默）；点击领取时自动确保新鲜验证码，调用 `billing/claim`，精准展示业务码反馈（`1003` 已领取、`1005` 名额用完及 `next_at` 恢复时间倒计时、`3007` 验证码失败重试）；
- **全平台与 Headless Parity**：5 个新增命令（`zcode_captcha_config`, `zcode_captcha_submit`, `zcode_plan_quota`, `zcode_claim_preview`, `zcode_claim`）在 axum Web 服务（`/api/zcode/*`）与 Tauri IPC 严格对等；12 种国际化语言包全量同步。

## 2. 实现期决策（对提案的修订及理由）

| # | 决策 | 理由 | 授权 |
|---|---|---|---|
| 1 | **端内无痕自动过码为主，人工滑块兜底**（未决项定板） | 用户授权。在 Webview 内调用阿里云官方 JS SDK，无痕验证通过率高且无需外挂 Node/jsdom 服务，兼顾自动化体验与零额外外部二进制依赖 | 用户批准 |
| 2 | **完整 T3 一次性收敛** | 用户批准完整 T3 范围（转发 + 验证码 + 模型映射 + 指纹/安装序 + 额度 + 限时套餐领取） | 用户批准 |
| 3 | **命名空间延续复用 `proxy.zai`**（§6.5 定板） | 条目已在 T2 并入 `proxy.zai.keys`，T3 档案以 `device_profile` 字段内嵌持久化，不引入冗余的全局 `proxy.zcode` 顶层配置，保持配置树扁平最小化 | 实现期内敛决策（延续 T1/T2 惯例） |
| 4 | **计费与套餐领取仅限手动按需** | billing 族端点存在 WAF 风险；手动按需调用不会扩大被动封禁风险面 | 延续 T2 决策 |

## 3. 协议事实规格（单一真相源：`providers/zcode_plan.rs` 常量 + 本表）

| 协议事实 | 规格值 |
|---|---|
| 客户端版本真相源 | `APP_VERSION = "3.14.3"`（出处：官方客户端现行版 ZCode.exe ProductVersion 3.14.3.7762 / app.asar；活动门槛依赖版本常量） |
| Plan 通道消息端点 | `POST https://zcode.z.ai/api/v1/zcode-plan/anthropic/v1/messages`（支持 count_tokens 路径拼接） |
| Plan 通道请求头 | `Authorization: Bearer <zcode JWT>`、`anthropic-version: 2023-06-01`、`X-ZCode-App-Version: 3.14.3`、`X-Platform: <platform>-<arch>`、`X-Device-Mid: <uuidv4>`、`X-Aliyun-Captcha-Verify-Param`、`X-Aliyun-Captcha-Region` 等 14 个固定头 |
| 验证码 SDK 产物 | `AliyunCaptcha.js` 成功回调 `captchaVerifyParam`（base64 JSON），TTL 约 2 分钟，进程内设定 90 秒新鲜窗（`CAPTCHA_FRESH_MS = 90_000`） |
| 验证码失败判定 | 响应 HTTP 400 且 body `code: 3007`；或 HTTP 403 且响应头包含 captcha/aliyun |
| 额度耗尽判定 | 响应 HTTP 402 或 body 包含 `quota`/`insufficient`/`balance`/`exhaust`/`额度`/`余额不足`，重试窗 30 分钟 |
| 限流判定 | 响应 HTTP 429，冷却窗口优先取 `Retry-After`，缺省默认 300 秒 |
| 运行配置端点 | `GET https://zcode.z.ai/api/v1/client/configs?app_version=3.14.3`（免鉴权，仅带 User-Agent；严禁带 platform，否则 3001） |
| 激活遥测端点 | `POST https://zcode.z.ai/api/v1/event/report`（无 Authorization，固定 16 字段激活上报体：app_launch / app_daily_active） |
| Plan 额度端点 | `GET https://zcode.z.ai/api/v1/zcode-plan/billing/balance`（Bearer JWT + 身份头，返回 balances 数组） |
| 限时套餐预览/领取 | `GET https://zcode.z.ai/api/v1/zcode-plan/billing/preview?app_version=3.14.3&platform=...`（404 视为无活动正常态）；`POST .../billing/claim` 携带验证码头与身份头 |

## 4. 验收证据（实际执行）

```text
cd src-tauri && cargo fmt -- --check                        # 通过
cd src-tauri && cargo clippy --all-targets --all-features   # 0 error（无新 warning）
cd src-tauri && cargo test zai                              # 17 passed / 0 failed
cd src-tauri && cargo test zcode                            # 30 passed / 0 failed（新增 14 例覆盖配置提取、SKU生成、头构建、模型名通配、失败分类、激活事件体、信封解析等）
npm run build                                               # tsc && vite build 通过（耗时 15.6s）
python3 locale 校验                                         # 12 个语言文件 proxy.config.zai.keys 全量具备 47 个键，无一遗漏
```

## 5. 未验证边界

- **count_tokens 在 Plan 通道上游的真实支持度**：根据 Anthropic 协议同形路由拼接至 Plan 通道；若上游服务未实现该端点将原样回传 404，不影响主消息流；
- **特定地区网络下阿里云验证码 CDN 加载**：国内环境下 `o.alicdn.com` 速度优秀，若在境外特定极限制网络环境可能需要走代理或前置解析。
