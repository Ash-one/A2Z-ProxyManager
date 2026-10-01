import { useEffect, useRef } from 'react';
import { request as invoke } from '../../utils/request';
import { useConfigStore } from '../../stores/useConfigStore';
import { ZaiKeyEntry, ZcodeCaptchaConfig, ZaiKeyStatusView } from '../../types/config';
import { loadCaptchaSdk } from '../../utils/captcha';
import { listen } from '@tauri-apps/api/event';
import { isTauri } from '../../utils/env';

/**
 * 全局 ZCode Plan 阿里云验证码后台无痕缓冲池守护器 (Captcha Buffer Pool Daemon)。
 * 挂载于 App 顶层，全生命周期常驻：
 * 1. 预热缓冲池 (Staggered Buffer Pool)：按 40s 代际步长为每个已启用 JWT 账号自动补给新鲜 Token，
 *    在后端形成最大容量为 3 的滑动窗口队列，确保外部请求 100% 毫秒级命中现成 Token；
 * 2. 严格串行队列 (Serial Solver Queue)：保证同一时刻仅运行 1 个阿里 SDK 实例，杜绝并发调用碰撞；
 * 3. 页面唤醒自愈 (Page Visibility Listener)：休眠或切回前台时主动检查并回补过期的 Token；
 * 4. 事件驱动即时求解：监听后端 `zcode://solve-captcha` 事件，按需被唤醒毫秒级补给。
 */
export const ZcodeCaptchaDaemon = () => {
    const { config } = useConfigStore();
    const lastSolvedRef = useRef<Map<string, number>>(new Map());
    const queueRef = useRef<Promise<void>>(Promise.resolve());
    const verifyCallbackRef = useRef<((param: string) => Promise<{ captchaResult: boolean; bizResult?: boolean }>) | null>(null);
    const captchaInstanceRef = useRef<{ verify?: () => void } | null>(null);

    // 单次求解核心实现
    const solveKey = async (entry: ZaiKeyEntry): Promise<boolean> => {
        return new Promise<boolean>(async (resolve) => {
            let finished = false;
            const finish = (ok: boolean) => {
                if (finished) return;
                finished = true;
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

    // 串行队列调度器：严格串行执行，避免阿里 SDK 在同一 DOM 树上发生并发竞争
    const enqueueSolve = (entry: ZaiKeyEntry): Promise<boolean> => {
        return new Promise<boolean>((resolve) => {
            queueRef.current = queueRef.current
                .then(async () => {
                    const result = await solveKey(entry);
                    // 每次求解后增加 1.2 秒平滑缓冲，避免连续频繁触发阿里风控
                    await new Promise((r) => setTimeout(r, 1200));
                    resolve(result);
                })
                .catch(() => {
                    resolve(false);
                });
        });
    };

    // 1. 定期代际预热缓冲池 (每 10 秒轮询，距上次刷新超过 40 秒即补充新代 Token)
    useEffect(() => {
        const checkAndPrewarm = async () => {
            const zai = config?.proxy?.zai;
            const jwtKeys = (zai?.keys || []).filter(
                (k) => k.enabled && k.mode === 'jwt' && k.key.trim().length > 0
            );
            if (jwtKeys.length === 0) return;

            const now = Date.now();
            for (const key of jwtKeys) {
                const last = lastSolvedRef.current.get(key.key) || 0;
                // 新鲜窗是 90s，代际预热阈值设为 40s：在第一代 Token 寿命过半时提前产出备用 Token 入池
                if (now - last > 40_000) {
                    await enqueueSolve(key);
                }
            }
        };

        const timer = setInterval(checkAndPrewarm, 10_000);
        // 初次加载延迟 2 秒后执行一次预热补给
        const initialTimer = setTimeout(checkAndPrewarm, 2000);

        // 页面从后台休眠唤醒或切回前台时，主动触发一次快速回补
        const handleVisibilityChange = () => {
            if (document.visibilityState === 'visible') {
                checkAndPrewarm();
            }
        };
        document.addEventListener('visibilitychange', handleVisibilityChange);

        return () => {
            clearInterval(timer);
            clearTimeout(initialTimer);
            document.removeEventListener('visibilitychange', handleVisibilityChange);
        };
    }, [config]);

    // 2. 监听 Tauri 事件（后端发来按需求解请求）
    useEffect(() => {
        if (!isTauri()) return;
        let unlistenFn: (() => void) | undefined;

        listen<{ accountId?: string; key?: string }>('zcode://solve-captcha', async (event) => {
            const zai = config?.proxy?.zai;
            const jwtKeys = (zai?.keys || []).filter(
                (k) => k.enabled && k.mode === 'jwt' && k.key.trim().length > 0
            );
            if (jwtKeys.length === 0) return;

            const target =
                jwtKeys.find(
                    (k) => k.key === event.payload?.key || (k.account_id && k.account_id === event.payload?.accountId)
                ) || jwtKeys[0];

            if (target) {
                await enqueueSolve(target);
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
            const jwtKeys = (zai?.keys || []).filter(
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
                        await enqueueSolve(target);
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
