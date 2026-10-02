use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures::StreamExt;
use serde_json::Value;
use tokio::time::Duration;

use crate::proxy::server::AppState;

fn map_model_for_zai(original: &str, state: &crate::proxy::ZaiConfig) -> String {
    let m = original.to_lowercase();
    if let Some(mapped) = state.model_mapping.get(original) {
        if !mapped.trim().is_empty() {
            return mapped.clone();
        }
    }
    if let Some(mapped) = state.model_mapping.get(&m) {
        if !mapped.trim().is_empty() {
            return mapped.clone();
        }
    }
    if m.starts_with("zai:") {
        return original[4..].to_string();
    }
    if m.starts_with("zcode:") {
        return original[6..].to_string();
    }
    if m.starts_with("glm-") {
        return original.to_string();
    }
    if !m.starts_with("claude-") {
        return original.to_string();
    }
    if m.contains("opus") && !state.models.opus.trim().is_empty() {
        return state.models.opus.clone();
    }
    if m.contains("haiku") && !state.models.haiku.trim().is_empty() {
        return state.models.haiku.clone();
    }
    if !state.models.sonnet.trim().is_empty() {
        return state.models.sonnet.clone();
    }
    // [反代容错] 未显式配置映射时，Claude 系列模型默认回退至 GLM-5.3
    "GLM-5.3".to_string()
}

fn join_base_url(base: &str, path: &str) -> Result<String, String> {
    let base = base.trim_end_matches('/');
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{}", path)
    };
    Ok(format!("{}{}", base, path))
}

fn build_client(
    upstream_proxy: Option<crate::proxy::config::UpstreamProxyConfig>,
    timeout_secs: u64,
) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(timeout_secs.max(5)));

    if let Some(config) = upstream_proxy {
        if config.enabled && !config.url.is_empty() {
            let url = crate::proxy::config::normalize_proxy_url(&config.url);
            let proxy = reqwest::Proxy::all(&url)
                .map_err(|e| format!("Invalid upstream proxy url: {}", e))?;
            builder = builder.proxy(proxy);
        }
    }

    builder
        .tcp_nodelay(true) // [FIX #307] Disable Nagle's algorithm to improve latency for small requests
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))
}

fn copy_passthrough_headers(incoming: &HeaderMap) -> HeaderMap {
    // Only forward a conservative set of headers to avoid leaking the local proxy key or cookies.
    let mut out = HeaderMap::new();

    for (k, v) in incoming.iter() {
        let key = k.as_str().to_ascii_lowercase();
        match key.as_str() {
            "content-type" | "accept" | "anthropic-version" | "user-agent" => {
                out.insert(k.clone(), v.clone());
            }
            // Some clients use these for streaming; safe to pass through.
            "accept-encoding" | "cache-control" => {
                out.insert(k.clone(), v.clone());
            }
            _ => {}
        }
    }

    out
}

fn set_zai_auth(headers: &mut HeaderMap, incoming: &HeaderMap, api_key: &str) {
    // Prefer to keep the same auth scheme as the incoming request:
    // - If the client used x-api-key (Anthropic style), replace it.
    // - Else if it used Authorization, replace it with Bearer.
    // - Else default to x-api-key.
    let has_x_api_key = incoming.contains_key("x-api-key");
    let has_auth = incoming.contains_key(header::AUTHORIZATION);

    if has_x_api_key || !has_auth {
        if let Ok(v) = HeaderValue::from_str(api_key) {
            headers.insert("x-api-key", v);
        }
    }

    if has_auth {
        if let Ok(v) = HeaderValue::from_str(&format!("Bearer {}", api_key)) {
            headers.insert(header::AUTHORIZATION, v);
        }
    }
}

/// Recursively remove cache_control from all nested objects/arrays
/// [FIX #290] This is a defensive fix that works regardless of serde annotations
pub fn deep_remove_cache_control(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if let Some(v) = map.remove("cache_control") {
                tracing::info!(
                    "[ISSUE-744] Deep Cleaning found nested cache_control: {:?}",
                    v
                );
            }
            for v in map.values_mut() {
                deep_remove_cache_control(v);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                deep_remove_cache_control(v);
            }
        }
        _ => {}
    }
}

