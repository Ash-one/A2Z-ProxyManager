//! zcode：每日定时领取活动套餐调度器（proxy.zai.auto_claim 配置驱动）。
//!
//! 设计约束：
//! - 复用 Plan 通道既有凭据与验证码机制：`claim_preview` → 缓冲池取新鲜验证码
//!   （不足时 Node 无头求解器补位）→ `claim_plan`；3007 时精准作废单枚验证码换码重试；
//! - 领取前先拉 billing/balance 已生效套餐集合做短路，避免为已领套餐白耗验证码；
//! - 探测频率克制：preview 未发现套餐时最多 5 次探测（60s 间隔），账号间 3s 隔离，
//!   避免 billing 端点连续查询触发 WAF（见 `zcode_plan::BILLING_BASE` 注释）；
//! - 配置热生效：调度循环每轮从 zai 状态现读 `auto_claim`（enabled/time），改配置免重启；
//! - 错过补跑：机器睡眠/进程暂停跨过时点后，唤醒时判定该 (日期, 时点) 未触发即补跑一次
//!   （语义对齐 launchd StartCalendarInterval）。

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Local, NaiveDate, Timelike};
use tokio::sync::RwLock;

use crate::proxy::config::{
    AutoClaimConfig, UpstreamProxyConfig, ZaiConfig, ZaiKeyEntry, ZaiKeyMode,
};
use crate::proxy::providers::zcode_plan as zplan;

/// preview 未发现可领套餐时的最大探测次数（含首次；覆盖活动延后上线的窗口）。
const PREVIEW_PROBES: usize = 5;
/// 相邻两次 preview 探测间隔（秒）。
const PREVIEW_PROBE_INTERVAL_SECS: u64 = 60;
/// 验证码被 3007 拒绝后的最大提交次数（含首次；每次换新鲜验证码）。
const CLAIM_CAPTCHA_ATTEMPTS: usize = 3;
/// 调度循环节拍（秒）：配置热生效与错过时点的补跑均以该粒度感知。
const TICK_SECS: u64 = 60;
/// 多账号顺序领取时的账号间隔离间隔（秒），降低 billing 端点连续命中强度。
const ACCOUNT_GAP_SECS: u64 = 3;

/// 解析 "HH:MM" 为 (小时, 分钟)；任何非法输入回退 (0, 0)。
pub fn parse_hhmm(s: &str) -> (u32, u32) {
    let parts: Vec<&str> = s.trim().split(':').collect();
    if let [h, m] = parts[..] {
        if let (Ok(h), Ok(m)) = (h.trim().parse::<u32>(), m.trim().parse::<u32>()) {
            if h < 24 && m < 60 {
                return (h, m);
            }
        }
    }
    (0, 0)
}

/// 当前时刻是否应触发：今天本地时区的目标时点已过（含相等）且该 (日期, 时点) 未触发过。
/// `last_fired` 以 (日期, 时点) 为粒度记录，修改触发时间后当日新时点仍可触发一次。
pub fn should_fire(
    now: DateTime<Local>,
    slot: (u32, u32),
    last_fired: Option<(NaiveDate, (u32, u32))>,
) -> bool {
    if last_fired == Some((now.date_naive(), slot)) {
        return false;
    }
    let (h, m) = slot;
    now.hour() > h || (now.hour() == h && now.minute() >= m)
}

/// 调度专属落盘日志：应用文件日志仅收 ERROR 级（modules/logger.rs），调度过程的
/// info/warn 平时只进 GUI 调试控制台，磁盘上无痕可查；此处独立追加写
/// auto_claim.log 留审计痕迹（长期停用会被 cleanup_legacy_logs 的 7 天过期清理掉）。
fn slog(msg: &str) {
    tracing::info!("{}", msg);
    let Ok(dir) = crate::modules::logger::get_log_dir() else {
        return;
    };
    use std::io::Write;
    let line = format!(
        "[{}] {}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        msg
    );
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("auto_claim.log"))
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// 进程内单例守卫：server stop/start 重启时避免重复拉起调度循环。
static SCHEDULER_SPAWNED: AtomicBool = AtomicBool::new(false);

