//! MonkeyCode 任务流的**下行翻译层**：ACP 事件 → 本网关的 OpenAI 兼容帧。
//!
//! ── 与 `stream.rs` 的分工 ─────────────────────────────────────
//! `stream.rs` 管连接与驱动（WS 握手、起始消息、超时、帧出口）；本文件是纯
//! 状态机：一条下行 JSON 帧进，零到多条 `chat.completion.chunk` 出，外加
//! 「要不要回过一条控制消息」（ping / reply-question）与「是不是终态」。
//! 拆文件只是因为单文件行数约定；两者是同一段时序的两半。
//!
//! ── ACP 事件映射 ─────────────────────────────────────────────
//! 上游实际推**会话更新信封**（2026-10 抓帧实证）：data 解 base64 后是
//! `{"sessionId", "update": {...}}`，`update.sessionUpdate` 取值与处理：
//! | sessionUpdate / type | 产出 |
//! |---|---|
//! | `agent_message_chunk` | `delta.content`（content 可为字符串或 `{type:"text",text}` 对象）|
//! | `agent_thought_chunk` | `delta.reasoning_content`（本仓思考通道）|
//! | `tool_call` | `delta.tool_calls`（信封形态 name 取 title/toolCallId）|
//! | `tool_call_update` | 参数增量帧（扁平形态；信封形态字段不齐时仅记日志）|
//! | `usage_update` | 信封形态给 used/size/cost（上下文水位，非 OpenAI 口径，仅记录）；扁平形态并入 usage |
//! | `plan` / `available_commands_update` | 仅 verbose 日志 |
//! | 未知 | 仅 verbose 日志 |
//! 扁平形态（`{"type": ...}`，旧文档）保留兼容，字段映射不变。
//!
//! 下行信封（`ping` / `task-started` / `task-running` / `task-ended` /
//! `task-error`）的处理与四个终态出口见 `stream.rs` 的模块头。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic，不碰网络、不持锁。

use bytes::Bytes;
use serde_json::{json, Map, Value};

use crate::server::logging;

/// `data: [DONE]\n\n`（OpenAI 流收尾帧；`stream.rs` 的异常收尾也用）
pub(super) const DONE_FRAME: &[u8] = b"data: [DONE]\n\n";

// ─── 翻译状态机 ────────────────────────────────────────────────

/// 一次工具调用的累积状态（ACP 的 `tool_call` / `tool_call_update`）。
struct ToolCallState {
    id: String,
    name: String,
    arguments: String,
}

/// Token 用量（报告口径：OpenAI 的三件套）。
#[derive(Default, Clone, Copy)]
struct Usage {
    input: i64,
    output: i64,
    total: i64,
}

impl Usage {
    fn seen(&self) -> bool {
        self.input != 0 || self.output != 0 || self.total != 0
    }

    fn to_json(&self) -> Value {
        json!({
            "prompt_tokens": self.input,
            "completion_tokens": self.output,
            "total_tokens": self.total,
        })
    }
}

/// 一个任务流的下行翻译状态机（流式 / 非流式共用）。
///
/// 上游的每个 ACP 事件在这里变成 OpenAI chunk；非流式收尾时同一份累积状态
/// （`content` / `reasoning` / `tools`）直接组装 `chat.completion`。
pub struct Translator {
    /// 面向客户端的响应 id（`chatcmpl-<taskId>`，参考同款）
    chat_id: String,
    /// 客户端请求的模型名（下发帧的 `model` 字段）
    model: String,
    /// 客户端是否要 usage 帧（`stream_options.include_usage`）
    include_usage: bool,
    content: String,
    reasoning: String,
    tools: Vec<ToolCallState>,
    /// 最近一次工具调用的下标（`tool_call_update` 的增量参数挂到它上面）
    last_tool_index: Option<usize>,
    usage: Usage,
    role_sent: bool,
    finished: bool,
}

