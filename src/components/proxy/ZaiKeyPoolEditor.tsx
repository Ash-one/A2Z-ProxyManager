import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Coins, Gift, KeyRound, Plus, RefreshCw, ShieldCheck, Sparkles, Trash2, X } from 'lucide-react';
import { request as invoke } from '../../utils/request';
import HelpTooltip from '../common/HelpTooltip';
import { showToast } from '../common/ToastContainer';
import {
    ZaiConfig,
    ZaiKeyEntry,
    ZaiKeyMode,
    ZaiKeyRuntimeStatus,
    ZaiKeyStatusView,
    ZaiProvider,
    ZcodeCaptchaConfig,
    ZcodeClaimPlan,
    ZcodeClaimResult,
    ZcodeOauthFlow,
    ZcodePollCommandResult,
} from '../../types/config';

interface Props {
    zai?: ZaiConfig;
    onChange: (updates: Partial<ZaiConfig>) => void;
    upstreamProxy?: unknown;
    requestTimeout?: number;
}

const DEFAULT_ZAI: ZaiConfig = {
    enabled: false,
    base_url: 'https://api.z.ai/api/anthropic',
    api_key: '',
    dispatch_mode: 'off',
    models: { opus: '', sonnet: '', haiku: '' },
    mcp: { enabled: false, web_search_enabled: false, web_reader_enabled: false, vision_enabled: false },
};

const STATUS_BADGE: Record<ZaiKeyRuntimeStatus, string> = {
    Active: 'badge-success',
    Invalid: 'badge-error',
    Exhausted: 'badge-warning',
    RateLimited: 'badge-info',
    Cooldown: 'badge-ghost',
    CaptchaNeeded: 'badge-warning',
};

// [zcode T1] 迁移显示：keys 为空且存在遗留单 api_key 时，按 base_url 推断 provider 物化为单条目
function inferProvider(baseUrl?: string): ZaiProvider {
    return (baseUrl || '').toLowerCase().includes('open.bigmodel.cn') ? 'bigmodel' : 'zai';
}

const entryMode = (e: ZaiKeyEntry): ZaiKeyMode => e.mode || 'api_key';
const entryIdentity = (e: ZaiKeyEntry) => `${entryMode(e)}:${e.key}`;

// [zcode T2] 手动导入判别（与后端 detect_imported_credential 同语义）：
// 3 段点分 → 订阅 JWT（Plan 通道）；单点两段 → id.secret 形态 API Key
function detectImported(raw: string): ZaiKeyMode | null {
    const trimmed = raw.trim();
    if (!trimmed || /\s/.test(trimmed)) return null;
    const segments = trimmed.split('.');
    if (segments.length === 3 && segments.every(Boolean)) return 'jwt';
    if (segments.length === 2 && segments.every(Boolean)) return 'api_key';
    return null;
}

import { loadCaptchaSdk } from '../../utils/captcha';

/**
 * z.ai / BigModel API Key 池编辑器（zcode T1/T2/T3）。
 * - 每个 Key 一行：启停 pill + 上游家族（仅 API Key 行）+ Key + 运行状态徽标；
 * - zcode T2：OAuth 免密登录（CLI 流程，自动开通订阅 API Key 入池）、
 *   手动导入自动判别 JWT / API Key、Plan JWT 专属操作；
 * - zcode T3：端内无痕自动过码、Plan 额度查询、限时套餐领取（preview + claim）；
 * - 保存时同步遗留 api_key 字段（= 首个可用 API Key 条目，跳过 JWT 行），保持向后兼容；
 * - 状态徽标来自 Key 池运行时（内存态），随保存/手动刷新拉取。
 */
