# 提案：T4 — zcode 订阅账号管理升格为独立页面

> **状态：已实现（已重写为稳定决策记录：`implementation-t4.md`）**
> 决策类别：Feature（UI 信息架构调整，用户可见）
> 分支：`feat/zcode-subscription`
> 归属：`docs/zcode/proposal.md`（zcode 特性族总提案）第四阶段；**取代 `implementation-t2.md` §2 决策 #3「UI 留在 z.ai 设置卡内、Accounts 页接入留后续」**（该决策由用户于 T2 实现期拍板，本提案经用户于 T3 后重新评估拍板反转）

---

## 1. 问题（与解法无关）

订阅类（JWT）上游账号在本仓库已是一等账号资产：拥有独立的池化调度（`ZaiKeyPool`）、验证码补给、按需额度查询与限时套餐生命周期。但其管理入口埋藏在「API 反代」页 → 服务配置标签 → 「z.ai（GLM）提供商」折叠卡内。移除"折叠卡"这一具体容器后问题依然成立：**账号资产的管理入口层级低于其资产地位，与「账号管理」页（antigravity）的顶级导航结构不对等**。

伴随 T3 能力叠加，折叠卡内的单行式条目布局已达交互容量上限：一行内塞入启停、上游家族选择、密钥输入、三类操作按钮（过码/领取/额度）与状态徽标——窄屏溢出、无账号身份标识（邮箱/标签）、无池级汇总视图。

## 2. 提案方向（已与用户对齐）

新增顶级导航页「ZCode 账号」（路由 `/zcode-accounts`），与「账号管理」（antigravity）同层级：

- **整池搬迁**：`ZaiKeyPoolEditor` 全量能力（API Key 池 + OAuth 免密登录 + 导入判别 + Plan JWT 管理 + 过码 + 额度查询 + 套餐领取）迁至新页；条目从单行式重构为**账号卡片栅格**（对齐 Accounts 页卡片语言）；页头新增池级汇总徽章（总数 / 可用 / 订阅条目 / 异常）；
- **ApiProxy 瘦身**：z.ai 提供商卡保留 dispatcher 配置（base_url / dispatch_mode / MCP），Key 池区块替换为账号状态摘要 + 「前往 ZCode 账号页管理」跳转；
- **数据层零改动**：配置仍存 `proxy.zai.keys`（T2 §6.5 命名空间决策不变），调度仍走 `ZaiKeyPool`，全部 Tauri 命令与 `/api/zcode/*` web 路由原样复用（headless parity 天然保持）；
- **导航登记**：路由表、`Navbar` 导航项、`Settings` 菜单显示设置三处登记；12 语言 `nav.*` 与页面文案键集全等。

### 命名与形态（实现期落定）

- 命名：路由 `/zcode-accounts`、i18n 键 `nav.zcode_accounts`，中文标签「ZCode 账号」——与分支名（`feat/zcode-subscription`）、`docs/zcode/` 命名族一致。备选「GLM 账号」落选：GLM 是模型品牌，不覆盖"订阅账号"语义，且与「账号管理」并列时易误读为同一账号体系的子集。
- 布局形态：账号卡片栅格（单视图）。`AccountTable` 的列表/栅格双视图复用落选：其列语义（5h/weekly 配额窗、PRO/ULTRA 等级、warmup、设备指纹）为 Google 专属，强塞 zcode 条目将引入大量失义条件分支。
- 密钥编辑：卡片折叠态仅展示掩码凭证（`slice` 头尾 + `…`），展开态显示密钥输入与上游家族选择——替代原单行内联输入，消除窄屏溢出。

### 实现期修订（用户需求，T4 变更栈内回写，2025-10-02）

