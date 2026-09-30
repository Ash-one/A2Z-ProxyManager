import { useCallback, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Plus, RefreshCw, Trash2 } from 'lucide-react';
import { request as invoke } from '../../utils/request';
import HelpTooltip from '../common/HelpTooltip';
import { ZaiConfig, ZaiKeyEntry, ZaiKeyRuntimeStatus, ZaiKeyStatusView, ZaiProvider } from '../../types/config';

interface Props {
    zai?: ZaiConfig;
    onChange: (updates: Partial<ZaiConfig>) => void;
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

/**
 * z.ai / BigModel API Key 池编辑器（zcode T1）。
 * - 每个 Key 一行：启停 pill + 上游家族 + Key + 运行状态徽标；
 * - 保存时同步遗留 api_key 字段（= 首个可用 Key），保持向后兼容；
 * - 状态徽标来自 Key 池运行时（内存态），随保存/手动刷新拉取。
 */
export const ZaiKeyPoolEditor = ({ zai: zaiProp, onChange }: Props) => {
    const { t } = useTranslation();
    const zai = zaiProp || DEFAULT_ZAI;
    const [statuses, setStatuses] = useState<ZaiKeyStatusView[]>([]);
    const [loadingStatus, setLoadingStatus] = useState(false);

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
            const firstUsable = next.find((k) => k.enabled && k.key.trim()) || next[0];
            onChange({ keys: next, api_key: firstUsable ? firstUsable.key : '' });
        },
        [onChange]
    );

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

    const addRow = () => {
        emit([...keys, { key: '', provider: 'zai', enabled: true }]);
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
                    <button className="btn btn-ghost btn-xs gap-1" onClick={addRow}>
                        <Plus size={12} />
                        {t('proxy.config.zai.keys.add')}
                    </button>
                </div>
            </div>

            {keys.length === 0 && (
                <div className="text-[11px] text-gray-400 italic">
                    {t('proxy.config.zai.keys.empty_hint')}
                </div>
            )}

            {keys.map((entry, idx) => {
                const st = statuses.find((s) => s.index === idx);
                return (
                    <div key={idx} className="flex items-center gap-2">
                        <input
                            type="checkbox"
                            className="toggle toggle-xs toggle-success"
                            checked={entry.enabled}
                            title={t('proxy.config.zai.enabled')}
                            onChange={(e) => updateRow(idx, { enabled: e.target.checked })}
                        />
                        <select
                            className="select select-xs select-bordered max-w-[110px]"
                            value={entry.provider}
                            onChange={(e) => updateRow(idx, { provider: e.target.value as ZaiProvider })}
                        >
                            <option value="zai">{t('proxy.config.zai.keys.provider_zai')}</option>
                            <option value="bigmodel">{t('proxy.config.zai.keys.provider_bigmodel')}</option>
                        </select>
                        <input
                            type="password"
                            className="input input-xs input-bordered flex-1 font-mono"
                            value={entry.key}
                            placeholder="sk-..."
                            onChange={(e) => updateRow(idx, { key: e.target.value })}
                        />
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
