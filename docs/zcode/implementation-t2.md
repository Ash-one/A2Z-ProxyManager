# 决策记录：T2 — OAuth 免密登录入池 + JWT 订阅账号（zcode 提案第二阶段）

> 状态：已实现（Idea → Result 收敛，沿 `docs/zai/` 与 T1 `implementation.md` 惯例）
> 归属提案：`docs/zcode/proposal.md` §2 T2（实现期修订已回写）
> 分支：`feat/zcode-subscription`

## 1. 交付结果（现在为真的事实）

- 用户在 z.ai 设置卡内点击 **OAuth 登录**：后端发起 `zcode.z.ai/api/v1/oauth/cli/init`（Bearer poll_token，本地 32 位 hex 生成，服务端下发则优先采用），前端拉起浏览器授权页后有界轮询 `poll/{flow_id}`（间隔取服务端 `poll_interval_sec`，本地 300s 兜底）；
- 登录就绪（`status=ready`）：后端完成 **业务 JWT 派生**（`api.z.ai/api/auth/z/login`）与**开钥链**（`getCustomerInfo` → `api_keys` 复用/创建 `zcode-api-key` → `copy` 取 secretKey → `id.secret`），产物两条款目入池：
  1. `mode=jwt`（Plan 通道 JWT，`provider=zcode_plan`，**T3 前不参与消息转发调度**，仅凭证存储 + 额度查询锚点）；
  2. `mode=api_key`（自动开通的订阅 API Key，`provider=zai`，**立即参与池化转发**，在 `api.z.ai` Anthropic 兼容端点直接消耗 Coding Plan 订阅额度，免验证码）；
- 两条款目经 `account_id`（zcode user id/email）配对——提案 T2"JWT 与同账号 API Key 并存"的锚点；开钥链任一步失败仅降级提示（手动粘贴 API Key 补全），不丢弃已取得的登录凭证；
- **手动导入**：粘贴框自动判别（3 段点分 → JWT；单点两段 → `id.secret` API Key），与后端 `detect_imported_credential` 同语义；
- **额度查询**：JWT 行按钮式手动按需查询（`/api/biz/subscription/list`，Bearer 业务 JWT），data 容错透传、前端摘要展示；
- **headless parity**：`zcode_oauth_start`/`zcode_oauth_poll`/`zcode_query_quota` 命令与 `/api/zcode/oauth/start|poll`、`/api/zcode/quota` Web 路由等价；后端无会话状态（流程态由前端持有并回传）。

## 2. 实现期决策（对提案的修订及理由）

