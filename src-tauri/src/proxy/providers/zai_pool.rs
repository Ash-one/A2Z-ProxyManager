//! zcode T1：z.ai / BigModel Anthropic 兼容上游 API Key 多账号池。
//!
//! 设计决策（权威记录：`docs/zcode/implementation.md`）：
//! - 与 Google `TokenManager` 平行的独立 Key 池：Google 账号生命周期逻辑零改动；
//! - 状态机对齐 `pipeline/policy.rs` 的 `UpstreamClassification`（全局唯一判定真理）：
//!   429/529 → 短时限速（Retry-After 到期自动恢复，不进冷却池）、5xx → 短冷却、
//!   401/403 → INVALID（重启或配置变更前跳过）、402 → EXHAUSTED；
//! - 运行态仅内存（重启即清零，重启后重试一次即可恢复标记）；
//!   Key 本体与启停随 `gui_config.json` 持久化；
//! - 通过配置指纹惰性同步，配置热更新路径（`update_zai` / admin 保存）零侵入；
//! - 进程级单例（沿 `SignatureCache::global()` 先例），axum 与 Tauri 命令共用。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, SystemTime};

use serde::Serialize;

use crate::proxy::config::{ZaiConfig, ZaiKeyEntry, ZaiKeyMode, ZaiProvider};
use crate::proxy::pipeline::policy::UpstreamClassification;

/// 429/529 且上游未提供 Retry-After 时的默认限速窗口（有界，防止热循环打 Key）
const DEFAULT_RATE_LIMIT_DELAY: Duration = Duration::from_secs(30);
/// 5xx 瞬时故障短冷却窗口（提案 T1：5xx 短冷却）
const TRANSIENT_COOLDOWN: Duration = Duration::from_secs(15);
/// 单请求内账号级失败转移的硬上限（有界重试；AGENTS.md 风险路径纪律）
pub const MAX_FAILOVER_ATTEMPTS: usize = 4;
/// zcode T3：Plan 通道额度耗尽重试窗（协议事实：exhausted 30min）
const PLAN_EXHAUSTED_WINDOW: Duration = Duration::from_secs(30 * 60);
/// zcode T3：Plan 通道 429 默认冷却（协议事实：cooling 300s）
const PLAN_RATE_LIMIT_WINDOW: Duration = Duration::from_secs(300);

/// Key 运行态（内存态，重启即清零）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ZaiKeyStatus {
    /// 正常参与轮询
    Active,
    /// 401/403：凭证失效，跳过直至重启或配置变更
    Invalid,
    /// 402：额度/配额耗尽，跳过直至重启或配置变更
    /// （zcode T3：Plan 通道携带 until = 30 分钟重试窗）
    Exhausted,
    /// 429/529：短时限速，Retry-After（或默认窗口）到期自动恢复
    RateLimited,
    /// 5xx：瞬时故障短冷却
    Cooldown,
    /// zcode T3：Plan 通道验证码挑战（3007/403+captcha）——需端内重新过码，
    /// 新参数提交后自动恢复 Active
    CaptchaNeeded,
}

impl ZaiKeyStatus {
    /// 当前时刻是否可被调度（限速/冷却到期自动恢复）。
    /// 纯函数：`until=None` 的 RateLimited/Cooldown 视为已恢复。
    pub fn is_available(self, until: Option<SystemTime>, now: SystemTime) -> bool {
        match self {
            ZaiKeyStatus::Active => true,
            ZaiKeyStatus::Invalid | ZaiKeyStatus::Exhausted | ZaiKeyStatus::CaptchaNeeded => false,
            ZaiKeyStatus::RateLimited | ZaiKeyStatus::Cooldown => {
                until.map(|t| t <= now).unwrap_or(true)
            }
        }
    }
}

/// 池内单条运行条目：配置快照 + 运行态
#[derive(Debug, Clone)]
struct PoolEntry {
    cfg: ZaiKeyEntry,
    status: ZaiKeyStatus,
    status_until: Option<SystemTime>,
    last_error: Option<String>,
}