impl Translator {
    pub(super) fn new(chat_id: String, model: String, include_usage: bool) -> Self {
        Self {
            chat_id,
            model,
            include_usage,
            content: String::new(),
            reasoning: String::new(),
            tools: Vec::new(),
            last_tool_index: None,
            usage: Usage::default(),
            role_sent: false,
            finished: false,
        }
    }

    /// 首帧：`delta.role = assistant`（本仓约定；参考没有这一帧，OpenAI
    /// 客户端按它建立 assistant 消息，早发比晚发稳）。只发一次。
    pub(super) fn role_frame(&mut self) -> Option<Bytes> {
        if self.role_sent {
            return None;
        }
        self.role_sent = true;
        Some(self.chunk_frame(json!({ "role": "assistant" }), None))
    }

    /// 一帧 `chat.completion.chunk`。
    fn chunk_frame(&self, delta: Value, finish_reason: Option<&str>) -> Bytes {
        let choice = json!({
            "index": 0,
            "delta": delta,
            "finish_reason": finish_reason,
        });
        sse_bytes(&json!({
            "id": self.chat_id,
            "object": "chat.completion.chunk",
            "created": logging::now_ms() / 1000,
            "model": self.model,
            "choices": [choice],
        }))
    }

    /// usage 帧（OpenAI 收尾形态：`choices` 为空数组）
    fn usage_frame(&self) -> Bytes {
        sse_bytes(&json!({
            "id": self.chat_id,
            "object": "chat.completion.chunk",
            "created": logging::now_ms() / 1000,
            "model": self.model,
            "choices": [],
            "usage": self.usage.to_json(),
        }))
    }

    /// 流内错误帧（HTTP 头早已发出，只能这样告诉客户端这次补全失败）。
    pub(super) fn error_frame(&self, message: &str) -> Bytes {
        sse_bytes(&json!({
            "id": self.chat_id,
            "object": "chat.completion.chunk",
            "created": logging::now_ms() / 1000,
            "model": self.model,
            "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
            "error": { "message": message, "type": "upstream_error" },
        }))
    }

