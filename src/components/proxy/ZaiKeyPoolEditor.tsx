import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import {
    AlertTriangle,
    ChevronUp,
    Coins,
    Gift,
    KeyRound,
    Pencil,
    Plus,
    RefreshCw,
    ShieldCheck,
    Sparkles,
    Trash2,
    Users,
    X,
} from 'lucide-react';
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

// [zcode auto-claim] 每日定时领取缺省值（与后端 AutoClaimConfig 默认对齐：启用 + 本地 00:00）
export const DEFAULT_AUTO_CLAIM = { enabled: true, time: '00:00' };

export const DEFAULT_ZAI: ZaiConfig = {
    enabled: false,
    base_url: 'https://api.z.ai/api/anthropic',
    api_key: '',
    models: { opus: '', sonnet: '', haiku: '' },
    mcp: { enabled: false, web_search_enabled: false, web_reader_enabled: false, vision_enabled: false },
    auto_claim: DEFAULT_AUTO_CLAIM,
};

const STATUS_BADGE: Record<ZaiKeyRuntimeStatus, string> = {
    Active: 'badge-success',
    Invalid: 'badge-error',
    Exhausted: 'badge-warning',
    RateLimited: 'badge-info',
    Cooldown: 'badge-ghost',
    CaptchaNeeded: 'badge-warning',
};

const ISSUE_STATUSES: ZaiKeyRuntimeStatus[] = ['Invalid', 'Exhausted', 'CaptchaNeeded'];

// [zcode T1] 迁移显示：keys 为空且存在遗留单 api_key 时，按 base_url 推断 provider 物化为单条目
function inferProvider(baseUrl?: string): ZaiProvider {
    return (baseUrl || '').toLowerCase().includes('open.bigmodel.cn') ? 'bigmodel' : 'zai';
}

const entryMode = (e: ZaiKeyEntry): ZaiKeyMode => e.mode || 'api_key';
const entryIdentity = (e: ZaiKeyEntry) => `${entryMode(e)}:${e.key}`;

// [zcode T4] 卡片折叠态的掩码凭证（短凭证全掩码，长凭证留头尾）
function maskKey(key: string): string {
    if (!key) return '';
    if (key.length <= 14) return `${key.slice(0, 3)}${'•'.repeat(Math.max(0, key.length - 3))}`;
    return `${key.slice(0, 12)}…${key.slice(-4)}`;
}

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

interface StatChipProps {
    icon: typeof Users;
    label: string;
    value: number | string;
    tone: string;
}

// [zcode T4] 池级汇总徽章
const StatChip = ({ icon: Icon, label, value, tone }: StatChipProps) => (
    <div className="flex items-center gap-1.5 rounded-full bg-white dark:bg-base-100 border border-gray-200 dark:border-base-200 px-3 py-1.5 shadow-sm">
        <Icon size={12} className={tone} />
        <span className="text-[10px] text-gray-400 whitespace-nowrap">{label}</span>
        <span className="text-xs font-semibold text-gray-700 dark:text-gray-200 tabular-nums">{value}</span>
    </div>
);

/**
 * z.ai / bigmodel API Key 池编辑器（zcode T1/T2/T3，T4 升格为独立页面主体）。
 * - [T4] 条目以账号卡片栅格呈现：身份（邮箱/标签）+ 掩码凭证 + 状态徽标 + 操作区，
 *   凭证编辑与上游家族选择收进卡片展开态；页首为池级汇总徽章 + 工具栏；
 * - zcode T2：OAuth 免密登录（CLI 流程，自动开通订阅 API Key 入池）、
 *   手动导入自动判别 JWT / API Key、Plan JWT 专属操作；
 * - zcode T3：端内无痕自动过码、Plan 额度查询、限时套餐领取（preview + claim）；
 * - 保存时同步遗留 api_key 字段（= 首个可用 API Key 条目，跳过 JWT 行），保持向后兼容；
 * - 状态徽标来自 Key 池运行时（内存态），随保存/手动刷新拉取。
 */