/// 对外状态视图（Tauri 命令 / axum admin 路由共用）
#[derive(Debug, Clone, Serialize)]
pub struct ZaiKeyStatusView {
    /// 在 `resolved_keys()` 中的下标（前端按行匹配）
    pub index: usize,
    /// 掩码展示（首 6 + 尾 4），绝不回传 Key 原文
    pub masked_key: String,
    pub provider: ZaiProvider,
    /// zcode T2：凭证模式（apiKey=可转发；jwt=Plan 通道待 T3）
    pub mode: ZaiKeyMode,
    /// zcode T2：账号配对身份（空 = 手动导入的独立 Key）
    pub account_id: String,
    pub enabled: bool,
    pub label: String,
    pub status: ZaiKeyStatus,
    /// 限速/冷却剩余秒数（无限期状态为 None）
    pub status_remaining_secs: Option<u64>,
    pub last_error: Option<String>,
}

/// 一次轮询选中的可用 Key
#[derive(Debug, Clone)]
pub struct SelectedZaiKey {
    pub key: String,
    pub provider: ZaiProvider,
    pub masked_key: String,
    /// zcode T3：凭证模式（jwt → Plan 通道转发分支）
    pub mode: ZaiKeyMode,
    /// zcode T3：账号配对身份（验证码存储键锚点）
    pub account_id: String,
    /// zcode T2：账号邮箱（流量监控归因展示用）
    pub user_email: String,
    /// 可选备注（流量监控归因展示用）
    pub label: String,
    /// zcode T3：持久化设备档案（Plan 通道身份头；可能为 Null → 运行时生成）
    pub device_profile: serde_json::Value,
}

/// 掩码展示：`sk-abcd...wxyz`；过短 Key 全掩码，避免泄露形态信息。
pub fn mask_key(key: &str) -> String {
    let k = key.trim();
    let chars: Vec<char> = k.chars().collect();
    if chars.len() <= 12 {
        return "***".to_string();
    }
    let head: String = chars.iter().take(6).collect();
    let tail: String = chars.iter().skip(chars.len() - 4).collect();
    format!("{}...{}", head, tail)
}

/// 进程级 Key 池（沿 `SignatureCache::global()` 先例）
pub struct ZaiKeyPool {
    inner: RwLock<PoolInner>,
    cursor: AtomicUsize,
}

#[derive(Debug, Default)]
struct PoolInner {
    fingerprint: u64,
    entries: Vec<PoolEntry>,
}