/// 拉起每日定时领取调度循环（随网关启动调用；进程内至多一个实例）。
pub fn spawn_scheduler(
    zai: Arc<RwLock<ZaiConfig>>,
    upstream_proxy: Arc<RwLock<UpstreamProxyConfig>>,
    request_timeout: u64,
) {
    if SCHEDULER_SPAWNED.swap(true, Ordering::SeqCst) {
        tracing::info!("[zcode auto-claim] 调度器已在运行，跳过重复拉起");
        return;
    }
    tokio::spawn(async move {
        slog("[zcode auto-claim] 调度器已启动（每轮热读取 proxy.zai.auto_claim）");
        scheduler_loop(zai, upstream_proxy, request_timeout).await;
    });
}

async fn scheduler_loop(
    zai: Arc<RwLock<ZaiConfig>>,
    upstream_proxy: Arc<RwLock<UpstreamProxyConfig>>,
    request_timeout: u64,
) {
    let mut last_fired: Option<(NaiveDate, (u32, u32))> = None;
    loop {
        tokio::time::sleep(Duration::from_secs(TICK_SECS)).await;
        let cfg: AutoClaimConfig = zai.read().await.auto_claim.clone();
        if !cfg.enabled {
            continue;
        }
        let slot = parse_hhmm(&cfg.time);
        let now = Local::now();
        if !should_fire(now, slot, last_fired) {
            continue;
        }
        last_fired = Some((now.date_naive(), slot));
        slog(&format!(
            "[zcode auto-claim] 定时触发（每日 {:02}:{:02}）",
            slot.0, slot.1
        ));
        run_claim_cycle(&zai, &upstream_proxy, request_timeout).await;
    }
}

async fn run_claim_cycle(
    zai: &RwLock<ZaiConfig>,
    upstream_proxy: &RwLock<UpstreamProxyConfig>,
    request_timeout: u64,
) {
    let zai_cfg = zai.read().await.clone();
    let proxy_cfg = upstream_proxy.read().await.clone();

    // 预热动态验证码配置：上游可能轮换 sceneId/prefix/region，Node 求解器跟随
    // client/configs 动态值（拉取失败回退协议默认值，不阻塞领取）。
    if let Err(e) = zplan::captcha_command_config(&proxy_cfg, request_timeout).await {
        slog(&format!(
            "[zcode auto-claim] 动态验证码配置拉取失败，回退默认值：{}",
            e
        ));
    }

    let jwt_entries: Vec<ZaiKeyEntry> = zai_cfg
        .keys
        .iter()
        .filter(|k| k.enabled && k.mode == ZaiKeyMode::Jwt && !k.key.trim().is_empty())
        .cloned()
        .collect();
    if jwt_entries.is_empty() {
        slog("[zcode auto-claim] 无启用的 JWT 订阅账号，本轮跳过");
        return;
    }
    for entry in &jwt_entries {
        let label = account_label(entry).to_string();
        let outcome = claim_account(entry, &proxy_cfg, request_timeout).await;
        match (
            &outcome.error,
            outcome.claimed.len(),
            outcome.missed.len(),
            outcome.already_active.len(),
        ) {
            (Some(e), ..) => slog(&format!(
                "[zcode auto-claim] 账号 {} 领取失败：{}",
                label, e
            )),
            (None, 0, 0, 0) => slog(&format!("[zcode auto-claim] 账号 {} 本轮无可领套餐", label)),
            (None, claimed, missed, active) => slog(&format!(
                "[zcode auto-claim] 账号 {} 领取完成：成功 {} 项，未领到 {} 项，已生效 {} 项",
                label, claimed, missed, active
            )),
        }
        if let Some((success, body)) = outcome.notification(&label) {
            match send_system_notification(success, &body) {
                Ok(()) => slog(&format!(
                    "[zcode auto-claim] 系统通知已发送（{}）：{}",
                    if success { "成功" } else { "失败" },
                    body
                )),
                Err(e) => slog(&format!(
                    "[zcode auto-claim] 系统通知发送失败：{}（正文：{}）",
                    e, body
                )),
            }
        }
        tokio::time::sleep(Duration::from_secs(ACCOUNT_GAP_SECS)).await;
    }
}

fn account_label(entry: &ZaiKeyEntry) -> &str {
    let label = entry.label.trim();
    if !label.is_empty() {
        return label;
    }
    let email = entry.user_email.trim();
    if !email.is_empty() {
        return email;
    }
    let id = entry.account_id.trim();
    if !id.is_empty() {
        return id;
    }
    "unnamed"
}