    /// 正常收尾：`finish` 帧 +（客户端要的话）usage 帧 + `[DONE]`（幂等）。
    pub(super) fn finish_frames(&mut self) -> Vec<Bytes> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let mut frames = vec![self.chunk_frame(json!({}), Some("stop"))];
        if self.include_usage {
            frames.push(self.usage_frame());
        }
        frames.push(Bytes::from_static(DONE_FRAME));
        frames
    }

    /// 正文增量
    fn push_content(&mut self, text: &str) -> Bytes {
        self.content.push_str(text);
        self.chunk_frame(json!({ "content": text }), None)
    }

    /// 思考增量（本仓走 reasoning 通道，见模块头）
    fn push_reasoning(&mut self, text: &str) -> Bytes {
        self.reasoning.push_str(text);
        self.chunk_frame(json!({ "reasoning_content": text }), None)
    }

    /// 工具调用（一次事件给全量参数；`index` 是客户端配对用的下标）
    fn push_tool_call(&mut self, name: &str, input: &str) -> Bytes {
        let index = self.tools.len();
        // id 用任务 id（去掉 `chatcmpl-` 前缀）拼 idx：ACP 的 tool_call 事件
        // 没有携带 id，客户端做 tool_use/tool_result 配对时需要稳定且唯一的值。
        let task = self.chat_id.strip_prefix("chatcmpl-").unwrap_or(&self.chat_id);
        let id = format!("call_{index}_{task}");
        self.tools.push(ToolCallState {
            id: id.clone(),
            name: name.to_string(),
            arguments: input.to_string(),
        });
        self.last_tool_index = Some(index);
        self.chunk_frame(
            json!({
                "tool_calls": [{
                    "index": index,
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": input },
                }],
            }),
            None,
        )
    }

    /// 工具参数的增量更新（`tool_call_update`）：只发 `arguments` 增量，
    /// 客户端按 index 累积成完整参数（OpenAI 流式工具调用的标准形态）。
    fn append_tool_arguments(&mut self, tool_name: &str, delta: &str) -> Option<Bytes> {
        if delta.is_empty() {
            return None;
        }
        let index = if tool_name.is_empty() {
            self.last_tool_index?
        } else {
            self.tools
                .iter()
                .rposition(|tool| tool.name == tool_name)?
        };
        let tool = self.tools.get_mut(index)?;
        tool.arguments.push_str(delta);
        self.last_tool_index = Some(index);
        Some(self.chunk_frame(
            json!({
                "tool_calls": [{
                    "index": index,
                    "function": { "arguments": delta },
                }],
            }),
            None,
        ))
    }

    /// 收尾时要上报给编排层记账的 usage（没有收到任何非零计数时 None ——
    /// 失败 / 无用量路径不杜撰 token，见 `stream.rs` 的收尾）。
    pub(super) fn reportable_usage(&self) -> Option<Value> {
        if self.usage.seen() {
            Some(self.usage.to_json())
        } else {
            None
        }
    }

    /// 收尾日志用的一句话摘要（只记长度、不落正文 —— 与 CatPaw 同一隐私口径）。
    pub(super) fn summary(&self) -> String {
        format!(
            "正文 {} 字、思考 {} 字、工具 {} 个",
            self.content.chars().count(),
            self.reasoning.chars().count(),
            self.tools.len(),
        )
    }

    /// 非流式的完整响应体（`chat.completion`）。
    pub(super) fn completion_body(&self) -> Value {
        let mut message = Map::new();
        message.insert("role".to_string(), Value::String("assistant".to_string()));
        message.insert(
            "content".to_string(),
            if self.content.is_empty() {
                Value::Null
            } else {
                Value::String(self.content.clone())
            },
        );
        if !self.reasoning.is_empty() {
            message.insert(
                "reasoning_content".to_string(),
                Value::String(self.reasoning.clone()),
            );
        }
        if !self.tools.is_empty() {
            let tools: Vec<Value> = self
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "id": tool.id,
                        "type": "function",
                        "function": { "name": tool.name, "arguments": tool.arguments },
                    })
                })
                .collect();
            message.insert("tool_calls".to_string(), Value::Array(tools));
        }
        let usage = if self.usage.seen() {
            self.usage.to_json()
        } else {
            json!({ "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0 })
        };
        json!({
            "id": self.chat_id,
            "object": "chat.completion",
            "created": logging::now_ms() / 1000,
            "model": self.model,
            "choices": [{
                "index": 0,
                "message": Value::Object(message),
                "finish_reason": "stop",
            }],
            "usage": usage,
        })
    }
}

/// 一条 SSE 帧的字节形态（`data: <json>\n\n`）
fn sse_bytes(value: &Value) -> Bytes {
    let text = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    Bytes::from(format!("data: {text}\n\n"))
}

// ─── 下行消息处理 ──────────────────────────────────────────────

/// 任务流的终态。
pub(super) enum Terminal {
    /// 正常结束（`task-ended`）
    Ended,
    /// 异常结束（原因进错误帧与请求日志）
    Failed(String),
}

/// 处理一条下行消息的结果。
#[derive(Default)]
pub(super) struct Handled {
    /// 要下发给客户端的 SSE 帧
    pub(super) frames: Vec<Bytes>,
    /// 要回给上游的控制消息（ping / reply-question）
    pub(super) reply: Option<String>,
    /// 终态（None = 继续）
    pub(super) terminal: Option<Terminal>,
}

