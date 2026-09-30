import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Coins, KeyRound, Plus, RefreshCw, Trash2 } from 'lucide-react';
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

/**
 * z.ai / BigModel API Key 池编辑器（zcode T1/T2）。
 * - 每个 Key 一行：启停 pill + 上游家族（仅 API Key 行）+ Key + 运行状态徽标；
 * - zcode T2：OAuth 免密登录（CLI 流程，自动开通订阅 API Key 入池）、
 *   手动导入自动判别 JWT / API Key、Plan JWT 行"待 T3"徽标、按需额度查询；
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
    const [quotaBusy, setQuotaBusy] = useState<string | null>(null);

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

    // [zcode T2] 手动按需额度查询（业务 JWT；结构未文档化 → 容错摘要展示）
    const queryQuota = async (entry: ZaiKeyEntry) => {
        if (!entry.business_jwt || quotaBusy) return;
        setQuotaBusy(entryIdentity(entry));
        try {
            const data = await invoke<unknown>('zcode_query_quota', {
                businessJwt: entry.business_jwt,
                upstreamProxy,
                requestTimeout,
            });
            const summary = summarizeQuota(data);
            showToast(
                summary || t('proxy.config.zai.keys.quota_raw', { data: JSON.stringify(data) }),
                'info',
                8000
            );
        } catch (e) {
            showToast(String(e), 'error', 8000);
        } finally {
            setQuotaBusy(null);
        }
    };

    return (
        <div className="space-y-2">
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

            {/* [zcode T2] 手动导入：粘贴即判别 */}
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
                            <button
                                className="btn btn-ghost btn-xs gap-1"
                                disabled={!entry.business_jwt || quotaBusy !== null}
                                title={
                                    entry.business_jwt
                                        ? t('proxy.config.zai.keys.quota_query')
                                        : t('proxy.config.zai.keys.quota_unavailable')
                                }
                                onClick={() => queryQuota(entry)}
                            >
                                <Coins size={12} className={quotaBusy === entryIdentity(entry) ? 'animate-pulse' : ''} />
                            </button>
                        )}
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
        </div>
    );
};

// [zcode T2] 订阅/额度响应容错摘要（schema 未公开文档化；识别常见字段则摘要，否则提示原文查看）
function summarizeQuota(data: unknown): string {
    if (data == null) return '';
    const records: unknown[] = Array.isArray(data)
        ? data
        : typeof data === 'object'
          ? (Object.values(data as Record<string, unknown>).find((v) => Array.isArray(v)) as
                | unknown[]
                | undefined) ?? [data]
          : [data];
    const lines: string[] = [];
    for (const item of records) {
        if (typeof item !== 'object' || item == null) continue;
        const obj = item as Record<string, unknown>;
        const name = ['plan_name', 'planName', 'name', 'product_name', 'productName', 'title']
            .map((k) => obj[k])
            .find((v) => typeof v === 'string') as string | undefined;
        const expire = [
            'expire_time',
            'expireTime',
            'expired_at',
            'expiredAt',
            'end_time',
            'endTime',
            'valid_until',
        ]
            .map((k) => obj[k])
            .find((v) => typeof v === 'string' || typeof v === 'number');
        const quota = ['remaining', 'remain', 'quota', 'balance', 'left', 'remaining_quota']
            .map((k) => obj[k])
            .find((v) => typeof v === 'number' || typeof v === 'string');
        const parts: string[] = [];
        if (name) parts.push(String(name));
        if (quota !== undefined) parts.push(String(quota));
        if (expire !== undefined) parts.push(String(expire));
        if (parts.length > 0) lines.push(parts.join(' · '));
    }
    return lines.join('\n');
}
