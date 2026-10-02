import { useTranslation } from 'react-i18next';
import { KeyRound, Zap } from 'lucide-react';
import { ZaiKeyPoolEditor, DEFAULT_ZAI } from '../components/proxy/ZaiKeyPoolEditor';
import { useConfigStore } from '../stores/useConfigStore';
import { ZaiConfig } from '../types/config';

/**
 * ZCode 账号页（zcode T4）——与「账号管理」（antigravity）同层级的订阅账号管理页。
 * 承载 z.ai / bigmodel API Key 池与 Coding Plan 订阅（OAuth 登录、导入、
 * 过码、额度查询、套餐领取）的统一管理；数据仍存 proxy.zai.keys，
 * 调度仍走 ZaiKeyPool（T4 提案：docs/zcode/proposal-t4-page.md）。
 */
function ZcodeAccounts() {
    const { t } = useTranslation();
    const { config, saveConfig } = useConfigStore();

    const updateZai = (updates: Partial<ZaiConfig>) => {
        if (!config) return;
        const base: ZaiConfig = config.proxy.zai || DEFAULT_ZAI;
        saveConfig(
            {
                ...config,
                proxy: {
                    ...config.proxy,
                    zai: { ...base, ...updates },
                },
            },
            true
        );
    };

    return (
        <div className="h-full overflow-y-auto">
            <div className="max-w-7xl mx-auto w-full p-5 space-y-4">
                <div className="flex-none">
                    <h1 className="text-lg font-semibold text-gray-900 dark:text-white flex items-center gap-2">
                        <KeyRound size={18} className="text-amber-500" />
                        {t('zcodeAccounts.title')}
                    </h1>
                </div>
                <ZaiKeyPoolEditor
                    zai={config?.proxy.zai}
                    onChange={updateZai}
                    upstreamProxy={config?.proxy.upstream_proxy}
                    requestTimeout={config?.proxy.request_timeout}
                />

                {/* [zcode T4 修订] 转发调度（自 API 反代页 z.ai 提供商卡迁入）。
                    样式与账号卡区分：配置面使用琥珀色浅底 + 左侧强调条，账号卡为白底实体卡 */}
                <div className="relative overflow-hidden rounded-xl border border-amber-200 dark:border-amber-500/20 bg-amber-50/60 dark:bg-amber-500/5 p-4 space-y-3">
                    <div className="absolute left-0 top-0 bottom-0 w-1 bg-amber-400 dark:bg-amber-500/60" />
                    <div className="flex items-center justify-between pl-2">
                        <div className="flex items-center gap-2">
                            <div className="w-7 h-7 rounded-lg bg-amber-100 dark:bg-amber-500/15 flex items-center justify-center text-amber-600 dark:text-amber-400 shrink-0">
                                <Zap size={14} />
                            </div>
                            <span className="text-xs font-semibold text-gray-800 dark:text-gray-100">
                                {t('proxy.config.zai.title')}
                            </span>
                        </div>
                        <button
                            type="button"
                            role="switch"
                            aria-checked={!!config?.proxy.zai?.enabled}
                            title={t('proxy.config.zai.enabled')}
                            onClick={() => updateZai({ enabled: !config?.proxy.zai?.enabled })}
                            className={`relative inline-flex h-5 w-9 shrink-0 items-center rounded-full transition-colors cursor-pointer ${
                                config?.proxy.zai?.enabled ? 'bg-emerald-500' : 'bg-gray-300 dark:bg-base-300'
                            }`}
                        >
                            <span
                                className={`inline-block h-4 w-4 transform rounded-full bg-white shadow transition-transform ${
                                    config?.proxy.zai?.enabled ? 'translate-x-[18px]' : 'translate-x-0.5'
                                }`}
                            />
                        </button>
                    </div>

                    <p className="text-[10px] text-gray-500 dark:text-gray-400 leading-relaxed pl-2">
                        {t('zcodeAccounts.dispatch_hint')}
                    </p>

                    <div className="pl-2 space-y-1">
                        <label className="text-[11px] font-medium text-gray-500 dark:text-gray-400">
                            {t('proxy.config.zai.base_url')}
                        </label>
                        <input
                            type="text"
                            value={config?.proxy.zai?.base_url || 'https://api.z.ai/api/anthropic'}
                            onChange={(e) => updateZai({ base_url: e.target.value })}
                            className="input input-sm h-8 min-h-8 text-xs input-bordered w-full font-mono bg-white dark:bg-base-100"
                        />
                    </div>
                </div>
            </div>
        </div>
    );
}

export default ZcodeAccounts;
