export const CAPTCHA_SDK_URL = 'https://o.alicdn.com/captcha-frontend/aliyunCaptcha/AliyunCaptcha.js';

export function loadCaptchaSdk(): Promise<void> {
    if (typeof window !== 'undefined' && (window as unknown as { initAliyunCaptcha?: unknown }).initAliyunCaptcha) {
        return Promise.resolve();
    }
    return new Promise((resolve, reject) => {
        const existing = document.querySelector<HTMLScriptElement>('script[data-zcode-captcha]');
        if (existing) {
            if ((window as unknown as { initAliyunCaptcha?: unknown }).initAliyunCaptcha) {
                resolve();
            } else {
                existing.addEventListener('load', () => resolve());
                existing.addEventListener('error', () => reject(new Error('Captcha SDK load failed')));
            }
            return;
        }
        const s = document.createElement('script');
        s.src = CAPTCHA_SDK_URL;
        s.async = true;
        s.dataset.zcodeCaptcha = '1';
        s.onload = () => resolve();
        s.onerror = () => reject(new Error('Captcha SDK load failed'));
        document.head.appendChild(s);
    });
}
