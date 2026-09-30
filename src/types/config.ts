export interface UpstreamProxyConfig {
    enabled: boolean;
    url: string;
}

export interface ProxyConfig {
    enabled: boolean;
    allow_lan_access?: boolean;
    auth_mode?: 'off' | 'strict' | 'all_except_health' | 'auto';
    port: number;
    api_key: string;
    admin_password?: string;
    auto_start: boolean;
    custom_mapping?: Record<string, string>;
    request_timeout: number;
    enable_logging: boolean;
    capture_health_logs?: boolean;
    log_retention?: LogRetentionConfig;
    debug_logging?: DebugLoggingConfig;
    upstream_proxy: UpstreamProxyConfig;
    zai?: ZaiConfig;
    scheduling?: StickySessionConfig;
    experimental?: ExperimentalConfig;
    user_agent_override?: string;
    saved_user_agent?: string;
    thinking_budget?: ThinkingBudgetConfig;
    global_system_prompt?: GlobalSystemPromptConfig;
    image_thinking_mode?: 'enabled' | 'disabled'; // [NEW] 图像思维模式开关
    only_raw_quota_models?: boolean; // [NEW] 是否只暴露真实配额模型
    proxy_pool?: ProxyPoolConfig;
}

export interface LogRetentionConfig {
    max_body_age_hours: number;
    max_storage_gb: number;
    max_disk_mb?: number;
    max_rows: number;
    max_age_days?: number;
}


// ============================================================================
// Thinking Budget 配置 (控制 AI 深度思考时的 Token 预算)
// ============================================================================

/** 思考预算控制权归属 */
export type ThinkingControlSource = 'gateway' | 'client';

/** Thinking Budget 处理模式 */
export type ThinkingBudgetMode = 'default' | 'custom' | 'auto' | 'passthrough' | 'adaptive';

/** Thinking Effort 等级 (仅 adaptive 模式) */
export type ThinkingEffort = 'low' | 'medium' | 'high';

/** Thinking Budget 配置 */
export interface ThinkingBudgetConfig {
    /** 控制权大选择：网关权威控制 (gateway) vs 客户端直接控制 (client) */
    control_source?: ThinkingControlSource;

    // --- Gemini Flash 系列配置 ---
    flash_mode?: ThinkingBudgetMode;
    flash_low?: number;       // 默认 1000
    flash_medium?: number;    // 默认 4000
    flash_high?: number;      // 默认 10000
    flash_tiered?: number;    // 默认 -1

    // --- Gemini Pro 系列配置（官方仅 Low 与 High 两档） ---
    pro_mode?: ThinkingBudgetMode;
    pro_low?: number;         // 默认 1001
    pro_high?: number;        // 默认 10001

    // --- Claude 系列配置 ---
    claude_mode?: ThinkingBudgetMode;
    claude_budget?: number;    // 统一思考预算 (默认 16000, 填 -1 自适应)
    claude_low?: number;       // 默认 1024
    claude_medium?: number;    // 默认 4096
    claude_high?: number;      // 默认 16000

    // --- 旧版兼容字段 ---
    mode?: ThinkingBudgetMode;
    custom_value?: number;
    effort?: ThinkingEffort;
    custom_low?: number;
    custom_medium?: number;
    custom_high?: number;
    custom_tiered?: number;
}

// ============================================================================
// 全局系统提示词配置
// ============================================================================

/** 全局系统提示词配置 */
export interface GlobalSystemPromptConfig {
    /** 是否启用 */
    enabled: boolean;
    /** 提示词内容 */
    content: string;
}

export interface DebugLoggingConfig {
    enabled: boolean;
    output_dir?: string;
}

export type SchedulingMode = 'CacheFirst' | 'Balance' | 'PerformanceFirst';

export interface StickySessionConfig {
    mode: SchedulingMode;
    max_wait_seconds: number;
}

export type ZaiDispatchMode = 'off' | 'exclusive' | 'pooled' | 'fallback';

export type ZaiProvider = 'zai' | 'bigmodel' | 'zcode_plan';

// [zcode T2] 凭证模式：apiKey=直连转发（T1 起）；jwt=Coding Plan 订阅 JWT（Plan 通道，T3 前不参与转发）
export type ZaiKeyMode = 'api_key' | 'jwt';

export interface ZaiKeyEntry {
    key: string;
    provider: ZaiProvider;
    enabled: boolean;
    label?: string;
    mode?: ZaiKeyMode;
    /** 账号配对身份（OAuth 登录产物；JWT 与同账号 API Key 并存的锚点） */
    account_id?: string;
    user_email?: string;
    /** api.z.ai 管理 JWT（billing/订阅查询专用，绝不用于消息转发） */
    business_jwt?: string;
    /** zcode T3：每账号设备档案（成套桌面 SKU + 稳定 device_mid，随配置持久化） */
    device_profile?: unknown;
}

export interface ZaiMcpConfig {
    enabled: boolean;
    web_search_enabled: boolean;
    web_reader_enabled: boolean;
    vision_enabled: boolean;
}

export interface ZaiModelDefaults {
    opus: string;
    sonnet: string;
    haiku: string;
}

export interface ZaiConfig {
    enabled: boolean;
    base_url: string;
    api_key: string;
    keys?: ZaiKeyEntry[];
    dispatch_mode: ZaiDispatchMode;
    model_mapping?: Record<string, string>;
    models: ZaiModelDefaults;
    mcp: ZaiMcpConfig;
}

// [zcode T1/T3] Key 池运行状态（掩码展示，与后端 ZaiKeyStatusView 对齐）
export type ZaiKeyRuntimeStatus = 'Active' | 'Invalid' | 'Exhausted' | 'RateLimited' | 'Cooldown' | 'CaptchaNeeded';

