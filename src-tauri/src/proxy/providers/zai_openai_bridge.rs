//! z.ai 通道 OpenAI 形态桥接（zcode T5，`docs/zcode/proposal-t5-protocols.md`）。
//!
//! 按 T5 有界实验结论（2026-10-02）：zcode JWT 在 `api.z.ai/api/coding/paas/v4` 与
//! `api.z.ai/api/paas/v4` 两个 OpenAI 兼容端点均返回 401 —— S2a 原生直传对 JWT
//! 凭证不可行。本桥接走 S1b：**OpenAI 入站（chat/completions 与 responses 两种
//! 形态）→ 网关转换为 Anthropic messages → 复用 `forward_anthropic_json` 全套
//! 既有机器（Key 池轮询 / 状态机 / 有界失败转移 / Plan 通道变换 / 验证码）→
//! Anthropic 响应/SSE 反向转换为对应 OpenAI 形态**。
//!
//! 通道形态按请求跟随入站协议：OpenAI 入站走本桥接，Anthropic 入站维持
//! `claude.rs` 原生路径，二者共享同一池与同一转发内核。

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde_json::{json, Value};

use super::zai_anthropic::forward_anthropic_json;
use crate::proxy::config::ZaiConfig;
use crate::proxy::server::AppState;

const DEFAULT_MAX_TOKENS: u64 = 4096;

pub fn is_glm_model_name(model: &str) -> bool {
    let lower = model.to_lowercase();
    lower.starts_with("glm-") || lower.starts_with("zai:") || lower.starts_with("zcode:")
}

/// 路由闸门（与 claude.rs 的固定分发语义一致）：提供商开启 + 池非空 + GLM 系模型。
pub fn should_divert(zai: &ZaiConfig, model: &str) -> bool {
    zai.enabled && !zai.resolved_keys().is_empty() && is_glm_model_name(model)
}

// ---------------------------------------------------------------------------
// 请求转换：OpenAI → Anthropic messages
// ---------------------------------------------------------------------------

fn collect_system_text(body: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(s) = body.get("instructions").and_then(Value::as_str) {
        if !s.is_empty() {
            parts.push(s.to_string());
        }
    }
    if let Some(msgs) = body.get("messages").and_then(Value::as_array) {
        for m in msgs {
            if m.get("role").and_then(Value::as_str) == Some("system") {
                if let Some(t) = m.get("content").and_then(Value::as_str) {
                    if !t.is_empty() {
                        parts.push(t.to_string());
                    }
                }
            }
        }
    }
    parts.join("\n\n")
}

