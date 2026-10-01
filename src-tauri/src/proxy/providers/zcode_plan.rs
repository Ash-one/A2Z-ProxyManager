//! zcode T3：Plan 通道（zcode.z.ai `/api/v1/zcode-plan`）完整仿真。
//!
//! 覆盖：Plan 消息转发头构建、模型名大小写规范化、失败分类（3007 验证码挑战 /
//! 402 额度 / 429 限速 / 401 凭证失效）、验证码参数进程内存储、每账号设备指纹
//! （成套桌面 SKU，一号一台）、首启安装序仿真（client/configs + 激活事件）、
//! Plan 额度（billing/balance）与限时套餐领取（billing/preview + claim）。
//!
//! 净室声明：全部实现仅依据公开协议事实（docs/zcode/implementation-t3.md 事实表）
//! 重写，未复制任何第三方源码。

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{OnceLock, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{json, Value};

use crate::proxy::config::{UpstreamProxyConfig, ZaiConfig, ZaiKeyEntry};

/// 客户端版本单一真相源（协议事实：官方桌面端现行版；活动投放按最低客户端版本
/// 门槛，官方升版后此处落后会导致 preview 恒空 ineligible——升版先改此常量）。
pub const APP_VERSION: &str = "3.14.3";
/// Plan 通道 Anthropic 兼容基座（消息与 count_tokens 同基座）。
pub const PLAN_BASE: &str = "https://zcode.z.ai/api/v1/zcode-plan/anthropic";

/// ZCode Plan 与 z.ai 支持的可反代模型列表（供代理端点 /v1/models、/v1/models/claude 等自动发现）
pub const ZCODE_SUPPORTED_MODELS: &[&str] = &[
    "GLM-5.3-Flash",
    "GLM-5.3",
    "GLM-5-Turbo",
    "GLM-5.1-Highspeed",
    "GLM-4.5-Air",
    "GLM-4.6V",
    "glm-5.3-flash",
    "glm-5.3",
    "glm-5-turbo",
    "glm-5.1-highspeed",
    "glm-4.5-air",
    "glm-4.6v",
    "glm-4-plus",
    "glm-4-air",
    "glm-4-flash",
    "glm-4-long",
];
/// 计费/领取基座（⚠️ WAF 风险点：连续查询易触发拦截，仅手动按需）。
pub const BILLING_BASE: &str = "https://zcode.z.ai/api/v1/zcode-plan";
/// 运行配置（免鉴权；实测带 platform 参数会被拒 3001，仅带 app_version）。
pub const CLIENT_CONFIGS_URL: &str = "https://zcode.z.ai/api/v1/client/configs";
/// 激活遥测（无 Authorization，官方端点不校验登录态）。
pub const EVENT_REPORT_URL: &str = "https://zcode.z.ai/api/v1/event/report";
/// 验证码挑战业务码（400 + body code=3007；或 403 + captcha 头）。
pub const PLAN_CAPTCHA_EXPIRED_CODE: i64 = 3007;
/// 激活事件元素（官方客户端每日活跃上报；device_mid+日期 为日活去重键）。
pub const ACTIVATION_ELEMENTS: [&str; 2] = ["app_launch", "app_daily_active"];
/// 消息请求 anthropic-version（协议事实）。
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// X-Title 固定形态（协议事实：官方桌面端 billing 头实证）。
const X_TITLE: &str = "Z Code@electron";
/// X-Release-Channel 固定值（协议事实）。
const RELEASE_CHANNEL: &str = "stable";

/// 验证码参数新鲜窗（协议事实：参数 TTL 约 2 分钟；留安全边际仅取前 90 秒）。
pub const CAPTCHA_FRESH_MS: u64 = 90_000;

/// client/configs 兜底验证码配置（协议事实：线上实测 region=cn；configs 拉取
/// 失败时使用，动态值优先）。
fn captcha_defaults() -> CaptchaConfig {
    CaptchaConfig {
        enabled: true,
        prefix: "no8xfe".to_string(),
        region: "cn".to_string(),
        scene_id: "11xygtvd".to_string(),
    }
}

/// 验证码场景配置（前端过码组件入参；动态取自 client/configs）。
#[derive(Debug, Clone, Serialize)]
pub struct CaptchaConfig {
    pub enabled: bool,
    pub prefix: String,
    pub region: String,
    pub scene_id: String,
}

/// 验证码命令配置（场景 + 客户端版本，前端展示与 SDK 入参）。
#[derive(Debug, Clone, Serialize)]
pub struct ZcodeCaptchaCommandConfig {
    pub enabled: bool,
    pub prefix: String,
    pub region: String,
    pub scene_id: String,
    pub app_version: String,
}

/// 拉取验证码命令配置：client/configs 动态值优先，拉取失败回退默认（不阻塞过码）。
pub async fn captcha_command_config(
    upstream_proxy: &UpstreamProxyConfig,
    request_timeout: u64,
) -> Result<ZcodeCaptchaCommandConfig, String> {
    let cfg = match fetch_client_configs(upstream_proxy, request_timeout).await {
        Ok(v) => extract_captcha_config(&v),
        Err(_) => captcha_defaults(),
    };
    Ok(ZcodeCaptchaCommandConfig {
        enabled: cfg.enabled,
        prefix: cfg.prefix,
        region: cfg.region,
        scene_id: cfg.scene_id,
        app_version: APP_VERSION.to_string(),
    })
}

/// 从 client/configs 响应容错提取验证码配置（data.configs.captcha；
/// 字段 sceneId/prefix/region/enabled，缺失项逐项回退默认值）。
pub fn extract_captcha_config(configs_body: &Value) -> CaptchaConfig {
    let mut out = captcha_defaults();
    let captcha = configs_body
        .get("data")
        .and_then(|d| d.get("configs"))
        .and_then(|c| c.get("captcha"))
        .cloned()
        .unwrap_or(Value::Null);
    if let Some(v) = captcha.get("enabled").and_then(|v| v.as_bool()) {
        out.enabled = v;
    }
    for (key, target) in [
        ("sceneId", &mut out.scene_id),
        ("prefix", &mut out.prefix),
        ("region", &mut out.region),
    ] {
        if let Some(v) = captcha
            .get(key)
            .or_else(|| captcha.get(&key.to_ascii_lowercase()))
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            *target = v;
        }
    }
    out
}

// ===== 每账号设备指纹（成套桌面 SKU，一号一台） =====