export const ZaiKeyPoolEditor = ({ zai: zaiProp, onChange, upstreamProxy, requestTimeout }: Props) => {
    const { t } = useTranslation();
    const zai = zaiProp || DEFAULT_ZAI;
    const [statuses, setStatuses] = useState<ZaiKeyStatusView[]>([]);
    const [loadingStatus, setLoadingStatus] = useState(false);
    const [oauthWaiting, setOauthWaiting] = useState(false);
    const [importValue, setImportValue] = useState('');

    // [zcode T3] 验证码与活动套餐状态
    const [captchaSolvingKey, setCaptchaSolvingKey] = useState<string | null>(null);
    const [claimTarget, setClaimTarget] = useState<ZaiKeyEntry | null>(null);
    const [claimPlans, setClaimPlans] = useState<ZcodeClaimPlan[]>([]);
    const [claimLoading, setClaimLoading] = useState(false);
    const [claimingId, setClaimingId] = useState<string | null>(null);

    // [zcode 额度详情弹窗状态]
    const [quotaModalOpen, setQuotaModalOpen] = useState(false);
    const [quotaTargetIndex, setQuotaTargetIndex] = useState<number>(0);
    const [quotaLoading, setQuotaLoading] = useState(false);
    const [quotaError, setQuotaError] = useState<string | null>(null);
    const [quotaResult, setQuotaResult] = useState<{
        planData?: unknown;
        subData?: unknown;
        account?: ZaiKeyEntry;
    } | null>(null);

    const captchaInstanceRef = useRef<{ verify?: () => void } | null>(null);
    const verifyCallbackRef = useRef<((param: string) => Promise<{ captchaResult: boolean; bizResult?: boolean }>) | null>(null);
    const lastCaptchaParamRef = useRef<{ param: string; region: string } | null>(null);
    const autoSolvedIdsRef = useRef<Set<string>>(new Set());

    // OAuth 轮询期间引用最新 keys，避免闭包过期
    const keysRef = useRef<ZaiKeyEntry[]>([]);
    const emitRef = useRef<(next: ZaiKeyEntry[]) => void>(() => {});

    const keys: ZaiKeyEntry[] = useMemo(() => {
        if (zai.keys && zai.keys.length > 0) return zai.keys;
        const legacy = (zai.api_key || '').trim();
        if (legacy) {
            return [{ key: legacy, provider: inferProvider(zai.base_url), enabled: true }];
        }
        return [];
    }, [zai.keys, zai.api_key, zai.base_url]);

    const emit = useCallback(
        (next: ZaiKeyEntry[]) => {
            // 遗留 api_key 字段只同步 API Key 条目（JWT 不能用于 api.z.ai 端点）
            const firstUsable = next.find(
                (k) => k.enabled && k.key.trim() && entryMode(k) === 'api_key'
            );
            onChange({ keys: next, api_key: firstUsable ? firstUsable.key : '' });
        },
        [onChange]
    );

    keysRef.current = keys;
    emitRef.current = emit;

    const refreshStatus = useCallback(async () => {
        try {
            setLoadingStatus(true);
            const views = await invoke<ZaiKeyStatusView[]>('get_zai_key_pool_status');
            setStatuses(Array.isArray(views) ? views : []);
        } catch {
            setStatuses([]);
        } finally {
            setLoadingStatus(false);
        }
    }, []);

    useEffect(() => {
        refreshStatus();
    }, [refreshStatus]);

    // 保存后延迟刷新状态徽标（等待后端池完成指纹同步）
    const signature = JSON.stringify(keys);
    useEffect(() => {
        const timer = setTimeout(() => {
            refreshStatus();
        }, 1000);
        return () => clearTimeout(timer);
    }, [signature, refreshStatus]);

    // [zcode T3] 端内无痕验证码求解函数
    const solveCaptcha = useCallback(
        async (entry: ZaiKeyEntry, silent = false): Promise<boolean> => {
            const id = entryIdentity(entry);
            if (captchaSolvingKey) return false;
            setCaptchaSolvingKey(id);
            if (!silent) {
                showToast(t('proxy.config.zai.keys.captcha_solving'), 'info', 3000);
            }
            return new Promise<boolean>(async (resolve) => {
                let resolved = false;
                const finish = (ok: boolean) => {
                    if (resolved) return;
                    resolved = true;
                    setCaptchaSolvingKey(null);
                    resolve(ok);
                };

                const timeout = setTimeout(() => {
                    finish(false);
                }, 60_000);

                try {
                    const cfg = await invoke<ZcodeCaptchaConfig>('zcode_captcha_config', {
                        upstreamProxy,
                        requestTimeout,
                    });
                    if (cfg.enabled === false) {
                        clearTimeout(timeout);
                        finish(false);
                        return;
                    }
                    await loadCaptchaSdk();

                    verifyCallbackRef.current = async (param: string) => {
                        clearTimeout(timeout);
                        try {
                            await invoke('zcode_captcha_submit', {
                                accountId: entry.account_id || '',
                                key: entry.key,
                                verifyParam: param,
                                region: cfg.region,
                            });
                            lastCaptchaParamRef.current = { param, region: cfg.region };
                            refreshStatus();
                            finish(true);
                            if (!silent) {
                                showToast(t('proxy.config.zai.keys.captcha_ok'), 'success');
                            }
                            return { captchaResult: true, bizResult: true };
                        } catch {
                            finish(false);
                            if (!silent) {
                                showToast(t('proxy.config.zai.keys.captcha_failed'), 'error');
                            }
                            return { captchaResult: false, bizResult: false };
                        }
                    };

                    const globalWindow = window as unknown as {
                        initAliyunCaptcha?: (opts: Record<string, unknown>) => void;
                    };

                    if (captchaInstanceRef.current?.verify) {
                        captchaInstanceRef.current.verify();
                    } else if (globalWindow.initAliyunCaptcha) {
                        globalWindow.initAliyunCaptcha({
                            SceneId: cfg.scene_id,
                            prefix: cfg.prefix,
                            mode: 'popup',
                            element: '#zcode-captcha-element',
                            button: '#zcode-captcha-button',
                            captchaVerifyCallback: async (param: string) => {
                                if (verifyCallbackRef.current) {
                                    return await verifyCallbackRef.current(param);
                                }
                                return { captchaResult: false };
                            },
                            onBizResultCallback: () => {},
                            getInstance: (instance: { verify?: () => void }) => {
                                captchaInstanceRef.current = instance;
                                try {
                                    instance?.verify?.();
                                } catch {
                                    document.getElementById('zcode-captcha-button')?.click();
                                }
                            },
                        });
                        setTimeout(() => {
                            document.getElementById('zcode-captcha-button')?.click();
                        }, 300);
                    } else {
                        clearTimeout(timeout);
                        finish(false);
                    }
                } catch {
                    clearTimeout(timeout);
                    finish(false);
                }
            });
        },
        [captchaSolvingKey, upstreamProxy, requestTimeout, refreshStatus, t]
    );

    // [zcode T3] 监测到 CaptchaNeeded 状态时，自动触发一次无痕求解
    useEffect(() => {
        const needy = statuses.find(
            (s) => s.status === 'CaptchaNeeded' && s.mode === 'jwt' && !autoSolvedIdsRef.current.has(`${s.mode}:${s.account_id || s.index}`)
        );
        if (needy && !captchaSolvingKey) {
            const entry = keys[needy.index];
            if (entry) {
                const autoKey = `${entryMode(entry)}:${entry.account_id || needy.index}`;
                autoSolvedIdsRef.current.add(autoKey);
                solveCaptcha(entry, true);
            }
        }
    }, [statuses, keys, captchaSolvingKey, solveCaptcha]);

    const updateRow = (idx: number, patch: Partial<ZaiKeyEntry>) => {
        emit(keys.map((k, i) => (i === idx ? { ...k, ...patch } : k)));
    };

    const removeRow = (idx: number) => {
        emit(keys.filter((_, i) => i !== idx));
    };

    // [zcode T2] 手动导入：粘贴即判别（JWT / API Key），无法识别时报错
    const importCredential = () => {
        const mode = detectImported(importValue);
        if (!mode) {
            showToast(t('proxy.config.zai.keys.import_invalid'), 'error');
            return;
        }
        const entry: ZaiKeyEntry =
            mode === 'jwt'
                ? {
                      key: importValue.trim(),
                      provider: 'zcode_plan',
                      enabled: true,
                      mode: 'jwt',
                      label: t('proxy.config.zai.keys.manual_jwt_label'),
                  }
                : {
                      key: importValue.trim(),
                      provider: inferProvider(zai.base_url),
                      enabled: true,
                      mode: 'api_key',
                  };
        const exists = keysRef.current.some((k) => entryIdentity(k) === entryIdentity(entry));
        if (exists) {
            showToast(t('proxy.config.zai.keys.import_duplicate'), 'warning');
            return;
        }
        emitRef.current([...keysRef.current, entry]);
        setImportValue('');
        showToast(t('proxy.config.zai.keys.import_ok'), 'success');
    };

    // [zcode T2] OAuth 免密登录（CLI 流程）：打开授权页 → 有界轮询 → 产物入池
    const startOauth = async () => {
        if (oauthWaiting) return;
        try {
            const flow = await invoke<ZcodeOauthFlow>('zcode_oauth_start', {
                upstreamProxy,
                requestTimeout,
            });
            try {
                const { openUrl } = await import('@tauri-apps/plugin-opener');
                await openUrl(flow.authorize_url);
            } catch {
                window.open(flow.authorize_url, '_blank', 'noopener,noreferrer');
            }
            setOauthWaiting(true);
            showToast(t('proxy.config.zai.keys.oauth_waiting'), 'info', 6000);
            const deadline = (flow.expires_at_ms || Date.now() + 300_000) - 1500;
            const intervalMs = Math.max(1, flow.poll_interval_secs || 2) * 1000;
            const timer = setInterval(async () => {
                if (Date.now() >= deadline) {
                    clearInterval(timer);
                    setOauthWaiting(false);
                    showToast(t('proxy.config.zai.keys.oauth_expired'), 'error');
                    return;
                }
                try {
                    const r = await invoke<ZcodePollCommandResult>('zcode_oauth_poll', {
                        flow,
                        upstreamProxy,
                        requestTimeout,
                    });
                    if (r.status === 'pending') return;
                    clearInterval(timer);
                    setOauthWaiting(false);
                    if (r.status === 'expired') {
                        showToast(t('proxy.config.zai.keys.oauth_expired'), 'error');
                        return;
                    }
                    const incoming = r.entries || [];
                    const existing = new Set(keysRef.current.map(entryIdentity));
                    const fresh = incoming.filter((e) => !existing.has(entryIdentity(e)));
                    if (fresh.length > 0) {
                        emitRef.current([...keysRef.current, ...fresh]);
                    }
                    if (r.warning) {
                        showToast(r.warning, 'warning', 8000);
                    } else {
                        showToast(t('proxy.config.zai.keys.oauth_success'), 'success');
                    }
                } catch (e) {
                    clearInterval(timer);
                    setOauthWaiting(false);
                    showToast(String(e), 'error', 8000);
                }
            }, intervalMs);
        } catch (e) {
            setOauthWaiting(false);
            showToast(String(e), 'error', 8000);
        }
    };

    // [zcode 额度查询] 获取账号配额与订阅详情（智能配对 JWT 与 API Key）
    const fetchQuotaData = async (target: ZaiKeyEntry) => {
        setQuotaLoading(true);
        setQuotaError(null);
        try {
            let planData: unknown = null;
            let subData: unknown = null;

            // 1. JWT 条目：优先查询 plan_balance；若有关联的 business_jwt 则同时查订阅
            if (entryMode(target) === 'jwt') {
                try {
                    planData = await invoke('zcode_plan_quota', {
                        zcodeJwt: target.key,
                        deviceProfile: target.device_profile ?? null,
                        upstreamProxy,
                        requestTimeout,
                    });
                } catch (err: unknown) {
                    console.warn('plan_quota error:', err);
                }
                const paired = keys.find(
                    (k) => k.account_id && k.account_id === target.account_id && k.business_jwt
                );
                const bizJwt = target.business_jwt || paired?.business_jwt;
                if (bizJwt) {
                    try {
                        subData = await invoke('zcode_query_quota', {
                            businessJwt: bizJwt,
                            upstreamProxy,
                            requestTimeout,
                        });
                    } catch (err: unknown) {
                        console.warn('sub_quota error:', err);
                    }
                }
            } else {
                // 2. API Key 条目：若存在同 account_id 的 JWT 则查 Plan 额度；若自身或关联条目有 business_jwt 则查订阅
                const pairedJwt = keys.find(
                    (k) => k.account_id && k.account_id === target.account_id && entryMode(k) === 'jwt'
                );
                if (pairedJwt) {
                    try {
                        planData = await invoke('zcode_plan_quota', {
                            zcodeJwt: pairedJwt.key,
                            deviceProfile: pairedJwt.device_profile ?? null,
                            upstreamProxy,
                            requestTimeout,
                        });
                    } catch (err: unknown) {
                        console.warn('plan_quota error from paired jwt:', err);
                    }
                }
                const bizJwt = target.business_jwt || pairedJwt?.business_jwt;
                if (bizJwt) {
                    try {
                        subData = await invoke('zcode_query_quota', {
                            businessJwt: bizJwt,
                            upstreamProxy,
                            requestTimeout,
                        });
                    } catch (err: unknown) {
                        console.warn('sub_quota error:', err);
                    }
                }
            }

            if (!planData && !subData) {
                throw new Error(
                    t('proxy.config.zai.keys.quota_fetch_failed', {
                        defaultValue: '未能获取到额度信息（该账号可能为手动添加的普通 API Key，无订阅查询权限）',
                    })
                );
            }

            setQuotaResult({ planData, subData, account: target });
        } catch (e: unknown) {
            setQuotaError(e instanceof Error ? e.message : String(e));
        } finally {
            setQuotaLoading(false);
        }
    };

    const openQuotaModal = (entry?: ZaiKeyEntry) => {
        let idx = 0;
        if (entry) {
            const foundIdx = keys.findIndex((k) => entryIdentity(k) === entryIdentity(entry));
            if (foundIdx >= 0) idx = foundIdx;
        }
        setQuotaTargetIndex(idx);
        setQuotaModalOpen(true);
        const target = entry || keys[idx];
        if (target) {
            fetchQuotaData(target);
        }
    };

    // [zcode T3] 打开活动套餐领取面板
    const openClaimModal = async (entry: ZaiKeyEntry) => {
        setClaimTarget(entry);
        setClaimLoading(true);
        setClaimPlans([]);
        try {
            const plans = await invoke<ZcodeClaimPlan[]>('zcode_claim_preview', {
                zcodeJwt: entry.key,
                deviceProfile: entry.device_profile ?? null,
                upstreamProxy,
                requestTimeout,
            });
            setClaimPlans(plans || []);
        } catch (e) {
            showToast(String(e), 'error');
        } finally {
            setClaimLoading(false);
        }
    };

    // [zcode T3] 执行单项活动套餐领取
    const executeClaim = async (plan: ZcodeClaimPlan) => {
        if (!claimTarget) return;
        setClaimingId(plan.plan_id);
        try {
            let param = lastCaptchaParamRef.current?.param;
            let reg = lastCaptchaParamRef.current?.region || 'cn';
            if (!param) {
                const ok = await solveCaptcha(claimTarget);
                if (!ok) {
                    showToast(t('proxy.config.zai.keys.claim_need_captcha'), 'error');
                    return;
                }
                param = lastCaptchaParamRef.current?.param;
                reg = lastCaptchaParamRef.current?.region || 'cn';
            }
            const res = await invoke<ZcodeClaimResult>('zcode_claim', {
                zcodeJwt: claimTarget.key,
                deviceProfile: claimTarget.device_profile ?? null,
                planId: plan.plan_id,
                verifyParam: param || '',
                region: reg,
                upstreamProxy,
                requestTimeout,
            });
            if (res.ok) {
                showToast(t('proxy.config.zai.keys.claim_ok', { name: plan.name || plan.plan_id }), 'success');
                openClaimModal(claimTarget);
            } else if (res.code === 1003) {
                showToast(t('proxy.config.zai.keys.claim_already'), 'warning');
            } else if (res.code === 1005) {
                const nextAtStr = res.next_at_ms ? ` (${new Date(res.next_at_ms).toLocaleTimeString()})` : '';
                showToast(t('proxy.config.zai.keys.claim_exhausted', { nextAt: nextAtStr }), 'warning');
            } else if (res.code === 3007) {
                lastCaptchaParamRef.current = null;
                showToast(t('proxy.config.zai.keys.claim_captcha_failed'), 'error');
            } else {
                showToast(res.message || t('proxy.config.zai.keys.claim_failed'), 'error');
            }
        } catch (e) {
            showToast(String(e), 'error');
        } finally {
            setClaimingId(null);
        }
    };

    return (
        <div className="space-y-2">
            {/* 隐藏的阿里云验证码容器与触发元素 */}
            <div id="zcode-captcha-element" style={{ position: 'fixed', right: 16, bottom: 16, zIndex: 99999 }} />
            <button id="zcode-captcha-button" type="button" style={{ display: 'none' }} />

            <div className="flex items-center justify-between">
                <label className="text-[11px] font-medium text-gray-500 dark:text-gray-400 flex items-center gap-1">
                    <span>{t('proxy.config.zai.keys.title')}</span>
                    <HelpTooltip text={t('proxy.config.zai.keys.title_tooltip')} iconSize={12} />
                </label>
                <div className="flex items-center gap-1">
                    <button
                        className="btn btn-ghost btn-xs gap-1"
                        onClick={refreshStatus}
                        disabled={loadingStatus}
                    >
                        <RefreshCw size={12} className={loadingStatus ? 'animate-spin' : ''} />
                        {t('proxy.config.zai.keys.refresh_status')}
                    </button>
                    <button
                        className="btn btn-ghost btn-xs gap-1 text-amber-600 dark:text-amber-400"
                        onClick={() => openQuotaModal()}
                        disabled={keys.length === 0}
                        title={t('proxy.config.zai.keys.quota_query_all')}
                    >
                        <Coins size={12} />
                        {t('proxy.config.zai.keys.quota_query_btn')}
                    </button>
                    <button
                        className="btn btn-ghost btn-xs gap-1 text-primary"
                        onClick={startOauth}
                        disabled={oauthWaiting}
                        title={t('proxy.config.zai.keys.oauth_tooltip')}
                    >
                        <KeyRound size={12} className={oauthWaiting ? 'animate-pulse' : ''} />
                        {oauthWaiting
                            ? t('proxy.config.zai.keys.oauth_waiting_short')
                            : t('proxy.config.zai.keys.oauth_login')}
                    </button>
                </div>
            </div>

            {/* 手动导入：粘贴即判别 */}
            <div className="flex items-center gap-1">
                <input
                    type="text"
                    className="input input-xs input-bordered flex-1 font-mono"
                    placeholder={t('proxy.config.zai.keys.import_placeholder')}
                    value={importValue}
                    onChange={(e) => setImportValue(e.target.value)}
                    onKeyDown={(e) => {
                        if (e.key === 'Enter') importCredential();
                    }}
                />
                <button className="btn btn-ghost btn-xs gap-1" onClick={importCredential}>
                    <Plus size={12} />
                    {t('proxy.config.zai.keys.import')}
                </button>
            </div>

            {keys.length === 0 && (
                <div className="text-[11px] text-gray-400 italic">
                    {t('proxy.config.zai.keys.empty_hint')}
                </div>
            )}

            {keys.map((entry, idx) => {
                const st = statuses.find((s) => s.index === idx);
                const isJwt = entryMode(entry) === 'jwt';
                const id = entryIdentity(entry);
                const isSolving = captchaSolvingKey === id;
                return (
                    <div key={idx} className="flex items-center gap-2">
                        <input
                            type="checkbox"
                            className="toggle toggle-xs toggle-success"
                            checked={entry.enabled}
                            title={t('proxy.config.zai.enabled')}
                            onChange={(e) => updateRow(idx, { enabled: e.target.checked })}
                        />
                        {isJwt ? (
                            <span
                                className="badge badge-xs badge-outline whitespace-nowrap"
                                title={t('proxy.config.zai.keys.mode_jwt_tooltip')}
                            >
                                {t('proxy.config.zai.keys.mode_jwt_badge')}
                            </span>
                        ) : (
                            <select
                                className="select select-xs select-bordered max-w-[110px]"
                                value={entry.provider}
                                onChange={(e) => updateRow(idx, { provider: e.target.value as ZaiProvider })}
                            >
                                <option value="zai">{t('proxy.config.zai.keys.provider_zai')}</option>
                                <option value="bigmodel">{t('proxy.config.zai.keys.provider_bigmodel')}</option>
                            </select>
                        )}
                        <input
                            type="password"
                            className="input input-xs input-bordered flex-1 font-mono"
                            value={entry.key}
                            placeholder={isJwt ? 'eyJhbGci...' : 'sk-...'}
                            onChange={(e) => updateRow(idx, { key: e.target.value })}
                        />
                        {isJwt && (
                            <>
                                <button
                                    className="btn btn-ghost btn-xs gap-1 text-emerald-600 dark:text-emerald-400"
                                    disabled={isSolving}
                                    title={t('proxy.config.zai.keys.captcha_solve')}
                                    onClick={() => solveCaptcha(entry)}
                                >
                                    <ShieldCheck size={12} className={isSolving ? 'animate-pulse' : ''} />
                                </button>
                                <button
                                    className="btn btn-ghost btn-xs gap-1 text-amber-600 dark:text-amber-400"
                                    title={t('proxy.config.zai.keys.claim')}
                                    onClick={() => openClaimModal(entry)}
                                >
                                    <Gift size={12} />
                                </button>
                            </>
                        )}
                        <button
                            className="btn btn-ghost btn-xs gap-1 text-amber-600 dark:text-amber-400"
                            title={t('proxy.config.zai.keys.quota_query')}
                            onClick={() => openQuotaModal(entry)}
                        >
                            <Coins size={12} />
                            <span className="text-[10px] hidden sm:inline">{t('proxy.config.zai.keys.quota_short')}</span>
                        </button>
                        {st && (
                            <span
                                className={`badge badge-xs whitespace-nowrap ${STATUS_BADGE[st.status] || 'badge-ghost'}`}
                                title={st.last_error || undefined}
                            >
                                {t(`proxy.config.zai.keys.status_${st.status.toLowerCase()}`)}
                                {st.status_remaining_secs != null
                                    ? ` ${st.status_remaining_secs}s`
                                    : ''}
                            </span>
                        )}
                        <button
                            className="btn btn-ghost btn-xs text-red-500"
                            onClick={() => removeRow(idx)}
                            title={t('common.delete')}
                        >
                            <Trash2 size={12} />
                        </button>
                    </div>
                );
            })}

            {/* [zcode T3] 限时活动套餐领取弹窗 */}
            {claimTarget && (
                <div className="modal modal-open">
                    <div className="modal-box relative max-w-md bg-white dark:bg-base-100 border border-base-300 shadow-2xl p-5">
                        <div className="flex items-center justify-between pb-3 border-b border-base-200">
                            <h3 className="text-sm font-semibold flex items-center gap-1.5">
                                <Gift size={16} className="text-amber-500" />
                                <span>{t('proxy.config.zai.keys.claim_title')}</span>
                            </h3>
                            <button
                                className="btn btn-ghost btn-xs btn-circle"
                                onClick={() => setClaimTarget(null)}
                            >
                                <X size={14} />
                            </button>
                        </div>
                        <div className="py-3 min-h-[120px] max-h-[320px] overflow-y-auto space-y-2">
                            {claimLoading ? (
                                <div className="flex items-center justify-center h-24 text-xs text-gray-400">
                                    <RefreshCw size={14} className="animate-spin mr-1.5" />
                                    <span>{t('common.loading')}</span>
                                </div>
                            ) : claimPlans.length === 0 ? (
                                <div className="text-xs text-gray-400 italic text-center py-8">
                                    {t('proxy.config.zai.keys.claim_empty')}
                                </div>
                            ) : (
                                claimPlans.map((plan) => (
                                    <div
                                        key={plan.plan_id}
                                        className="flex items-center justify-between p-2.5 rounded-lg border border-base-200 bg-base-50/50 dark:bg-base-200/20"
                                    >
                                        <div className="flex-1 pr-3">
                                            <div className="text-xs font-medium text-gray-800 dark:text-gray-200">
                                                {plan.name || plan.plan_id}
                                            </div>
                                            {plan.description && (
                                                <div className="text-[11px] text-gray-400 mt-0.5 line-clamp-2">
                                                    {plan.description}
                                                </div>
                                            )}
                                        </div>
                                        <button
                                            className="btn btn-primary btn-xs whitespace-nowrap"
                                            disabled={claimingId !== null}
                                            onClick={() => executeClaim(plan)}
                                        >
                                            {claimingId === plan.plan_id ? (
                                                <>
                                                    <RefreshCw size={11} className="animate-spin" />
                                                    {t('proxy.config.zai.keys.claim_claiming')}
                                                </>
                                            ) : (
                                                t('proxy.config.zai.keys.claim_action')
                                            )}
                                        </button>
                                    </div>
                                ))
                            )}
                        </div>
                    </div>
                    <div className="modal-backdrop bg-black/40" onClick={() => setClaimTarget(null)} />
                </div>
            )}

            {/* [zcode] 账号额度与配额详情弹窗 */}
            {quotaModalOpen && (
                <div className="modal modal-open">
                    <div className="modal-box relative max-w-lg bg-white dark:bg-base-100 border border-base-300 shadow-2xl p-5">
                        <div className="flex items-center justify-between pb-3 border-b border-base-200">
                            <h3 className="text-sm font-semibold flex items-center gap-1.5">
                                <Coins size={16} className="text-amber-500" />
                                <span>{t('proxy.config.zai.keys.quota_modal_title')}</span>
                            </h3>
                            <div className="flex items-center gap-1">
                                <button
                                    className="btn btn-ghost btn-xs btn-circle"
                                    title={t('proxy.config.zai.keys.quota_refresh')}
                                    disabled={quotaLoading}
                                    onClick={() => {
                                        const target = keys[quotaTargetIndex];
                                        if (target) fetchQuotaData(target);
                                    }}
                                >
                                    <RefreshCw size={13} className={quotaLoading ? 'animate-spin' : ''} />
                                </button>
                                <button
                                    className="btn btn-ghost btn-xs btn-circle"
                                    onClick={() => {
                                        setQuotaModalOpen(false);
                                        setQuotaResult(null);
                                    }}
                                >
                                    <X size={14} />
                                </button>
                            </div>
                        </div>

                        {/* 多账号切换器 */}
                        {keys.length > 1 && (
                            <div className="pt-3 pb-1">
                                <label className="text-[11px] text-gray-400 mb-1 block">
                                    {t('proxy.config.zai.keys.quota_account')}
                                </label>
                                <select
                                    className="select select-xs select-bordered w-full font-mono text-xs"
                                    value={quotaTargetIndex}
                                    onChange={(e) => {
                                        const newIdx = Number(e.target.value);
                                        setQuotaTargetIndex(newIdx);
                                        const target = keys[newIdx];
                                        if (target) fetchQuotaData(target);
                                    }}
                                >
                                    {keys.map((k, i) => (
                                        <option key={i} value={i}>
                                            #{i + 1} {k.user_email || k.account_id || k.label || (entryMode(k) === 'jwt' ? 'Plan JWT' : 'API Key')} ({k.key.slice(0, 10)}...)
                                        </option>
                                    ))}
                                </select>
                            </div>
                        )}

                        <div className="py-3 min-h-[140px] max-h-[380px] overflow-y-auto space-y-3">
                            {quotaLoading ? (
                                <div className="flex flex-col items-center justify-center h-32 text-xs text-gray-400 gap-2">
                                    <RefreshCw size={18} className="animate-spin text-amber-500" />
                                    <span>{t('common.loading')}</span>
                                </div>
                            ) : quotaError ? (
                                <div className="p-3 bg-red-50 dark:bg-red-950/30 border border-red-200 dark:border-red-900 rounded-lg text-xs text-red-600 dark:text-red-400">
                                    {quotaError}
                                </div>
                            ) : quotaResult ? (
                                (() => {
                                    const balances = parseBalances(quotaResult.planData);
                                    const subs = parseSubscriptions(quotaResult.subData);
                                    return (
                                        <div className="space-y-4">
                                            {/* Coding Plan 余额 */}
                                            {balances.length > 0 && (
                                                <div className="space-y-2">
                                                    <h4 className="text-[11px] font-bold text-gray-400 uppercase tracking-widest">
                                                        {t('proxy.config.zai.keys.quota_plan_section')}
                                                    </h4>
                                                    <div className="space-y-2">
                                                        {balances.map((b, i) => (
                                                            <div
                                                                key={i}
                                                                className="p-3 rounded-xl border border-base-200 bg-base-50/50 dark:bg-base-200/30 space-y-2"
                                                            >
                                                                <div className="flex items-center justify-between">
                                                                    <div className="flex items-center gap-1.5 font-medium text-xs text-gray-800 dark:text-gray-200">
                                                                        <Sparkles size={13} className="text-amber-500" />
                                                                        <span>{b.name}</span>
                                                                    </div>
                                                                    <span className="badge badge-sm badge-ghost text-[10px] font-mono font-medium">
                                                                        {b.remaining.toLocaleString()} / {b.total.toLocaleString()}
                                                                    </span>
                                                                </div>
                                                                <div className="w-full bg-gray-200 dark:bg-base-300 rounded-full h-1.5 overflow-hidden">
                                                                    <div
                                                                        className={`h-full transition-all duration-300 ${
                                                                            b.percent > 50
                                                                                ? 'bg-emerald-500'
                                                                                : b.percent > 20
                                                                                  ? 'bg-amber-500'
                                                                                  : 'bg-rose-500'
                                                                        }`}
                                                                        style={{ width: `${b.percent}%` }}
                                                                    />
                                                                </div>
                                                                <div className="flex items-center justify-between text-[10px] text-gray-400">
                                                                    <span>
                                                                        {t('proxy.config.zai.keys.quota_remaining')}: {b.percent}%
                                                                    </span>
                                                                    {b.expiresAt && (
                                                                        <span>
                                                                            {t('proxy.config.zai.keys.quota_expires')}: {b.expiresAt}
                                                                        </span>
                                                                    )}
                                                                </div>
                                                            </div>
                                                        ))}
                                                    </div>
                                                </div>
                                            )}

                                            {/* Subscription 套餐 */}
                                            {subs.length > 0 && (
                                                <div className="space-y-2">
                                                    <h4 className="text-[11px] font-bold text-gray-400 uppercase tracking-widest">
                                                        {t('proxy.config.zai.keys.quota_sub_section')}
                                                    </h4>
                                                    <div className="space-y-1.5">
                                                        {subs.map((s, i) => (
                                                            <div
                                                                key={i}
                                                                className="p-2.5 rounded-lg border border-base-200 bg-base-50/50 dark:bg-base-200/20 flex items-center justify-between text-xs"
                                                            >
                                                                <div>
                                                                    <span className="font-medium text-gray-800 dark:text-gray-200">
                                                                        {s.planName}
                                                                    </span>
                                                                    {s.expireTime && (
                                                                        <span className="text-[10px] text-gray-400 block">
                                                                            {t('proxy.config.zai.keys.quota_expires')}: {s.expireTime}
                                                                        </span>
                                                                    )}
                                                                </div>
                                                                <span
                                                                    className={`badge badge-xs ${
                                                                        s.status.toLowerCase() === 'active'
                                                                            ? 'badge-success'
                                                                            : 'badge-ghost'
                                                                    }`}
                                                                >
                                                                    {s.status}
                                                                </span>
                                                            </div>
                                                        ))}
                                                    </div>
                                                </div>
                                            )}

                                            {balances.length === 0 && subs.length === 0 && (
                                                <div className="text-xs text-gray-400 italic text-center py-6">
                                                    {t('proxy.config.zai.keys.quota_empty')}
                                                </div>
                                            )}

                                            {/* Collapsible raw details */}
                                            <details className="group">
                                                <summary className="cursor-pointer text-[10px] text-gray-400 hover:text-gray-600 transition-colors">
                                                    Raw JSON
                                                </summary>
                                                <pre className="mt-1 p-2 bg-gray-50 dark:bg-base-300 rounded text-[9px] font-mono overflow-x-auto max-h-32">
                                                    {JSON.stringify(quotaResult, null, 2)}
                                                </pre>
                                            </details>
                                        </div>
                                    );
                                })()
                            ) : (
                                <div className="text-xs text-gray-400 italic text-center py-6">
                                    {t('proxy.config.zai.keys.quota_empty')}
                                </div>
                            )}
                        </div>
                    </div>
                    <div
                        className="modal-backdrop bg-black/40"
                        onClick={() => {
                            setQuotaModalOpen(false);
                            setQuotaResult(null);
                        }}
                    />
                </div>
            )}
        </div>
    );
};