/// 单账号领取结果汇总（通知与日志的统一数据源）。
#[derive(Debug, Default)]
struct ClaimOutcome {
    /// 成功领取的套餐名。
    claimed: Vec<String>,
    /// 发现了但未能领取的条目（含原因），如名额用完 / 验证码多次被拒。
    missed: Vec<String>,
    /// 本已生效而无需重复领取的条目（手动 / 其他端先行领取）。
    already_active: Vec<String>,
    /// 流程级错误（balance / preview 等前置查询失败），发生时本轮该账号未执行领取。
    error: Option<String>,
}

impl ClaimOutcome {
    /// 通知判定：成功或失败时返回 Some((是否成功, 通知正文))；
    /// 静默日（探测窗口内无可领套餐且今日无生效套餐）返回 None，不打扰用户。
    fn notification(&self, label: &str) -> Option<(bool, String)> {
        if let Some(err) = &self.error {
            return Some((false, format!("❌ 账号 {} 领取失败：{}", label, err)));
        }
        if !self.claimed.is_empty() {
            let mut body = format!("🎁 账号 {} 领取成功：{}", label, self.claimed.join("、"));
            if !self.missed.is_empty() {
                body.push_str(&format!("；未领到：{}", self.missed.join("、")));
            }
            return Some((true, body));
        }
        if !self.missed.is_empty() {
            return Some((
                false,
                format!("❌ 账号 {} 未能领取：{}", label, self.missed.join("、")),
            ));
        }
        if !self.already_active.is_empty() {
            return Some((
                true,
                format!(
                    "♻️ 账号 {} 今日礼包已生效，无需重复领取：{}",
                    label,
                    self.already_active.join("、")
                ),
            ));
        }
        None
    }
}

/// 发送系统级通知（macOS 通知中心等；无头模式下无 AppHandle，静默跳过仅留日志）。
fn send_system_notification(success: bool, body: &str) -> Result<(), String> {
    let Some(app) = crate::modules::log_bridge::get_app_handle() else {
        slog("[zcode auto-claim] 无头模式，跳过系统通知");
        return Ok(());
    };
    use tauri_plugin_notification::NotificationExt;
    let title = if success {
        "A2Z 自动领取 · 成功"
    } else {
        "A2Z 自动领取 · 失败"
    };
    app.notification()
        .builder()
        .title(title)
        .body(body)
        .show()
        .map_err(|e| e.to_string())
}