/// 成套桌面 SKU 表（协议事实：官方桌面端常见 platform×arch×os_version×screen
/// 绑定组合与权重；禁止字段笛卡尔积，账号池不含 linux）。
const SKU_TABLE: &[(u32, &str, &str, &str, &str)] = &[
    // Apple silicon MacBook Air/Pro 13–14"（darwin 24 = Sequoia，25 = Tahoe）
    (10, "darwin", "arm64", "24.5.0", "1512x982"),
    (10, "darwin", "arm64", "24.6.0", "1512x982"),
    (8, "darwin", "arm64", "24.5.0", "1728x1117"),
    (8, "darwin", "arm64", "24.6.0", "1728x1117"),
    (8, "darwin", "arm64", "25.5.0", "1512x982"),
    (6, "darwin", "arm64", "25.5.0", "1728x1117"),
    (5, "darwin", "arm64", "23.6.0", "1512x982"),
    (4, "darwin", "arm64", "23.6.0", "1728x1117"),
    (4, "darwin", "arm64", "24.5.0", "2560x1440"),
    (3, "darwin", "arm64", "24.6.0", "2560x1600"),
    (2, "darwin", "arm64", "25.5.0", "2560x1440"),
    (2, "darwin", "arm64", "24.5.0", "3840x2160"),
    // Intel Mac 存量
    (2, "darwin", "x64", "23.6.0", "1920x1080"),
    (2, "darwin", "x64", "22.6.0", "1440x900"),
    (1, "darwin", "x64", "23.6.0", "2560x1440"),
    // Windows 11 主流 + 少量 Win10
    (8, "win32", "x64", "10.0.22631", "1920x1080"),
    (7, "win32", "x64", "10.0.26100", "1920x1080"),
    (5, "win32", "x64", "10.0.22631", "2560x1440"),
    (4, "win32", "x64", "10.0.26200", "1920x1080"),
    (3, "win32", "x64", "10.0.26100", "2560x1440"),
    (3, "win32", "x64", "10.0.22621", "1920x1080"),
    (2, "win32", "x64", "10.0.22631", "3840x2160"),
    (2, "win32", "x64", "10.0.19045", "1920x1080"),
    (1, "win32", "x64", "10.0.19045", "1366x768"),
    (1, "win32", "x64", "10.0.26100", "2560x1600"),
    (1, "win32", "x64", "10.0.22000", "1920x1080"),
];

/// 语言-时区真实地区对（X-Client-Language ↔ X-Client-Timezone 同源成套）。
const REGION_PAIRS: &[(&str, &str)] = &[
    ("zh-CN", "Asia/Shanghai"),
    ("en-US", "America/New_York"),
    ("en-GB", "Europe/London"),
    ("ja-JP", "Asia/Tokyo"),
    ("ko-KR", "Asia/Seoul"),
];

/// 每账号设备档案：字段成套分配、一经分配即稳定（device_mid 永不逐请求随机）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeviceProfile {
    pub platform: String,
    pub arch: String,
    pub os_category: String,
    pub os_version: String,
    pub screen: String,
    pub language: String,
    pub timezone: String,
    pub device_mid: String,
}

impl DeviceProfile {
    /// X-Platform 头形态：`{platform}-{arch}`。
    pub fn platform_arch(&self) -> String {
        format!("{}-{}", self.platform, self.arch)
    }

    /// 从条目持久化的 `device_profile` JSON 恢复（关键字段齐备才接受）。
    pub fn from_value(v: &Value) -> Option<DeviceProfile> {
        let obj = v.as_object()?;
        let get = |k: &str| {
            obj.get(k)
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };
        Some(DeviceProfile {
            platform: get("platform")?,
            arch: get("arch")?,
            os_category: get("os_category")?,
            os_version: get("os_version")?,
            screen: get("screen")?,
            language: get("language")?,
            timezone: get("timezone")?,
            device_mid: get("device_mid")?,
        })
    }

    pub fn to_value(&self) -> Value {
        json!({
            "platform": self.platform,
            "arch": self.arch,
            "os_category": self.os_category,
            "os_version": self.os_version,
            "screen": self.screen,
            "language": self.language,
            "timezone": self.timezone,
            "device_mid": self.device_mid,
        })
    }
}

/// 轻量随机源（xorshift64*；系统时钟纳秒 + 原子计数播种，避免引入 rand 依赖）。
fn next_random() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| (d.as_secs() ^ (d.subsec_nanos() as u64)) | 1)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    let mut s = nanos
        ^ (COUNTER.fetch_add(1, Ordering::Relaxed) as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    s ^= s >> 12;
    s ^= s << 25;
    s ^= s >> 27;
    s.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

/// 按权重抽样一个成套 SKU，并绑定全新 device_mid（UUIDv4）与真实地区对。
pub fn generate_device_profile() -> DeviceProfile {
    let total: u32 = SKU_TABLE.iter().map(|(w, ..)| w).sum();
    let mut roll = next_random() % total.max(1) as u64;
    let mut picked = SKU_TABLE[0];
    for sku in SKU_TABLE {
        if roll < sku.0 as u64 {
            picked = *sku;
            break;
        }
        roll -= sku.0 as u64;
    }
    let (_, platform, arch, os_version, screen) = picked;
    let (language, timezone) = REGION_PAIRS[(next_random() as usize) % REGION_PAIRS.len()];
    DeviceProfile {
        platform: platform.to_string(),
        arch: arch.to_string(),
        os_category: os_category_for(platform).to_string(),
        os_version: os_version.to_string(),
        screen: screen.to_string(),
        language: language.to_string(),
        timezone: timezone.to_string(),
        device_mid: uuid::Uuid::new_v4().to_string(),
    }
}

/// X-Os-Category 取值推断（协议事实未完全公开形态；darwin→mac / win32→windows，
/// 未验证边界已在 implementation-t3.md 声明）。
fn os_category_for(platform: &str) -> &'static str {
    match platform {
        "darwin" => "mac",
        "win32" => "windows",
        _ => "linux",
    }
}

/// 进程内档案缓存：手动导入的 JWT 条目无持久化档案时，同账号进程内稳定复用。
fn runtime_profiles() -> &'static RwLock<HashMap<String, DeviceProfile>> {
    static INSTANCE: OnceLock<RwLock<HashMap<String, DeviceProfile>>> = OnceLock::new();
    INSTANCE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// 条目的验证码存储键：OAuth 条目用 account_id，手动导入条目回退 Key 本体。
pub fn captcha_key_for(account_id: &str, entry_key: &str) -> String {
    let id = account_id.trim();
    if id.is_empty() {
        entry_key.trim().to_string()
    } else {
        id.to_string()
    }
}

