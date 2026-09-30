//! zcode T2：OAuth CLI 登录 + 订阅凭证链（净室实现）。
//!
//! 权威决策记录：`docs/zcode/implementation-t2.md`。本模块仅依据公开协议事实文档
//! （端点、请求/响应形态、状态码语义）重写，未接触、未搬运任何参考实现代码。
//!
//! 协议事实（净室规格来源）：
//! - 登录（CLI polling 流程，流程 300s 过期）：
//!   `POST {ZCODE}/api/v1/oauth/cli/init`，`Authorization: Bearer <poll_token>`，
//!   body `{"provider":"zai"}` → `{"code":0,"data":{flow_id, authorize_url, expires_at, poll_interval_sec}}`；
//!   用户浏览器打开 `authorize_url` 登录后轮询
//!   `GET {ZCODE}/api/v1/oauth/cli/poll/{flow_id}`（Bearer poll_token）→
//!   pending：`data.status == "pending"`；ready：`data.status == "ready"`，
//!   `data.token`（zcode JWT）+ `data.zai.access_token` + `data.user`；
//!   body `code == 3004` → 登录会话过期（提案 T2 定义的过期信号）。
//!   poll_token 兼容两种协议形态：服务端下发（init 响应 data 内）优先，
//!   未下发时本地生成 32 位 hex（兼容旧协议，与桌面端一致）。
//! - 业务 JWT：`POST {APIZ}/api/auth/z/login`，body `{"token": <zai access_token>}`
//!   → `data.access_token`（仅用于管理面：billing/订阅/开钥，绝不用于消息转发）。
//! - 开钥链（best-effort；各端点已由协议事实确认，响应 schema 未公开文档化 → 容错解析，
//!   任一步失败即整体降级为"手动粘贴 Key 补全"，不阻塞登录）：
//!   `GET {APIZ}/api/biz/customer/getCustomerInfo` → 选 org + project；
//!   `GET/POST {APIZ}/api/biz/v1/organization/{org}/projects/{proj}/api_keys`
//!   （复用名为 `zcode-api-key` 的既有 Key，否则创建）；
//!   `GET .../api_keys/copy/{apiKey}` → secretKey；最终 Key = `{apiKey}.{secretKey}`。
//!   该 Key 在 api.z.ai Anthropic 兼容端点直连消耗 Coding Plan 订阅额度（免验证码）。
//! - 订阅查询：`GET {APIZ}/api/biz/subscription/list`（Bearer 业务 JWT），响应结构
//!   未公开文档化 → 原样透传 data 由前端容错展示。
//!
//! 安全边界：所有令牌仅本地存储；日志与错误信息绝不回显令牌原文。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::proxy::config::{UpstreamProxyConfig, ZaiKeyEntry, ZaiKeyMode, ZaiProvider};

/// 单一真相源常量（提案 §6.2 收口纪律）：zcode OAuth/管理面端点。
pub const ZCODE_BASE_URL: &str = "https://zcode.z.ai";
pub const APIZ_BASE_URL: &str = "https://api.z.ai";
/// 桌面端同款开钥名：复用既有 Key，避免每次登录重复创建。
pub const OAUTH_KEY_NAME: &str = "zcode-api-key";
/// 登录会话过期信号（提案 T2）。
pub const OAUTH_SESSION_EXPIRED_CODE: i64 = 3004;
/// 登录流程服务端过期窗口（协议事实：300s；本地兜底上界）。
pub const OAUTH_FLOW_TTL_SECS: u64 = 300;

/// 一次登录流程的客户端态（前端持有并在 poll 时回传，后端无会话状态）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZcodeOauthFlow {
    pub flow_id: String,
    pub authorize_url: String,
    /// 服务端下发优先；未下发时本地生成（协议兼容两种形态）。
    pub poll_token: String,
    /// 本地兜底截止（unix 毫秒）。
    pub expires_at_ms: u64,
    pub poll_interval_secs: u64,
}