/// 一条下行文本帧 → 帧 / 回复 / 终态。
pub(super) fn handle_message(translator: &mut Translator, raw: &str) -> Handled {
    let parsed = match serde_json::from_str::<Value>(raw) {
        Ok(value) => value,
        Err(_) => {
            // 参考同样忽略非 JSON 消息（`catch { /* 忽略 */ }`）
            logging::verbose("[MonkeyCode]", "忽略任务流里的非 JSON 消息");
            return Handled::default();
        }
    };
    match parsed.get("type").and_then(Value::as_str).unwrap_or("") {
        // 心跳：回一条 ping（参考 `task-runner.ts` 的 `msg.type === "ping"`）
        "ping" => Handled {
            reply: Some(json!({ "type": "ping" }).to_string()),
            ..Handled::default()
        },
        "task-started" => {
            logging::verbose("[MonkeyCode]", "任务轮次开始（task-started）");
            Handled::default()
        }
        "task-running" => handle_running(translator, &parsed),
        "task-ended" => {
            merge_final_usage(translator, parsed.get("data"));
            Handled {
                frames: translator.finish_frames(),
                terminal: Some(Terminal::Ended),
                ..Handled::default()
            }
        }
        "task-error" => {
            let detail = error_detail(parsed.get("data"));
            Handled {
                terminal: Some(Terminal::Failed(detail)),
                ..Handled::default()
            }
        }
        other => {
            logging::verbose(
                "[MonkeyCode]",
                &format!("忽略任务流消息类型：{other}"),
            );
            Handled::default()
        }
    }
}

/// `task-running` / `task-ended` 的 data 载荷解码。
///
/// 2026-10 抓帧实证：上游 `acp_event` 的 data 是 **base64(JSON) 字符串**
/// （如 `eyJzZXNzaW9uSWQi…`），直接 `serde_json::from_str` 必然失败、事件被
/// 整体丢弃——这是「网页有回复、网关空正文」的直接原因。这里按
/// 「JSON 串 → base64(JSON)」双形态解析（与 `parse_question` 同款兜底）。
fn decode_payload(data: &str) -> Option<Value> {
    let text = data.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        return Some(value);
    }
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD.decode(text).ok()?;
    let text = String::from_utf8(decoded).ok()?;
    serde_json::from_str::<Value>(&text).ok()
}

/// `task-running`：ACP 事件（转帧）或 Agent 提问（自动回复）。
fn handle_running(translator: &mut Translator, message: &Value) -> Handled {
    match message.get("kind").and_then(Value::as_str).unwrap_or("") {
        "acp_event" => {
            let Some(data) = message.get("data").and_then(Value::as_str) else {
                logging::verbose("[MonkeyCode]", "acp_event 缺少 data 字符串");
                return Handled::default();
            };
            let Some(acp) = decode_payload(data) else {
                logging::verbose("[MonkeyCode]", "acp_event 的 data 无法按 JSON / base64(JSON) 解码");
                return Handled::default();
            };
            handle_acp(translator, &acp)
        }
        "acp_ask_user_question" => {
            let reply = auto_reply_question(message.get("data"));
            if reply.is_none() {
                logging::verbose(
                    "[MonkeyCode]",
                    "Agent 提问无法解析，未自动回复（任务可能停在提问处）",
                );
            }
            Handled {
                reply,
                ..Handled::default()
            }
        }
        other => {
            logging::verbose(
                "[MonkeyCode]",
                &format!("忽略 task-running kind={other}"),
            );
            Handled::default()
        }
    }
}