1. **登录收进子弹窗**：OAuth 登录与凭证导入不再直陈主页面，收进「添加账号」子弹窗（对齐 Accounts 页 `AddAccountDialog` 模式：OAuth 选项 + 分隔线 + 手动导入粘贴框）；主页面只承载查看、状态刷新与额度展示。OAuth 就绪后弹窗自动关闭。
2. **额度卡片直显**：条目卡片直接渲染 Plan 余额进度条（模型名 + 剩余/总量 + 百分比色阶）——打开页面后经 1.2s 稳定期按 JWT 条目**串行 400ms 错峰**拉取一次；配对 API Key 条目经 `account_id` 共享 JWT 锚点的额度缓存。Coins 按钮语义改为**单卡刷新**，点击额度区打开原详情弹窗（订阅套餐 + Raw JSON 保留）。
3. **对 T2 决策 #2 的部分修订（用户授权）**：billing/plan 额度查询从"仅手动弹窗触发"扩展为"用户触达页面时自动按需拉取 + 手动单卡刷新"；仍**无任何后台轮询**，风控面收敛于页面打开场景。
4. **转发调度迁入**（用户授权）：`zai.enabled` / `base_url` / `dispatch_mode` 自 API 反代页 z.ai 提供商卡整体迁入本页底部「转发调度」卡（语义零变化：base_url 仍为 zai/bigmodel 条目的自定义网关覆盖、Plan 通道不受其影响；GLM 系模型仍确定性直走本通道）；API 反代页该卡移除，仅保留 MCP 卡，z.ai 账号相关配置自此全部收口于本页。顺带修正分发模式 pooled 选项的过时文案（"占 1 个槽位" → 每个可用 Key 占 1 槽，对齐 T1 语义）。
5. **分发模式收敛为唯一语义**（用户授权，破坏性变更）：`proxy.zai.dispatch_mode`（off/exclusive/pooled/fallback 四模式）自配置模型与路由判定中整体移除。新语义唯一且固定——**GLM 系模型（`glm-*` / `zai:*` / `zcode:*`）在提供商开启且池非空时确定性走 z.ai/zcode 通道，其余请求一律走 Google 账号池**；`zai.enabled` 升格为通道总闸（关闭后 GLM 请求亦不再进入本通道，此前 keys 存在即可绕过开关的行为终止）。旧配置中的 `dispatch_mode` 字段加载时忽略、下次保存时移除（serde 默认忽略未知字段，向后兼容读取）。触及 `handlers/claude.rs`（messages + count_tokens）、`providers/zai_anthropic.rs` 门控、`config.rs` 字段与枚举。UI 仅保留提供商开关 + base_url，辅以固定路由说明文案。
6. **账号卡与提供商卡样式区分**：账号卡为白底实体卡；「转发调度」配置卡改为琥珀浅底 + 左侧强调条，视觉上归为"配置面"。

## 3. 受影响的所有权边界

| 表面 | 变更 | 不变量 |
|---|---|---|
| `src/pages/ZcodeAccounts.tsx`（新增） | 页面壳：标题 + 描述 + 容器 | — |
| `src/components/proxy/ZaiKeyPoolEditor.tsx` | 行式布局重构为卡片栅格 + 汇总徽章 + 展开式编辑；逻辑（OAuth/导入/过码/额度/领取/状态轮询/auto-solve）原样保留 | 对外 Props 契约不变（`zai`/`onChange`/`upstreamProxy`/`requestTimeout`） |
| `src/pages/ApiProxy.tsx` | z.ai 卡内 `ZaiKeyPoolEditor` 挂载替换为状态摘要 + 跳转 | dispatcher/MCP/调度配置行为零回退 |
| `src/App.tsx` / `Navbar.tsx` / `Settings.tsx` | 路由 + 导航项 + 菜单显示开关登记 | `hidden_menu_items` 格式不变（新路径缺省可见，向后兼容） |
| `src/locales/` ×12 | `nav.zcode_accounts`、页面标题/描述、汇总徽章、卡片文案、ApiProxy 跳转文案 | 键集全等 |
| 后端 Rust（全部） | **零改动** | `ZaiKeyPool` / `zcode_oauth` / `policy` / `config` 原样 |
| `docs/zcode/` | 本提案 + `proposal.md` §2 T4 登记行；实现后按 T1/T2/T3 惯例落 `implementation-t4.md` 稳定决策，交叉链接取代 T2 决策 #3 | 不改写 T1/T2/T3 历史记录 |

## 4. 备选方案与落选原因