/// 轮询单次结果。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ZcodePollOutcome {
    Pending,
    Expired,
    Ready {
        zcode_jwt: String,
        zai_access_token: String,
        user_email: String,
        account_id: String,
    },
}

/// 登录完成后的入池产物。
#[derive(Debug, Clone, Serialize)]
pub struct ZcodeLoginResult {
    /// JWT 条目（Plan 通道凭证，T3 前不参与转发）+（开钥成功时）同账号 API Key 条目。
    pub entries: Vec<ZaiKeyEntry>,
    /// 开钥链降级说明（成功开钥时为 None）。
    pub warning: Option<String>,
}

// ===== 容错提取器（纯函数，单测覆盖多种响应形态） =====

/// 从 `{"code":x,"data":...,"message"/"msg":...}` 信封中取 `(code, data, message)`。
fn split_envelope(body: &Value) -> (Option<i64>, Option<&Value>, Option<String>) {
    let code = body.get("code").and_then(|c| c.as_i64());
    let data = body.get("data").filter(|d| !d.is_null());
    let message = ["message", "msg", "error"]
        .iter()
        .find_map(|k| body.get(*k).and_then(|m| m.as_str()).map(|s| s.to_string()));
    (code, data, message)
}

/// 从 OAuth poll 的 user 对象容错提取 (account_id, email)。
pub fn extract_user_fields(user: &Value) -> (String, String) {
    let id = ["id", "user_id", "userId", "uid", "uuid"]
        .iter()
        .find_map(|k| user.get(*k))
        .and_then(value_to_clean_string)
        .unwrap_or_default();
    let email = ["email", "user_email", "mail", "account"]
        .iter()
        .find_map(|k| user.get(*k))
        .and_then(value_to_clean_string)
        .unwrap_or_default();
    (id, email)
}

fn value_to_clean_string(v: &Value) -> Option<String> {
    let s = match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    let t = s.trim().to_string();
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

/// 在对象中按候选键名取首个非空字符串。
fn first_string_field(obj: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| obj.get(*k))
        .and_then(value_to_clean_string)
}

/// 从 getCustomerInfo 响应容错提取 (org_id, project_id)。
/// 协议事实（参考实现确认）：`data.organizations[]`，org 字段 `organizationId`/
/// `organizationName`，内嵌 `projects[]`（`projectId`/`projectName`）；
/// 优先"默认机构"/"默认项目"，否则取首个；另保留平铺 id 字段等兜底形态。
pub fn extract_org_project(body: &Value) -> Option<(String, String)> {
    let data = body.get("data").filter(|d| !d.is_null()).unwrap_or(body);

    // 形态 A：data.organizations[] 数组，每项含 organizationId 与 projects[]
    if let Some(list) = data.get("organizations").and_then(|v| v.as_array()) {
        // "默认机构"优先（参考实现同语义），否则首个
        let org = list
            .iter()
            .find(|o| {
                o.get("organizationName")
                    .and_then(|n| n.as_str())
                    .map(|n| n.contains("默认"))
                    .unwrap_or(false)
            })
            .or_else(|| list.first());
        if let Some(org) = org {
            if let Some(org_id) =
                first_string_field(org, &["organizationId", "id", "orgId", "org_id", "uuid"])
            {
                let projects = org.get("projects").and_then(|v| v.as_array());
                let proj = projects.and_then(|projects| {
                    projects
                        .iter()
                        .find(|p| {
                            p.get("projectName")
                                .and_then(|n| n.as_str())
                                .map(|n| n.contains("默认"))
                                .unwrap_or(false)
                        })
                        .or_else(|| projects.first())
                });
                if let Some(proj) = proj {
                    if let Some(proj_id) = first_string_field(
                        proj,
                        &["projectId", "id", "project_id", "proj_id", "uuid"],
                    ) {
                        return Some((org_id, proj_id));
                    }
                }
                // org 无内嵌 projects：项目可能平铺在 data 顶层
                if let Some(proj_id) =
                    first_string_field(data, &["project_id", "projectId", "proj_id"])
                {
                    return Some((org_id, proj_id));
                }
            }
        }
    }

    // 形态 B：data 平铺 organization_id/project_id（或 org_id）
    let org_id = first_string_field(
        data,
        &["organization_id", "org_id", "orgId", "organizationId"],
    );
    let proj_id = first_string_field(data, &["project_id", "projectId", "proj_id"]);
    if let (Some(o), Some(p)) = (org_id, proj_id) {
        return Some((o, p));
    }

    // 形态 C：单对象 data.organization{id} + data.project{id}
    if let (Some(org), Some(proj)) = (data.get("organization"), data.get("project")) {
        if let (Some(o), Some(p)) = (
            first_string_field(org, &["id", "uuid"]),
            first_string_field(proj, &["id", "uuid"]),
        ) {
            return Some((o, p));
        }
    }
    None
}