/// 解析条目设备档案：持久化值优先 → 进程内缓存 → 现生成（进程内稳定）。
pub fn profile_for_entry(entry: &ZaiKeyEntry) -> DeviceProfile {
    profile_for_parts(&entry.account_id, &entry.key, &entry.device_profile)
}

/// 转发路径的档案解析（SelectedZaiKey 视图：account_id + key + 持久化档案 JSON）。
pub fn profile_for_parts(
    account_id: &str,
    entry_key: &str,
    device_profile: &Value,
) -> DeviceProfile {
    if let Some(p) = DeviceProfile::from_value(device_profile) {
        return p;
    }
    let key = captcha_key_for(account_id, entry_key);
    if let Ok(guard) = runtime_profiles().read() {
        if let Some(p) = guard.get(&key) {
            return p.clone();
        }
    }
    let generated = generate_device_profile();
    if let Ok(mut guard) = runtime_profiles().write() {
        guard.insert(key, generated.clone());
    }
    generated
}

// ===== 验证码参数存储（进程内，TTL 约 2 分钟，取前 90 秒为新鲜） =====

#[derive(Debug, Clone)]
pub struct CaptchaParam {
    pub param: String,
    pub region: String,
    pub issued_at_ms: u64,
}

/// 缓冲池最大容量（每个账号槽位常驻保留最多 3 个新鲜验证码）
pub const CAPTCHA_BUFFER_CAPACITY: usize = 3;

/// 验证码参数进程内缓冲池（前端过码组件提交 → 缓冲池滑动存储与复用 → 3007 挑战精准剔除）。
pub struct ZcodeCaptchaStore {
    inner: RwLock<HashMap<String, VecDeque<CaptchaParam>>>,
    notify: tokio::sync::Notify,
}

impl ZcodeCaptchaStore {
    pub fn global() -> &'static Self {
        static INSTANCE: OnceLock<ZcodeCaptchaStore> = OnceLock::new();
        INSTANCE.get_or_init(|| ZcodeCaptchaStore {
            inner: RwLock::new(HashMap::new()),
            notify: tokio::sync::Notify::new(),
        })
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    pub fn store(&self, key: &str, param: String, region: String) {
        if param.trim().is_empty() {
            return;
        }
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Self::now_ms();
        let deque = guard.entry(key.to_string()).or_default();
        // 1. 淘汰已过期条目
        deque.retain(|p| now.saturating_sub(p.issued_at_ms) < CAPTCHA_FRESH_MS);
        // 2. 去重已存在的相同 param
        if let Some(pos) = deque.iter().position(|p| p.param == param) {
            deque.remove(pos);
        }
        // 3. 压入最新条目（队尾）
        deque.push_back(CaptchaParam {
            param,
            region,
            issued_at_ms: now,
        });
        // 4. 超出容量上限时淘汰最老的条目（队头）
        while deque.len() > CAPTCHA_BUFFER_CAPACITY {
            deque.pop_front();
        }
        drop(guard);
        self.notify.notify_waiters();
    }

    fn peek(&self, key: &str) -> Option<CaptchaParam> {
        let guard = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Self::now_ms();
        guard.get(key).and_then(|deque| {
            deque
                .iter()
                .rev()
                .find(|p| now.saturating_sub(p.issued_at_ms) < CAPTCHA_FRESH_MS)
                .cloned()
        })
    }

    pub fn is_fresh(&self, key: &str) -> bool {
        self.peek(key).is_some()
    }

    /// 取新鲜参数（不消费：TTL 窗口内可复用于多个请求，返回最新的新鲜条目）。
    pub fn take_fresh(&self, key: &str) -> Option<CaptchaParam> {
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Self::now_ms();
        let deque = guard.get_mut(key)?;
        deque.retain(|p| now.saturating_sub(p.issued_at_ms) < CAPTCHA_FRESH_MS);
        deque.back().cloned()
    }

    /// 统计指定 key 下当前存活的新鲜验证码数量
    #[allow(dead_code)]
    pub fn count_fresh(&self, key: &str) -> usize {
        let guard = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Self::now_ms();
        guard
            .get(key)
            .map(|deque| {
                deque
                    .iter()
                    .filter(|p| now.saturating_sub(p.issued_at_ms) < CAPTCHA_FRESH_MS)
                    .count()
            })
            .unwrap_or(0)
    }