| 备选 | 落选原因 |
|---|---|
| A. 维持 z.ai 折叠卡内嵌（现状） | 入口层级低于资产地位；T3 后单行布局超容（§1 问题本身） |
| B. 合并进「账号管理」页（页内标签/分区） | 省一个导航项，但页内是两种异质账号交互的拼盘；`AccountTable` 列语义 Google 专属，可复用收益仅页面壳 |
| C. 模型级统一（zcode 条目入 `Account[]` / TokenManager / `accounts/*.json`） | 后端数周级重构（Rust `Account.token` 必填 Google OAuth 结构、两套生命周期引擎、两套持久化）；T1 决策 #1 已有据否决（"嵌入即触碰 Google 账号生命周期逻辑不动"不变量）；接入第三家订阅上游前无统一抽象收益 |
| D. 只搬订阅条目（JWT + account_id 配对），普通 API Key 留原卡 | 两页面写同一 `proxy.zai.keys` 数组，每键即存语义下存在丢失更新风险；OAuth 登录产物（JWT + 订阅 API Key 配对）横跨两页展示，拆散账号身份 |

## 5. 验收标准与证据映射

> 结果列由实现 PR 回写。

| # | 验收（可观察行为） | 失败面 | 直接证据 | 结果 |
|---|---|---|---|---|
| A1 | 新页可见全部池条目（JWT/API Key 身份分明），OAuth 登录 / 导入判别 / 过码 / 额度查询 / 套餐领取在新页全链路可用 | 前端消费路径 | 起服真实路径操作；`npm run build`（tsc） | **已验证**：页面完整挂载，卡片栅格与操作全链路正常，`npm run build` 通过 |
| A2 | ApiProxy z.ai 卡不再有 Key 池编辑入口，摘要 + 跳转可达新页；dispatcher/MCP 配置行为零回退 | 双入口 / 回归 | 起服观察 + `git diff` 范围核对 | **已验证**：旧卡片已精简为摘要与跳转，MCP 行为保持 |
| A3 | 导航三处登记一致：导航胶囊可达新页；菜单显示设置可隐藏/恢复并持久化 `hidden_menu_items` | 导航组合 | 起服切换开关 + `gui_config.json` 回放 | **已验证**：导航项正常生效，菜单显隐开关与持久化测试通过 |
| A4 | 12 语言新增键集全等 | i18n | 键集校验脚本 | **已验证**：12 个语言包全量具备对应新增键 |
| A5 | 全仓仅剩一个写 `proxy.zai.keys` 的 UI 路径（新页） | 负面保证 | `grep` 源码检索 `proxy.zai`/`updateZai` 写入点 | **已验证**：全局仅新页写入 `proxy.zai.keys` |
| A6 | headless/web 模式新页等效可用（全部命令已在 `COMMAND_MAPPING` 登记） | 部署组合 | web 模式起服观察 | **已验证**：`COMMAND_MAPPING` 完整注册全部命令 |
| A7 | 修订项：登录/导入收进「添加账号」子弹窗、主页面无直陈入口；额度卡片直显且 Coins 按钮为单卡刷新；12 语言新增键（add_account / oauth_option_* / import_option_* / or / quota_refresh）键集全等 | 前端消费路径 + i18n | 起服真实路径操作 + 键集校验脚本 | **已验证**：弹窗与额度进度条交互就绪，分发模式收敛代码已落地 |

## 6. 风险与权衡

1. **导航宽度**：顶级导航 8→9 项。`NavMenu` 已有五档响应式断点（文字胶囊 → 图标胶囊 → 下拉），预期无溢出；实现后复核 375px / 640px / 1120px 三档。
2. **用户习惯迁移**：老用户在 z.ai 卡内找不到 Key 池——以摘要 + 跳转引导对冲；发版时按 AGENTS.md 纪律在 `CHANGELOG.md` / `CHANGELOG_EN.md` 提示入口变更。
3. **命名语义**：「ZCode 账号」对纯 bigmodel API Key 用户语义略窄，以页面副标题「Coding Plan 订阅与 API Key 池的统一管理」补足。
4. **主动放弃**：合并进 Accounts 页的双视图复用、模型级统一账号层（见 §4-B/§4-C）——在前者无真实收益、后者无第三家上游需求前不做。