export interface ZaiKeyStatusView {
    index: number;
    masked_key: string;
    provider: ZaiProvider;
    mode?: ZaiKeyMode;
    account_id?: string;
    enabled: boolean;
    label: string;
    status: ZaiKeyRuntimeStatus;
    status_remaining_secs: number | null;
    last_error: string | null;
}

// [zcode T2] OAuth CLI 登录流程态（后端无会话状态，前端持有并回传轮询）
export interface ZcodeOauthFlow {
    flow_id: string;
    authorize_url: string;
    poll_token: string;
    expires_at_ms: number;
    poll_interval_secs: number;
}

export interface ZcodePollCommandResult {
    status: 'pending' | 'expired' | 'ready';
    entries: ZaiKeyEntry[];
    warning?: string | null;
}

// [zcode T3] 验证码场景配置（client/configs 动态取值 + app_version）
export interface ZcodeCaptchaConfig {
    enabled: boolean;
    prefix: string;
    region: string;
    scene_id: string;
    app_version: string;
}

// [zcode T3] 限时套餐预览项
export interface ZcodeClaimPlan {
    plan_id: string;
    name: string;
    description: string;
    priority?: unknown;
    grants?: unknown;
}

// [zcode T3] 限时套餐领取结果
export interface ZcodeClaimResult {
    ok: boolean;
    code: number;
    message: string;
    next_at_ms?: number | null;
    data?: unknown;
}

export interface ScheduledWarmupConfig {
    enabled: boolean;
    monitored_models: string[];
}

export interface QuotaProtectionConfig {
    enabled: boolean;
    threshold_percentage: number; // 1-99
    monitored_models: string[];
}

export interface PinnedQuotaModelsConfig {
    models: string[];
}

export interface ExperimentalConfig {
    enable_usage_scaling: boolean;
    compression_level?: string;
    context_compression_threshold_l1?: number;
    context_compression_threshold_l2?: number;
    context_compression_threshold_l3?: number;
    payload_storage_mode?: 'simple' | 'full';
    log_retention_days?: number;
    thinking_store_enabled?: boolean;
    thinking_retention_days?: number;
    thinking_max_memory_turns?: number;
}

export interface CircuitBreakerConfig {
    enabled: boolean;
    backoff_steps: number[];
    lock_on_zero_quota?: boolean;
}

export interface AppConfig {
    language: string;
    theme: string;
    auto_refresh: boolean;
    refresh_interval: number;
    auto_sync: boolean;
    sync_interval: number;
    default_export_path?: string;
    antigravity_executable?: string; // [NEW] 手动指定的反重力程序路径
    antigravity_ide_executable?: string; // [NEW] 手动指定的 Antigravity IDE 程序路径
    antigravity_cli_executable?: string; // [NEW] 手动指定的 Antigravity CLI (agy) 路径
    antigravity_args?: string[]; // [NEW] Antigravity 启动参数
    auto_launch?: boolean; // 开机自动启动
    auto_check_update?: boolean; // 自动检查更新
    update_check_interval?: number; // 更新检查间隔（小时）
    update_channel?: 'stable' | 'beta'; // 更新通道：正式版 vs 预览版
    accounts_page_size?: number; // 账号列表每页显示数量,默认 0 表示自动计算
    hidden_menu_items?: string[]; // 隐藏的菜单项路径列表
    scheduled_warmup: ScheduledWarmupConfig;
    quota_protection: QuotaProtectionConfig; // [NEW] 配额保护配置
    pinned_quota_models: PinnedQuotaModelsConfig; // [NEW] 配额关注列表
    circuit_breaker: CircuitBreakerConfig; // [NEW] 熔断器配置
    proxy: ProxyConfig;
    cloudflared: CloudflaredConfig; // [NEW] Cloudflared 配置
    lightweight_mode?: boolean; // [NEW] 轻量模式：关闭到托盘时释放 WebView
    suggestion_delete_thinking_store?: boolean; // [NEW] 建议删除历史思考块缓存开关
    thinking_cleanup_dismissed?: boolean; // [NEW] 用户是否已确认/忽略该建议
    dismissed_thinking_cleanup_version?: string; // [NEW] 用户已确认或忽略建议的目标版本号
}

// ============================================================================
// Cloudflared (CF隧道) 类型定义
// ============================================================================

export type TunnelMode = 'quick' | 'auth';

export interface CloudflaredConfig {
    enabled: boolean;
    mode: TunnelMode;
    port: number;
    token?: string;
    use_http2: boolean;
}

export interface CloudflaredStatus {
    installed: boolean;
    version?: string;
    running: boolean;
    url?: string;
    error?: string;
}

// ============================================================================
// 代理池类型定义
// ============================================================================

export interface ProxyAuth {
    username: string;
    password?: string;
}

export interface ProxyEntry {
    id: string;
    name: string;
    url: string;
    auth?: ProxyAuth;
    enabled: boolean;
    priority: number;
    tags: string[];
    max_accounts?: number;
    health_check_url?: string;
    last_check_time?: number;
    is_healthy: boolean;
    latency?: number; // [NEW] 延迟 (毫秒)
}

// export type ProxyPoolMode = 'global' | 'per_account' | 'hybrid'; // [REMOVED]

export type ProxySelectionStrategy = 'round_robin' | 'random' | 'priority' | 'least_connections' | 'weighted_round_robin';

export interface ProxyPoolConfig {
    enabled: boolean;
    // mode: ProxyPoolMode; // [REMOVED]
    proxies: ProxyEntry[];
    health_check_interval: number;
    auto_failover: boolean;
    strategy: ProxySelectionStrategy;
    account_bindings?: Record<string, string>;
}