/// chat `messages[]` 中文本内容的宽松提取（string 或 content parts 数组）。
fn openai_content_to_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| {
                // chat content part: {type: "text"|"output_text"|"input_text", text: "..."}
                p.get("text").and_then(Value::as_str).map(str::to_string)
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn openai_tools_to_anthropic(body: &Value, anthropic: &mut Value) {
    // chat 形态: {type:"function", function:{name, description, parameters}}
    // responses 形态（扁平）: {type:"function", name, description, parameters}
    let tools = match body.get("tools").and_then(Value::as_array) {
        Some(t) => t,
        None => return,
    };
    let converted: Vec<Value> = tools
        .iter()
        .filter_map(|t| {
            let name = t
                .pointer("/function/name")
                .or_else(|| t.get("name"))
                .and_then(Value::as_str)?;
            let description = t
                .pointer("/function/description")
                .or_else(|| t.get("description"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let parameters = t
                .pointer("/function/parameters")
                .or_else(|| t.get("parameters"))
                .cloned()
                .unwrap_or_else(|| json!({"type": "object"}));
            Some(json!({
                "name": name,
                "description": description,
                "input_schema": parameters,
            }))
        })
        .collect();
    if !converted.is_empty() {
        anthropic["tools"] = Value::Array(converted);
    }
}

fn apply_common_generation_params(body: &Value, anthropic: &mut Value) {
    // Anthropic 必填 max_tokens；chat/responses 侧缺省时兜底
    let max_tokens = body
        .get("max_tokens")
        .or_else(|| body.get("max_output_tokens"))
        .and_then(Value::as_u64)
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_MAX_TOKENS);
    anthropic["max_tokens"] = json!(max_tokens);

    if let Some(t) = body.get("temperature").and_then(Value::as_f64) {
        anthropic["temperature"] = json!(t);
    }
    if let Some(t) = body.get("top_p").and_then(Value::as_f64) {
        anthropic["top_p"] = json!(t);
    }
    if let Some(stop) = body.get("stop").or_else(|| body.get("stop_sequences")) {
        match stop {
            Value::String(s) => anthropic["stop_sequences"] = json!([s]),
            Value::Array(arr) => {
                let seqs: Vec<&Value> = arr.iter().collect();
                if !seqs.is_empty() {
                    anthropic["stop_sequences"] = json!(seqs);
                }
            }
            _ => {}
        }
    }
    if let Some(stream) = body.get("stream").and_then(Value::as_bool) {
        anthropic["stream"] = json!(stream);
    }

    // tool_choice: chat 形态 {type: "auto"|"none"|"required"|"function", function?}；
    // responses 形态 "auto"|"none"|"required"|{type:"function", name}
    let choice = body.get("tool_choice");
    match choice {
        Some(Value::String(s)) => match s.as_str() {
            "none" => anthropic["tool_choice"] = json!({"type": "none"}),
            "required" => anthropic["tool_choice"] = json!({"type": "any"}),
            _ => {}
        },
        Some(Value::Object(_)) => {
            let ctype = choice.and_then(|c| c.get("type")).and_then(Value::as_str);
            match ctype {
                Some("none") => anthropic["tool_choice"] = json!({"type": "none"}),
                Some("required") => anthropic["tool_choice"] = json!({"type": "any"}),
                Some("auto") => anthropic["tool_choice"] = json!({"type": "auto"}),
                Some("function") | Some("tool") => {
                    let name = choice
                        .and_then(|c| c.pointer("/function/name").or_else(|| c.get("name")))
                        .and_then(Value::as_str);
                    if let Some(name) = name {
                        anthropic["tool_choice"] = json!({"type": "tool", "name": name});
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
}

/// chat/completions 请求体 → Anthropic messages 请求体。
/// tool 角色消息（工具结果）合并进紧随 tool_calls 的 user 消息（tool_result 块）。
pub fn convert_chat_request(body: &Value) -> Value {
    let mut anthropic = json!({
        "model": body.get("model").cloned().unwrap_or_else(|| json!("glm-5.3-flash")),
        "messages": [],
    });

    let system = collect_system_text(body);
    if !system.is_empty() {
        anthropic["system"] = json!(system);
    }

    let mut messages: Vec<Value> = Vec::new();
    if let Some(msgs) = body.get("messages").and_then(Value::as_array) {
        // 待归属的 tool_result 块：tool 消息必须并入下一条 user 消息
        let mut pending_tool_results: Vec<Value> = Vec::new();
        for m in msgs {
            let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
            match role {
                "system" => continue, // 已并入 system
                "tool" => {
                    let tool_use_id = m
                        .get("tool_call_id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let content = openai_content_to_text(
                        m.get("content").unwrap_or(&Value::String(String::new())),
                    );
                    pending_tool_results.push(json!({
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": content,
                    }));
                }
                "assistant" => {
                    if !pending_tool_results.is_empty() {
                        // Anthropic 约束：tool_result 必须紧跟含 tool_use 的 user 消息。
                        // 但此处 tool 结果出现在 assistant 消息之后（顺序异常），仍归入
                        // 一条合成的 user 消息以保住约束。
                        messages.push(json!({"role": "user", "content": pending_tool_results}));
                        pending_tool_results = Vec::new();
                    }
                    let mut blocks: Vec<Value> = Vec::new();
                    if let Some(text) = m.get("content").and_then(Value::as_str) {
                        if !text.is_empty() {
                            blocks.push(json!({"type": "text", "text": text}));
                        }
                    } else if let Some(text) = m.get("content").map(openai_content_to_text) {
                        if !text.is_empty() {
                            blocks.push(json!({"type": "text", "text": text}));
                        }
                    }
                    if let Some(tool_calls) = m.get("tool_calls").and_then(Value::as_array) {
                        for tc in tool_calls {
                            let id = tc.get("id").and_then(Value::as_str).unwrap_or("");
                            let name = tc
                                .pointer("/function/name")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            let args_raw = tc
                                .pointer("/function/arguments")
                                .and_then(Value::as_str)
                                .unwrap_or("{}");
                            let input: Value =
                                serde_json::from_str(args_raw).unwrap_or_else(|_| json!({}));
                            blocks.push(json!({
                                "type": "tool_use",
                                "id": id,
                                "name": name,
                                "input": input,
                            }));
                        }
                    }
                    if blocks.is_empty() {
                        blocks.push(json!({"type": "text", "text": " "}));
                    }
                    messages.push(json!({"role": "assistant", "content": blocks}));
                }
                _ => {
                    // user（含可能挂起的 tool_result：tool 结果必须并入下一条 user 消息）
                    let text = openai_content_to_text(
                        m.get("content").unwrap_or(&Value::String(String::new())),
                    );
                    let content: Value = if pending_tool_results.is_empty() {
                        if text.is_empty() {
                            json!(" ")
                        } else {
                            json!(text)
                        }
                    } else {
                        let mut blocks = std::mem::take(&mut pending_tool_results);
                        if !text.is_empty() {
                            blocks.push(json!({"type": "text", "text": text}));
                        }
                        json!(blocks)
                    };
                    messages.push(json!({"role": "user", "content": content}));
                }
            }
        }
        if !pending_tool_results.is_empty() {
            messages.push(json!({"role": "user", "content": pending_tool_results}));
        }
    }

    if messages.is_empty() {
        messages.push(json!({"role": "user", "content": " "}));
    }
    anthropic["messages"] = json!(messages);

    openai_tools_to_anthropic(body, &mut anthropic);
    apply_common_generation_params(body, &mut anthropic);
    anthropic
}

/// Responses（/v1/responses，Codex）请求体 → Anthropic messages 请求体。
/// 已知限制（T5 v1）：`reasoning` 条目跳过（不回灌历史推理）；`previous_response_id`
/// 服务端历史恢复不走本桥接（Codex 默认 store=false 全量发历史，不受影响）。
pub fn convert_responses_request(body: &Value) -> Value {
    let mut anthropic = json!({
        "model": body.get("model").cloned().unwrap_or_else(|| json!("glm-5.3-flash")),
        "messages": [],
    });

    let mut system = String::new();
    if let Some(s) = body.get("instructions").and_then(Value::as_str) {
        if !s.is_empty() {
            system = s.to_string();
        }
    }

    let mut messages: Vec<Value> = Vec::new();
    let mut push_message = |role: &str, blocks: Vec<Value>| {
        if blocks.is_empty() {
            return;
        }
        // 合并连续同角色消息（Anthropic 允许，但合并更稳）
        if let Some(last) = messages.last_mut() {
            if last.get("role").and_then(Value::as_str) == Some(role) {
                if let Some(arr) = last.get_mut("content").and_then(Value::as_array_mut) {
                    arr.extend(blocks);
                    return;
                }
            }
        }
        messages.push(json!({"role": role, "content": blocks}));
    };

    match body.get("input") {
        Some(Value::String(text)) => {
            if !text.is_empty() {
                push_message("user", vec![json!({"type": "text", "text": text})]);
            }
        }
        Some(Value::Array(items)) => {
            for item in items {
                let item_type = item
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("message");
                match item_type {
                    "message" => {
                        let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
                        let anthropic_role = if role == "assistant" {
                            "assistant"
                        } else {
                            "user"
                        };
                        let mut blocks: Vec<Value> = Vec::new();
                        match item.get("content") {
                            Some(Value::String(text)) => {
                                if !text.is_empty() {
                                    blocks.push(json!({"type": "text", "text": text}));
                                }
                            }
                            Some(Value::Array(parts)) => {
                                for p in parts {
                                    let ptype = p.get("type").and_then(Value::as_str).unwrap_or("");
                                    if ptype == "input_text"
                                        || ptype == "output_text"
                                        || ptype == "text"
                                    {
                                        if let Some(t) = p.get("text").and_then(Value::as_str) {
                                            if !t.is_empty() {
                                                blocks.push(json!({"type": "text", "text": t}));
                                            }
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                        push_message(anthropic_role, blocks);
                    }
                    "function_call" => {
                        let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                        let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or("");
                        let args_raw = item
                            .get("arguments")
                            .and_then(Value::as_str)
                            .unwrap_or("{}");
                        let input: Value =
                            serde_json::from_str(args_raw).unwrap_or_else(|_| json!({}));
                        push_message(
                            "assistant",
                            vec![json!({
                                "type": "tool_use",
                                "id": call_id,
                                "name": name,
                                "input": input,
                            })],
                        );
                    }
                    "function_call_output" => {
                        let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or("");
                        let output = match item.get("output") {
                            Some(Value::String(s)) => s.clone(),
                            Some(v) => v.to_string(),
                            None => String::new(),
                        };
                        push_message(
                            "user",
                            vec![json!({
                                "type": "tool_result",
                                "tool_use_id": call_id,
                                "content": output,
                            })],
                        );
                    }
                    "reasoning" => {
                        // T5 v1 限制：跳过历史推理条目
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }

    if messages.is_empty() {
        messages.push(json!({"role": "user", "content": " "}));
    }

    anthropic["messages"] = json!(messages);
    if !system.is_empty() {
        anthropic["system"] = json!(system);
    }
    openai_tools_to_anthropic(body, &mut anthropic);
    apply_common_generation_params(body, &mut anthropic);
    anthropic
}

// ---------------------------------------------------------------------------
// 响应转换：Anthropic → OpenAI（chat 与 responses 两种形态）
// ---------------------------------------------------------------------------

fn anthropic_stop_reason_to_chat(stop_reason: Option<&str>) -> Option<&'static str> {
    match stop_reason {
        Some("tool_use") => Some("tool_calls"),
        Some("max_tokens") => Some("length"),
        Some("stop_sequence") | Some("end_turn") => Some("stop"),
        _ => None,
    }
}

fn anthropic_text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| {
                if b.get("type").and_then(Value::as_str) == Some("text") {
                    b.get("text").and_then(Value::as_str)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn anthropic_tool_calls_of(content: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    if let Value::Array(blocks) = content {
        for b in blocks {
            if b.get("type").and_then(Value::as_str) == Some("tool_use") {
                out.push(json!({
                    "id": b.get("id").cloned().unwrap_or_else(|| json!("")),
                    "type": "function",
                    "function": {
                        "name": b.get("name").cloned().unwrap_or_else(|| json!("")),
                        "arguments": serde_json::to_string(
                            b.get("input").unwrap_or(&json!({}))
                        ).unwrap_or_else(|_| "{}".to_string()),
                    },
                }));
            }
        }
    }
    out
}

/// Anthropic 非流式响应 → OpenAI chat.completion 对象。
pub fn convert_anthropic_json_to_chat(anthropic: &Value, requested_model: &str) -> Value {
    let content = anthropic
        .get("content")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let text = anthropic_text_of(&content);
    let tool_calls = anthropic_tool_calls_of(&content);
    let stop_reason =
        anthropic_stop_reason_to_chat(anthropic.get("stop_reason").and_then(Value::as_str));

    let mut message = json!({"role": "assistant"});
    if !text.is_empty() {
        message["content"] = json!(text);
    } else {
        message["content"] = Value::Null;
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = json!(tool_calls);
    }

    let mut usage = json!({});
    if let Some(u) = anthropic.get("usage") {
        usage = json!({
            "prompt_tokens": u.get("input_tokens").cloned().unwrap_or_else(|| json!(0)),
            "completion_tokens": u.get("output_tokens").cloned().unwrap_or_else(|| json!(0)),
            "total_tokens": u.get("input_tokens").cloned().unwrap_or_else(|| json!(0)).as_u64().unwrap_or(0)
                + u.get("output_tokens").cloned().unwrap_or_else(|| json!(0)).as_u64().unwrap_or(0),
        });
    }

    json!({
        "id": anthropic.get("id").cloned().unwrap_or_else(|| json!("chatcmpl-zai")),
        "object": "chat.completion",
        "created": chrono::Utc::now().timestamp(),
        "model": requested_model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": stop_reason,
        }],
        "usage": usage,
    })
}

/// Anthropic 非流式响应 → Responses 对象（/v1/responses 非流式返回体）。
pub fn convert_anthropic_json_to_responses(anthropic: &Value, requested_model: &str) -> Value {
    let content = anthropic
        .get("content")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let mut output: Vec<Value> = Vec::new();

    let text = anthropic_text_of(&content);
    if !text.is_empty() {
        output.push(json!({
            "type": "message",
            "id": format!("msg_{}", anthropic.get("id").cloned().unwrap_or_else(|| json!("msg")).as_str().unwrap_or("msg")),
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": text, "annotations": []}],
        }));
    }
    if let Value::Array(blocks) = &content {
        for b in blocks {
            if b.get("type").and_then(Value::as_str) == Some("tool_use") {
                output.push(json!({
                    "type": "function_call",
                    "id": format!("fc_{}", b.get("id").cloned().unwrap_or_else(|| json!("")).as_str().unwrap_or("")),
                    "call_id": b.get("id").cloned().unwrap_or_else(|| json!("")),
                    "name": b.get("name").cloned().unwrap_or_else(|| json!("")),
                    "arguments": serde_json::to_string(b.get("input").unwrap_or(&json!({})))
                        .unwrap_or_else(|_| "{}".to_string()),
                    "status": "completed",
                }));
            }
        }
    }

    let usage = match anthropic.get("usage") {
        Some(u) => json!({
            "input_tokens": u.get("input_tokens").cloned().unwrap_or_else(|| json!(0)),
            "output_tokens": u.get("output_tokens").cloned().unwrap_or_else(|| json!(0)),
            "total_tokens": u.get("input_tokens").cloned().unwrap_or_else(|| json!(0)).as_u64().unwrap_or(0)
                + u.get("output_tokens").cloned().unwrap_or_else(|| json!(0)).as_u64().unwrap_or(0),
        }),
        None => json!({"input_tokens": 0, "output_tokens": 0, "total_tokens": 0}),
    };

    json!({
        "id": format!("resp_{}", uuid::Uuid::new_v4()),
        "object": "response",
        "created_at": chrono::Utc::now().timestamp(),
        "status": "completed",
        "model": requested_model,
        "output": output,
        "usage": usage,
        "parallel_tool_calls": true,
        "tool_choice": "auto",
        "tools": [],
    })
}

// ---------------------------------------------------------------------------
// 流式转换：Anthropic SSE → OpenAI 形态 SSE
// ---------------------------------------------------------------------------

/// 流式转换状态机（chat 块流 / responses 事件流共用解析，分发展现）。
#[derive(Default)]
pub struct SseBridgeState {
    pub response_id: String,
    pub model: String,
    pub stop_reason: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// 已输出的 Responses output 条目（assembled for response.completed）
    pub output_items: Vec<Value>,
    /// 当前 content block
    pub current_block_type: Option<String>,
    pub tool_call_index: usize,
    pub current_item_index: usize,
    pub current_tool_call_id: String,
    pub current_tool_name: String,
    pub tool_arguments: String,
    pub text_accumulated: String,
    pub sequence: u64,
}

impl SseBridgeState {
    pub fn new() -> Self {
        Self::default()
    }

    fn next_seq(&mut self) -> u64 {
        self.sequence += 1;
        self.sequence
    }

    /// 处理一条 Anthropic SSE 事件（event 名 + data JSON），产出 OpenAI 形态事件。
    /// `shape`：true = responses 事件流，false = chat 块流。
    pub fn ingest(
        &mut self,
        event: &str,
        data: &Value,
        shape_responses: bool,
    ) -> Vec<(String, Value)> {
        let mut out: Vec<(String, Value)> = Vec::new();
        match event {
            "message_start" => {
                if let Some(msg) = data.get("message") {
                    self.response_id = msg
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("msg_zai")
                        .to_string();
                    self.model = msg
                        .get("model")
                        .and_then(Value::as_str)
                        .unwrap_or("glm")
                        .to_string();
                    if let Some(u) = msg.get("usage") {
                        self.input_tokens =
                            u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
                        self.output_tokens =
                            u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
                    }
                }
                if shape_responses {
                    let response_shell = json!({
                        "id": format!("resp_{}", uuid::Uuid::new_v4()),
                        "object": "response",
                        "created_at": chrono::Utc::now().timestamp(),
                        "status": "in_progress",
                        "model": self.model,
                        "output": [],
                    });
                    out.push((
                        "response.created".to_string(),
                        json!({"type": "response.created", "sequence_number": self.next_seq(), "response": response_shell.clone()}),
                    ));
                    out.push((
                        "response.in_progress".to_string(),
                        json!({"type": "response.in_progress", "sequence_number": self.next_seq(), "response": response_shell}),
                    ));
                } else {
                    out.push((
                        "message".to_string(),
                        json!({
                            "id": format!("chatcmpl-{}", self.response_id),
                            "object": "chat.completion.chunk",
                            "created": chrono::Utc::now().timestamp(),
                            "model": self.model,
                            "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": null}],
                        }),
                    ));
                }
            }
            "content_block_start" => {
                let block = data
                    .get("content_block")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let btype = block
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("text")
                    .to_string();
                self.current_block_type = Some(btype.clone());
                self.text_accumulated = String::new();
                self.tool_arguments = String::new();
                match btype.as_str() {
                    "tool_use" => {
                        self.current_tool_call_id = block
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        self.current_tool_name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        self.tool_call_index += 1;
                        self.current_item_index += 1;
                        if shape_responses {
                            out.push((
                                "response.output_item.added".to_string(),
                                json!({
                                    "type": "response.output_item.added",
                                    "sequence_number": self.next_seq(),
                                    "output_index": self.current_item_index,
                                    "item": {
                                        "type": "function_call",
                                        "id": format!("fc_{}", self.current_tool_call_id),
                                        "call_id": self.current_tool_call_id,
                                        "name": self.current_tool_name,
                                        "arguments": "",
                                        "status": "in_progress",
                                    },
                                }),
                            ));
                        } else {
                            out.push((
                                "message".to_string(),
                                json!({
                                    "id": format!("chatcmpl-{}", self.response_id),
                                    "object": "chat.completion.chunk",
                                    "created": chrono::Utc::now().timestamp(),
                                    "model": self.model,
                                    "choices": [{
                                        "index": 0,
                                        "delta": {"tool_calls": [{
                                            "index": self.tool_call_index - 1,
                                            "id": self.current_tool_call_id,
                                            "type": "function",
                                            "function": {"name": self.current_tool_name, "arguments": ""},
                                        }]},
                                        "finish_reason": null,
                                    }],
                                }),
                            ));
                        }
                    }
                    "text" => {
                        if shape_responses {
                            out.push((
                                "response.output_item.added".to_string(),
                                json!({
                                    "type": "response.output_item.added",
                                    "sequence_number": self.next_seq(),
                                    "output_index": self.current_item_index,
                                    "item": {
                                        "type": "message",
                                        "id": format!("msg_{}", self.response_id),
                                        "role": "assistant",
                                        "status": "in_progress",
                                        "content": [],
                                    },
                                }),
                            ));
                            out.push((
                                "response.content_part.added".to_string(),
                                json!({
                                    "type": "response.content_part.added",
                                    "sequence_number": self.next_seq(),
                                    "item_id": format!("msg_{}", self.response_id),
                                    "output_index": self.current_item_index,
                                    "content_index": 0,
                                    "part": {"type": "output_text", "text": "", "annotations": []},
                                }),
                            ));
                        }
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let delta = data.get("delta").cloned().unwrap_or_else(|| json!({}));
                let dtype = delta.get("type").and_then(Value::as_str).unwrap_or("");
                match dtype {
                    "text_delta" => {
                        let text = delta.get("text").and_then(Value::as_str).unwrap_or("");
                        self.text_accumulated.push_str(text);
                        if shape_responses {
                            out.push((
                                "response.output_text.delta".to_string(),
                                json!({
                                    "type": "response.output_text.delta",
                                    "sequence_number": self.next_seq(),
                                    "item_id": format!("msg_{}", self.response_id),
                                    "output_index": self.current_item_index,
                                    "content_index": 0,
                                    "delta": text,
                                }),
                            ));
                        } else {
                            out.push((
                                "message".to_string(),
                                json!({
                                    "id": format!("chatcmpl-{}", self.response_id),
                                    "object": "chat.completion.chunk",
                                    "created": chrono::Utc::now().timestamp(),
                                    "model": self.model,
                                    "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}],
                                }),
                            ));
                        }
                    }
                    "input_json_delta" => {
                        let partial = delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        self.tool_arguments.push_str(partial);
                        if shape_responses {
                            out.push((
                                "response.function_call_arguments.delta".to_string(),
                                json!({
                                    "type": "response.function_call_arguments.delta",
                                    "sequence_number": self.next_seq(),
                                    "item_id": format!("fc_{}", self.current_tool_call_id),
                                    "output_index": self.current_item_index,
                                    "delta": partial,
                                }),
                            ));
                        } else {
                            out.push((
                                "message".to_string(),
                                json!({
                                    "id": format!("chatcmpl-{}", self.response_id),
                                    "object": "chat.completion.chunk",
                                    "created": chrono::Utc::now().timestamp(),
                                    "model": self.model,
                                    "choices": [{
                                        "index": 0,
                                        "delta": {"tool_calls": [{
                                            "index": self.tool_call_index - 1,
                                            "function": {"arguments": partial},
                                        }]},
                                        "finish_reason": null,
                                    }],
                                }),
                            ));
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let btype = self.current_block_type.take().unwrap_or_default();
                match btype.as_str() {
                    "text" => {
                        let text = std::mem::take(&mut self.text_accumulated);
                        self.output_items.push(json!({
                            "type": "message",
                            "id": format!("msg_{}", self.response_id),
                            "role": "assistant",
                            "status": "completed",
                            "content": [{"type": "output_text", "text": text, "annotations": []}],
                        }));
                        if shape_responses {
                            out.push((
                                "response.output_item.done".to_string(),
                                json!({
                                    "type": "response.output_item.done",
                                    "sequence_number": self.next_seq(),
                                    "output_index": self.current_item_index,
                                    "item": self.output_items.last().cloned().unwrap_or_else(|| json!({})),
                                }),
                            ));
                            self.current_item_index += 1;
                        }
                    }
                    "tool_use" => {
                        let args = std::mem::take(&mut self.tool_arguments);
                        let item = json!({
                            "type": "function_call",
                            "id": format!("fc_{}", self.current_tool_call_id),
                            "call_id": self.current_tool_call_id,
                            "name": self.current_tool_name,
                            "arguments": if args.is_empty() { "{}".to_string() } else { args },
                            "status": "completed",
                        });
                        self.output_items.push(item.clone());
                        if shape_responses {
                            out.push((
                                "response.function_call_arguments.done".to_string(),
                                json!({
                                    "type": "response.function_call_arguments.done",
                                    "sequence_number": self.next_seq(),
                                    "item_id": format!("fc_{}", self.current_tool_call_id),
                                    "output_index": self.current_item_index,
                                    "arguments": item["arguments"].clone(),
                                }),
                            ));
                            out.push((
                                "response.output_item.done".to_string(),
                                json!({
                                    "type": "response.output_item.done",
                                    "sequence_number": self.next_seq(),
                                    "output_index": self.current_item_index,
                                    "item": item,
                                }),
                            ));
                        }
                        // chat 形态的 tool 调用收尾在 message_delta 的 finish_reason 中体现
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(delta) = data.get("delta") {
                    if let Some(sr) = delta.get("stop_reason").and_then(Value::as_str) {
                        self.stop_reason = Some(sr.to_string());
                    }
                }
                if let Some(u) = data.get("usage") {
                    // z.ai 上游在 message_delta.usage 回传权威 input/output（message_start 为 0 占位）
                    if let Some(i) = u.get("input_tokens").and_then(Value::as_u64) {
                        self.input_tokens = i;
                    }
                    if let Some(o) = u.get("output_tokens").and_then(Value::as_u64) {
                        self.output_tokens = o;
                    }
                }
                if !shape_responses {
                    let finish = anthropic_stop_reason_to_chat(self.stop_reason.as_deref());
                    out.push((
                        "message".to_string(),
                        json!({
                            "id": format!("chatcmpl-{}", self.response_id),
                            "object": "chat.completion.chunk",
                            "created": chrono::Utc::now().timestamp(),
                            "model": self.model,
                            "choices": [{"index": 0, "delta": {}, "finish_reason": finish}],
                        }),
                    ));
                }
            }
            "message_stop" => {
                if shape_responses {
                    let completed = json!({
                        "id": format!("resp_{}", uuid::Uuid::new_v4()),
                        "object": "response",
                        "created_at": chrono::Utc::now().timestamp(),
                        "status": "completed",
                        "model": self.model,
                        "output": self.output_items.clone(),
                        "usage": {
                            "input_tokens": self.input_tokens,
                            "output_tokens": self.output_tokens,
                            "total_tokens": self.input_tokens + self.output_tokens,
                        },
                    });
                    out.push((
                        "response.completed".to_string(),
                        json!({"type": "response.completed", "sequence_number": self.next_seq(), "response": completed}),
                    ));
                } else {
                    // OpenAI chat 形态收尾补发 usage 块（空 choices + 顶层 usage，为
                    // 监控中间件的 token 统计提供上游权威数值，避免退化为请求体估算）
                    if self.input_tokens > 0 || self.output_tokens > 0 {
                        out.push((
                            "message".to_string(),
                            json!({
                                "id": format!("chatcmpl-{}", self.response_id),
                                "object": "chat.completion.chunk",
                                "created": chrono::Utc::now().timestamp(),
                                "model": self.model,
                                "choices": [],
                                "usage": {
                                    "prompt_tokens": self.input_tokens,
                                    "completion_tokens": self.output_tokens,
                                    "total_tokens": self.input_tokens + self.output_tokens,
                                },
                            }),
                        ));
                    }
                    out.push(("done".to_string(), json!("[DONE]")));
                }
            }
            "error" => {
                // 上游错误事件透传（保持失败语义；chat 形态以 data 行输出，responses 以 error 事件输出）
                out.push((
                    if shape_responses { "error".to_string() } else { "message".to_string() },
                    if shape_responses {
                        json!({"type": "error", "sequence_number": self.next_seq(), "data": data.clone()})
                    } else {
                        json!({"error": data.clone()})
                    },
                ));
            }
            "ping" | _ => {}
        }
        out
    }
}

// ---------------------------------------------------------------------------
// 入口：分流 → 转换 → 复用 forward_anthropic_json → 反向转换
// ---------------------------------------------------------------------------

/// 从字节流中增量解析 Anthropic SSE 帧（以空行分隔），返回完整帧缓冲。
/// 有状态：跨 chunk 保留残帧。
struct SseFrameSplitter {
    buffer: Vec<u8>,
}

impl SseFrameSplitter {
    fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    /// 喂入新字节，返回已完整成帧的（event 名, data JSON）序列。
    fn feed(&mut self, bytes: &[u8]) -> Vec<(String, Value)> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        loop {
            let Some(pos) = find_frame_end(&self.buffer) else {
                break;
            };
            let frame: Vec<u8> = self.buffer.drain(..=pos).collect();
            if let Some(ev) = parse_sse_frame(&frame) {
                events.push(ev);
            }
        }
        events
    }
}

/// 帧以 `\n\n`（或 `\r\n\r\n`）结束；返回帧结束字节（含分隔符）的下标。
fn find_frame_end(buf: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some(i + 1);
        }
        if buf[i] == b'\r' && i + 3 < buf.len() && &buf[i..i + 4] == b"\r\n\r\n" {
            return Some(i + 3);
        }
        i += 1;
    }
    None
}

/// 解析单个 SSE 帧：取 `event:` 行与 `data:` 行。
fn parse_sse_frame(frame: &[u8]) -> Option<(String, Value)> {
    let text = String::from_utf8_lossy(frame);
    let mut event = String::new();
    let mut data = String::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            event = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim_start());
        }
    }
    if data.is_empty() {
        return None;
    }
    let parsed: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
    Some((event, parsed))
}

fn serialize_openai_sse(events: &[(String, Value)], shape_responses: bool) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, data) in events {
        if !shape_responses {
            if name == "done" {
                out.extend_from_slice(b"data: [DONE]\n\n");
            } else {
                out.extend_from_slice(format!("data: {}\n\n", data).as_bytes());
            }
        } else {
            out.extend_from_slice(format!("event: {}\ndata: {}\n\n", name, data).as_bytes());
        }
    }
    out
}

/// [zcode T5] 流量监控归因头透传：桥接重建响应时保留内层 zai 通道的
/// `X-Account-Email` / `X-Mapped-Model`，保证 OpenAI 协议入口的流量监控与 Anthropic 入口等价。
fn copy_monitor_headers(
    mut builder: axum::http::response::Builder,
    from: &axum::http::HeaderMap,
) -> axum::http::response::Builder {
    for name in ["x-account-email", "x-mapped-model"] {
        if let Some(v) = from.get(name) {
            builder = builder.header(name, v.clone());
        }
    }
    builder
}

async fn forward_via_anthropic(
    state: &AppState,
    headers: &HeaderMap,
    anthropic_body: Value,
    requested_model: &str,
    shape_responses: bool,
) -> Response {
    let message_count = anthropic_body
        .get("messages")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
    let resp = forward_anthropic_json(
        state,
        Method::POST,
        "/v1/messages",
        headers,
        anthropic_body,
        message_count,
    )
    .await;

    let status = resp.status();
    if status.is_client_error() || status.is_server_error() {
        // 上游错误原样透传（状态码 + 原始体），失败语义不被桥接吞掉
        return resp;
    }

    let inner_headers = resp.headers().clone();
    let is_sse = resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|c| c.contains("text/event-stream"))
        .unwrap_or(false);

    if is_sse {
        let mut bridge = SseBridgeState::new();
        let mut splitter = SseFrameSplitter::new();
        let mut upstream = resp.into_body().into_data_stream();
        let out = async_stream::stream! {
            yield Ok::<Bytes, std::io::Error>(Bytes::new());
            while let Some(chunk) = upstream.next().await {
                let bytes = match chunk {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!("[zai-openai-bridge] upstream stream error: {}", e);
                        break;
                    }
                };
                for (event, data) in splitter.feed(&bytes) {
                    let events = bridge.ingest(&event, &data, shape_responses);
                    let serialized = serialize_openai_sse(&events, shape_responses);
                    if !serialized.is_empty() {
                        yield Ok(Bytes::from(serialized));
                    }
                }
            }
            // 冲刷残帧 + 收尾
            for (event, data) in splitter.feed(&[]) {
                let events = bridge.ingest(&event, &data, shape_responses);
                let serialized = serialize_openai_sse(&events, shape_responses);
                if !serialized.is_empty() {
                    yield Ok(Bytes::from(serialized));
                }
            }
            if !shape_responses {
                yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
            }
        };
        copy_monitor_headers(
            Response::builder()
                .status(StatusCode::OK)
                .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
                .header(axum::http::header::CACHE_CONTROL, "no-cache"),
            &inner_headers,
        )
        .body(Body::from_stream(out))
        .unwrap_or_else(|_| {
            (StatusCode::INTERNAL_SERVER_ERROR, "stream build failed").into_response()
        })
    } else {
        let bytes = match axum::body::to_bytes(resp.into_body(), 64 * 1024 * 1024).await {
            Ok(b) => b,
            Err(e) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    format!("z.ai response read failed: {}", e),
                )
                    .into_response()
            }
        };
        let anthropic: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => {
                // 非 JSON 体原样透传
                return copy_monitor_headers(
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(axum::http::header::CONTENT_TYPE, "application/json"),
                    &inner_headers,
                )
                .body(Body::from(bytes))
                .unwrap_or_else(|_| {
                    (StatusCode::INTERNAL_SERVER_ERROR, "body build failed").into_response()
                });
            }
        };
        let converted = if shape_responses {
            convert_anthropic_json_to_responses(&anthropic, requested_model)
        } else {
            convert_anthropic_json_to_chat(&anthropic, requested_model)
        };
        copy_monitor_headers(
            Response::builder()
                .status(StatusCode::OK)
                .header(axum::http::header::CONTENT_TYPE, "application/json"),
            &inner_headers,
        )
        .body(Body::from(
            serde_json::to_vec(&converted).unwrap_or_default(),
        ))
        .unwrap_or_else(|_| {
            (StatusCode::INTERNAL_SERVER_ERROR, "body build failed").into_response()
        })
    }
}

// ---------------------------------------------------------------------------
// 对外入口
// ---------------------------------------------------------------------------

/// /v1/chat/completions（含误投的 Responses 形态）→ z.ai 通道。
pub async fn forward_chat(state: &AppState, headers: HeaderMap, body: Value) -> Response {
    let requested_model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("glm-5.3-flash")
        .to_string();
    let is_responses_shape = body.get("messages").is_none() && body.get("input").is_some();
    let anthropic_body = if is_responses_shape {
        convert_responses_request(&body)
    } else {
        convert_chat_request(&body)
    };
    forward_via_anthropic(state, &headers, anthropic_body, &requested_model, false).await
}

/// /v1/responses（Codex）→ z.ai 通道。
pub async fn forward_responses(state: &AppState, headers: HeaderMap, body: Value) -> Response {
    let requested_model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("glm-5.3-flash")
        .to_string();
    let anthropic_body = convert_responses_request(&body);
    forward_via_anthropic(state, &headers, anthropic_body, &requested_model, true).await
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_copy_monitor_headers_propagates_attribution() {
        let mut inner = axum::http::HeaderMap::new();
        inner.insert(
            "x-account-email",
            axum::http::HeaderValue::from_static("owner@example.com"),
        );
        inner.insert(
            "x-mapped-model",
            axum::http::HeaderValue::from_static("GLM-5.3-Flash"),
        );

        let resp = copy_monitor_headers(Response::builder(), &inner)
            .status(StatusCode::OK)
            .body(())
            .unwrap();
        assert_eq!(
            resp.headers().get("x-account-email").unwrap(),
            "owner@example.com"
        );
        assert_eq!(
            resp.headers().get("x-mapped-model").unwrap(),
            "GLM-5.3-Flash"
        );

        // 内层无归因头时不引入空值
        let resp = copy_monitor_headers(Response::builder(), &axum::http::HeaderMap::new())
            .body(())
            .unwrap();
        assert!(resp.headers().get("x-account-email").is_none());
        assert!(resp.headers().get("x-mapped-model").is_none());
    }

    #[test]
    fn test_is_glm_model_name() {
        assert!(is_glm_model_name("glm-5.3-flash"));
        assert!(is_glm_model_name("GLM-4.6V"));
        assert!(is_glm_model_name("GLM-zero-preview"));
        assert!(is_glm_model_name("zai:glm-4.7"));
        assert!(is_glm_model_name("zcode:plan"));
        assert!(!is_glm_model_name("claude-3-7-sonnet"));
        assert!(!is_glm_model_name("gemini-2.5-pro"));
        assert!(!is_glm_model_name("gpt-4o"));
    }

    #[test]
    fn test_should_divert() {
        use crate::proxy::config::{ZaiConfig, ZaiKeyEntry, ZaiKeyMode, ZaiProvider};

        let mut zai = ZaiConfig {
            enabled: false,
            keys: vec![],
            ..Default::default()
        };
        // 提供商未启用
        assert!(!should_divert(&zai, "glm-5.3-flash"));

        // 提供商启用但池为空
        zai.enabled = true;
        assert!(!should_divert(&zai, "glm-5.3-flash"));

        // 提供商启用且池非空
        zai.keys.push(ZaiKeyEntry {
            key: "test-key".to_string(),
            provider: ZaiProvider::Zai,
            enabled: true,
            label: "test".to_string(),
            mode: ZaiKeyMode::ApiKey,
            account_id: String::new(),
            user_email: String::new(),
            business_jwt: String::new(),
            device_profile: serde_json::Value::Null,
        });
        assert!(should_divert(&zai, "glm-5.3-flash"));
        assert!(should_divert(&zai, "GLM-4.6V"));
        // 非 GLM 模型不分流
        assert!(!should_divert(&zai, "claude-3-7-sonnet"));
        assert!(!should_divert(&zai, "gemini-2.5-flash"));
    }

    #[test]
    fn test_convert_chat_request_basic() {
        let chat_body = json!({
            "model": "glm-5.3-flash",
            "messages": [
                {"role": "system", "content": "You are a helpful assistant."},
                {"role": "user", "content": "Hello!"},
                {"role": "assistant", "content": "Hi there!"},
                {"role": "user", "content": "How are you?"}
            ],
            "max_tokens": 1024,
            "temperature": 0.7,
            "top_p": 0.9,
            "stream": true
        });

        let anthropic = convert_chat_request(&chat_body);
        assert_eq!(anthropic["model"], "glm-5.3-flash");
        assert_eq!(anthropic["system"], "You are a helpful assistant.");
        assert_eq!(anthropic["max_tokens"], 1024);
        assert_eq!(anthropic["temperature"], 0.7);
        assert_eq!(anthropic["top_p"], 0.9);
        assert_eq!(anthropic["stream"], true);

        let messages = anthropic["messages"].as_array().unwrap();
        // system 消息被提取到 system 字段，messages[] 剩余 3 条
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "Hello!");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"], "How are you?");
    }

    #[test]
    fn test_convert_chat_request_tools_and_tool_results() {
        let chat_body = json!({
            "model": "glm-5.3-flash",
            "messages": [
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_123",
                        "type": "function",
                        "function": {
                            "name": "get_weather",
                            "arguments": "{\"location\":\"Beijing\"}"
                        }
                    }]
                },
                {
                    "role": "tool",
                    "tool_call_id": "call_123",
                    "content": "Sunny, 25C"
                },
                {
                    "role": "user",
                    "content": "What is the weather?"
                }
            ],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get current weather",
                    "parameters": {
                        "type": "object",
                        "properties": {"location": {"type": "string"}},
                        "required": ["location"]
                    }
                }
            }],
            "tool_choice": "required"
        });

        let anthropic = convert_chat_request(&chat_body);

        // tool 定义转换
        let tools = anthropic["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "get_weather");
        assert_eq!(tools[0]["description"], "Get current weather");
        assert_eq!(tools[0]["input_schema"]["type"], "object");

        // tool_choice
        assert_eq!(anthropic["tool_choice"]["type"], "any");

        // 验证 assistant 的 tool_use 与 user 的 tool_result 合并
        let messages = anthropic["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "assistant");
        let assistant_blocks = messages[0]["content"].as_array().unwrap();
        assert_eq!(assistant_blocks[0]["type"], "tool_use");
        assert_eq!(assistant_blocks[0]["id"], "call_123");
        assert_eq!(assistant_blocks[0]["name"], "get_weather");
        assert_eq!(assistant_blocks[0]["input"]["location"], "Beijing");

        assert_eq!(messages[1]["role"], "user");
        let user_blocks = messages[1]["content"].as_array().unwrap();
        assert_eq!(user_blocks[0]["type"], "tool_result");
        assert_eq!(user_blocks[0]["tool_use_id"], "call_123");
        assert_eq!(user_blocks[0]["content"], "Sunny, 25C");
        assert_eq!(user_blocks[1]["type"], "text");
        assert_eq!(user_blocks[1]["text"], "What is the weather?");
    }

    #[test]
    fn test_convert_responses_request_basic() {
        let resp_body = json!({
            "model": "glm-5.3-flash",
            "instructions": "Be concise.",
            "input": "Calculate 1 + 1",
            "max_output_tokens": 512
        });

        let anthropic = convert_responses_request(&resp_body);
        assert_eq!(anthropic["model"], "glm-5.3-flash");
        assert_eq!(anthropic["system"], "Be concise.");
        assert_eq!(anthropic["max_tokens"], 512);

        let messages = anthropic["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        let content_blocks = messages[0]["content"].as_array().unwrap();
        assert_eq!(content_blocks[0]["text"], "Calculate 1 + 1");
    }

    #[test]
    fn test_convert_responses_request_function_call_and_output() {
        let resp_body = json!({
            "model": "glm-5.3-flash",
            "instructions": "System instruction",
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "Run command"}]
                },
                {
                    "type": "function_call",
                    "call_id": "call_abc",
                    "name": "bash",
                    "arguments": "{\"command\":\"ls\"}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_abc",
                    "output": "file1.txt\nfile2.txt"
                }
            ]
        });

        let anthropic = convert_responses_request(&resp_body);
        let messages = anthropic["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);

        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "assistant");
        let assistant_blocks = messages[1]["content"].as_array().unwrap();
        assert_eq!(assistant_blocks[0]["type"], "tool_use");
        assert_eq!(assistant_blocks[0]["id"], "call_abc");
        assert_eq!(assistant_blocks[0]["name"], "bash");

        assert_eq!(messages[2]["role"], "user");
        let user_blocks = messages[2]["content"].as_array().unwrap();
        assert_eq!(user_blocks[0]["type"], "tool_result");
        assert_eq!(user_blocks[0]["tool_use_id"], "call_abc");
        assert_eq!(user_blocks[0]["content"], "file1.txt\nfile2.txt");
    }

    #[test]
    fn test_convert_anthropic_json_to_chat() {
        let anthropic_resp = json!({
            "id": "msg_01X9",
            "type": "message",
            "role": "assistant",
            "content": [
                {"type": "text", "text": "Hello, world!"}
            ],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 15,
                "output_tokens": 5
            }
        });

        let chat = convert_anthropic_json_to_chat(&anthropic_resp, "glm-5.3-flash");
        assert_eq!(chat["object"], "chat.completion");
        assert_eq!(chat["model"], "glm-5.3-flash");
        assert_eq!(chat["choices"][0]["message"]["content"], "Hello, world!");
        assert_eq!(chat["choices"][0]["finish_reason"], "stop");
        assert_eq!(chat["usage"]["prompt_tokens"], 15);
        assert_eq!(chat["usage"]["completion_tokens"], 5);
        assert_eq!(chat["usage"]["total_tokens"], 20);
    }

    #[test]
    fn test_convert_anthropic_json_to_responses() {
        let anthropic_resp = json!({
            "id": "msg_01X9",
            "type": "message",
            "role": "assistant",
            "content": [
                {"type": "text", "text": "I can help with that."},
                {
                    "type": "tool_use",
                    "id": "call_456",
                    "name": "lookup",
                    "input": {"query": "rust"}
                }
            ],
            "stop_reason": "tool_use",
            "usage": {
                "input_tokens": 25,
                "output_tokens": 12
            }
        });

        let responses = convert_anthropic_json_to_responses(&anthropic_resp, "glm-5.3-flash");
        assert_eq!(responses["object"], "response");
        assert_eq!(responses["status"], "completed");
        assert_eq!(responses["model"], "glm-5.3-flash");

        let output = responses["output"].as_array().unwrap();
        assert_eq!(output.len(), 2);
        assert_eq!(output[0]["type"], "message");
        assert_eq!(output[0]["content"][0]["text"], "I can help with that.");
        assert_eq!(output[1]["type"], "function_call");
        assert_eq!(output[1]["call_id"], "call_456");
        assert_eq!(output[1]["name"], "lookup");
        assert_eq!(output[1]["arguments"], "{\"query\":\"rust\"}");
    }

    #[test]
    fn test_sse_bridge_chat_streaming() {
        let mut state = SseBridgeState::new();

        // 1. message_start
        let events = state.ingest(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": "msg_abc",
                    "model": "glm-5.3-flash",
                    "role": "assistant",
                    "usage": {"input_tokens": 10, "output_tokens": 0}
                }
            }),
            false,
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].1["object"], "chat.completion.chunk");
        assert_eq!(events[0].1["choices"][0]["delta"]["role"], "assistant");

        // 2. content_block_start (text)
        state.ingest(
            "content_block_start",
            &json!({"type": "content_block_start", "content_block": {"type": "text", "text": ""}}),
            false,
        );

        // 3. content_block_delta (text_delta)
        let delta_events = state.ingest(
            "content_block_delta",
            &json!({"type": "content_block_delta", "delta": {"type": "text_delta", "text": "Hello"}}),
            false,
        );
        assert_eq!(delta_events.len(), 1);
        assert_eq!(delta_events[0].1["choices"][0]["delta"]["content"], "Hello");

        // 4. message_delta (stop_reason: end_turn, 权威 usage 刷新 input)
        let finish_events = state.ingest(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn"},
                "usage": {"input_tokens": 12, "output_tokens": 5}
            }),
            false,
        );
        assert_eq!(finish_events.len(), 1);
        assert_eq!(finish_events[0].1["choices"][0]["finish_reason"], "stop");

        // 5. message_stop -> 收尾 usage 块（12 in + 5 out）+ [DONE]
        let stop_events = state.ingest("message_stop", &json!({"type": "message_stop"}), false);
        assert_eq!(stop_events.len(), 2);
        assert!(stop_events[0].1["choices"].as_array().unwrap().is_empty());
        assert_eq!(stop_events[0].1["usage"]["prompt_tokens"], 12);
        assert_eq!(stop_events[0].1["usage"]["completion_tokens"], 5);
        assert_eq!(stop_events[0].1["usage"]["total_tokens"], 17);
        assert_eq!(stop_events[1].0, "done");
        assert_eq!(stop_events[1].1, json!("[DONE]"));
    }

    #[test]
    fn test_sse_bridge_chat_streaming_without_usage_skips_usage_chunk() {
        let mut state = SseBridgeState::new();
        // 未收到任何上游 usage 时不得虚构 0 值 usage 块
        state.ingest(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {"id": "msg_x", "model": "glm-5.3-flash", "usage": {}}
            }),
            false,
        );
        let stop_events = state.ingest("message_stop", &json!({"type": "message_stop"}), false);
        assert_eq!(stop_events.len(), 1);
        assert_eq!(stop_events[0].0, "done");
        assert_eq!(stop_events[0].1, json!("[DONE]"));
    }

    #[test]
    fn test_sse_bridge_responses_streaming() {
        let mut state = SseBridgeState::new();

        // 1. message_start -> response.created + response.in_progress
        let start_events = state.ingest(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": "msg_codex",
                    "model": "glm-5.3-flash",
                    "usage": {"input_tokens": 20, "output_tokens": 0}
                }
            }),
            true,
        );
        assert_eq!(start_events.len(), 2);
        assert_eq!(start_events[0].0, "response.created");
        assert_eq!(start_events[1].0, "response.in_progress");

        // 2. content_block_start (tool_use) -> response.output_item.added
        let tool_start_events = state.ingest(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "content_block": {
                    "type": "tool_use",
                    "id": "call_xyz",
                    "name": "edit"
                }
            }),
            true,
        );
        assert_eq!(tool_start_events.len(), 1);
        assert_eq!(tool_start_events[0].0, "response.output_item.added");
        assert_eq!(tool_start_events[0].1["item"]["type"], "function_call");
        assert_eq!(tool_start_events[0].1["item"]["name"], "edit");

        // 3. content_block_delta (input_json_delta) -> response.function_call_arguments.delta
        let arg_events = state.ingest(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "delta": {
                    "type": "input_json_delta",
                    "partial_json": "{\"file\":\"main.rs\"}"
                }
            }),
            true,
        );
        assert_eq!(arg_events.len(), 1);
        assert_eq!(arg_events[0].0, "response.function_call_arguments.delta");
        assert_eq!(arg_events[0].1["delta"], "{\"file\":\"main.rs\"}");

        // 4. content_block_stop -> response.function_call_arguments.done + response.output_item.done
        let tool_stop_events = state.ingest(
            "content_block_stop",
            &json!({"type": "content_block_stop"}),
            true,
        );
        assert_eq!(tool_stop_events.len(), 2);
        assert_eq!(
            tool_stop_events[0].0,
            "response.function_call_arguments.done"
        );
        assert_eq!(tool_stop_events[1].0, "response.output_item.done");

        // 5. message_stop -> response.completed
        let stop_events = state.ingest("message_stop", &json!({"type": "message_stop"}), true);
        assert_eq!(stop_events.len(), 1);
        assert_eq!(stop_events[0].0, "response.completed");
    }
}