    /// 等待新鲜参数（在指定时间内等待前端过码组件提交；一旦提交立即唤醒返回）。
    pub async fn wait_fresh(
        &self,
        key: &str,
        timeout: std::time::Duration,
    ) -> Option<CaptchaParam> {
        if let Some(param) = self.take_fresh(key) {
            return Some(param);
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.notify.notified();
            tokio::select! {
                _ = notified => {
                    if let Some(param) = self.take_fresh(key) {
                        return Some(param);
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    return self.take_fresh(key);
                }
            }
        }
    }

    /// 全量作废指定 key 的所有验证码缓存
    pub fn invalidate(&self, key: &str) {
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.remove(key);
    }

    /// 精准作废指定失败的 token（例如遇到 3007 时剔除特定 param）；
    /// 返回缓冲池内是否仍有其他可用新鲜 token。
    pub fn invalidate_param(&self, key: &str, failed_param: &str) -> bool {
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Self::now_ms();
        if let Some(deque) = guard.get_mut(key) {
            deque.retain(|p| {
                p.param != failed_param && now.saturating_sub(p.issued_at_ms) < CAPTCHA_FRESH_MS
            });
            let remaining = !deque.is_empty();
            if !remaining {
                guard.remove(key);
            }
            remaining
        } else {
            false
        }
    }
}

/// 向前端派发“需要过码”事件（Tauri 环境下派发至前端全局后台守候器）
pub fn emit_captcha_needed_event(account_id: &str, key: &str) {
    if let Some(app) = crate::modules::log_bridge::get_app_handle() {
        use tauri::Emitter;
        let _ = app.emit(
            "zcode://solve-captcha",
            serde_json::json!({
                "accountId": account_id,
                "key": key,
            }),
        );
    }
}

// ===== Plan 通道请求头（官方客户端身份头完整集） =====

/// 构建 Plan 通道请求头（协议事实 §客户端身份头完整集）：
/// 身份头全集 + Bearer JWT + （过码后）验证码头。
/// `verify` 为 None 时不附验证码头（billing 只读端点不需要）。
pub fn build_plan_headers(
    profile: &DeviceProfile,
    zcode_jwt: &str,
    verify: Option<(&str, &str)>,
) -> Vec<(String, String)> {
    let mut headers = vec![
        (
            "Authorization".to_string(),
            format!("Bearer {}", zcode_jwt.trim()),
        ),
        (
            "anthropic-version".to_string(),
            ANTHROPIC_VERSION.to_string(),
        ),
        ("Content-Type".to_string(), "application/json".to_string()),
        ("User-Agent".to_string(), format!("ZCode/{APP_VERSION}")),
        ("X-ZCode-App-Version".to_string(), APP_VERSION.to_string()),
        ("X-ZCode-Agent".to_string(), "glm".to_string()),
        ("X-Platform".to_string(), profile.platform_arch()),
        ("X-Os-Category".to_string(), profile.os_category.clone()),
        ("X-Release-Channel".to_string(), RELEASE_CHANNEL.to_string()),
        ("X-Client-Language".to_string(), profile.language.clone()),
        ("X-Client-Timezone".to_string(), profile.timezone.clone()),
        ("X-Title".to_string(), X_TITLE.to_string()),
        (
            "HTTP-Referer".to_string(),
            "https://zcode.z.ai/".to_string(),
        ),
        ("X-Device-Mid".to_string(), profile.device_mid.clone()),
    ];
    if let Some((param, region)) = verify {
        if !param.trim().is_empty() {
            headers.push((
                "X-Aliyun-Captcha-Verify-Param".to_string(),
                param.trim().to_string(),
            ));
            if !region.trim().is_empty() {
                headers.push((
                    "X-Aliyun-Captcha-Region".to_string(),
                    region.trim().to_string(),
                ));
            }
        }
    }
    headers
}

/// Plan 通道请求 URL：入站路径（`/v1/messages`、`/v1/messages/count_tokens`）
/// 拼接到 Plan Anthropic 兼容基座。
pub fn plan_request_url(path: &str) -> String {
    let p = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    format!("{PLAN_BASE}{}", p.trim_end_matches('/'))
}

// ===== 模型名大小写规范化（Plan 通道大小写敏感） =====

/// Plan 通道模型名规范化（通配规则优先，AGENTS.md 纪律）：
/// 按 `-` 分段——`glm` → `GLM`；纯数字/点段保持；其余段首字母大写、余字不动。
/// 幂等：已是规范形态（`GLM-5.3-Flash`）原样返回。
/// 实测覆盖：`glm-5.3-flash`→`GLM-5.3-Flash`、`glm-5-turbo`→`GLM-5-Turbo`、
/// `glm-5.1-highspeed`→`GLM-5.1-Highspeed`、`glm-4.5-air`→`GLM-4.5-Air`、
/// `glm-4.6v`→`GLM-4.6V`。
pub fn canonicalize_plan_model(model: &str) -> String {
    let model = model.trim();
    if model.is_empty() {
        return model.to_string();
    }
    model
        .split('-')
        .map(|seg| {
            if seg.eq_ignore_ascii_case("glm") {
                "GLM".to_string()
            } else if let Some(first_alpha) = seg.chars().position(|c| c.is_ascii_alphabetic()) {
                if first_alpha > 0
                    && seg[..first_alpha]
                        .chars()
                        .all(|c| c.is_ascii_digit() || c == '.' || c == ':')
                {
                    // 如 "4.6v" / "4v"：数字前缀保留，视觉/变体字母全大写
                    let (prefix, suffix) = seg.split_at(first_alpha);
                    format!("{}{}", prefix, suffix.to_ascii_uppercase())
                } else {
                    let mut cs = seg.chars();
                    match cs.next() {
                        Some(first) => first.to_uppercase().collect::<String>() + cs.as_str(),
                        None => seg.to_string(),
                    }
                }
            } else {
                seg.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("-")
}

// ===== 失败分类（池状态机依据） =====

/// Plan 通道失败分类（协议事实 §被拒信号）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanFailure {
    /// 403 + captcha 挑战头 / 400 + code=3007：刷新 verifyParam 原账号重试
    CaptchaChallenge,
    /// 402 / body 含 quota|insufficient|balance|exhaust|额度|余额不足（30 分钟窗）
    Exhausted,
    /// 429 限速（默认 300s 冷却）
    RateLimited,
    /// 401 / 403 非验证码：凭证失效
    InvalidAuth,
    /// 5xx 瞬时故障
    Transient,
    /// 其余请求级错误（模型不存在、参数错误等）：换账号无益
    RequestLevel,
}

impl PlanFailure {
    /// 是否账号级失败（应失败转移换下一槽位）。
    pub fn should_failover(self) -> bool {
        !matches!(self, PlanFailure::RequestLevel)
    }
}

/// 响应头名集合中是否出现验证码挑战标记（403 挑战形态：响应携带 captcha 头）。
pub fn challenge_header_present(header_names: &[String]) -> bool {
    header_names.iter().any(|h| {
        let h = h.to_ascii_lowercase();
        h.contains("captcha") || h.contains("aliyun")
    })
}

/// body 业务码提取（`{code: 3007, ...}` 信封；非 JSON/缺 code → None）。
pub fn envelope_code(body: &str) -> Option<i64> {
    let v: Value = serde_json::from_str(body).ok()?;
    v.get("code").and_then(|c| {
        c.as_i64()
            .or_else(|| c.as_str().and_then(|s| s.parse().ok()))
    })
}

/// Plan 通道失败分类（纯函数）。
pub fn classify_plan_failure(status: u16, body: &str, captcha_challenge: bool) -> PlanFailure {
    if captcha_challenge || envelope_code(body) == Some(PLAN_CAPTCHA_EXPIRED_CODE) {
        return PlanFailure::CaptchaChallenge;
    }
    match status {
        401 => PlanFailure::InvalidAuth,
        402 => PlanFailure::Exhausted,
        403 => PlanFailure::InvalidAuth,
        429 => PlanFailure::RateLimited,
        500..=599 => PlanFailure::Transient,
        _ => {
            let lowered = body.to_lowercase();
            if [
                "quota",
                "insufficient",
                "balance",
                "exhaust",
                "额度",
                "余额不足",
            ]
            .iter()
            .any(|m| lowered.contains(m) || body.contains(m))
            {
                PlanFailure::Exhausted
            } else {
                PlanFailure::RequestLevel
            }
        }
    }
}

// ===== HTTP 请求封装（复用 zcode_oauth 的客户端与信封读取） =====

fn http_client(
    upstream_proxy: &UpstreamProxyConfig,
    request_timeout: u64,
) -> Result<reqwest::Client, String> {
    crate::proxy::providers::zcode_oauth::build_http_client(upstream_proxy, request_timeout)
}

/// 带自定义头的 JSON 请求；非 2xx 不抛错，返回 (status, body, 头名集合) 供分类。
async fn send_json(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    headers: &[(String, String)],
    json_body: Option<Value>,
) -> Result<(u16, String, Vec<String>), String> {
    let mut req = client.request(method, url);
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    if let Some(b) = json_body {
        req = req.json(&b);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("Upstream request failed: {}", e))?;
    let status = resp.status().as_u16();
    let header_names: Vec<String> = resp
        .headers()
        .iter()
        .map(|(k, _)| k.as_str().to_string())
        .collect();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("Failed to read response: {}", e))?;
    Ok((status, body, header_names))
}

// ===== 首启安装序仿真（client/configs + 激活事件） =====

/// 拉取运行配置（免鉴权；User-Agent 仅版本头；platform 参数被拒 3001 不携带）。
pub async fn fetch_client_configs(
    upstream_proxy: &UpstreamProxyConfig,
    request_timeout: u64,
) -> Result<Value, String> {
    let client = http_client(upstream_proxy, request_timeout)?;
    let url = format!("{CLIENT_CONFIGS_URL}?app_version={APP_VERSION}");
    let headers = vec![("User-Agent".to_string(), format!("ZCode/{APP_VERSION}"))];
    let (status, body, _) = send_json(&client, reqwest::Method::GET, &url, &headers, None).await?;
    if status != 200 {
        return Err(format!("client/configs HTTP {status}"));
    }
    let v: Value = serde_json::from_str(&body)
        .map_err(|_| "client/configs returned non-JSON response".to_string())?;
    let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        return Err(format!("client/configs business code {code}"));
    }
    Ok(v)
}

/// 激活事件体（协议事实：官方 sendReport 字段集固定 16 个，逐字段一致）。
pub fn build_activation_event_body(element: &str, profile: &DeviceProfile, user_id: &str) -> Value {
    json!({
        "event_id": uuid::Uuid::new_v4().to_string(),
        "client_timezone": profile.timezone,
        "client_language": profile.language,
        "element_name": element,
        "event_region": "app",
        "event_type": "view",
        "event_text": "",
        "event_extra_detail": {},
        "user_id": user_id,
        "screen_resolution": profile.screen,
        "app_version": APP_VERSION,
        "device_os_category": profile.os_category,
        "device_os_version": profile.os_version,
        "device_mid": profile.device_mid,
        "mac_id": "",
        "marketing_params": "{}",
    })
}

/// 单条激活事件上报（无 Authorization；HTTP/业务码失败返回 Err，由调用方容错）。
pub async fn report_activation_event(
    client: &reqwest::Client,
    profile: &DeviceProfile,
    user_id: &str,
    element: &str,
) -> Result<(), String> {
    let body = build_activation_event_body(element, profile, user_id);
    let headers = vec![("Content-Type".to_string(), "application/json".to_string())];
    let (status, text, _) = send_json(
        client,
        reqwest::Method::POST,
        EVENT_REPORT_URL,
        &headers,
        Some(body),
    )
    .await?;
    if status >= 400 {
        return Err(format!("event/report {element} HTTP {status}"));
    }
    let code = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v.get("code").and_then(|c| c.as_i64()))
        .unwrap_or(-1);
    if code != 0 {
        return Err(format!("event/report {element} business code {code}"));
    }
    Ok(())
}