/// 一个 ACP 事件 → 帧。
///
/// 上游实际推送**两种形态**（2026-10 抓帧实证，见模块头）：
///   - 会话更新信封（主形态）：`{"sessionId", "update": {...}}`，`update`
///     里带 `sessionUpdate` 字段（`agent_message_chunk` / `usage_update` /
///     `tool_call` / `available_commands_update` …）；
///   - 扁平事件（旧文档形态）：`{"type": "agent_message_chunk", ...}`。
/// 先解开 `update` 信封，再按 `sessionUpdate` → `type` 的顺序分派；两套
/// 字段都对不上但带 `content` 的，按内容块处理（信封形态的 content 是
/// `{type:"text", text}` 对象，见 `chunk_text`）。
fn handle_acp(translator: &mut Translator, acp: &Value) -> Handled {
    let event = match acp.get("update") {
        Some(update) if update.is_object() => update,
        _ => acp,
    };
    if let Some(kind) = event.get("sessionUpdate").and_then(Value::as_str) {
        return match kind {
            "agent_message_chunk" => push_chunk(translator, event, false),
            "agent_thought_chunk" => push_chunk(translator, event, true),
            "tool_call" => {
                let name = tool_name_of(event);
                let input = text_field(event.get("rawInput"));
                logging::verbose(
                    "[MonkeyCode]",
                    &format!("工具调用 {}（{}）", name, preview(&input, 120)),
                );
                Handled {
                    frames: vec![translator.push_tool_call(&name, &input)],
                    ..Handled::default()
                }
            }
            "tool_call_update" => {
                let delta = text_field(event.get("rawInput"));
                let name = tool_name_of(event);
                logging::verbose(
                    "[MonkeyCode]",
                    &format!(
                        "工具调用更新 name={} delta={}",
                        if name.is_empty() { "-" } else { &name },
                        preview(&delta, 120),
                    ),
                );
                match translator.append_tool_arguments(&name, &delta) {
                    Some(frame) => Handled {
                        frames: vec![frame],
                        ..Handled::default()
                    },
                    None => Handled::default(),
                }
            }
            // 上游 usage_update 给的是 used / size / cost（上下文水位），
            // 不是 OpenAI 三件套口径；不强行映射成假数据，只留痕。
            "usage_update" => {
                logging::verbose("[MonkeyCode]", "收到 usage_update（信封形态，仅记录）");
                Handled::default()
            }
            "available_commands_update" | "plan" => {
                logging::verbose("[MonkeyCode]", &format!("收到 {kind}（仅记录）"));
                Handled::default()
            }
            other => {
                logging::verbose("[MonkeyCode]", &format!("忽略会话更新：{other}"));
                Handled::default()
            }
        };
    }
    match event.get("type").and_then(Value::as_str).unwrap_or("") {
        "agent_message_chunk" => push_chunk(translator, event, false),
        "agent_thought_chunk" => push_chunk(translator, event, true),
        "tool_call" => {
            let name = event
                .get("tool_name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .unwrap_or("unknown")
                .to_string();
            let input = text_field(event.get("tool_input"));
            logging::verbose(
                "[MonkeyCode]",
                &format!("工具调用 {}（{}）", name, preview(&input, 120)),
            );
            Handled {
                frames: vec![translator.push_tool_call(&name, &input)],
                ..Handled::default()
            }
        }
        "tool_call_update" => {
            let delta = {
                let text = text_field(event.get("tool_input"));
                if text.is_empty() {
                    text_field(event.get("delta"))
                } else {
                    text
                }
            };
            let name = event
                .get("tool_name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            let status = event.get("status").and_then(Value::as_str).unwrap_or("");
            logging::verbose(
                "[MonkeyCode]",
                &format!(
                    "工具调用更新 status={} name={} delta={}",
                    if status.is_empty() { "-" } else { status },
                    if name.is_empty() { "-" } else { &name },
                    preview(&delta, 120),
                ),
            );
            // 参考只打日志不产帧；文档（06-acp-event-reference）把 tool_input /
            // delta 描述为**增量**，OpenAI 侧按增量下发客户端才能拼出完整参数
            // （与参考 Responses 分支的 function_call_arguments.delta 同口径）。
            match translator.append_tool_arguments(&name, &delta) {
                Some(frame) => Handled {
                    frames: vec![frame],
                    ..Handled::default()
                },
                None => Handled::default(),
            }
        }
        "usage_update" => {
            merge_usage(&mut translator.usage, event);
            Handled::default()
        }
        "plan" => {
            logging::verbose("[MonkeyCode]", "收到 plan 事件（仅记录）");
            Handled::default()
        }
        "available_commands_update" => {
            logging::verbose("[MonkeyCode]", "收到 available_commands_update（仅记录）");
            Handled::default()
        }
        other => {
            logging::verbose("[MonkeyCode]", &format!("忽略 ACP 事件：{other}"));
            Handled::default()
        }
    }
}

/// 内容 / 思考块的公共出口（`reasoning` 选通道；空文本不发帧）。
fn push_chunk(translator: &mut Translator, event: &Value, reasoning: bool) -> Handled {
    let text = chunk_text(event);
    if text.is_empty() {
        return Handled::default();
    }
    let frame = if reasoning {
        translator.push_reasoning(&text)
    } else {
        translator.push_content(&text)
    };
    Handled {
        frames: vec![frame],
        ..Handled::default()
    }
}

/// 工具名：信封形态给 `title`（展示名）+ `toolCallId`；扁平形态给
/// `tool_name`。三者按序取第一个非空，都没有回落 "unknown"。
fn tool_name_of(event: &Value) -> String {
    for key in ["tool_name", "title", "toolCallId"] {
        let value = event
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if !value.is_empty() {
            return value.to_string();
        }
    }
    "unknown".to_string()
}

/// ACP 文本块：`text` 优先；`content` 为字符串直接用、为对象取其 `text`
/// （信封形态的 content 是 `{type:"text", text}`，2026-10 抓帧实证；
/// 参考 `acp.text || acp.content` 只覆盖字符串形态）。
fn chunk_text(event: &Value) -> String {
    if let Some(text) = event.get("text").and_then(Value::as_str) {
        return text.to_string();
    }
    match event.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(content) if content.is_object() => content
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

/// 取一个字段的文本形态（字符串原样；其它 JSON 值取序列化文本）。
fn text_field(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

/// Agent 提问 → `reply-question`（自动回复，参考 `task-runner.ts:214-231`）。
///
/// `data` 在参考里是 **JSON 串**（`JSON.parse(msg.data)`）；文档的下行表另有
/// 「base64 编码的提问数据」一说（01-task-stream.md），因此解析失败时再试一次
/// base64 → JSON 解码（两种形态都认，避免任务卡在提问处）。
/// 解析全失败时返回 None（参考的 catch 分支同样不回）。
fn auto_reply_question(data: Option<&Value>) -> Option<String> {
    let payload = match data {
        Some(Value::String(raw)) => parse_question(raw)?,
        Some(value @ Value::Object(_)) => value.clone(),
        _ => return None,
    };
    let request_id = payload
        .get("request_id")
        .and_then(Value::as_str)
        .or_else(|| payload.get("id").and_then(Value::as_str))
        .unwrap_or("")
        .to_string();
    Some(
        json!({
            "type": "reply-question",
            "data": json!({
                "request_id": request_id,
                "answers_json": "",
                "cancelled": false,
            })
            .to_string(),
        })
        .to_string(),
    )
}

/// 提问载荷解析：先当 JSON，再当 base64(JSON)（见 `auto_reply_question`）。
fn parse_question(raw: &str) -> Option<Value> {
    let text = raw.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        return Some(value);
    }
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD.decode(text).ok()?;
    let text = String::from_utf8(decoded).ok()?;
    serde_json::from_str::<Value>(&text).ok()
}

/// `task-error` 的 data → 可读原因（字符串直接用；对象取 error/message/msg）。
fn error_detail(data: Option<&Value>) -> String {
    match data {
        Some(Value::String(text)) if !text.trim().is_empty() => text.trim().to_string(),
        Some(Value::Object(object)) => ["error", "message", "msg"]
            .iter()
            .find_map(|key| object.get(*key).and_then(Value::as_str))
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| "任务执行出错（上游未给原因）".to_string()),
        _ => "任务执行出错（上游未给原因）".to_string(),
    }
}

/// `usage_update` / `task-ended.usage` 的计数并入（参考按**累计值**处理：
/// 非零才覆盖，`06-acp-event-reference` 明确「当前代理实现将其作为累计值」）。
fn merge_usage(usage: &mut Usage, source: &Value) {
    for (key, slot) in [
        ("input_tokens", 0usize),
        ("output_tokens", 1),
        ("total_tokens", 2),
    ] {
        let value = source
            .get(key)
            .and_then(Value::as_i64)
            .or_else(|| source.get(key).and_then(Value::as_f64).map(|n| n as i64))
            .unwrap_or(0);
        if value == 0 {
            continue;
        }
        match slot {
            0 => usage.input = value,
            1 => usage.output = value,
            _ => usage.total = value,
        }
    }
}

/// `task-ended` 的 data（JSON 串或对象）里的最终 usage（01-task-stream 下行表：
/// `task-ended` 的 data 可能是 `{"usage": {...}}`）。
fn merge_final_usage(translator: &mut Translator, data: Option<&Value>) {
    let Some(data) = data else {
        return;
    };
    let payload = match data {
        Value::String(text) => match decode_payload(text) {
            Some(value) => value,
            None => return,
        },
        other => other.clone(),
    };
    let usage = payload.get("usage").unwrap_or(&payload);
    merge_usage(&mut translator.usage, usage);
}

/// 日志用的截断预览（按字符，避免把中文切成半个字）。
fn preview(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    /// 把 ACP 事件按上游真实形态打包成 WS 下行帧文本（data = base64(JSON)）。
    fn envelope_frame(acp: Value) -> String {
        let encoded = base64::engine::general_purpose::STANDARD.encode(acp.to_string());
        json!({ "type": "task-running", "kind": "acp_event", "data": encoded }).to_string()
    }

    fn new_translator() -> Translator {
        let mut translator = Translator::new("chatcmpl-t".into(), "m".into(), false);
        let _ = translator.role_frame();
        translator
    }

    /// 2026-10 抓帧主形态：base64(JSON) 的会话更新信封 + content 对象。
    #[test]
    fn envelope_chunk_base64_produces_content() {
        let acp = json!({
            "sessionId": "ses_x",
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "你好！我是 MonkeyCode。" }
            }
        });
        let mut translator = new_translator();
        let handled = handle_message(&mut translator, &envelope_frame(acp));
        assert!(!handled.frames.is_empty(), "信封 + base64 形态必须产出内容帧");
        let text = String::from_utf8_lossy(&handled.frames[0]).to_string();
        assert!(text.contains("MonkeyCode"), "内容帧应包含正文，实际：{text}");
    }

    /// 扁平形态（旧文档）：data 为 JSON 串。
    #[test]
    fn flat_chunk_json_still_works() {
        let data = json!({ "type": "agent_message_chunk", "text": "hi" }).to_string();
        let frame = json!({ "type": "task-running", "kind": "acp_event", "data": data })
            .to_string();
        let mut translator = new_translator();
        let handled = handle_message(&mut translator, &frame);
        assert!(!handled.frames.is_empty(), "扁平 JSON 形态仍应产出内容帧");
    }

    /// task-ended 的 data 同样是 base64(JSON)，必须识别为终态。
    #[test]
    fn task_ended_base64_terminates() {
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(json!({ "exit_code": 0, "message": "completed" }).to_string());
        let frame = json!({ "type": "task-ended", "data": encoded }).to_string();
        let mut translator = new_translator();
        let handled = handle_message(&mut translator, &frame);
        assert!(
            matches!(handled.terminal, Some(Terminal::Ended)),
            "base64 data 的 task-ended 必须到达终态"
        );
    }

    /// 聚合出口：信封内容最终进 completion_body 的正文（非空）。
    #[test]
    fn aggregate_body_carries_text() {
        let acp = json!({
            "sessionId": "ses_x",
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "正文片段" }
            }
        });
        let mut translator = new_translator();
        let _ = handle_message(&mut translator, &envelope_frame(acp));
        let body = translator.completion_body();
        let text = body
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(text.contains("正文片段"), "聚合正文应包含内容，实际：{text}");
    }
}