| # | 决策 | 理由 | 授权 |
|---|---|---|---|
| 1 | **消息转发凭证 = OAuth 链开通的 API Key**；JWT 是 Plan 通道凭证、转发待 T3（提案原文"以 JWT 发起请求"修订） | 两份独立协议事实源一致证实：订阅额度经自动开通的 API Key 在 `api.z.ai` 直接消耗、免验证码；JWT 通道（`zcode.z.ai`）需每请求阿里云无痕验证码（= T3）。机制修正，用户可见结果不变（登录→入池→订阅额度转发） | 提案归属内的协议事实校正（未完成提案的机制修订，已回写提案 A3/§4-D） |
| 2 | **billing 手动按需查询**，不做后台错峰轮询 | billing/* 有 WAF 拦截风险；手动查询风控面不扩大（用户选择） | 用户确认 |
| 3 | **UI 留在 z.ai 设置卡内**，Accounts 页接入留后续 | 延续 T1 最小 UI 面决策；OAuth/导入/额度在池编辑器内闭环（用户选择） | 用户确认 |
| 4 | **开钥链容错解析 + 手动粘贴兜底**（不硬编码猜测 schema） | `getCustomerInfo`/`api_keys`/`copy` 响应 schema 未公开文档化；按 AGENTS.md"风险路径替换须有已验证回退"纪律，任一步解析失败降级为手动粘贴，登录凭证不丢失 | 实现期（净室纪律下唯一诚实解） |

## 3. 协议事实规格（单一真相源：`providers/zcode_oauth.rs` 常量 + 本表）

| 事实 | 值 |
|---|---|
| 登录 init | `POST https://zcode.z.ai/api/v1/oauth/cli/init`，`Authorization: Bearer <poll_token>`，body `{"provider":"zai"}` → `data{flow_id, authorize_url, expires_at, poll_interval_sec}` |
| 登录 poll | `GET https://zcode.z.ai/api/v1/oauth/cli/poll/{flow_id}`（Bearer poll_token）→ pending：`data.status="pending"`；ready：`data.status="ready"` + `data.token`（zcode JWT）+ `data.zai.access_token` + `data.user` |
| 会话过期 | body `code=3004` / HTTP 404|410 / 本地 300s 兜底 → 过期，前端提示重新发起 |
| 业务 JWT | `POST https://api.z.ai/api/auth/z/login`，body `{"token":<zai access_token>}` → `data.access_token`（仅管理面：billing/开钥；绝不用于消息转发） |
| 开钥链 | `GET /api/biz/customer/getCustomerInfo` → org+project；`GET/POST /api/biz/v1/organization/{org}/projects/{proj}/api_keys`（复用名为 `zcode-api-key` 的既有 Key）；`GET .../api_keys/copy/{apiKey}` → secretKey；最终 `{apiKey}.{secretKey}` |
| 订阅查询 | `GET https://api.z.ai/api/biz/subscription/list`（Bearer 业务 JWT）；响应结构未公开文档化 → data 原样透传 |
| 通道模型 | API Key（含订阅开通 Key）→ `api.z.ai` 直连，免验证码；Plan JWT → `zcode.z.ai` 通道，每请求 `X-Aliyun-Captcha-Verify-Param`（T3 范围） |

净室声明：本阶段实现仅依据公开协议事实文档（pi-zcode-provider PROTOCOL.md——对 ZCode 3.10.1 逆向并经真实 API 验证的事实清单；zcode2api README/ARCHITECTURE.md 的通道模型事实），未接触、未搬运任何 zcode2api 源代码；提案 §6.4 的提交声明在此留档。

## 4. 验收证据（实际执行）

命令记录（提交前复核）：

```text
cd src-tauri && cargo fmt -- --check          # 通过
cd src-tauri && cargo clippy --all-targets --all-features   # 0 error
cd src-tauri && cargo test zai                # 17 passed / 0 failed
cd src-tauri && cargo test zcode              # 13 passed / 0 failed（poll 信封/3004、开钥容错解析、入池组装、user/org/key 提取）
npm run build                                 # tsc + vite 构建通过（chunk 警告为预存）
python3 locale 键集校验                        # 12 文件 proxy.config.zai.keys 键集全等（30 键）
```

单测覆盖：`zcode_oauth.rs` 内嵌 13 例（envelope 拆分、3004 信封映射、user/org/project/api_key/secretKey 容错提取含多形态 fixture、入池条目组装与账号配对）；`zai_pool.rs` 内嵌新增 2 例（jwt 模式调度排除 + 快照 mode/account_id、mode 变更触发指纹重同步）；`tests/zai_key_pool_tests.rs` 新增 1 例（`detect_imported_credential` 判别规则）+ ZcodePlan base URL 不受全局覆盖断言。

## 5. 未验证边界

- **真实账号 OAuth 端到端未执行**（环境无 z.ai 订阅凭证）：init/poll/开钥链/订阅查询的实际响应字段名以协议事实文档为准，容错解析覆盖多形态但未经真机确认；首次真实登录若开钥链降级，按 UI 提示手动粘贴 API Key 即可补全；
- **开钥链响应 schema**（getCustomerInfo/api_keys/copy）未公开文档化——解析失败路径即为此边界设计；
- **订阅查询响应结构**未文档化——前端容错摘要展示，识别常见字段失败时回退原始 JSON；
- 全量测试矩阵按 AGENTS.md 留给 CI。

## 6. T3 展望

Plan 通道消息转发（verify_param 注入 + 挑战失效换码重试）、每账号指纹/首启安装序仿真、限时套餐领取、模型名大小写敏感映射；JWT 条目在 T3 落地后经 `account_id` 配对激活"订阅额度通道 ↔ API Key 回退通道"的通道间回退语义。
