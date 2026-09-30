//! zcode T1：API Key 池 — 配置迁移（A2）与环境变量解析（A6 headless parity）纯逻辑测试。
//! 池状态机/选择逻辑的聚焦测试见 `providers/zai_pool.rs` 内嵌 `#[cfg(test)]`。

use crate::proxy::config::{parse_zai_keys_env, ZaiConfig, ZaiProvider};

#[test]
fn legacy_single_api_key_migrates_to_pool() {
    // 旧配置：单 api_key + 默认 base_url → 迁移为单条 zai 条目
    let cfg = ZaiConfig {
        api_key: "sk-legacy".to_string(),
        ..ZaiConfig::default()
    };
    let keys = cfg.resolved_keys();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].key, "sk-legacy");
    assert_eq!(keys[0].provider, ZaiProvider::Zai);
    assert!(keys[0].enabled);
}

#[test]
fn legacy_bigmodel_base_url_infers_provider() {
    // 旧配置指向 bigmodel → 迁移条目携带 BigModel provider
    let cfg = ZaiConfig {
        api_key: "bm-legacy".to_string(),
        base_url: "https://open.bigmodel.cn/api/anthropic".to_string(),
        ..ZaiConfig::default()
    };
    let keys = cfg.resolved_keys();
    assert_eq!(keys[0].provider, ZaiProvider::BigModel);
}

#[test]
fn keys_list_takes_precedence_over_legacy_api_key() {
    // keys 非空时遗留 api_key 被忽略（迁移只进不退，行为不回退）
    let cfg = ZaiConfig {
        api_key: "sk-legacy".to_string(),
        keys: vec![crate::proxy::ZaiKeyEntry {
            key: "sk-new".to_string(),
            provider: ZaiProvider::BigModel,
            enabled: true,
            label: String::new(),
        }],
        ..ZaiConfig::default()
    };
    let keys = cfg.resolved_keys();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].key, "sk-new");
    assert_eq!(keys[0].provider, ZaiProvider::BigModel);
    // 旧格式 JSON 回放：无 keys 字段时 serde 默认为空 → 走迁移路径
    let replayed: ZaiConfig = serde_json::from_str(
        r#"{"enabled":true,"base_url":"https://api.z.ai/api/anthropic","api_key":"sk-old"}"#,
    )
    .expect("legacy config json must deserialize");
    assert_eq!(replayed.resolved_keys()[0].key, "sk-old");
}

#[test]
fn effective_base_url_rules() {
    // 规范地址或缺省 → 按条目 provider 解析
    assert_eq!(
        ZaiConfig::effective_base_url(ZaiProvider::BigModel, "https://api.z.ai/api/anthropic"),
        "https://open.bigmodel.cn/api/anthropic"
    );
    assert_eq!(
        ZaiConfig::effective_base_url(ZaiProvider::Zai, ""),
        "https://api.z.ai/api/anthropic"
    );
    // 自定义网关 → 全局覆盖（T1 前行为不变）
    assert_eq!(
        ZaiConfig::effective_base_url(
            ZaiProvider::Zai,
            "https://my-gateway.example.com/anthropic/"
        ),
        "https://my-gateway.example.com/anthropic"
    );
}

#[test]
fn parse_zai_keys_env_syntax() {
    let entries = parse_zai_keys_env("sk-a, bigmodel:sk-b\nsk-c;zai:sk-d");
    assert_eq!(entries.len(), 4);
    assert_eq!(entries[0].provider, ZaiProvider::Zai);
    assert_eq!(entries[1].provider, ZaiProvider::BigModel);
    assert_eq!(entries[1].key, "sk-b");
    assert_eq!(entries[2].provider, ZaiProvider::Zai);
    assert_eq!(entries[3].provider, ZaiProvider::Zai);
    assert_eq!(entries[3].key, "sk-d");
    // 空串 / 纯分隔符 → 空池（保持配置文件值不变）
    assert!(parse_zai_keys_env(" , ;\n").is_empty());
}