impl ZaiKeyPool {
    pub fn global() -> &'static Self {
        static INSTANCE: OnceLock<ZaiKeyPool> = OnceLock::new();
        INSTANCE.get_or_init(ZaiKeyPool::new)
    }

    fn new() -> Self {
        Self {
            inner: RwLock::new(PoolInner::default()),
            cursor: AtomicUsize::new(0),
        }
    }

    /// 配置指纹：按序覆盖 (key, provider, enabled)。任一变化即触发重建。
    fn fingerprint(entries: &[ZaiKeyEntry]) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        entries.len().hash(&mut hasher);
        for e in entries {
            e.key.hash(&mut hasher);
            e.provider.hash(&mut hasher);
            e.enabled.hash(&mut hasher);
            e.mode.hash(&mut hasher);
            e.account_id.hash(&mut hasher);
        }
        hasher.finish()
    }

    /// 惰性同步：指纹一致则零开销返回；变化时重建条目，
    /// 按 (key, provider) 身份保留既有运行态（改备注/增删 Key 不清空其他 Key 的标记）。
    fn sync_if_needed(&self, zai: &ZaiConfig) {
        let resolved = zai.resolved_keys();
        let fp = Self::fingerprint(&resolved);

        if let Ok(guard) = self.inner.read() {
            if guard.fingerprint == fp {
                return;
            }
        }

        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.fingerprint == fp {
            return; // Double-Checked Locking：并发同步已由他者完成
        }
        guard.entries = resolved
            .into_iter()
            .map(|cfg| {
                match guard
                    .entries
                    .iter()
                    .find(|prev| prev.cfg.key == cfg.key && prev.cfg.provider == cfg.provider)
                {
                    Some(prev) => PoolEntry {
                        cfg,
                        status: prev.status,
                        status_until: prev.status_until,
                        last_error: prev.last_error.clone(),
                    },
                    None => PoolEntry {
                        cfg,
                        status: ZaiKeyStatus::Active,
                        status_until: None,
                        last_error: None,
                    },
                }
            })
            .collect();
        guard.fingerprint = fp;
    }

    /// 当前可参与调度的条目下标（enabled + 非空 + 状态机放行）。
    /// zcode T3：`mode=Jwt` 条目（Plan 通道）仅在其账号的验证码参数处于新鲜窗内
    /// 可调度（无新鲜参数 → 无效请求，浪费账号风控额度）。
    fn available_indices(entries: &[PoolEntry], now: SystemTime) -> Vec<usize> {
        let captcha_store = crate::proxy::providers::zcode_plan::ZcodeCaptchaStore::global();
        entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                e.cfg.enabled
                    && !e.cfg.key.trim().is_empty()
                    && e.status.is_available(e.status_until, now)
                    && match e.cfg.mode {
                        ZaiKeyMode::ApiKey => true,
                        ZaiKeyMode::Jwt => captcha_store.is_fresh(
                            &crate::proxy::providers::zcode_plan::captcha_key_for(
                                &e.cfg.account_id,
                                &e.cfg.key,
                            ),
                        ),
                    }
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// 可用 Key 数（pooled 模式下 = z.ai 侧槽位数；提案 T1：每个可用 Key = 1 槽）
    pub fn available_count(&self, zai: &ZaiConfig) -> usize {
        self.sync_if_needed(zai);
        let guard = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::available_indices(&guard.entries, SystemTime::now()).len()
    }

    /// 轮询选择下一个可用 Key。
    /// 优先选择立即就绪（ApiKey 或新鲜 JWT）的条目；
    /// 若无立即就绪条目，但存在未失效的 JWT 槽位，则返回该 JWT 槽位以便进入按需等待过码流程。
    pub fn select_key(&self, zai: &ZaiConfig) -> Option<SelectedZaiKey> {
        self.sync_if_needed(zai);
        let guard = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let available = Self::available_indices(&guard.entries, SystemTime::now());
        let candidate_index = if !available.is_empty() {
            let idx = self.cursor.fetch_add(1, Ordering::Relaxed) % available.len();
            available[idx]
        } else {
            // 兜底候选：启用的 JWT 条目（处于 Active 或 CaptchaNeeded 状态，且非 Invalid/Exhausted）
            let jwt_candidates: Vec<usize> = guard
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| {
                    e.cfg.enabled
                        && !e.cfg.key.trim().is_empty()
                        && (e.status == ZaiKeyStatus::Active
                            || e.status == ZaiKeyStatus::CaptchaNeeded)
                        && e.cfg.mode == ZaiKeyMode::Jwt
                })
                .map(|(i, _)| i)
                .collect();
            if jwt_candidates.is_empty() {
                return None;
            }
            let idx = self.cursor.fetch_add(1, Ordering::Relaxed) % jwt_candidates.len();
            jwt_candidates[idx]
        };

        let entry = &guard.entries[candidate_index];
        Some(SelectedZaiKey {
            key: entry.cfg.key.clone(),
            provider: entry.cfg.provider,
            masked_key: mask_key(&entry.cfg.key),
            mode: entry.cfg.mode,
            account_id: entry.cfg.account_id.clone(),
            user_email: entry.cfg.user_email.clone(),
            label: entry.cfg.label.clone(),
            device_profile: entry.cfg.device_profile.clone(),
        })
    }

    /// 成功上报：清空瞬时标记（Invalid/Exhausted 只能经重启/配置变更恢复，
    /// 但被标记的 Key 不会被调度，因此成功必然来自已恢复的 Key）。
    pub fn report_success(&self, zai: &ZaiConfig, key: &str) {
        self.sync_if_needed(zai);
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = guard.entries.iter_mut().find(|e| e.cfg.key == key) {
            entry.status = ZaiKeyStatus::Active;
            entry.status_until = None;
            entry.last_error = None;
        }
    }

    /// 分类并上报一次失败尝试，返回统一分类供调用方做失败转移决策。
    /// 状态机映射（对齐 `UpstreamClassification`）：
    /// - `RateLimited`（429/529）→ RateLimited（Retry-After / 默认窗口到期自动恢复）
    /// - `TransientServerError`（5xx）→ Cooldown（短冷却）
    /// - `OtherClientError(401|403)` → Invalid
    /// - `OtherClientError(402)` → Exhausted
    /// - 其余（404/400/签名错误等请求级错误）→ 不改变 Key 状态
    pub fn classify_and_report(
        &self,
        zai: &ZaiConfig,
        key: &str,
        status: u16,
        retry_after_header: Option<&str>,
        body: &str,
    ) -> UpstreamClassification {
        let classification = UpstreamClassification::classify(status, body, retry_after_header);
        self.sync_if_needed(zai);
        let now = SystemTime::now();
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = guard.entries.iter_mut().find(|e| e.cfg.key == key) {
            let next = match &classification {
                UpstreamClassification::RateLimited { retry_after } => {
                    let delay = retry_after
                        .map(Duration::from_secs)
                        .unwrap_or(DEFAULT_RATE_LIMIT_DELAY);
                    Some((
                        ZaiKeyStatus::RateLimited,
                        now.checked_add(delay),
                        format!("{} rate-limited", status),
                    ))
                }
                UpstreamClassification::TransientServerError => Some((
                    ZaiKeyStatus::Cooldown,
                    now.checked_add(TRANSIENT_COOLDOWN),
                    format!("{} transient server error", status),
                )),
                UpstreamClassification::OtherClientError(code @ (401 | 403)) => Some((
                    ZaiKeyStatus::Invalid,
                    None,
                    format!("{} credential rejected", code),
                )),
                UpstreamClassification::OtherClientError(402) => Some((
                    ZaiKeyStatus::Exhausted,
                    None,
                    "402 quota exhausted".to_string(),
                )),
                _ => None,
            };
            if let Some((status, until, err)) = next {
                entry.status = status;
                entry.status_until = until;
                entry.last_error = Some(err);
            }
        }
        classification
    }

    /// zcode T3：Plan 通道失败分类上报（协议事实 §被拒信号）。
    /// - CaptchaChallenge → CaptchaNeeded（同时失效该账号验证码缓存）
    /// - Exhausted → Exhausted（30 分钟重试窗，到期自动恢复）
    /// - RateLimited → RateLimited（Retry-After / 默认 300s）
    /// - InvalidAuth → Invalid
    /// - Transient → Cooldown（短冷却）
    /// - RequestLevel → 不改变 Key 状态
    /// 返回分类供调用方做失败转移决策。
    /// 分类并上报一次 Plan 通道失败尝试（携带触发失败的具体验证码参数）。
    /// 状态机映射：
    /// - CaptchaChallenge（3007/403+captcha）→ 精准作废该验证码；若缓冲池仍有其他新鲜 Token 则保留 Active
    ///   以便在当前请求内瞬间完成重试，仅当缓冲池耗尽时才置为 CaptchaNeeded。
    pub fn classify_plan_and_report_with_param(
        &self,
        zai: &ZaiConfig,
        key: &str,
        failure: crate::proxy::providers::zcode_plan::PlanFailure,
        retry_after_header: Option<&str>,
        failed_param: Option<&str>,
    ) -> crate::proxy::providers::zcode_plan::PlanFailure {
        use crate::proxy::providers::zcode_plan::{PlanFailure, ZcodeCaptchaStore};
        self.sync_if_needed(zai);
        let now = SystemTime::now();
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = guard.entries.iter_mut().find(|e| e.cfg.key == key) {
            let next = match failure {
                PlanFailure::CaptchaChallenge => {
                    let captcha_key = crate::proxy::providers::zcode_plan::captcha_key_for(
                        &entry.cfg.account_id,
                        &entry.cfg.key,
                    );
                    let has_remaining = if let Some(param) = failed_param {
                        ZcodeCaptchaStore::global().invalidate_param(&captcha_key, param)
                    } else {
                        ZcodeCaptchaStore::global().invalidate(&captcha_key);
                        false
                    };
                    // 派发后台过码事件以补充缓冲池
                    crate::proxy::providers::zcode_plan::emit_captcha_needed_event(
                        &entry.cfg.account_id,
                        &entry.cfg.key,
                    );

                    if has_remaining {
                        tracing::info!(
                            "[zcode buffer pool] Captcha token rejected, but standby token exists in buffer for slot {}",
                            mask_key(key)
                        );
                        // 缓冲池尚有备用新鲜 token，保持 Active 供立即重试
                        None
                    } else {
                        Some((
                            ZaiKeyStatus::CaptchaNeeded,
                            None,
                            "captcha challenge (3007)".to_string(),
                        ))
                    }
                }
                PlanFailure::Exhausted => Some((
                    ZaiKeyStatus::Exhausted,
                    now.checked_add(PLAN_EXHAUSTED_WINDOW),
                    "plan quota exhausted (30min window)".to_string(),
                )),
                PlanFailure::RateLimited => {
                    let delay = retry_after_header
                        .and_then(|s| s.trim().parse::<u64>().ok())
                        .map(Duration::from_secs)
                        .unwrap_or(PLAN_RATE_LIMIT_WINDOW);
                    Some((
                        ZaiKeyStatus::RateLimited,
                        now.checked_add(delay),
                        "plan rate-limited".to_string(),
                    ))
                }
                PlanFailure::InvalidAuth => Some((
                    ZaiKeyStatus::Invalid,
                    None,
                    "plan credential rejected".to_string(),
                )),
                PlanFailure::Transient => Some((
                    ZaiKeyStatus::Cooldown,
                    now.checked_add(TRANSIENT_COOLDOWN),
                    "plan transient server error".to_string(),
                )),
                PlanFailure::RequestLevel => None,
            };
            if let Some((status, until, err)) = next {
                entry.status = status;
                entry.status_until = until;
                entry.last_error = Some(err);
            }
        }
        failure
    }

    /// 分类并上报一次 Plan 通道失败尝试，返回分类供调用方做失败转移决策。
    #[allow(dead_code)]
    pub fn classify_plan_and_report(
        &self,
        zai: &ZaiConfig,
        key: &str,
        failure: crate::proxy::providers::zcode_plan::PlanFailure,
        retry_after_header: Option<&str>,
    ) -> crate::proxy::providers::zcode_plan::PlanFailure {
        self.classify_plan_and_report_with_param(zai, key, failure, retry_after_header, None)
    }

    /// zcode T3：标记需要过码（调度发现无新鲜验证码时）。
    pub fn mark_captcha_needed(&self, zai: &ZaiConfig, key: &str) {
        self.sync_if_needed(zai);
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = guard.entries.iter_mut().find(|e| e.cfg.key == key) {
            if entry.status != ZaiKeyStatus::CaptchaNeeded {
                entry.status = ZaiKeyStatus::CaptchaNeeded;
                entry.status_until = None;
                entry.last_error = Some("captcha param missing or stale".to_string());
            }
        }
    }

    /// zcode T3：新验证码提交后清除 CaptchaNeeded 标记（按验证码存储键匹配）。
    pub fn clear_captcha_needed(&self, zai: &ZaiConfig, captcha_key: &str) {
        self.sync_if_needed(zai);
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for entry in guard.entries.iter_mut() {
            let entry_captcha_key = crate::proxy::providers::zcode_plan::captcha_key_for(
                &entry.cfg.account_id,
                &entry.cfg.key,
            );
            if entry_captcha_key == captcha_key && entry.status == ZaiKeyStatus::CaptchaNeeded {
                entry.status = ZaiKeyStatus::Active;
                entry.status_until = None;
                entry.last_error = None;
            }
        }
    }

    /// 失败转移判定：该分类是否为“账号级故障”，应立即换下一个可用 Key 原地重试。
    /// 请求级错误（404 模型不存在、400 签名污染等）不转移 —— 换 Key 无益。
    pub fn should_failover(classification: &UpstreamClassification) -> bool {
        matches!(
            classification,
            UpstreamClassification::RateLimited { .. }
                | UpstreamClassification::TransientServerError
                | UpstreamClassification::OtherClientError(401..=403)
        )
    }

    /// 状态快照（状态查询命令/路由用）。只读展示：到期的限速/冷却在此归一化为 Active。
    pub fn status_snapshot(&self, zai: &ZaiConfig) -> Vec<ZaiKeyStatusView> {
        self.sync_if_needed(zai);
        let guard = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = SystemTime::now();
        guard
            .entries
            .iter()
            .enumerate()
            .map(|(index, e)| {
                let (display, remaining) = match e.status {
                    ZaiKeyStatus::RateLimited | ZaiKeyStatus::Cooldown => match e.status_until {
                        Some(until) if until > now => (
                            e.status,
                            Some(until.duration_since(now).map(|d| d.as_secs()).unwrap_or(0)),
                        ),
                        _ => (ZaiKeyStatus::Active, None),
                    },
                    // zcode T3：Plan 通道 Exhausted 携带 30 分钟 until，到期归一化恢复
                    ZaiKeyStatus::Exhausted => match e.status_until {
                        Some(until) if until > now => (
                            e.status,
                            Some(until.duration_since(now).map(|d| d.as_secs()).unwrap_or(0)),
                        ),
                        Some(_) => (ZaiKeyStatus::Active, None),
                        None => (e.status, None),
                    },
                    other => (other, None),
                };
                ZaiKeyStatusView {
                    index,
                    masked_key: mask_key(&e.cfg.key),
                    provider: e.cfg.provider,
                    mode: e.cfg.mode,
                    account_id: e.cfg.account_id.clone(),
                    enabled: e.cfg.enabled,
                    label: e.cfg.label.clone(),
                    status: display,
                    status_remaining_secs: remaining,
                    last_error: e.last_error.clone(),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(key: &str, provider: ZaiProvider, enabled: bool) -> ZaiKeyEntry {
        ZaiKeyEntry {
            key: key.to_string(),
            provider,
            enabled,
            label: String::new(),
            mode: crate::proxy::config::ZaiKeyMode::ApiKey,
            account_id: String::new(),
            user_email: String::new(),
            business_jwt: String::new(),
            device_profile: serde_json::Value::Null,
        }
    }

    /// zcode T2：Plan 通道 JWT 条目（不参与消息转发调度）
    fn jwt_entry(key: &str, enabled: bool) -> ZaiKeyEntry {
        ZaiKeyEntry {
            key: key.to_string(),
            provider: ZaiProvider::ZcodePlan,
            enabled,
            label: "Plan JWT".to_string(),
            mode: crate::proxy::config::ZaiKeyMode::Jwt,
            account_id: "acc-1".to_string(),
            user_email: "user@x.io".to_string(),
            business_jwt: "biz.jwt".to_string(),
            device_profile: serde_json::Value::Null,
        }
    }

    fn config(entries: Vec<ZaiKeyEntry>) -> ZaiConfig {
        ZaiConfig {
            enabled: true,
            keys: entries,
            ..ZaiConfig::default()
        }
    }

    #[test]
    fn selection_round_robins_across_available_keys() {
        let cfg = config(vec![
            entry("key-a", ZaiProvider::Zai, true),
            entry("key-b", ZaiProvider::BigModel, true),
            entry("key-c", ZaiProvider::Zai, true),
        ]);
        let pool = ZaiKeyPool::new();
        let picked: Vec<String> = (0..3).map(|_| pool.select_key(&cfg).unwrap().key).collect();
        assert_eq!(picked, vec!["key-a", "key-b", "key-c"]);
        assert_eq!(pool.available_count(&cfg), 3);
    }

    #[test]
    fn disabled_and_empty_keys_are_skipped() {
        let cfg = config(vec![
            entry("key-a", ZaiProvider::Zai, false),
            entry("   ", ZaiProvider::Zai, true),
            entry("key-b", ZaiProvider::Zai, true),
        ]);
        let pool = ZaiKeyPool::new();
        for _ in 0..3 {
            assert_eq!(pool.select_key(&cfg).unwrap().key, "key-b");
        }
        assert_eq!(pool.available_count(&cfg), 1);
    }

    #[test]
    fn invalid_and_exhausted_keys_are_skipped() {
        let cfg = config(vec![
            entry("key-a", ZaiProvider::Zai, true),
            entry("key-b", ZaiProvider::Zai, true),
            entry("key-c", ZaiProvider::Zai, true),
        ]);
        let pool = ZaiKeyPool::new();
        pool.classify_and_report(&cfg, "key-a", 401, None, "");
        pool.classify_and_report(&cfg, "key-c", 402, None, "");
        assert_eq!(pool.available_count(&cfg), 1);
        for _ in 0..3 {
            assert_eq!(pool.select_key(&cfg).unwrap().key, "key-b");
        }
        let snap = pool.status_snapshot(&cfg);
        assert_eq!(snap[0].status, ZaiKeyStatus::Invalid);
        assert_eq!(snap[2].status, ZaiKeyStatus::Exhausted);
    }

    #[test]
    fn rate_limited_keys_recover_after_window() {
        let cfg = config(vec![entry("key-a", ZaiProvider::Zai, true)]);
        let pool = ZaiKeyPool::new();
        // Retry-After: 2（秒）→ 2 秒内不可用，到期自动恢复
        pool.classify_and_report(&cfg, "key-a", 429, Some("2"), "");
        let now = SystemTime::now();
        assert_eq!(pool.available_count(&cfg), 0);
        let snap = pool.status_snapshot(&cfg);
        assert_eq!(snap[0].status, ZaiKeyStatus::RateLimited);
        assert!(snap[0].status_remaining_secs.unwrap() <= 2);
        // 纯函数验证到期恢复（不引入 sleep）
        assert!(!ZaiKeyStatus::RateLimited.is_available(
            Some(now + Duration::from_secs(2)),
            now + Duration::from_secs(1)
        ));
        assert!(ZaiKeyStatus::RateLimited.is_available(
            Some(now + Duration::from_secs(2)),
            now + Duration::from_secs(2)
        ));
        // 5xx 短冷却同理
        pool.report_success(&cfg, "key-a");
        pool.classify_and_report(&cfg, "key-a", 503, None, "");
        let snap = pool.status_snapshot(&cfg);
        assert_eq!(snap[0].status, ZaiKeyStatus::Cooldown);
        assert!(snap[0].status_remaining_secs.unwrap() <= 15);
    }

    #[test]
    fn request_level_errors_do_not_touch_key_state() {
        let cfg = config(vec![entry("key-a", ZaiProvider::Zai, true)]);
        let pool = ZaiKeyPool::new();
        // 404 模型不存在 / 400 签名污染：请求级错误，Key 状态不变
        pool.classify_and_report(&cfg, "key-a", 404, None, "{\"error\":\"model not found\"}");
        pool.classify_and_report(
            &cfg,
            "key-a",
            400,
            None,
            "{\"error\":\"invalid signature in thinking block\"}",
        );
        let snap = pool.status_snapshot(&cfg);
        assert_eq!(snap[0].status, ZaiKeyStatus::Active);
        assert!(!ZaiKeyPool::should_failover(
            &UpstreamClassification::ModelNotFound
        ));
    }

    #[test]
    fn success_clears_transient_marks() {
        let cfg = config(vec![entry("key-a", ZaiProvider::Zai, true)]);
        let pool = ZaiKeyPool::new();
        pool.classify_and_report(&cfg, "key-a", 500, None, "");
        assert_eq!(pool.available_count(&cfg), 0);
        pool.report_success(&cfg, "key-a");
        assert_eq!(pool.available_count(&cfg), 1);
        let snap = pool.status_snapshot(&cfg);
        assert_eq!(snap[0].status, ZaiKeyStatus::Active);
        assert!(snap[0].last_error.is_none());
    }

    #[test]
    fn config_sync_preserves_status_for_unchanged_keys() {
        let pool = ZaiKeyPool::new();
        let cfg_a = config(vec![
            entry("key-a", ZaiProvider::Zai, true),
            entry("key-b", ZaiProvider::Zai, true),
        ]);
        pool.classify_and_report(&cfg_a, "key-a", 403, None, "");
        assert_eq!(pool.available_count(&cfg_a), 1);

        // 配置变化：新增 key-c → key-a 的 Invalid 标记保留
        let cfg_b = config(vec![
            entry("key-a", ZaiProvider::Zai, true),
            entry("key-b", ZaiProvider::Zai, true),
            entry("key-c", ZaiProvider::Zai, true),
        ]);
        assert_eq!(pool.available_count(&cfg_b), 2);
        let snap = pool.status_snapshot(&cfg_b);
        assert_eq!(snap[0].status, ZaiKeyStatus::Invalid);
        assert_eq!(snap[2].status, ZaiKeyStatus::Active);
    }

    #[test]
    fn failover_classes_match_account_level_failures() {
        assert!(ZaiKeyPool::should_failover(
            &UpstreamClassification::RateLimited {
                retry_after: Some(1)
            }
        ));
        assert!(ZaiKeyPool::should_failover(
            &UpstreamClassification::TransientServerError
        ));
        assert!(ZaiKeyPool::should_failover(
            &UpstreamClassification::OtherClientError(401)
        ));
        assert!(ZaiKeyPool::should_failover(
            &UpstreamClassification::OtherClientError(402)
        ));
        assert!(ZaiKeyPool::should_failover(
            &UpstreamClassification::OtherClientError(403)
        ));
        assert!(!ZaiKeyPool::should_failover(
            &UpstreamClassification::ModelNotFound
        ));
        assert!(!ZaiKeyPool::should_failover(
            &UpstreamClassification::OtherClientError(400)
        ));
        assert!(!ZaiKeyPool::should_failover(
            &UpstreamClassification::InternalGatewayMessage
        ));
    }

    #[test]
    fn masked_key_never_reveals_short_keys() {
        assert_eq!(mask_key("short"), "***");
        assert_eq!(mask_key("sk-abcdefghijkl"), "sk-abc...ijkl");
    }

    // ===== zcode T2：Plan 通道 JWT 条目不参与消息转发调度 =====

    #[test]
    fn jwt_mode_entries_are_excluded_from_scheduling() {
        // JWT（Plan 通道，T3 前不可转发）+ 同账号 API Key：仅 API Key 计入槽位
        let cfg = config(vec![
            jwt_entry("jwt.def.sig", true),
            entry("key-a", ZaiProvider::Zai, true),
        ]);
        let pool = ZaiKeyPool::new();
        assert_eq!(pool.available_count(&cfg), 1);

        // 禁用 API Key 后仅剩 JWT → 无可用槽位（而非把 JWT 当作可用凭证）
        let cfg_jwt_only = config(vec![jwt_entry("jwt.def.sig", true)]);
        assert_eq!(pool.available_count(&cfg_jwt_only), 0);

        // 状态快照仍完整展示 JWT 条目（含 mode / account_id）
        let snap = pool.status_snapshot(&cfg);
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].mode, crate::proxy::config::ZaiKeyMode::Jwt);
        assert_eq!(snap[0].account_id, "acc-1");
        assert_eq!(snap[1].mode, crate::proxy::config::ZaiKeyMode::ApiKey);
    }

    #[test]
    fn mode_change_resynchronizes_entry_identity() {
        // 同 key 但 mode 变化 → 指纹变化触发重同步（条目身份 = key+provider+enabled+mode+account）
        let pool = ZaiKeyPool::new();
        let cfg = config(vec![entry("shared", ZaiProvider::Zai, true)]);
        assert_eq!(pool.available_count(&cfg), 1);
        let cfg_jwt = config(vec![ZaiKeyEntry {
            mode: crate::proxy::config::ZaiKeyMode::Jwt,
            ..entry("shared", ZaiProvider::Zai, true)
        }]);
        assert_eq!(pool.available_count(&cfg_jwt), 0);
    }
}