interface ParsedBalance {
    name: string;
    model?: string;
    total: number;
    used: number;
    remaining: number;
    percent: number;
    expiresAt?: string;
}

function parseBalances(data: unknown): ParsedBalance[] {
    if (!data || typeof data !== 'object') return [];
    const obj = data as Record<string, unknown>;
    const rawList: unknown[] = Array.isArray(obj.balances)
        ? (obj.balances as unknown[])
        : Array.isArray(data)
          ? (data as unknown[])
          : [];

    const result: ParsedBalance[] = [];
    for (const item of rawList) {
        if (!item || typeof item !== 'object') continue;
        const b = item as Record<string, unknown>;
        const name = String(b.show_name || b.showName || b.model || b.plan_name || 'Coding Plan');
        const model = b.model ? String(b.model) : undefined;
        const total = Number(b.total_units ?? b.totalUnits ?? b.total ?? 0);
        const remaining = Number(b.remaining_units ?? b.remainingUnits ?? b.remaining ?? b.balance ?? 0);
        const used = Number(b.used_units ?? b.usedUnits ?? b.used ?? Math.max(0, total - remaining));
        const percent = total > 0 ? Math.min(100, Math.max(0, Math.round((remaining / total) * 100))) : 0;

        let expiresAt: string | undefined;
        const exp = b.expires_at ?? b.expire_time ?? b.expireTime ?? b.expired_at;
        if (typeof exp === 'number') {
            const ms = exp < 1e11 ? exp * 1000 : exp;
            expiresAt = new Date(ms).toLocaleString();
        } else if (typeof exp === 'string' && exp.trim()) {
            expiresAt = exp;
        }

        result.push({ name, model, total, used, remaining, percent, expiresAt });
    }
    return result;
}

interface ParsedSub {
    planName: string;
    status: string;
    expireTime?: string;
}

function parseSubscriptions(data: unknown): ParsedSub[] {
    if (!data) return [];
    const list: unknown[] = Array.isArray(data)
        ? (data as unknown[])
        : typeof data === 'object' && Array.isArray((data as Record<string, unknown>).list)
          ? ((data as Record<string, unknown>).list as unknown[])
          : [data];

    const result: ParsedSub[] = [];
    for (const item of list) {
        if (!item || typeof item !== 'object') continue;
        const s = item as Record<string, unknown>;
        const planName = String(s.planName || s.plan_name || s.product_name || s.name || '订阅套餐');
        const status = String(s.status || s.state || 'active');
        const expireTime =
            s.expireTime || s.expire_time || s.expiredAt || s.valid_until
                ? String(s.expireTime || s.expire_time || s.expiredAt || s.valid_until)
                : undefined;
        result.push({ planName, status, expireTime });
    }
    return result;
}