/// 从订阅 JWT 解出 user_id（payload 的 user_id/sub/id 声明；容错：失败返回空串）。
pub fn jwt_user_id(zcode_jwt: &str) -> String {
    let seg = zcode_jwt.trim().split('.').nth(1).unwrap_or("");
    let decoded = |s: &str| -> Option<Value> {
        let bytes = crate::proxy::providers::zcode_oauth::base64_url_decode(s)?;
        serde_json::from_slice(&bytes).ok()
    };
    let payload = decoded(seg).or_else(|| decoded(&format!("{seg}===")));
    let claims = payload.unwrap_or(Value::Null);
    for key in ["user_id", "userId", "sub", "id", "uid"] {
        if let Some(v) = claims
            .get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            return v;
        }
    }
    String::new()
}

static INSTALL_CONFIGS_DONE: OnceLock<()> = OnceLock::new();

/// 按官方首启顺序对全部 JWT 条目执行安装序（进程内幂等：configs 仅拉一次、
/// 每账号激活事件仅报一次；app_launch 每次进程启动都发与官方一致）。
/// 任何失败都不阻断主流程，错误列表留痕（调用方仅日志）。
pub async fn ensure_install_sequences(
    zai: &ZaiConfig,
    upstream_proxy: &UpstreamProxyConfig,
    request_timeout: u64,
) -> Vec<String> {
    static ACCOUNTS_DONE: OnceLock<RwLock<HashSet<String>>> = OnceLock::new();
    let accounts_done = ACCOUNTS_DONE.get_or_init(|| RwLock::new(HashSet::new()));

    let mut errors: Vec<String> = Vec::new();
    if INSTALL_CONFIGS_DONE.get().is_none() {
        match fetch_client_configs(upstream_proxy, request_timeout).await {
            Ok(_) => {
                let _ = INSTALL_CONFIGS_DONE.set(());
            }
            Err(e) => errors.push(format!("client/configs failed: {e}")),
        }
    }

    let client = match http_client(upstream_proxy, request_timeout) {
        Ok(c) => c,
        Err(e) => {
            errors.push(format!("http client: {e}"));
            return errors;
        }
    };
    for entry in zai.resolved_keys() {
        if entry.mode != crate::proxy::config::ZaiKeyMode::Jwt || !entry.enabled {
            continue;
        }
        let key = captcha_key_for(&entry.account_id, &entry.key);
        let already = accounts_done
            .read()
            .map(|g| g.contains(&key))
            .unwrap_or(false);
        if already {
            continue;
        }
        let profile = profile_for_entry(&entry);
        let user_id = jwt_user_id(&entry.key);
        for element in ACTIVATION_ELEMENTS {
            if let Err(e) = report_activation_event(&client, &profile, &user_id, element).await {
                errors.push(format!("{key}/{element}: {e}"));
            }
        }
        if let Ok(mut guard) = accounts_done.write() {
            guard.insert(key);
        }
    }
    errors
}

// ===== Plan 额度（billing/balance，PlanSlot 数据源） =====