/// 从 api_keys 列表响应中容错找出名为 `zcode-api-key` 的既有 Key。
pub fn extract_existing_api_key(body: &Value) -> Option<String> {
    let data = body.get("data").filter(|d| !d.is_null()).unwrap_or(body);
    let list = find_first_array(data, &["api_keys", "apiKeys", "keys", "items", "list"])?;
    for item in list {
        let name = first_string_field(item, &["name", "key_name", "label"]).unwrap_or_default();
        if name == OAUTH_KEY_NAME {
            if let Some(key) =
                first_string_field(item, &["key", "apiKey", "api_key", "token", "id"])
            {
                return Some(key);
            }
        }
    }
    None
}

/// 从创建/详情响应中提取新建 Key（与列表同构，但无名称过滤）。
pub fn extract_created_api_key(body: &Value) -> Option<String> {
    let data = body.get("data").filter(|d| !d.is_null()).unwrap_or(body);
    if let Some(list) = find_first_array(data, &["api_keys", "apiKeys", "keys", "items", "list"]) {
        for item in list {
            if let Some(key) =
                first_string_field(item, &["key", "apiKey", "api_key", "token", "id"])
            {
                return Some(key);
            }
        }
    }
    first_string_field(data, &["key", "apiKey", "api_key", "token", "id"])
}

/// 从 copy 端点响应提取 secretKey。
pub fn extract_secret_key(body: &Value) -> Option<String> {
    let data = body.get("data").filter(|d| !d.is_null()).unwrap_or(body);
    first_string_field(data, &["secretKey", "secret_key", "secret", "key"])
}

fn find_first_array<'a>(root: &'a Value, keys: &[&str]) -> Option<&'a Vec<Value>> {
    keys.iter()
        .find_map(|k| root.get(*k))
        .and_then(|v| v.as_array())
        .or_else(|| {
            // 兜底：data 本身就是数组
            root.as_array()
        })
}

// ===== 登录产物组装（纯函数） =====

/// 由登录产物组装入池条目：JWT 条目恒有；API Key 条目仅开钥成功时存在。
/// 同账号两凭证经 `account_id` 配对（提案 T2"JWT 与同账号 API Key 并存"锚点）。
pub fn build_account_entries(
    zcode_jwt: &str,
    business_jwt: &str,
    provisioned_key: Option<&str>,
    account_id: &str,
    user_email: &str,
) -> Vec<ZaiKeyEntry> {
    let identity = if !user_email.is_empty() {
        user_email.to_string()
    } else {
        account_id.to_string()
    };
    let mut entries = vec![ZaiKeyEntry {
        key: zcode_jwt.to_string(),
        provider: ZaiProvider::ZcodePlan,
        enabled: true,
        label: if identity.is_empty() {
            "OAuth 登录（Plan 通道）".to_string()
        } else {
            format!("OAuth {identity}（Plan 通道）")
        },
        mode: ZaiKeyMode::Jwt,
        account_id: account_id.to_string(),
        user_email: user_email.to_string(),
        business_jwt: business_jwt.to_string(),
    }];
    if let Some(k) = provisioned_key {
        entries.push(ZaiKeyEntry {
            key: k.to_string(),
            provider: ZaiProvider::Zai,
            enabled: true,
            label: if identity.is_empty() {
                "OAuth 自动开通".to_string()
            } else {
                format!("OAuth {identity} 自动开通")
            },
            mode: ZaiKeyMode::ApiKey,
            account_id: account_id.to_string(),
            user_email: user_email.to_string(),
            business_jwt: business_jwt.to_string(),
        });
    }
    entries
}

