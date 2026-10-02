import { useTranslation } from 'react-i18next';
import { KeyRound } from 'lucide-react';
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
            </div>
        </div>
    );
}

export default ZcodeAccounts;