/// 查询 Plan 额度（Bearer JWT + 身份头；data 原样返回由前端容错展示）。
/// 协议事实：`data.balances[]`（show_name/model/total_units/used_units/
/// remaining_units/expires_at）。⚠️ billing 族有 WAF 风险，仅手动按需调用。
pub async fn plan_balance(
    zcode_jwt: &str,
    profile: &DeviceProfile,
    upstream_proxy: &UpstreamProxyConfig,
    request_timeout: u64,
) -> Result<Value, String> {
    let client = http_client(upstream_proxy, request_timeout)?;
    let url = format!("{BILLING_BASE}/billing/balance");
    let headers = build_plan_headers(profile, zcode_jwt, None);
    let (status, body, _) = send_json(&client, reqwest::Method::GET, &url, &headers, None).await?;
    if status != 200 {
        return Err(format!(
            "billing/balance HTTP {status}: {}",
            truncate_body(&body)
        ));
    }
    let v: Value = serde_json::from_str(&body)
        .map_err(|_| "billing/balance returned non-JSON response".to_string())?;
    if let Some(code) = v.get("code").and_then(|c| c.as_i64()) {
        if code != 0 {
            return Err(format!("billing/balance business code {code}"));
        }
    }
    Ok(v.get("data").cloned().unwrap_or(Value::Null))
}

// ===== 限时套餐领取（billing/preview + billing/claim） =====

/// 领取计划视图（容错解析 previews/plans 数组，字段名两种形态都认）。
#[derive(Debug, Clone, Serialize)]
pub struct ZcodeClaimPlan {
    pub plan_id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub priority: Value,
    /// 原样透传（结构未完全文档化）。
    #[serde(default)]
    pub grants: Value,
}

/// 领取结果（业务码语义对齐官方客户端映射；next_at 仅 1005 携带）。
#[derive(Debug, Clone, Serialize)]
pub struct ZcodeClaimResult {
    pub ok: bool,
    pub code: i64,
    pub message: String,
    /// 1005（今日名额用完）：名额恢复时间（data.plan.ends_at 秒 → 毫秒）。
    #[serde(default)]
    pub next_at_ms: Option<u64>,
    /// 成功时 data 原样（server_time + plan 生效窗口）。
    #[serde(default)]
    pub data: Option<Value>,
}

fn truncate_body(body: &str) -> String {
    body.chars().take(160).collect()
}

fn clean_str(v: &Value, keys: &[&str]) -> String {
    for k in keys {
        if let Some(s) = v
            .get(k)
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            return s;
        }
    }
    String::new()
}

/// 查询可领取套餐（活动未上线时 404 属正常态 → 空列表静默）。
/// 查询参数：app_version 必带；platform 参数 preview 容忍。
pub async fn claim_preview(
    zcode_jwt: &str,
    profile: &DeviceProfile,
    upstream_proxy: &UpstreamProxyConfig,
    request_timeout: u64,
) -> Result<Vec<ZcodeClaimPlan>, String> {
    let client = http_client(upstream_proxy, request_timeout)?;
    let url = format!(
        "{BILLING_BASE}/billing/preview?app_version={APP_VERSION}&platform={}",
        profile.platform_arch()
    );
    let headers = build_plan_headers(profile, zcode_jwt, None);
    let (status, body, _) = send_json(&client, reqwest::Method::GET, &url, &headers, None).await?;
    if status == 404 {
        return Ok(Vec::new());
    }
    if status != 200 {
        return Err(format!(
            "billing/preview HTTP {status}: {}",
            truncate_body(&body)
        ));
    }
    let v: Value = serde_json::from_str(&body)
        .map_err(|_| "billing/preview returned non-JSON response".to_string())?;
    let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
    if code != 0 {
        return Err(format!("billing/preview business code {code}"));
    }
    let data = v.get("data").cloned().unwrap_or(Value::Null);
    // 协议事实两种形态：data.plans[] / data.previews[]（文档与实现措辞差异，都认）
    let list = ["plans", "previews", "items", "list"]
        .iter()
        .find_map(|k| data.get(k).and_then(|v| v.as_array()))
        .or_else(|| data.as_array())
        .cloned()
        .unwrap_or_default();
    let plans = list
        .iter()
        .filter_map(|item| {
            let plan_id = clean_str(item, &["plan_id", "planId", "id"]);
            if plan_id.is_empty() {
                return None;
            }
            Some(ZcodeClaimPlan {
                plan_id,
                name: clean_str(item, &["name", "plan_name", "planName", "title"]),
                description: clean_str(item, &["description", "desc"]),
                priority: item.get("priority").cloned().unwrap_or(Value::Null),
                grants: item
                    .get("grants")
                    .or_else(|| item.get("grant_items"))
                    .or_else(|| item.get("grantItems"))
                    .cloned()
                    .unwrap_or(Value::Null),
            })
        })
        .collect();
    Ok(plans)
}

