import { useEffect, useRef } from 'react';
import { request as invoke } from '../../utils/request';
import { useConfigStore } from '../../stores/useConfigStore';
import { ZaiKeyEntry, ZcodeCaptchaConfig, ZaiKeyStatusView } from '../../types/config';
import { loadCaptchaSdk } from '../../utils/captcha';
import { listen } from '@tauri-apps/api/event';
import { isTauri } from '../../utils/env';

/**
 * 全局 ZCode Plan 阿里云验证码后台无痕守护器。
 * 挂载于 App 顶层，全生命周期常驻：
 * 1. 定期（每 60s）在后台无痕预热刷新验证码，确保上游接口始终持有 90s 内新鲜令牌；
 * 2. 监听后端 `zcode://solve-captcha` 事件，按需被唤醒执行秒级求解；
 * 3. 轮询检测到 `CaptchaNeeded` 时自动尝试自愈求解，彻底实现全自动无感调用。
 */
export const ZcodeCaptchaDaemon = () => {
    const { config } = useConfigStore();
    const isSolvingRef = useRef(false);
    const lastSolvedRef = useRef<Map<string, number>>(new Map());
    const verifyCallbackRef = useRef<((param: string) => Promise<{ captchaResult: boolean; bizResult?: boolean }>) | null>(null);
    const captchaInstanceRef = useRef<{ verify?: () => void } | null>(null);

    const solveKey = async (entry: ZaiKeyEntry): Promise<boolean> => {
        if (isSolvingRef.current) return false;
        isSolvingRef.current = true;

        return new Promise<boolean>(async (resolve) => {
            let finished = false;
            const finish = (ok: boolean) => {
                if (finished) return;
                finished = true;
                isSolvingRef.current = false;
                resolve(ok);
            };

            const timeout = setTimeout(() => {
                finish(false);
            }, 30_000);

            try {
                const upstreamProxy = config?.proxy?.upstream_proxy;
                const requestTimeout = config?.proxy?.request_timeout || 60;
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
                        lastSolvedRef.current.set(entry.key, Date.now());
                        finish(true);
                        return { captchaResult: true, bizResult: true };
                    } catch {
                        finish(false);
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
                        element: '#zcode-captcha-global-element',
                        button: '#zcode-captcha-global-button',
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
                                document.getElementById('zcode-captcha-global-button')?.click();
                            }
                        },
                    });
                    setTimeout(() => {
                        document.getElementById('zcode-captcha-global-button')?.click();
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
    };

    // 1. 定期主动保活（每 60 秒检查一次是否有已启用的 JWT 条目需要刷新）
    useEffect(() => {
        const checkAndPrewarm = async () => {
            const zai = config?.proxy?.zai;
            if (!zai?.enabled) return;

            const jwtKeys = (zai.keys || []).filter(
                (k) => k.enabled && k.mode === 'jwt' && k.key.trim().length > 0
            );
            if (jwtKeys.length === 0) return;

            const now = Date.now();
            for (const key of jwtKeys) {
                const last = lastSolvedRef.current.get(key.key) || 0;
                // 新鲜窗是 90s，超过 60s 未刷新则主动无痕刷新
                if (now - last > 60_000) {
                    await solveKey(key);
                    break; // 每次只刷新一个，避免并发竞争
                }
            }
        };

        const timer = setInterval(checkAndPrewarm, 15_000);
        // 初次加载延迟 2 秒后执行一次预热
        const initialTimer = setTimeout(checkAndPrewarm, 2000);

        return () => {
            clearInterval(timer);
            clearTimeout(initialTimer);
        };
    }, [config]);

    // 2. 监听 Tauri 事件（后端发来按需求解请求）
    useEffect(() => {
        if (!isTauri()) return;
        let unlistenFn: (() => void) | undefined;

        listen<{ accountId?: string; key?: string }>('zcode://solve-captcha', async (event) => {
            const zai = config?.proxy?.zai;
            if (!zai) return;
            const jwtKeys = (zai.keys || []).filter(
                (k) => k.enabled && k.mode === 'jwt' && k.key.trim().length > 0
            );
            const target =
                jwtKeys.find(
                    (k) => k.key === event.payload?.key || (k.account_id && k.account_id === event.payload?.accountId)
                ) || jwtKeys[0];

            if (target) {
                await solveKey(target);
            }
        }).then((u) => {
            unlistenFn = u;
        });

        return () => {
            if (unlistenFn) unlistenFn();
        };
    }, [config]);

    // 3. 定期查询运行状态，若后端标记为 CaptchaNeeded 则立即自愈
    useEffect(() => {
        const checkStatus = async () => {
            const zai = config?.proxy?.zai;
            if (!zai?.enabled) return;

            const jwtKeys = (zai.keys || []).filter(
                (k) => k.enabled && k.mode === 'jwt' && k.key.trim().length > 0
            );
            if (jwtKeys.length === 0) return;

            try {
                const statuses = await invoke<ZaiKeyStatusView[]>('get_zai_key_pool_status');
                const needy = statuses.find(
                    (s) => s.status === 'CaptchaNeeded' && s.mode === 'jwt' && s.enabled
                );
                if (needy) {
                    const target = jwtKeys[needy.index] || jwtKeys[0];
                    if (target) {
                        await solveKey(target);
                    }
                }
            } catch {
                // Ignore transient query failures
            }
        };

        const interval = setInterval(checkStatus, 10_000);
        return () => clearInterval(interval);
    }, [config]);

    return (
        <div style={{ position: 'fixed', right: 16, bottom: 16, zIndex: 99999, pointerEvents: 'none' }}>
            <div id="zcode-captcha-global-element" style={{ pointerEvents: 'auto' }} />
            <button id="zcode-captcha-global-button" type="button" style={{ display: 'none' }} />
        </div>
    );
};