// ===== HTTP 编排 =====

fn build_http_client(
    upstream_proxy: &UpstreamProxyConfig,
    request_timeout: u64,
) -> Result<reqwest::Client, String> {
    // OAuth/管理面对齐官方 CLI wire 形态（zcode.cjs createZaiCliOAuthClient /
    // 参考实现 httpx）：HTTP/1.1、无浏览器伪装头；仅应用层自定义头。
    let mut builder = reqwest::Client::builder()
        .http1_only()
        .timeout(std::time::Duration::from_secs(request_timeout.max(10)));
    if upstream_proxy.enabled && !upstream_proxy.url.is_empty() {
        let proxy = reqwest::Proxy::all(&upstream_proxy.url)
            .map_err(|e| format!("Invalid upstream proxy url: {}", e))?;
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))
}

/// 本地 poll_token：64 位 hex（对齐官方 CLI `randomBytes(32).toString("hex")`
/// 与参考实现 `secrets.token_hex(32)`；实测 32 位 hex 被服务端判 `invalid_flow`）。
fn generate_local_poll_token() -> String {
    let a = uuid::Uuid::new_v4().simple();
    let b = uuid::Uuid::new_v4().simple();
    format!("{a}{b}")
}

async fn api_json(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    bearer: Option<&str>,
    json_body: Option<Value>,
) -> Result<Value, String> {
    let mut req = client
        .request(method, url)
        .header("Content-Type", "application/json");
    if let Some(b) = bearer {
        req = req.header("Authorization", format!("Bearer {b}"));
    }
    if let Some(b) = json_body {
        req = req.json(&b);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("Upstream request failed: {}", e))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("Failed to read response: {}", e))?;
    let body: Value = serde_json::from_str(&text)
        .map_err(|_| format!("Upstream returned non-JSON response (HTTP {status})"))?;
    let (code, _, message) = split_envelope(&body);
    if let Some(c) = code {
        if c != 0 {
            return Err(format!(
                "Upstream business error (code {}{})",
                c,
                message
                    .map(|m| format!(": {m}"))
                    .unwrap_or_else(|| " (no message)".to_string())
            ));
        }
    } else if !status.is_success() {
        return Err(format!("Upstream returned HTTP {}", status.as_u16()));
    }
    Ok(body)
}