export const ZaiKeyPoolEditor = ({ zai: zaiProp, onChange, upstreamProxy, requestTimeout }: Props) => {
    const { t } = useTranslation();
    const zai = zaiProp || DEFAULT_ZAI;
    // [zcode auto-claim] 每日定时领取（后端调度器热读取该配置；改动经 onChange 即时保存）
    const autoClaim = zai.auto_claim ?? DEFAULT_AUTO_CLAIM;
    const updateAutoClaim = (patch: Partial<{ enabled: boolean; time: string }>) =>
        onChange({ auto_claim: { ...autoClaim, ...patch } });
    const [statuses, setStatuses] = useState<ZaiKeyStatusView[]>([]);
    const [loadingStatus, setLoadingStatus] = useState(false);
    const [oauthWaiting, setOauthWaiting] = useState(false);
    const [importValue, setImportValue] = useState('');
    // [zcode T4] 展开编辑的条目下标（index 基准：编辑密钥会改变 entryIdentity）
    const [expandedIdx, setExpandedIdx] = useState<number | null>(null);
    // [zcode T4 修订] 「添加账号」子弹窗（对齐 antigravity AddAccountDialog 模式）
    const [addOpen, setAddOpen] = useState(false);
    // [zcode T4 修订] 卡片额度直显缓存（anchor = jwt:<key>，配对 API Key 共享同锚点）
    const [quotaCache, setQuotaCache] = useState<Record<string, CardQuota>>({});
    const quotaCacheRef = useRef<Record<string, CardQuota>>({});
    const quotaFetchingRef = useRef<Set<string>>(new Set());

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
    const lastCaptchaParamRef = useRef<{ param: string; region: string; issuedAt: number } | null>(null);
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

    // [zcode T4] 池级汇总
    const poolStats = useMemo(() => {
        const jwtCount = keys.filter((k) => entryMode(k) === 'jwt').length;
        const hasStatuses = statuses.length > 0;
        const available = keys.reduce((acc, k, i) => {
            const st = statuses.find((s) => s.index === i);
            return acc + (st?.status === 'Active' && k.enabled ? 1 : 0);
        }, 0);
        const issues = statuses.filter((s) => ISSUE_STATUSES.includes(s.status)).length;
        return {
            total: keys.length,
            jwtCount,
            available: hasStatuses ? available : null,
            issues: hasStatuses ? issues : null,
        };
    }, [keys, statuses]);

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
                            lastCaptchaParamRef.current = { param, region: cfg.region, issuedAt: Date.now() };
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
        if (expandedIdx === idx) setExpandedIdx(null);
        else if (expandedIdx !== null && expandedIdx > idx) setExpandedIdx(expandedIdx - 1);
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
                    setAddOpen(false);
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

    // [zcode T4 修订] 额度来源解析：JWT 条目自身即 Plan 凭证；API Key 条目借 account_id 配对的 JWT
    const resolveQuotaSources = useCallback(
        (target: ZaiKeyEntry): { zcodeJwt: string | null; deviceProfile: unknown; bizJwt: string | null } => {
            if (entryMode(target) === 'jwt') {
                const paired = keys.find(
                    (k) => k.account_id && k.account_id === target.account_id && k.business_jwt
                );
                return {
                    zcodeJwt: target.key,
                    deviceProfile: target.device_profile ?? null,
                    bizJwt: target.business_jwt || paired?.business_jwt || null,
                };
            }
            const pairedJwt = keys.find(
                (k) => k.account_id && k.account_id === target.account_id && entryMode(k) === 'jwt'
            );
            return {
                zcodeJwt: pairedJwt?.key ?? null,
                deviceProfile: pairedJwt?.device_profile ?? null,
                bizJwt: target.business_jwt || pairedJwt?.business_jwt || null,
            };
        },
        [keys]
    );

    // [zcode T4 修订] 共享额度请求（Plan 余额 + 订阅详情；两者皆空视为无查询权限）
    const requestQuota = useCallback(
        async (target: ZaiKeyEntry): Promise<{ planData: unknown; subData: unknown }> => {
            const src = resolveQuotaSources(target);
            let planData: unknown = null;
            let subData: unknown = null;
            if (src.zcodeJwt) {
                try {
                    planData = await invoke('zcode_plan_quota', {
                        zcodeJwt: src.zcodeJwt,
                        deviceProfile: src.deviceProfile,
                        upstreamProxy,
                        requestTimeout,
                    });
                } catch (err: unknown) {
                    console.warn('plan_quota error:', err);
                }
            }
            if (src.bizJwt) {
                try {
                    subData = await invoke('zcode_query_quota', {
                        businessJwt: src.bizJwt,
                        upstreamProxy,
                        requestTimeout,
                    });
                } catch (err: unknown) {
                    console.warn('sub_quota error:', err);
                }
            }
            if (!planData && !subData) {
                throw new Error(
                    t('proxy.config.zai.keys.quota_fetch_failed', {
                        defaultValue: '未能获取到额度信息（该账号可能为手动添加的普通 API Key，无订阅查询权限）',
                    })
                );
            }
            return { planData, subData };
        },
        [resolveQuotaSources, upstreamProxy, requestTimeout, t]
    );

    const putQuota = useCallback((anchor: string, q: CardQuota) => {
        quotaCacheRef.current = { ...quotaCacheRef.current, [anchor]: q };
        setQuotaCache(quotaCacheRef.current);
    }, []);

    // [zcode T4 修订] 拉取单账号额度入卡片缓存（Coins 按钮刷新 + 页面打开时自动直显）
    const fetchQuotaIntoCache = useCallback(
        async (entry: ZaiKeyEntry) => {
            const src = resolveQuotaSources(entry);
            if (!src.zcodeJwt) return;
            const anchor = `jwt:${src.zcodeJwt}`;
            if (quotaFetchingRef.current.has(anchor)) return;
            quotaFetchingRef.current.add(anchor);
            putQuota(anchor, { ...(quotaCacheRef.current[anchor] || { balances: [], subs: [] }), loading: true, error: undefined });
            try {
                const { planData, subData } = await requestQuota(entry);
                putQuota(anchor, {
                    balances: parseBalances(planData),
                    subs: parseSubscriptions(subData),
                    loading: false,
                    error: undefined,
                    fetchedAt: Date.now(),
                });
            } catch (e: unknown) {
                putQuota(anchor, {
                    balances: [],
                    subs: [],
                    loading: false,
                    error: e instanceof Error ? e.message : String(e),
                    fetchedAt: Date.now(),
                });
            } finally {
                quotaFetchingRef.current.delete(anchor);
            }
        },
        [resolveQuotaSources, requestQuota, putQuota]
    );

    // 卡片条目对应的额度缓存锚点（无可配对 JWT 时为 null = 无额度可查）
    const quotaAnchorFor = useCallback(
        (entry: ZaiKeyEntry): string | null => {
            if (entryMode(entry) === 'jwt') {
                const seg = entry.key.trim().split('.');
                return seg.length === 3 && seg.every(Boolean) ? `jwt:${entry.key}` : null;
            }
            const pairedJwt = keys.find(
                (k) => k.account_id && k.account_id === entry.account_id && entryMode(k) === 'jwt'
            );
            return pairedJwt ? `jwt:${pairedJwt.key}` : null;
        },
        [keys]
    );

    // [zcode T4 修订] 页面打开 / 凭证稳定后 1.2s，串行（400ms 错峰）拉取各 JWT 账号额度直显；
    // 仍为用户触达页面时的按需拉取，不做后台轮询（T2 决策 #2 的用户授权修订）
    const quotaSignature = keys
        .map((k) => `${entryMode(k)}:${k.enabled ? 1 : 0}:${k.key}`)
        .join('|');
    useEffect(() => {
        const timer = setTimeout(() => {
            const todo = keys.filter((e) => {
                if (entryMode(e) !== 'jwt' || !e.enabled) return false;
                const anchor = quotaAnchorFor(e);
                return !!anchor && !quotaCacheRef.current[anchor] && !quotaFetchingRef.current.has(anchor);
            });
            if (todo.length === 0) return;
            (async () => {
                for (const entry of todo) {
                    await fetchQuotaIntoCache(entry);
                    await new Promise((r) => setTimeout(r, 400));
                }
            })();
        }, 1200);
        return () => clearTimeout(timer);
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [quotaSignature, fetchQuotaIntoCache, quotaAnchorFor]);

    // [zcode 额度查询] 弹窗详情（沿用原行为：Plan 余额 + 订阅 data 原样透传）
    const fetchQuotaData = async (target: ZaiKeyEntry) => {
        setQuotaLoading(true);
        setQuotaError(null);
        try {
            const { planData, subData } = await requestQuota(target);
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
        // 验证码参数新鲜度：上游 TTL 约 2 分钟，仅复用 90s 内的缓存参数，过期强制重解
        const CAPTCHA_FRESH_MS = 90_000;
        const acquireCaptcha = async (
            forceFresh: boolean,
        ): Promise<{ param: string; region: string } | null> => {
            const cached = lastCaptchaParamRef.current;
            if (!forceFresh && cached && Date.now() - cached.issuedAt < CAPTCHA_FRESH_MS) {
                return cached;
            }
            const ok = await solveCaptcha(claimTarget);
            if (!ok) return null;
            return lastCaptchaParamRef.current;
        };
        const doClaim = (captcha: { param: string; region: string }) =>
            invoke<ZcodeClaimResult>('zcode_claim', {
                zcodeJwt: claimTarget.key,
                deviceProfile: claimTarget.device_profile ?? null,
                planId: plan.plan_id,
                verifyParam: captcha.param,
                region: captcha.region,
                upstreamProxy,
                requestTimeout,
            });
        try {
            let captcha = await acquireCaptcha(false);
            if (!captcha) {
                showToast(t('proxy.config.zai.keys.claim_need_captcha'), 'error');
                return;
            }
            let res = await doClaim(captcha);
            // 3007：验证码被拒（上游 400+3007 已由后端归一为业务码）→ 换新鲜验证码重试一次
            if (res.code === 3007) {
                lastCaptchaParamRef.current = null;
                showToast(t('proxy.config.zai.keys.claim_captcha_retry'), 'info', 3000);
                const fresh = await acquireCaptcha(true);
                if (fresh) {
                    res = await doClaim(fresh);
                }
            }
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
        <div className="space-y-4">
            {/* 隐藏的阿里云验证码容器与触发元素 */}
            <div id="zcode-captcha-element" style={{ position: 'fixed', right: 16, bottom: 16, zIndex: 99999 }} />
            <button id="zcode-captcha-button" type="button" style={{ display: 'none' }} />

            {/* 池级汇总徽章 + 工具栏 */}
            <div className="flex items-center gap-2 flex-wrap">
                <HelpTooltip text={t('proxy.config.zai.keys.title_tooltip')} iconSize={12} />
                <StatChip icon={Users} label={t('zcodeAccounts.chips_total')} value={poolStats.total} tone="text-gray-500" />
                <StatChip
                    icon={ShieldCheck}
                    label={t('zcodeAccounts.chips_available')}
                    value={poolStats.available ?? '—'}
                    tone="text-emerald-500"
                />
                <StatChip icon={Sparkles} label={t('zcodeAccounts.chips_subscription')} value={poolStats.jwtCount} tone="text-amber-500" />
                <StatChip
                    icon={AlertTriangle}
                    label={t('zcodeAccounts.chips_issue')}
                    value={poolStats.issues ?? '—'}
                    tone="text-rose-500"
                />
                <div className="flex-1" />
                <div className="flex items-center gap-1">
                    <button
                        className="btn btn-sm h-8 min-h-8 px-3 text-xs btn-ghost gap-1"
                        onClick={refreshStatus}
                        disabled={loadingStatus}
                    >
                        <RefreshCw size={12} className={loadingStatus ? 'animate-spin' : ''} />
                        {t('proxy.config.zai.keys.refresh_status')}
                    </button>
                    <button
                        className="btn btn-sm h-8 min-h-8 px-3 text-xs gap-1 border-none bg-amber-500 hover:bg-amber-600 text-white"
                        onClick={() => setAddOpen(true)}
                    >
                        <Plus size={12} />
                        {t('zcodeAccounts.add_account')}
                    </button>
                </div>
            </div>

            {/* [zcode auto-claim] 每日定时领取选项：开关 + 触发时点（本地 HH:MM），改动即时保存并生效 */}
            <div className="flex items-center justify-between gap-3 rounded-xl border border-gray-200 dark:border-base-200 bg-white dark:bg-base-100 px-4 py-3 shadow-sm">
                <div className="flex items-center gap-2.5 min-w-0">
                    <div className="w-8 h-8 rounded-lg bg-amber-50 dark:bg-amber-500/10 text-amber-500 flex items-center justify-center shrink-0">
                        <Gift size={15} />
                    </div>
                    <div className="min-w-0">
                        <div className="text-xs font-semibold text-gray-800 dark:text-gray-100">
                            {t('proxy.config.zai.auto_claim.title')}
                        </div>
                        <div className="text-[10px] text-gray-400 truncate">
                            {autoClaim.enabled
                                ? t('proxy.config.zai.auto_claim.on_desc', { time: autoClaim.time })
                                : t('proxy.config.zai.auto_claim.off_desc')}
                        </div>
                    </div>
                    <HelpTooltip text={t('proxy.config.zai.auto_claim.tooltip')} iconSize={12} />
                </div>
                <div className="flex items-center gap-2.5 shrink-0">
                    <input
                        type="time"
                        value={autoClaim.time}
                        disabled={!autoClaim.enabled}
                        onChange={(e) => updateAutoClaim({ time: e.target.value || '00:00' })}
                        aria-label={t('proxy.config.zai.auto_claim.time')}
                        className="input input-xs input-bordered font-mono tabular-nums disabled:opacity-40"
                    />
                    <button
                        type="button"
                        role="switch"
                        aria-checked={autoClaim.enabled}
                        title={
                            autoClaim.enabled
                                ? t('proxy.config.zai.auto_claim.enabled')
                                : t('proxy.config.zai.auto_claim.disabled')
                        }
                        onClick={() => updateAutoClaim({ enabled: !autoClaim.enabled })}
                        className={`relative inline-flex h-5 w-9 shrink-0 items-center rounded-full transition-colors cursor-pointer ${
                            autoClaim.enabled ? 'bg-emerald-500' : 'bg-gray-300 dark:bg-base-300'
                        }`}
                    >
                        <span
                            className={`inline-block h-4 w-4 transform rounded-full bg-white shadow transition-transform ${
                                autoClaim.enabled ? 'translate-x-[18px]' : 'translate-x-0.5'
                            }`}
                        />
                    </button>
                </div>
            </div>

            {keys.length === 0 ? (
                <div className="rounded-xl border-2 border-dashed border-gray-200 dark:border-base-200 py-12 px-6 flex flex-col items-center justify-center gap-2.5 text-center">
                    <div className="w-12 h-12 rounded-2xl bg-amber-50 dark:bg-amber-500/10 flex items-center justify-center text-amber-500">
                        <Sparkles size={22} />
                    </div>
                    <div className="text-sm font-medium text-gray-600 dark:text-gray-300">
                        {t('zcodeAccounts.empty_title')}
                    </div>
                    <div className="text-xs text-gray-400 max-w-md leading-relaxed">
                        {t('zcodeAccounts.empty_desc')}
                    </div>
                </div>
            ) : (
                <div className="grid grid-cols-1 md:grid-cols-2 xl:grid-cols-3 gap-3 items-start">
                    {keys.map((entry, idx) => {
                        const st = statuses.find((s) => s.index === idx);
                        const isJwt = entryMode(entry) === 'jwt';
                        const id = entryIdentity(entry);
                        const isSolving = captchaSolvingKey === id;
                        const isExpanded = expandedIdx === idx;
                        const identity =
                            entry.user_email || entry.account_id || entry.label || t('zcodeAccounts.card_unnamed');
                        // [zcode T4 修订] 额度直显锚点（JWT 自身或配对 JWT 的缓存）
                        const quotaAnchor = quotaAnchorFor(entry);
                        const cardQuota = quotaAnchor ? quotaCache[quotaAnchor] : undefined;
                        const quotaRefreshing = !!cardQuota?.loading;
                        return (
                            <div
                                key={idx}
                                className={`rounded-xl border bg-white dark:bg-base-100 p-4 flex flex-col gap-3 shadow-sm transition-colors ${
                                    !entry.enabled
                                        ? 'opacity-55 border-gray-100 dark:border-base-200'
                                        : 'border-gray-200 dark:border-base-200 hover:border-gray-300 dark:hover:border-base-300'
                                }`}
                            >
                                {/* 身份区：图标 + 名称 + 掩码凭证 + 状态 + 启停 */}
                                <div className="flex items-start justify-between gap-2">
                                    <div className="flex items-center gap-2.5 min-w-0">
                                        <div
                                            className={`w-9 h-9 rounded-lg flex items-center justify-center shrink-0 ${
                                                isJwt
                                                    ? 'bg-amber-50 dark:bg-amber-500/10 text-amber-500'
                                                    : 'bg-blue-50 dark:bg-blue-500/10 text-blue-500'
                                            }`}
                                        >
                                            {isJwt ? <Sparkles size={16} /> : <KeyRound size={16} />}
                                        </div>
                                        <div className="min-w-0">
                                            <div className="text-xs font-semibold text-gray-800 dark:text-gray-100 truncate">
                                                {identity}
                                            </div>
                                            <div
                                                className="text-[10px] text-gray-400 font-mono truncate mt-0.5"
                                                title={t('zcodeAccounts.card_credential')}
                                            >
                                                {maskKey(entry.key)}
                                            </div>
                                        </div>
                                    </div>
                                    <div className="flex items-center gap-1.5 shrink-0">
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
                                        {/* [zcode T4 修订] 自绘开关：开启即翠绿，状态一目了然 */}
                                        <button
                                            type="button"
                                            role="switch"
                                            aria-checked={entry.enabled}
                                            title={t('proxy.config.zai.enabled')}
                                            onClick={() => updateRow(idx, { enabled: !entry.enabled })}
                                            className={`relative inline-flex h-5 w-9 shrink-0 items-center rounded-full transition-colors cursor-pointer ${
                                                entry.enabled ? 'bg-emerald-500' : 'bg-gray-300 dark:bg-base-300'
                                            }`}
                                        >
                                            <span
                                                className={`inline-block h-4 w-4 transform rounded-full bg-white shadow transition-transform ${
                                                    entry.enabled ? 'translate-x-[18px]' : 'translate-x-0.5'
                                                }`}
                                            />
                                        </button>
                                    </div>
                                </div>

                                {/* 元信息：模式/上游徽标 + 展开编辑 */}
                                <div className="flex items-center gap-1.5 flex-wrap">
                                    {isJwt ? (
                                        <span
                                            className="badge badge-xs badge-outline whitespace-nowrap"
                                            title={t('proxy.config.zai.keys.mode_jwt_tooltip')}
                                        >
                                            {t('proxy.config.zai.keys.mode_jwt_badge')}
                                        </span>
                                    ) : (
                                        <span className="badge badge-xs badge-ghost whitespace-nowrap">
                                            {entry.provider === 'bigmodel'
                                                ? t('proxy.config.zai.keys.provider_bigmodel')
                                                : t('proxy.config.zai.keys.provider_zai')}
                                        </span>
                                    )}
                                    <button
                                        className="btn btn-ghost btn-xs btn-circle text-gray-400 hover:text-gray-600 dark:hover:text-gray-300"
                                        title={isExpanded ? t('zcodeAccounts.card_collapse') : t('zcodeAccounts.card_edit')}
                                        onClick={() => setExpandedIdx(isExpanded ? null : idx)}
                                    >
                                        {isExpanded ? <ChevronUp size={13} /> : <Pencil size={13} />}
                                    </button>
                                </div>

                                {/* 展开态：凭证编辑 + 上游家族（仅 API Key 条目） */}
                                {isExpanded && (
                                    <div className="space-y-2 rounded-lg bg-gray-50 dark:bg-base-200/60 p-2.5">
                                        <label className="text-[10px] font-medium text-gray-400 block">
                                            {t('zcodeAccounts.card_credential')}
                                        </label>
                                        <input
                                            type="password"
                                            className="input input-xs input-bordered w-full font-mono"
                                            value={entry.key}
                                            placeholder={isJwt ? 'eyJhbGci...' : 'sk-...'}
                                            onChange={(e) => updateRow(idx, { key: e.target.value })}
                                        />
                                        {!isJwt && (
                                            <div className="space-y-1">
                                                <label className="text-[10px] font-medium text-gray-400 block">
                                                    {t('zcodeAccounts.card_upstream')}
                                                </label>
                                                <select
                                                    className="select select-xs select-bordered w-full"
                                                    value={entry.provider}
                                                    onChange={(e) =>
                                                        updateRow(idx, { provider: e.target.value as ZaiProvider })
                                                    }
                                                >
                                                    <option value="zai">{t('proxy.config.zai.keys.provider_zai')}</option>
                                                    <option value="bigmodel">
                                                        {t('proxy.config.zai.keys.provider_bigmodel')}
                                                    </option>
                                                </select>
                                            </div>
                                        )}
                                    </div>
                                )}

                                {/* [zcode T4 修订] 额度直显（打开页面自动拉取，点击条目看详情弹窗，Coins 按钮刷新） */}
                                {quotaAnchor && cardQuota && (
                                    cardQuota.loading && cardQuota.balances.length === 0 ? (
                                        <div className="space-y-2" title={t('common.loading')}>
                                            <div className="h-1 w-2/3 rounded-full bg-gray-200 dark:bg-base-300" />
                                            <div className="h-1 w-full rounded-full bg-gray-200 dark:bg-base-300" />
                                            <div className="h-1 w-1/2 rounded-full bg-gray-200 dark:bg-base-300" />
                                        </div>
                                    ) : cardQuota.balances.length === 0 ? (
                                        <div
                                            className="text-[10px] text-gray-300 dark:text-gray-500 italic cursor-pointer truncate"
                                            title={cardQuota.error || undefined}
                                            onClick={() => openQuotaModal(entry)}
                                        >
                                            {t('proxy.config.zai.keys.quota_empty')}
                                        </div>
                                    ) : (
                                        <div
                                            className="space-y-2 cursor-pointer"
                                            title={t('proxy.config.zai.keys.quota_modal_title')}
                                            onClick={() => openQuotaModal(entry)}
                                        >
                                            {cardQuota.balances.map((b, bi) => (
                                                <div key={bi} className="space-y-1">
                                                    <div className="flex items-center justify-between gap-2">
                                                        <span className="text-[10px] text-gray-500 dark:text-gray-400 truncate">
                                                            {b.name}
                                                        </span>
                                                        <span className="text-[10px] font-mono text-gray-400 tabular-nums shrink-0">
                                                            {b.remaining.toLocaleString()} / {b.total.toLocaleString()}
                                                        </span>
                                                    </div>
                                                    <div className="w-full bg-gray-200 dark:bg-base-300 rounded-full h-1 overflow-hidden">
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
                                                </div>
                                            ))}
                                        </div>
                                    )
                                )}

                                {/* 操作区（图标方块按钮，悬浮提示见 title） */}
                                <div className="flex items-center gap-1 pt-1 mt-auto border-t border-gray-100 dark:border-base-200">
                                    {isJwt && (
                                        <button
                                            className="btn btn-ghost btn-xs h-7 w-7 min-h-0 p-0 text-emerald-600 dark:text-emerald-400"
                                            disabled={isSolving}
                                            title={t('proxy.config.zai.keys.captcha_solve')}
                                            onClick={() => solveCaptcha(entry)}
                                        >
                                            <ShieldCheck size={14} className={isSolving ? 'animate-pulse' : ''} />
                                        </button>
                                    )}
                                    {isJwt && (
                                        <button
                                            className="btn btn-ghost btn-xs h-7 w-7 min-h-0 p-0 text-amber-600 dark:text-amber-400"
                                            title={t('proxy.config.zai.keys.claim')}
                                            onClick={() => openClaimModal(entry)}
                                        >
                                            <Gift size={14} />
                                        </button>
                                    )}
                                    {quotaAnchor && (
                                        <button
                                            className="btn btn-ghost btn-xs h-7 w-7 min-h-0 p-0 text-amber-600 dark:text-amber-400"
                                            title={t('zcodeAccounts.quota_refresh')}
                                            disabled={quotaRefreshing}
                                            onClick={() => fetchQuotaIntoCache(entry)}
                                        >
                                            <Coins size={14} className={quotaRefreshing ? 'animate-pulse' : ''} />
                                        </button>
                                    )}
                                    <div className="flex-1" />
                                    <button
                                        className="btn btn-ghost btn-xs h-7 w-7 min-h-0 p-0 text-red-500"
                                        onClick={() => removeRow(idx)}
                                        title={t('common.delete')}
                                    >
                                        <Trash2 size={14} />
                                    </button>
                                </div>
                            </div>
                        );
                    })}
                </div>
            )}

            {/* [zcode T4 修订] 添加账号子弹窗（对齐 antigravity AddAccountDialog 模式）：OAuth 免密登录 / 手动导入 */}
            {addOpen && (
                <div className="modal modal-open">
                    <div className="modal-box relative max-w-md bg-white dark:bg-base-100 border border-base-300 shadow-2xl p-5">
                        <div className="flex items-center justify-between pb-3 border-b border-base-200">
                            <h3 className="text-sm font-semibold flex items-center gap-1.5">
                                <Plus size={16} className="text-amber-500" />
                                <span>{t('zcodeAccounts.add_account')}</span>
                            </h3>
                            <button className="btn btn-ghost btn-xs btn-circle" onClick={() => setAddOpen(false)}>
                                <X size={14} />
                            </button>
                        </div>

                        <div className="py-4 space-y-3">
                            {/* 方式一：OAuth 免密登录 */}
                            <div className="rounded-xl border border-gray-200 dark:border-base-200 p-4 space-y-2.5">
                                <div className="flex items-start gap-2">
                                    <div className="w-8 h-8 rounded-lg bg-amber-50 dark:bg-amber-500/10 flex items-center justify-center text-amber-500 shrink-0">
                                        <KeyRound size={15} />
                                    </div>
                                    <div className="min-w-0">
                                        <div className="text-xs font-semibold text-gray-800 dark:text-gray-100">
                                            {t('zcodeAccounts.oauth_option_title')}
                                        </div>
                                        <p className="text-[10px] text-gray-400 leading-relaxed mt-0.5">
                                            {t('zcodeAccounts.oauth_option_desc')}
                                        </p>
                                    </div>
                                </div>
                                <button
                                    className="btn btn-sm w-full gap-1 text-xs border-none bg-amber-500 hover:bg-amber-600 text-white"
                                    onClick={startOauth}
                                    disabled={oauthWaiting}
                                >
                                    <KeyRound size={12} className={oauthWaiting ? 'animate-pulse' : ''} />
                                    {oauthWaiting
                                        ? t('proxy.config.zai.keys.oauth_waiting_short')
                                        : t('proxy.config.zai.keys.oauth_login')}
                                </button>
                            </div>

                            {/* 分隔 */}
                            <div className="flex items-center gap-2">
                                <div className="flex-1 border-t border-gray-100 dark:border-base-200" />
                                <span className="text-[10px] text-gray-300 dark:text-gray-500">{t('zcodeAccounts.or')}</span>
                                <div className="flex-1 border-t border-gray-100 dark:border-base-200" />
                            </div>

                            {/* 方式二：手动导入 */}
                            <div className="rounded-xl border border-gray-200 dark:border-base-200 p-4 space-y-2.5">
                                <div className="flex items-start gap-2">
                                    <div className="w-8 h-8 rounded-lg bg-blue-50 dark:bg-blue-500/10 flex items-center justify-center text-blue-500 shrink-0">
                                        <Plus size={15} />
                                    </div>
                                    <div className="min-w-0">
                                        <div className="text-xs font-semibold text-gray-800 dark:text-gray-100">
                                            {t('zcodeAccounts.import_option_title')}
                                        </div>
                                        <p className="text-[10px] text-gray-400 leading-relaxed mt-0.5">
                                            {t('zcodeAccounts.import_option_desc')}
                                        </p>
                                    </div>
                                </div>
                                <div className="flex items-center gap-1.5">
                                    <input
                                        type="text"
                                        className="input input-sm h-8 min-h-8 text-xs input-bordered flex-1 font-mono"
                                        placeholder={t('proxy.config.zai.keys.import_placeholder')}
                                        value={importValue}
                                        onChange={(e) => setImportValue(e.target.value)}
                                        onKeyDown={(e) => {
                                            if (e.key === 'Enter') importCredential();
                                        }}
                                    />
                                    <button
                                        className="btn btn-sm h-8 min-h-8 px-3 text-xs btn-ghost gap-1 shrink-0"
                                        onClick={importCredential}
                                    >
                                        <Plus size={12} />
                                        {t('proxy.config.zai.keys.import')}
                                    </button>
                                </div>
                            </div>
                        </div>
                    </div>
                    <div className="modal-backdrop bg-black/40" onClick={() => setAddOpen(false)} />
                </div>
            )}

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

// [zcode T4 修订] 卡片直显额度缓存条目
interface CardQuota {
    balances: ParsedBalance[];
    subs: ParsedSub[];
    loading: boolean;
    error?: string;
    fetchedAt?: number;
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