/// 单账号领取流程：balance 短路 → preview 探测 → 换码重试式领取。返回结果汇总。
async fn claim_account(
    entry: &ZaiKeyEntry,
    upstream_proxy: &UpstreamProxyConfig,
    request_timeout: u64,
) -> ClaimOutcome {
    let mut outcome = ClaimOutcome::default();
    let profile = zplan::profile_for_entry(entry);
    let captcha_key = zplan::captcha_key_for(&entry.account_id, &entry.key);

    // 1) 已生效套餐集合：命中则跳过领取，避免为已领套餐白耗一枚验证码（1003 前置短路）。
    let balance =
        match zplan::plan_balance(&entry.key, &profile, upstream_proxy, request_timeout).await {
            Ok(v) => v,
            Err(e) => {
                outcome.error = Some(format!("查询配额失败：{}", e));
                return outcome;
            }
        };
    let active_plan_ids: HashSet<String> = balance
        .get("plans")
        .and_then(|p| p.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|p| {
                    p.get("plan_id")
                        .and_then(|i| i.as_str())
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                })
                .collect()
        })
        .unwrap_or_default();

    // 今日内到期（≤24h）的生效套餐 = "今日礼包已领取"的锚点：
    // preview 对已领套餐不再展示（实测返回空列表），balance 是唯一可靠信号。
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let today_active_names: Vec<String> = balance
        .get("plans")
        .and_then(|p| p.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|p| {
                    let ends_at = p.get("ends_at").and_then(|e| e.as_f64())? as u64;
                    if ends_at > now_secs && ends_at - now_secs <= 86_400 {
                        let name = p.get("name").and_then(|n| n.as_str()).unwrap_or("");
                        Some(if name.is_empty() {
                            p.get("plan_id")
                                .and_then(|i| i.as_str())
                                .unwrap_or("unknown")
                                .to_string()
                        } else {
                            name.to_string()
                        })
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    // 2) 探测可领套餐：活动未上线（404 → 空列表）属正常态，窗口内重试直至发现。
    //    已领日（今日有生效套餐）直接短路：preview 不会展示已领套餐，继续探测纯属浪费。
    let plans = if today_active_names.is_empty() {
        let mut plans = Vec::new();
        for probe in 1..=PREVIEW_PROBES {
            match zplan::claim_preview(&entry.key, &profile, upstream_proxy, request_timeout).await
            {
                Ok(found) => {
                    plans = found;
                    if !plans.is_empty() {
                        break;
                    }
                    if probe < PREVIEW_PROBES {
                        tokio::time::sleep(Duration::from_secs(PREVIEW_PROBE_INTERVAL_SECS)).await;
                    }
                }
                Err(e) => {
                    outcome.error = Some(format!("探测可领套餐失败：{}", e));
                    return outcome;
                }
            }
        }
        if plans.is_empty() {
            slog("[zcode auto-claim] 探测窗口内无可领活动套餐");
            return outcome;
        }
        plans
    } else {
        slog(&format!(
            "[zcode auto-claim] 账号 {} 今日礼包已生效（{}），跳过探测与领取",
            account_label(entry),
            today_active_names.join("、")
        ));
        outcome.already_active = today_active_names;
        return outcome;
    };

    // 3) 逐项领取：缓冲池新鲜验证码优先，不足时 Node 无头求解补位；3007 精准作废换码重试。
    for plan in &plans {
        if active_plan_ids.contains(&plan.plan_id) {
            slog(&format!(
                "[zcode auto-claim] 套餐 {}（{}）已生效，跳过",
                plan.plan_id, plan.name
            ));
            outcome.already_active.push(plan.name.clone());
            continue;
        }
        let mut settled = false;
        for attempt in 1..=CLAIM_CAPTCHA_ATTEMPTS {
            let captcha = match zplan::ZcodeCaptchaStore::global().take_fresh(&captcha_key) {
                Some(c) => Some(c),
                None => zplan::solve_captcha_via_node(&entry.account_id, &entry.key).await,
            };
            let Some(captcha) = captcha else {
                tracing::warn!(
                    "[zcode auto-claim] 验证码求解失败（第 {}/{} 次），套餐 {}",
                    attempt,
                    CLAIM_CAPTCHA_ATTEMPTS,
                    plan.plan_id
                );
                continue;
            };
            let res = zplan::claim_plan(
                &entry.key,
                &profile,
                &plan.plan_id,
                &captcha.param,
                &captcha.region,
                upstream_proxy,
                request_timeout,
            )
            .await;
            if res.ok {
                tracing::info!(
                    "[zcode auto-claim] 🎉 领取成功: {}（{}）",
                    plan.name,
                    plan.plan_id
                );
                outcome.claimed.push(plan.name.clone());
                settled = true;
            } else {
                match res.code {
                    // 终态：重试无意义
                    1001 | 1002 | 1003 | 1004 | 1005 => {
                        tracing::info!(
                            "[zcode auto-claim] 套餐 {}（{}）不可领：{}",
                            plan.plan_id,
                            plan.name,
                            res.message
                        );
                        outcome
                            .missed
                            .push(format!("{}（{}）", plan.name, res.message));
                        settled = true;
                    }
                    // 验证码被拒：作废该枚，换新鲜验证码重试
                    3007 => {
                        zplan::ZcodeCaptchaStore::global()
                            .invalidate_param(&captcha_key, &captcha.param);
                        tracing::warn!(
                            "[zcode auto-claim] 验证码被拒（3007），换码重试（第 {}/{} 次），套餐 {}",
                            attempt,
                            CLAIM_CAPTCHA_ATTEMPTS,
                            plan.plan_id
                        );
                    }
                    other => {
                        tracing::warn!(
                            "[zcode auto-claim] 套餐 {}（{}）领取异常（code {}）：{}",
                            plan.plan_id,
                            plan.name,
                            other,
                            res.message
                        );
                        outcome
                            .missed
                            .push(format!("{}（code {} {}）", plan.name, other, res.message));
                        settled = true;
                    }
                }
            }
            if settled {
                break;
            }
        }
        if !settled {
            // 全部换码重试均未通过（求解失败 / 3007 耗尽）
            outcome
                .missed
                .push(format!("{}（验证码验证未通过）", plan.name));
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn parse_hhmm_valid_and_fallback() {
        assert_eq!(parse_hhmm("00:00"), (0, 0));
        assert_eq!(parse_hhmm("09:30"), (9, 30));
        assert_eq!(parse_hhmm("23:59"), (23, 59));
        assert_eq!(parse_hhmm("7:05"), (7, 5));
        assert_eq!(parse_hhmm("  12:40 "), (12, 40));
        // 非法输入一律回退 00:00
        assert_eq!(parse_hhmm("24:00"), (0, 0));
        assert_eq!(parse_hhmm("12:60"), (0, 0));
        assert_eq!(parse_hhmm("abc"), (0, 0));
        assert_eq!(parse_hhmm(""), (0, 0));
        assert_eq!(parse_hhmm("12"), (0, 0));
    }

    #[test]
    fn should_fire_respects_slot_and_last_fired() {
        let at = |h: u32, m: u32| Local.with_ymd_and_hms(2026, 10, 5, h, m, 0).unwrap();
        let slot = (0, 0);
        // 时点未到不触发
        assert!(!should_fire(at(6, 0), (12, 0), None));
        // 时点到达/已过即触发（含错过后补跑：23:59 仍可补跑当天 00:00 的份额）
        assert!(should_fire(at(0, 0), slot, None));
        assert!(should_fire(at(0, 30), slot, None));
        assert!(should_fire(at(23, 59), slot, None));
        // 当日同一 (日期, 时点) 已触发不再重复
        assert!(!should_fire(
            at(0, 1),
            slot,
            Some((at(0, 0).date_naive(), slot))
        ));
        assert!(!should_fire(
            at(13, 0),
            slot,
            Some((at(0, 0).date_naive(), slot))
        ));
        // 次日重新触发
        let next_day = Local.with_ymd_and_hms(2026, 10, 6, 0, 0, 0).unwrap();
        assert!(should_fire(
            next_day,
            slot,
            Some((next_day.date_naive().pred_opt().unwrap(), slot))
        ));
        // 次日时点未到不触发（即便前一日已触发）
        let next_morning = Local.with_ymd_and_hms(2026, 10, 6, 6, 0, 0).unwrap();
        assert!(!should_fire(
            next_morning,
            (12, 0),
            Some((next_morning.date_naive().pred_opt().unwrap(), (12, 0)))
        ));
        // 当日修改触发时点：新时点未触发过仍会触发
        assert!(should_fire(
            at(12, 0),
            (12, 0),
            Some((at(12, 0).date_naive(), (0, 0)))
        ));
        // 同一时点同日已触发则不重复
        assert!(!should_fire(
            at(12, 5),
            (12, 0),
            Some((at(12, 0).date_naive(), (12, 0)))
        ));
    }

    #[test]
    fn auto_claim_config_defaults() {
        let cfg = AutoClaimConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.time, "00:00");
        // 旧配置 JSON 缺字段 → serde default 补齐，行为等同默认每日零点
        let parsed: AutoClaimConfig = serde_json::from_str("{}").unwrap();
        assert!(parsed.enabled);
        assert_eq!(parsed.time, "00:00");
    }

    #[test]
    fn claim_outcome_notification_policy() {
        // 成功：claimed 非空（途中另有未领到的，附在正文）
        let out = ClaimOutcome {
            claimed: vec!["礼包A".into()],
            missed: vec!["礼包B（daily quota exhausted）".into()],
            already_active: Vec::new(),
            error: None,
        };
        let (success, body) = out.notification("acc1").unwrap();
        assert!(success);
        assert!(body.contains("礼包A"));
        assert!(body.contains("礼包B"));

        // 失败：流程级错误优先呈现
        let out = ClaimOutcome {
            error: Some("查询配额失败".into()),
            ..Default::default()
        };
        let (success, body) = out.notification("acc1").unwrap();
        assert!(!success);
        assert!(body.contains("查询配额失败"));

        // 失败：没有成功领取但存在未领到条目
        let out = ClaimOutcome {
            missed: vec!["礼包A（验证码验证未通过）".into()],
            ..Default::default()
        };
        let (success, _) = out.notification("acc1").unwrap();
        assert!(!success);

        // 已生效：无需重复领取（按成功样式推送中性提示）
        let out = ClaimOutcome {
            already_active: vec!["礼包A".into()],
            ..Default::default()
        };
        let (success, body) = out.notification("acc1").unwrap();
        assert!(success);
        assert!(body.contains("已生效"));

        // 静默日：无成功、无未领到、无已生效（无可领套餐且今日无生效套餐）
        assert!(ClaimOutcome::default().notification("acc1").is_none());
    }
}