/// 发起登录流程（双协议形态自适应，均为协议事实）：
/// 1. 新协议（服务端下发 poll_token）：init 不带认证头，响应 data 内含 poll_token；
///    服务端把 Bearer 校验为 flow 引用，本地随机 token 会被拒（实测 `3004 invalid_flow`）。
/// 2. 旧协议兼容（ZCode 3.10.1 事实）：init 带 `Authorization: Bearer <本地 poll_token>`，
///    响应无 poll_token，轮询沿用本地 token。
pub async fn oauth_start(
    upstream_proxy: &UpstreamProxyConfig,
    request_timeout: u64,
) -> Result<ZcodeOauthFlow, String> {
    let client = build_http_client(upstream_proxy, request_timeout)?;
    let local_poll_token = generate_local_poll_token();
    let url = format!("{ZCODE_BASE_URL}/api/v1/oauth/cli/init");
    let payload = serde_json::json!({ "provider": "zai" });

    let init = api_json(
        &client,
        reqwest::Method::POST,
        &url,
        None,
        Some(payload.clone()),
    )
    .await;
    let (body, fallback_token) = match init {
        Ok(b) => (b, None),
        Err(new_protocol_err) => {
            let legacy = api_json(
                &client,
                reqwest::Method::POST,
                &url,
                Some(&local_poll_token),
                Some(payload),
            )
            .await;
            match legacy {
                Ok(b) => (b, Some(local_poll_token.clone())),
                Err(legacy_err) => {
                    return Err(format!(
                        "OAuth init failed (server-issued poll_token form: {new_protocol_err}; legacy local-poll_token form: {legacy_err})"
                    ))
                }
            }
        }
    };
    let data = body
        .get("data")
        .cloned()
        .ok_or("OAuth init response missing data")?;
    let flow_id = data
        .get("flow_id")
        .or_else(|| data.get("flowId"))
        .and_then(value_to_clean_string)
        .ok_or("OAuth init response missing flow_id")?;
    let authorize_url = data
        .get("authorize_url")
        .or_else(|| data.get("authorizeUrl"))
        .and_then(value_to_clean_string)
        .ok_or("OAuth init response missing authorize_url")?;
    // 轮询凭证：服务端下发优先（新协议）；旧协议回退形态下沿用本地 token；
    // 新形态但响应缺失 poll_token → schema 漂移，明确报错（本地 token 此时无效）
    let poll_token = data
        .get("poll_token")
        .or_else(|| data.get("pollToken"))
        .and_then(value_to_clean_string)
        .or_else(|| fallback_token.clone())
        .ok_or_else(|| {
            "OAuth init succeeded (server-issued form) but response missing poll_token; cannot poll — please report this response shape".to_string()
        })?;
    let poll_interval_secs = data
        .get("poll_interval_sec")
        .or_else(|| data.get("pollIntervalSec"))
        .and_then(|v| v.as_u64())
        .unwrap_or(2)
        .clamp(1, 10);
    let expires_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
        + OAUTH_FLOW_TTL_SECS * 1000;
    Ok(ZcodeOauthFlow {
        flow_id,
        authorize_url,
        poll_token,
        expires_at_ms,
        poll_interval_secs,
    })
}

/// 轮询单次。code=3004 / HTTP 404|410 / 本地过期 → Expired。
pub async fn oauth_poll_once(
    flow: &ZcodeOauthFlow,
    upstream_proxy: &UpstreamProxyConfig,
    request_timeout: u64,
) -> Result<ZcodePollOutcome, String> {
    if now_ms() >= flow.expires_at_ms {
        return Ok(ZcodePollOutcome::Expired);
    }
    let client = build_http_client(upstream_proxy, request_timeout)?;
    let url = format!("{ZCODE_BASE_URL}/api/v1/oauth/cli/poll/{}", flow.flow_id);
    let send = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", flow.poll_token))
        .send()
        .await;
    let resp = match send {
        Ok(r) => r,
        Err(_) => return Ok(ZcodePollOutcome::Pending), // 网络抖动视为未就绪，由前端超时兜底
    };
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("Failed to read response: {}", e))?;
    let body: Value = serde_json::from_str(&text)
        .map_err(|_| format!("OAuth poll returned non-JSON response (HTTP {status})"))?;
    let (code, data, _) = split_envelope(&body);
    // 协议事实：poll 4xx 承载 code=3004 → 会话过期（重新发起）；其余 4xx → 终态失败
    if code == Some(OAUTH_SESSION_EXPIRED_CODE) {
        return Ok(ZcodePollOutcome::Expired);
    }
    if status.is_client_error() {
        return Err(format!(
            "OAuth poll failed (HTTP {}, code {:?})",
            status.as_u16(),
            code
        ));
    }
    if !status.is_success() {
        return Ok(ZcodePollOutcome::Pending); // 5xx 视为上游抖动，继续轮询
    }
    let data = data.cloned().ok_or("OAuth poll response missing data")?;
    let flow_status = data
        .get("status")
        .and_then(|s| s.as_str())
        .unwrap_or("pending");
    match flow_status {
        "ready" => {
            // 协议事实：ready 产物字段两种结构——data.token / data.accessToken，
            // zai access_token 在 data.zai.access_token；zcode JWT 亦可能为 zcodejwttoken
            let zcode_jwt = data
                .get("token")
                .or_else(|| data.get("accessToken"))
                .or_else(|| data.get("zcodejwttoken"))
                .and_then(value_to_clean_string)
                .ok_or("OAuth poll ready response missing token")?;
            let zai_access_token = data
                .get("zai")
                .and_then(|z| z.get("access_token"))
                .or_else(|| data.get("zaiAccessToken"))
                .and_then(value_to_clean_string)
                .unwrap_or_default();
            let (account_id, user_email) = data
                .get("user")
                .map(extract_user_fields)
                .unwrap_or_default();
            Ok(ZcodePollOutcome::Ready {
                zcode_jwt,
                zai_access_token,
                user_email,
                account_id,
            })
        }
        _ => Ok(ZcodePollOutcome::Pending),
    }
}