/// 领取套餐（需要有效验证码：缺版本/平台头即使验证码有效也 3007——身份头全集已带）。
/// 业务码：1001 套餐不存在 / 1002 活动结束 / 1003 已领取 / 1004 不符合条件 /
/// 1005 名额用完（next_at=data.plan.ends_at×1000）/ 3001 参数错误 /
/// 3007 验证码失败 / 401 未登录。
pub async fn claim_plan(
    zcode_jwt: &str,
    profile: &DeviceProfile,
    plan_id: &str,
    verify_param: &str,
    verify_region: &str,
    upstream_proxy: &UpstreamProxyConfig,
    request_timeout: u64,
) -> ZcodeClaimResult {
    if plan_id.trim().is_empty() {
        return ZcodeClaimResult {
            ok: false,
            code: -1,
            message: "plan_id is empty".to_string(),
            next_at_ms: None,
            data: None,
        };
    }
    let run = async {
        let client = http_client(upstream_proxy, request_timeout)?;
        let url = format!("{BILLING_BASE}/billing/claim");
        let mut headers =
            build_plan_headers(profile, zcode_jwt, Some((verify_param, verify_region)));
        headers.push(("X-Platform".to_string(), profile.platform_arch()));
        headers.push(("X-ZCode-App-Version".to_string(), APP_VERSION.to_string()));
        headers.push(("X-Device-Mid".to_string(), profile.device_mid.clone()));
        let (status, body, _) = send_json(
            &client,
            reqwest::Method::POST,
            &url,
            &headers,
            Some(json!({ "plan_id": plan_id.trim() })),
        )
        .await?;
        if status == 401 {
            return Ok(ZcodeClaimResult {
                ok: false,
                code: 401,
                message: "credential rejected (401)".to_string(),
                next_at_ms: None,
                data: None,
            });
        }
        if status != 200 {
            return Ok(ZcodeClaimResult {
                ok: false,
                code: status as i64,
                message: format!("billing/claim HTTP {status}: {}", truncate_body(&body)),
                next_at_ms: None,
                data: None,
            });
        }
        let v: Value = serde_json::from_str(&body)
            .map_err(|_| "billing/claim returned non-JSON response".to_string())?;
        let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
        let data = v.get("data").cloned();
        if code == 0 {
            return Ok(ZcodeClaimResult {
                ok: true,
                code: 0,
                message: "claimed".to_string(),
                next_at_ms: None,
                data,
            });
        }
        let next_at_ms = data
            .as_ref()
            .and_then(|d| d.get("plan"))
            .and_then(|p| p.get("ends_at"))
            .and_then(|e| e.as_f64())
            .map(|secs| (secs * 1000.0) as u64);
        let message = match code {
            1001 => "plan not found".to_string(),
            1002 => "campaign ended or unavailable".to_string(),
            1003 => "already claimed".to_string(),
            1004 => "not eligible".to_string(),
            1005 => "daily quota exhausted".to_string(),
            3001 => "invalid claim params".to_string(),
            3007 => "captcha verification failed".to_string(),
            other => format!("claim failed (code {other})"),
        };
        Ok(ZcodeClaimResult {
            ok: false,
            code,
            message,
            next_at_ms,
            data,
        })
    };
    match run.await {
        Ok(r) => r,
        Err(e) => ZcodeClaimResult {
            ok: false,
            code: -1,
            message: e,
            next_at_ms: None,
            data: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== 验证码配置提取 =====

    #[test]
    fn captcha_config_defaults_when_missing() {
        let out = extract_captcha_config(&json!({"code": 0, "data": {}}));
        assert!(out.enabled);
        assert_eq!(out.prefix, "no8xfe");
        assert_eq!(out.region, "cn");
        assert_eq!(out.scene_id, "11xygtvd");
    }

    #[test]
    fn captcha_config_dynamic_values_win() {
        let body = json!({
            "code": 0,
            "data": {"configs": {"captcha": {
                "enabled": true, "prefix": "pxy1", "region": "sgp", "sceneId": "scene9"
            }}}
        });
        let out = extract_captcha_config(&body);
        assert_eq!(out.prefix, "pxy1");
        assert_eq!(out.region, "sgp");
        assert_eq!(out.scene_id, "scene9");
    }

    // ===== 设备指纹 =====

    #[test]
    fn generated_profile_has_consistent_sku_and_stable_mid() {
        let p = generate_device_profile();
        assert!(!p.device_mid.is_empty());
        assert_eq!(p.platform_arch(), format!("{}-{}", p.platform, p.arch));
        assert!(SKU_TABLE.iter().any(|(_, pl, ar, os, sc)| *pl == p.platform
            && *ar == p.arch
            && *os == p.os_version
            && *sc == p.screen));
        assert!(REGION_PAIRS
            .iter()
            .any(|(l, t)| *l == p.language && *t == p.timezone));
    }

    #[test]
    fn profile_round_trips_through_json() {
        let p = generate_device_profile();
        let restored = DeviceProfile::from_value(&p.to_value()).expect("restore");
        assert_eq!(p, restored);
        assert!(DeviceProfile::from_value(&json!({"device_mid": ""})).is_none());
    }

    #[test]
    fn profile_for_entry_prefers_persisted_value() {
        let persisted = generate_device_profile();
        let entry = ZaiKeyEntry {
            key: "jwt.t.x".to_string(),
            provider: crate::proxy::config::ZaiProvider::ZcodePlan,
            enabled: true,
            label: String::new(),
            mode: crate::proxy::config::ZaiKeyMode::Jwt,
            account_id: "acc-7".to_string(),
            user_email: String::new(),
            business_jwt: String::new(),
            device_profile: persisted.to_value(),
        };
        assert_eq!(profile_for_entry(&entry), persisted);
    }

    // ===== 验证码存储 =====

    #[test]
    fn captcha_store_freshness_and_invalidate() {
        let store = ZcodeCaptchaStore {
            inner: RwLock::new(HashMap::new()),
            notify: tokio::sync::Notify::new(),
        };
        assert!(!store.is_fresh("acc-1"));
        store.store("acc-1", "token-1".to_string(), "cn".to_string());
        assert!(store.is_fresh("acc-1"));
        assert_eq!(store.count_fresh("acc-1"), 1);
        assert_eq!(store.take_fresh("acc-1").unwrap().param, "token-1");

        // 压入第二个与第三个 token，take_fresh 应返回最新压入的条目
        store.store("acc-1", "token-2".to_string(), "cn".to_string());
        store.store("acc-1", "token-3".to_string(), "cn".to_string());
        assert_eq!(store.count_fresh("acc-1"), 3);
        assert_eq!(store.take_fresh("acc-1").unwrap().param, "token-3");

        // 压入第四个 token，超过 CAPTCHA_BUFFER_CAPACITY (3)，最老条目 (token-1) 应被淘汰
        store.store("acc-1", "token-4".to_string(), "cn".to_string());
        assert_eq!(store.count_fresh("acc-1"), 3);
        assert_eq!(store.take_fresh("acc-1").unwrap().param, "token-4");

        // 测试精准失效：作废 token-4，缓冲池内仍剩余 2 个可用 token (token-2, token-3)
        let has_more = store.invalidate_param("acc-1", "token-4");
        assert!(has_more);
        assert_eq!(store.count_fresh("acc-1"), 2);
        assert_eq!(store.take_fresh("acc-1").unwrap().param, "token-3");

        // 全量失效
        store.invalidate("acc-1");
        assert!(!store.is_fresh("acc-1"));
        assert_eq!(store.count_fresh("acc-1"), 0);

        // 空参数不入库
        store.store("acc-2", "  ".to_string(), "cn".to_string());
        assert!(!store.is_fresh("acc-2"));
    }

    #[test]
    fn captcha_key_prefers_account_id() {
        assert_eq!(captcha_key_for("acc-9", "jwt.x.y"), "acc-9");
        assert_eq!(captcha_key_for("", "jwt.x.y"), "jwt.x.y");
        assert_eq!(captcha_key_for("  ", "jwt.x.y"), "jwt.x.y");
    }

    // ===== 请求头 =====

    #[test]
    fn plan_headers_carry_full_identity_set() {
        let profile = DeviceProfile {
            platform: "darwin".into(),
            arch: "arm64".into(),
            os_category: "mac".into(),
            os_version: "24.5.0".into(),
            screen: "1512x982".into(),
            language: "zh-CN".into(),
            timezone: "Asia/Shanghai".into(),
            device_mid: "0f0e-uuid".into(),
        };
        let headers = build_plan_headers(&profile, "jwt-token", Some(("cGFyYW0", "cn")));
        let get = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("Authorization").as_deref(), Some("Bearer jwt-token"));
        assert_eq!(get("User-Agent").as_deref(), Some("ZCode/3.14.3"));
        assert_eq!(get("X-ZCode-App-Version").as_deref(), Some("3.14.3"));
        assert_eq!(get("X-ZCode-Agent").as_deref(), Some("glm"));
        assert_eq!(get("X-Platform").as_deref(), Some("darwin-arm64"));
        assert_eq!(get("X-Os-Category").as_deref(), Some("mac"));
        assert_eq!(get("X-Release-Channel").as_deref(), Some("stable"));
        assert_eq!(get("X-Client-Language").as_deref(), Some("zh-CN"));
        assert_eq!(get("X-Client-Timezone").as_deref(), Some("Asia/Shanghai"));
        assert_eq!(get("X-Title").as_deref(), Some("Z Code@electron"));
        assert_eq!(get("HTTP-Referer").as_deref(), Some("https://zcode.z.ai/"));
        assert_eq!(get("X-Device-Mid").as_deref(), Some("0f0e-uuid"));
        assert_eq!(
            get("X-Aliyun-Captcha-Verify-Param").as_deref(),
            Some("cGFyYW0")
        );
        assert_eq!(get("X-Aliyun-Captcha-Region").as_deref(), Some("cn"));
        assert_eq!(get("anthropic-version").as_deref(), Some("2023-06-01"));
        // 无验证码时不附验证码头（billing 只读端点形态）
        let no_captcha = build_plan_headers(&profile, "jwt-token", None);
        assert!(no_captcha
            .iter()
            .all(|(k, _)| !k.starts_with("X-Aliyun-Captcha")));
    }

    #[test]
    fn plan_url_joins_paths() {
        assert_eq!(
            plan_request_url("/v1/messages"),
            "https://zcode.z.ai/api/v1/zcode-plan/anthropic/v1/messages"
        );
        assert_eq!(
            plan_request_url("v1/messages/count_tokens"),
            "https://zcode.z.ai/api/v1/zcode-plan/anthropic/v1/messages/count_tokens"
        );
    }

    // ===== 模型名规范化 =====

    #[test]
    fn canonicalize_plan_model_rules() {
        assert_eq!(canonicalize_plan_model("glm-5.3-flash"), "GLM-5.3-Flash");
        assert_eq!(canonicalize_plan_model("glm-5.3"), "GLM-5.3");
        assert_eq!(canonicalize_plan_model("glm-5-turbo"), "GLM-5-Turbo");
        assert_eq!(
            canonicalize_plan_model("glm-5.1-highspeed"),
            "GLM-5.1-Highspeed"
        );
        assert_eq!(canonicalize_plan_model("glm-4.5-air"), "GLM-4.5-Air");
        assert_eq!(canonicalize_plan_model("glm-4.6v"), "GLM-4.6V");
        // 幂等：规范形态原样返回
        assert_eq!(canonicalize_plan_model("GLM-5.3-Flash"), "GLM-5.3-Flash");
        assert_eq!(canonicalize_plan_model("GLM-5.3"), "GLM-5.3");
        assert_eq!(canonicalize_plan_model(""), "");
    }

    // ===== 失败分类 =====

    #[test]
    fn classify_plan_failure_matrix() {
        assert_eq!(
            classify_plan_failure(400, "{\"code\":3007,\"message\":\"captcha\"}", false),
            PlanFailure::CaptchaChallenge
        );
        assert!(challenge_header_present(&[
            "x-captcha-challenge".to_string()
        ]));
        assert_eq!(
            classify_plan_failure(403, "forbidden", true),
            PlanFailure::CaptchaChallenge
        );
        assert_eq!(
            classify_plan_failure(403, "forbidden", false),
            PlanFailure::InvalidAuth
        );
        assert_eq!(
            classify_plan_failure(401, "", false),
            PlanFailure::InvalidAuth
        );
        assert_eq!(
            classify_plan_failure(402, "", false),
            PlanFailure::Exhausted
        );
        assert_eq!(
            classify_plan_failure(429, "", false),
            PlanFailure::RateLimited
        );
        assert_eq!(
            classify_plan_failure(
                400,
                "{\"error\":{\"message\":\"quota insufficient\"}}",
                false
            ),
            PlanFailure::Exhausted
        );
        assert_eq!(
            classify_plan_failure(400, "{\"error\":\"余额不足\"}", false),
            PlanFailure::Exhausted
        );
        assert_eq!(
            classify_plan_failure(503, "", false),
            PlanFailure::Transient
        );
        assert_eq!(
            classify_plan_failure(400, "{\"error\":\"model not found\"}", false),
            PlanFailure::RequestLevel
        );
        assert!(PlanFailure::CaptchaChallenge.should_failover());
        assert!(!PlanFailure::RequestLevel.should_failover());
    }

    // ===== 激活事件体 =====

    #[test]
    fn activation_event_body_has_fixed_16_fields() {
        let profile = generate_device_profile();
        let body = build_activation_event_body("app_launch", &profile, "user-1");
        let obj = body.as_object().expect("object");
        assert_eq!(obj.len(), 16);
        assert_eq!(obj["element_name"], "app_launch");
        assert_eq!(obj["event_region"], "app");
        assert_eq!(obj["event_type"], "view");
        assert_eq!(obj["user_id"], "user-1");
        assert_eq!(obj["screen_resolution"], profile.screen);
        assert_eq!(obj["app_version"], APP_VERSION);
        assert_eq!(obj["device_os_category"], profile.os_category);
        assert_eq!(obj["device_os_version"], profile.os_version);
        assert_eq!(obj["device_mid"], profile.device_mid);
        assert_eq!(obj["mac_id"], "");
        assert_eq!(obj["marketing_params"], "{}");
        assert!(obj["event_id"].as_str().is_some());
        assert_eq!(obj["event_text"], "");
        assert!(obj["event_extra_detail"].is_object());
    }

    #[test]
    fn jwt_user_id_reads_payload_claims() {
        use base64::Engine;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"user_id":"u-123","email":"x@y.io"}"#);
        let jwt = format!("aaa.{payload}.bbb");
        assert_eq!(jwt_user_id(&jwt), "u-123");
        assert_eq!(jwt_user_id("not-a-jwt"), "");
    }

    // ===== 领取结果解析（纯解析部分借 envelope_code / clean_str 间接覆盖） =====

    #[test]
    fn envelope_code_parses_number_and_string() {
        assert_eq!(envelope_code("{\"code\":3007}"), Some(3007));
        assert_eq!(envelope_code("{\"code\":\"3007\"}"), Some(3007));
        assert_eq!(envelope_code("not json"), None);
        assert_eq!(envelope_code("{\"message\":\"x\"}"), None);
    }
}