pub async fn forward_anthropic_json(
    state: &AppState,
    method: Method,
    path: &str,
    incoming_headers: &HeaderMap,
    mut body: Value,
    message_count: usize, // [NEW v4.0.0] Pass message count for rewind detection
) -> Response {
    use crate::proxy::providers::zai_pool::{ZaiKeyPool, MAX_FAILOVER_ATTEMPTS};

    let zai = state.zai.read().await.clone();
    let is_glm_model = body
        .get("model")
        .and_then(|v| v.as_str())
        .map(|m| {
            let lower = m.to_lowercase();
            lower.starts_with("glm-") || lower.starts_with("zai:") || lower.starts_with("zcode:")
        })
        .unwrap_or(false);
    // [zcode T4 修订] 唯一分发语义：本转发函数仅承接 GLM 系列模型；提供商开关为通道总闸。
    let zai_enabled = zai.enabled && is_glm_model;
    if !zai_enabled {
        return (StatusCode::BAD_REQUEST, "z.ai is disabled").into_response();
    }

    if let Some(model) = body.get("model").and_then(|v| v.as_str()) {
        let mapped = map_model_for_zai(model, &zai);
        body["model"] = Value::String(mapped.clone());

        // [FIX] Caching for z.ai (to support thinking-filter)
        if let Some(sig) = body
            .get("thinking")
            .and_then(|t| t.get("signature"))
            .and_then(|s| s.as_str())
        {
            crate::proxy::SignatureCache::global().cache_session_signature(
                "zai-session",
                sig.to_string(),
                message_count,
            );
            crate::proxy::SignatureCache::global().cache_thinking_family(sig.to_string(), mapped);
        }
    }

    // [FIX #290] Clean cache_control before sending to Anthropic API
    // This prevents "Extra inputs are not permitted" errors
    deep_remove_cache_control(&mut body);

    // [FIX #307] Explicitly serialize body to Vec<u8> to ensure Content-Length is set correctly.
    // This avoids "Transfer-Encoding: chunked" for small bodies which caused connection errors.
    let body_bytes = serde_json::to_vec(&body).unwrap_or_default();

    let timeout_secs = state.request_timeout.max(5);
    let upstream_proxy = state.upstream_proxy.read().await.clone();
    let client = match build_client(Some(upstream_proxy), timeout_secs) {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };

    // [zcode T1] 多 Key 轮询 + 账号级失败转移：
    // 每个可用 Key 占一个槽位（提案 A1）；账号级失败（429/529、5xx、401/403、402）
    // 在响应尚未下发客户端前原地换下一个可用 Key 重试，尝试次数有界。
    // [zcode T3] JWT 槽位走 Plan 通道（zcode.z.ai zcode-plan）：Bearer JWT +
    // 客户端身份头全集 + 验证码头；新鲜验证码是 JWT 槽位可调度的前提（池侧已过滤，
    // 此处二次防御）。
    let pool = ZaiKeyPool::global();
    let max_attempts = pool.available_count(&zai).clamp(1, MAX_FAILOVER_ATTEMPTS);

    for attempt in 0..max_attempts {
        let selected = match pool.select_key(&zai) {
            Some(s) => s,
            None => {
                // 区分“从未配置”与“全部不可用”，两者都是网关内部错误（防自噬，见 policy.rs）
                if zai.resolved_keys().is_empty() {
                    return (StatusCode::BAD_REQUEST, "z.ai api_key is not set").into_response();
                }
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "All z.ai API keys are unavailable (disabled, invalid, exhausted or rate-limited)",
                )
                    .into_response();
            }
        };

        // [zcode T3] Plan 通道分支准备（JWT 槽位）
        let is_plan = selected.mode == crate::proxy::config::ZaiKeyMode::Jwt;
        let plan_captcha_key = crate::proxy::providers::zcode_plan::captcha_key_for(
            &selected.account_id,
            &selected.key,
        );
        let mut plan_captcha_param: Option<String> = None;

        let (url, slot_headers, slot_body_bytes) = if is_plan {
            use crate::proxy::providers::zcode_plan as zplan;
            let captcha = match zplan::ZcodeCaptchaStore::global().take_fresh(&plan_captcha_key) {
                Some(c) => Some(c),
                None => {
                    // 无新鲜验证码：先尝试通过本地 Node 求解器极速求解（~500ms），
                    // 同时派发前端事件并支持 wait_fresh 等待
                    pool.mark_captcha_needed(&zai, &selected.key);
                    zplan::emit_captcha_needed_event(&selected.account_id, &selected.key);

                    if let Some(c) =
                        zplan::solve_captcha_via_node(&selected.account_id, &selected.key).await
                    {
                        Some(c)
                    } else {
                        tracing::info!(
                            "[zcode T3] plan slot {} waiting for fresh captcha (up to 15s)...",
                            selected.masked_key
                        );
                        zplan::ZcodeCaptchaStore::global()
                            .wait_fresh(&plan_captcha_key, std::time::Duration::from_secs(15))
                            .await
                    }
                }
            };

            let Some(captcha) = captcha else {
                tracing::warn!(
                    "[zcode T3] plan slot {} skipped: no fresh captcha param after wait",
                    selected.masked_key
                );
                continue;
            };

            plan_captcha_param = Some(captcha.param.clone());
            // 成功取得新鲜验证码后恢复为 Active
            pool.clear_captcha_needed(&zai, &plan_captcha_key);
            let profile = zplan::profile_for_parts(
                &selected.account_id,
                &selected.key,
                &selected.device_profile,
            );
            // Plan 通道反风控变换：注入官方 system 块、metadata.user_id 及 cache_control
            let mut plan_body = body.clone();
            let model_str = plan_body
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("GLM-5.3-Flash")
                .to_string();
            let user_id = {
                let uid = zplan::jwt_user_id(&selected.key);
                if uid.is_empty() {
                    selected.account_id.clone()
                } else {
                    uid
                }
            };
            zplan::transform_plan_request_body(&mut plan_body, &model_str, &user_id);
            let bytes = serde_json::to_vec(&plan_body).unwrap_or_default();
            let url = zplan::plan_request_url(path);
            let headers = zplan::build_plan_headers(
                &profile,
                &selected.key,
                Some((&captcha.param, &captcha.region)),
            );
            (url, headers, bytes)
        } else {
            let base_url = crate::proxy::config::ZaiConfig::effective_base_url(
                selected.provider,
                &zai.base_url,
            );
            let url = match join_base_url(&base_url, path) {
                Ok(u) => u,
                Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
            };
            let mut headers = copy_passthrough_headers(incoming_headers);
            set_zai_auth(&mut headers, incoming_headers, &selected.key);
            // Ensure JSON content type.
            headers
                .entry(header::CONTENT_TYPE)
                .or_insert(HeaderValue::from_static("application/json"));
            let header_list = headers
                .iter()
                .filter_map(|(k, v)| {
                    let name = k.as_str().to_string();
                    v.to_str().ok().map(|val| (name, val.to_string()))
                })
                .collect();
            (url, header_list, body_bytes.clone())
        };

        tracing::debug!(
            "Forwarding request (plan={}) (len: {} bytes): {} [key {}, attempt {}/{}]",
            is_plan,
            slot_body_bytes.len(),
            url,
            selected.masked_key,
            attempt + 1,
            max_attempts
        );

        let mut req = client.request(method.clone(), &url).body(slot_body_bytes);
        for (k, v) in &slot_headers {
            if let (Ok(name), Ok(val)) = (
                header::HeaderName::from_bytes(k.as_bytes()),
                HeaderValue::from_str(v),
            ) {
                req = req.header(name, val);
            }
        }
        let req = req;

        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                // 网络级失败与具体 Key 无关：不更新状态机，直接上抛
                return (
                    StatusCode::BAD_GATEWAY,
                    format!("Upstream request failed: {}", e),
                )
                    .into_response();
            }
        };

        let status_u16 = resp.status().as_u16();

        // 成功（<400）：状态机清瞬时标记，流式透传（覆盖 SSE 与非 SSE）
        if status_u16 < 400 {
            pool.report_success(&zai, &selected.key);
            let status = StatusCode::from_u16(status_u16).unwrap_or(StatusCode::BAD_GATEWAY);
            let mut out = Response::builder().status(status);
            if let Some(ct) = resp.headers().get(header::CONTENT_TYPE) {
                out = out.header(header::CONTENT_TYPE, ct.clone());
            }
            let stream = resp.bytes_stream().map(|chunk| match chunk {
                Ok(b) => Ok::<Bytes, std::io::Error>(b),
                Err(e) => Ok(Bytes::from(format!("Upstream stream error: {}", e))),
            });
            return out.body(Body::from_stream(stream)).unwrap_or_else(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to build response",
                )
                    .into_response()
            });
        }

        // 错误路径：读取错误体（响应尚未下发客户端，可安全失败转移）
        let retry_after_header = resp
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let content_type = resp.headers().get(header::CONTENT_TYPE).cloned();
        let header_names: Vec<String> = resp
            .headers()
            .iter()
            .map(|(k, _)| k.as_str().to_string())
            .collect();
        let error_body = resp.text().await.unwrap_or_default();
        let (account_level, classification_log) = if is_plan {
            use crate::proxy::providers::zcode_plan as zplan;
            // Plan 通道分类（协议事实 §被拒信号）：3007/403+captcha → 换码，
            // 402 → 30 分钟耗尽窗，429 → 300s，401/403 → 凭证失效
            let challenge = zplan::challenge_header_present(&header_names);
            let failure = zplan::classify_plan_failure(status_u16, &error_body, challenge);
            let failure = pool.classify_plan_and_report_with_param(
                &zai,
                &selected.key,
                failure,
                retry_after_header.as_deref(),
                plan_captcha_param.as_deref(),
            );
            let level = failure.should_failover();
            (level, format!("plan:{failure:?}"))
        } else {
            let classification = pool.classify_and_report(
                &zai,
                &selected.key,
                status_u16,
                retry_after_header.as_deref(),
                &error_body,
            );
            (
                ZaiKeyPool::should_failover(&classification),
                format!("{classification:?}"),
            )
        };
        tracing::warn!(
            "[zcode T1] z.ai key {} got {} (classification: {}), account-level failover: {}",
            selected.masked_key,
            status_u16,
            classification_log,
            account_level
        );
        if account_level && attempt + 1 < max_attempts {
            // 账号级失败：该 Key 已被标记并在后续选择中被跳过，原地换下一个可用 Key
            continue;
        }

        // 请求级错误 / 尝试耗尽：原样透传上游错误
        let status = StatusCode::from_u16(status_u16).unwrap_or(StatusCode::BAD_GATEWAY);
        let mut out = Response::builder().status(status);
        if let Some(ct) = content_type {
            out = out.header(header::CONTENT_TYPE, ct);
        }
        return out.body(Body::from(error_body)).unwrap_or_else(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to build response",
            )
                .into_response()
        });
    }

    // 循环每一支都会 return；此分支仅为类型完备
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "Failed to build response",
    )
        .into_response()
}