/// 登录产物 → 业务 JWT + 开钥链 → 入池条目。任一管理面步骤失败仅降级并说明，
/// 不丢弃已取得的登录凭证。
pub async fn complete_login(
    outcome: ZcodePollOutcome,
    upstream_proxy: &UpstreamProxyConfig,
    request_timeout: u64,
) -> Result<ZcodeLoginResult, String> {
    let (zcode_jwt, zai_access_token, user_email, account_id) = match outcome {
        ZcodePollOutcome::Pending => return Err("OAuth flow is still pending".to_string()),
        ZcodePollOutcome::Expired => return Err("OAuth session expired (3004)".to_string()),
        ZcodePollOutcome::Ready {
            zcode_jwt,
            zai_access_token,
            user_email,
            account_id,
        } => (zcode_jwt, zai_access_token, user_email, account_id),
    };

    let client = build_http_client(upstream_proxy, request_timeout)?;

    // 业务 JWT（billing/订阅/开钥专用）
    let business_jwt = if zai_access_token.is_empty() {
        String::new()
    } else {
        let url = format!("{APIZ_BASE_URL}/api/auth/z/login");
        match api_json(
            &client,
            reqwest::Method::POST,
            &url,
            None,
            Some(serde_json::json!({ "token": zai_access_token })),
        )
        .await
        {
            Ok(body) => body
                .pointer("/data/access_token")
                .and_then(value_to_clean_string)
                .unwrap_or_default(),
            Err(_) => String::new(), // 降级：无业务 JWT 则 billing 不可用，转发不受影响
        }
    };

    // 开钥链（best-effort，schema 未文档化 → 容错解析，失败即降级手动粘贴）
    let provisioned = provision_api_key(&client, &business_jwt).await;
    let (provisioned_key, warning) = match provisioned {
        Ok(k) => (Some(k), None),
        Err(step) => (
            None,
            Some(format!(
                "OAuth 登录成功，但自动开通 API Key 失败（{step}）；请手动粘贴 API Key 补全该账号"
            )),
        ),
    };

    Ok(ZcodeLoginResult {
        entries: build_account_entries(
            &zcode_jwt,
            &business_jwt,
            provisioned_key.as_deref(),
            &account_id,
            &user_email,
        ),
        warning,
    })
}

async fn provision_api_key(client: &reqwest::Client, business_jwt: &str) -> Result<String, String> {
    if business_jwt.is_empty() {
        return Err("业务 JWT 不可用".to_string());
    }

    // 1. org/project
    let info_url = format!("{APIZ_BASE_URL}/api/biz/customer/getCustomerInfo");
    let info = api_json(
        client,
        reqwest::Method::GET,
        &info_url,
        Some(business_jwt),
        None,
    )
    .await?;
    let (org_id, project_id) = extract_org_project(&info)
        .ok_or("getCustomerInfo 响应中未识别出 organization/project（schema 未文档化）")?;

    // 2. 复用既有 zcode-api-key，否则创建
    let list_url =
        format!("{APIZ_BASE_URL}/api/biz/v1/organization/{org_id}/projects/{project_id}/api_keys");
    let list = api_json(
        client,
        reqwest::Method::GET,
        &list_url,
        Some(business_jwt),
        None,
    )
    .await?;
    let api_key = match extract_existing_api_key(&list) {
        Some(k) => k,
        None => {
            let created = api_json(
                client,
                reqwest::Method::POST,
                &list_url,
                Some(business_jwt),
                Some(serde_json::json!({ "name": OAUTH_KEY_NAME })),
            )
            .await?;
            extract_created_api_key(&created)
                .ok_or("api_keys 创建响应中未识别出 key 字段（schema 未文档化）")?
        }
    };

    // 3. secretKey → 最终 Key = "{apiKey}.{secretKey}"
    let copy_url = format!("{list_url}/copy/{api_key}");
    let copy = api_json(
        client,
        reqwest::Method::GET,
        &copy_url,
        Some(business_jwt),
        None,
    )
    .await?;
    let secret = extract_secret_key(&copy)
        .ok_or("api_keys/copy 响应中未识别出 secretKey（schema 未文档化）")?;
    Ok(format!("{api_key}.{secret}"))
}

/// 订阅/额度查询（手动按需，Bearer 业务 JWT）。响应结构未公开文档化 → data 原样透传。
pub async fn query_subscription(
    business_jwt: &str,
    upstream_proxy: &UpstreamProxyConfig,
    request_timeout: u64,
) -> Result<Value, String> {
    if business_jwt.trim().is_empty() {
        return Err("该条目没有业务 JWT（仅 OAuth 登录的账号支持额度查询）".to_string());
    }
    let client = build_http_client(upstream_proxy, request_timeout)?;
    let url = format!("{APIZ_BASE_URL}/api/biz/subscription/list");
    let body = api_json(
        &client,
        reqwest::Method::GET,
        &url,
        Some(business_jwt),
        None,
    )
    .await?;
    Ok(body.get("data").cloned().unwrap_or(serde_json::Value::Null))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ===== envelope =====

    #[test]
    fn envelope_splits_code_data_message() {
        let body = json!({"code": 0, "data": {"status": "pending"}, "message": "ok"});
        let (code, data, msg) = split_envelope(&body);
        assert_eq!(code, Some(0));
        assert_eq!(data.unwrap()["status"], "pending");
        assert_eq!(msg.as_deref(), Some("ok"));
    }

    // ===== poll outcome mapping =====

    #[tokio::test]
    async fn poll_body_3004_maps_to_expired() {
        // body code=3004（HTTP 200）→ Expired；通过 mock server 语义无法单测 HTTP 层，
        // 此处验证信封解析分支：3004 优先于 data.status。
        let body = json!({"code": 3004, "data": {"status": "ready"}});
        let (code, _, _) = split_envelope(&body);
        assert_eq!(code, Some(OAUTH_SESSION_EXPIRED_CODE));
    }

    // ===== user extraction =====

    #[test]
    fn user_fields_probe_common_shapes() {
        let (id, email) = extract_user_fields(&json!({"id": "u-1", "email": "a@b.c"}));
        assert_eq!(id, "u-1");
        assert_eq!(email, "a@b.c");
        let (id, email) = extract_user_fields(&json!({"userId": 42, "mail": "x@y.z"}));
        assert_eq!(id, "42");
        assert_eq!(email, "x@y.z");
        let (id, email) = extract_user_fields(&json!({"nickname": "n"}));
        assert_eq!(id, "");
        assert_eq!(email, "");
    }

    // ===== org/project extraction =====

    #[test]
    fn org_project_from_nested_org_array() {
        let body = json!({
            "code": 0,
            "data": {"organizations": [
                {"id": "org-1", "projects": [{"id": "proj-1"}, {"id": "proj-2"}]}
            ]}
        });
        assert_eq!(
            extract_org_project(&body),
            Some(("org-1".into(), "proj-1".into()))
        );
    }

    #[test]
    fn org_project_from_confirmed_real_shape() {
        // 协议事实（参考实现确认）：organizations[].organizationId + projects[].projectId
        let body = json!({
            "code": 0,
            "data": {"organizations": [
                {"organizationId": "orgA", "organizationName": "测试机构",
                 "projects": [{"projectId": "projA", "projectName": "测试项目"}]},
                {"organizationId": "orgB", "organizationName": "默认机构",
                 "projects": [{"projectId": "projB", "projectName": "默认项目"}]}
            ]}
        });
        assert_eq!(
            extract_org_project(&body),
            Some(("orgB".into(), "projB".into()))
        );
    }

    #[test]
    fn local_poll_token_is_64_hex() {
        // 服务端把 Bearer 当 flow 引用做格式校验：须为 64 位 hex
        // （32 位 hex 实测被判 invalid_flow）
        let t = generate_local_poll_token();
        assert_eq!(t.len(), 64);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn existing_key_found_by_real_field_name() {
        // 协议事实：api_keys 条目的 Key 字段为 apiKey
        let body = json!({"code": 0, "data": [
            {"name": "other", "apiKey": "aaa.bbb"},
            {"name": "zcode-api-key", "apiKey": "id1.secret1"}
        ]});
        assert_eq!(extract_existing_api_key(&body), Some("id1.secret1".into()));
    }

    #[test]
    fn org_project_from_flat_fields() {
        let body = json!({"data": {"org_id": "o9", "project_id": "p9"}});
        assert_eq!(extract_org_project(&body), Some(("o9".into(), "p9".into())));
    }

    #[test]
    fn org_project_from_object_shape() {
        let body = json!({
            "code": 0,
            "data": {"organization": {"id": "oA"}, "project": {"id": "pA"}}
        });
        assert_eq!(extract_org_project(&body), Some(("oA".into(), "pA".into())));
    }

    #[test]
    fn org_project_unrecognized_returns_none() {
        let body = json!({"code": 0, "data": {"unrelated": true}});
        assert_eq!(extract_org_project(&body), None);
    }

    // ===== api key extraction =====

    #[test]
    fn existing_key_found_by_name() {
        let body = json!({"code": 0, "data": {"api_keys": [
            {"name": "other", "key": "aaa.bbb"},
            {"name": "zcode-api-key", "key": "id1.secret1"}
        ]}});
        assert_eq!(extract_existing_api_key(&body), Some("id1.secret1".into()));
    }

    #[test]
    fn existing_key_absent_returns_none() {
        let body = json!({"code": 0, "data": {"api_keys": [{"name": "other", "key": "k"}]}});
        assert_eq!(extract_existing_api_key(&body), None);
    }

    #[test]
    fn created_key_probed_from_list_shape() {
        let body = json!({"data": {"api_keys": [{"name": "zcode-api-key", "key": "new.key"}]}});
        assert_eq!(extract_created_api_key(&body), Some("new.key".into()));
        let body2 = json!({"data": {"key": "direct.key"}});
        assert_eq!(extract_created_api_key(&body2), Some("direct.key".into()));
    }

    #[test]
    fn secret_key_probed() {
        let body = json!({"data": {"secretKey": "s3cret"}});
        assert_eq!(extract_secret_key(&body), Some("s3cret".into()));
    }

    // ===== entry assembly =====

    #[test]
    fn build_entries_pairs_jwt_and_api_key_by_account() {
        let entries = build_account_entries(
            "jwt.def.sig",
            "biz.jwt",
            Some("id.secret"),
            "acc-1",
            "user@x.io",
        );
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].mode, ZaiKeyMode::Jwt);
        assert_eq!(entries[0].provider, ZaiProvider::ZcodePlan);
        assert_eq!(entries[1].mode, ZaiKeyMode::ApiKey);
        assert_eq!(entries[1].provider, ZaiProvider::Zai);
        assert_eq!(entries[0].account_id, "acc-1");
        assert_eq!(entries[1].account_id, "acc-1");
        assert_eq!(entries[1].business_jwt, "biz.jwt");
        assert!(entries[0].label.contains("user@x.io"));
    }

    #[test]
    fn build_entries_without_provisioned_key_has_jwt_only() {
        let entries = build_account_entries("jwt.def.sig", "", None, "acc-2", "");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].mode, ZaiKeyMode::Jwt);
        assert!(entries[0].business_jwt.is_empty());
        assert!(!entries[0].label.is_empty());
    }
}
