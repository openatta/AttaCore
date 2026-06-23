//! Anthropic Messages API 请求体类型。
//!
//! 字段名 / 形状对应 API；与 attacode-base::Message / ContentBlock 配合使用。
//! 详见 docs/RUST_ARCHITECTURE.md §6.1。

use base::message::{ContentBlock, Role};
use serde::Serialize;
use serde_json::Value;

/// Built-in Anthropic tool types (e.g., `web_search_20250305`) that go into the
/// `tools` array alongside regular function `ToolDef` entries.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum BuiltinTool {
    /// Server-side web search (requires beta header
    /// `anthropic-beta: web-search-20250305-2025-03-05`).
    #[serde(rename = "web_search_20250305")]
    WebSearch {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        allowed_domains: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        blocked_domains: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_uses: Option<u32>,
    },
}

/// Messages.create 的请求体。`stream: true` 永远开启（loop 是流式的）。
///
/// 注：自定义 `Serialize` —— tools + anthropic_tools 合并成一个 JSON
/// `tools` 数组发送，built-in tools 与 function tools 在同一数组里。
#[derive(Debug, Clone)]
pub struct MessagesRequest {
    pub model: String,
    pub max_tokens: u32,

    /// system prompt 是字符串数组（每段可独立带 cache_control）。
    /// 见 docs/SYSTEM_PROMPT.md §1。
    pub system: Vec<SystemBlock>,

    pub messages: Vec<MessageParam>,

    pub tools: Vec<ToolDef>,

    /// Built-in Anthropic tools (web_search_20250305, etc.) merged into the
    /// same `tools` JSON array as `tools` during serialization.
    pub anthropic_tools: Vec<BuiltinTool>,

    pub tool_choice: Option<ToolChoice>,

    /// 永远 true（流式）
    pub stream: bool,

    /// `None` = 不设字段（让模型按默认走，思考模型仍会思考）。
    /// `Some(Disabled)` = 显式关闭思考（DeepSeek V4 + 部分 Anthropic-compat
    /// 端点接受；-历史 baseline 都用了这条以避免多 turn 400）。
    /// `Some(Enabled{...}) / Some(Adaptive)` = 开思考。
    pub thinking: Option<ThinkingConfig>,

    /// 当 thinking 开启时**必须** None；否则 API 报错
    pub temperature: Option<f32>,

    pub top_p: Option<f32>,

    pub top_k: Option<u32>,

    pub stop_sequences: Vec<String>,

    /// `{ user_id }`，匿名化后的 device + session 标识；用于 abuse 监控
    pub metadata: Option<RequestMetadata>,

    /// beta header 列表：走 HTTP `anthropic-beta` 头而非 body。
    pub betas: Vec<String>,

    /// fast / extended-output / 任务预算等高级参数
    pub speed: Option<Speed>,
}

impl MessagesRequest {
    /// 一个最小可用的请求构造器：模型 + 一条 user 文字。
    /// system / tools 为空；适合冒烟测试和单元测。
    pub fn minimal(model: impl Into<String>, user_text: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            max_tokens: 1024,
            system: Vec::new(),
            messages: vec![MessageParam {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: user_text.into(),
                    cache_control: None,
                }],
            }],
            tools: Vec::new(),
            anthropic_tools: Vec::new(),
            tool_choice: None,
            stream: true,
            thinking: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: Vec::new(),
            metadata: None,
            betas: Vec::new(),
            speed: None,
        }
    }
}

// Custom Serialize: merges `tools` and `anthropic_tools` into a single
// `tools` JSON array so built-in tools (web_search_20250305) appear in the
// same array as function tools, matching the Anthropic Messages API contract.
impl Serialize for MessagesRequest {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;

        let mut map = s.serialize_map(None)?;

        // Required scalar fields
        map.serialize_entry("model", &self.model)?;
        map.serialize_entry("max_tokens", &self.max_tokens)?;
        map.serialize_entry("system", &self.system)?;
        map.serialize_entry("messages", &self.messages)?;
        map.serialize_entry("stream", &self.stream)?;

        // Merge tools + anthropic_tools into one JSON array
        let mut all_tools: Vec<Value> =
            Vec::with_capacity(self.tools.len() + self.anthropic_tools.len());
        for t in &self.tools {
            all_tools.push(serde_json::to_value(t).map_err(serde::ser::Error::custom)?);
        }
        for bt in &self.anthropic_tools {
            all_tools.push(serde_json::to_value(bt).map_err(serde::ser::Error::custom)?);
        }
        map.serialize_entry("tools", &all_tools)?;

        // Optional fields (only if non-None / non-empty)
        if let Some(ref tc) = self.tool_choice {
            map.serialize_entry("tool_choice", tc)?;
        }
        if let Some(ref th) = self.thinking {
            map.serialize_entry("thinking", th)?;
        }
        if let Some(ref t) = self.temperature {
            map.serialize_entry("temperature", t)?;
        }
        if let Some(ref p) = self.top_p {
            map.serialize_entry("top_p", p)?;
        }
        if let Some(ref k) = self.top_k {
            map.serialize_entry("top_k", k)?;
        }
        if !self.stop_sequences.is_empty() {
            map.serialize_entry("stop_sequences", &self.stop_sequences)?;
        }
        if let Some(ref m) = self.metadata {
            map.serialize_entry("metadata", m)?;
        }
        if let Some(ref sp) = self.speed {
            map.serialize_entry("speed", sp)?;
        }

        map.end()
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SystemBlock {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
}

impl SystemBlock {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text {
            text: s.into(),
            cache_control: None,
        }
    }
    pub fn text_cached(s: impl Into<String>, cc: CacheControl) -> Self {
        Self::Text {
            text: s.into(),
            cache_control: Some(cc),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MessageParam {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
    /// ToolSearch 延迟加载标志
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defer_loading: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    Auto {
        #[serde(skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    Any {
        #[serde(skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    Tool {
        name: String,
    },
    None,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CacheControl {
    Ephemeral {
        #[serde(skip_serializing_if = "Option::is_none")]
        ttl: Option<CacheTtl>,
        #[serde(skip_serializing_if = "Option::is_none")]
        scope: Option<CacheScope>,
    },
}

impl CacheControl {
    pub fn ephemeral_5m() -> Self {
        Self::Ephemeral {
            ttl: Some(CacheTtl::FiveMin),
            scope: None,
        }
    }
    pub fn ephemeral_1h() -> Self {
        Self::Ephemeral {
            ttl: Some(CacheTtl::OneHour),
            scope: None,
        }
    }
    pub fn ephemeral_1h_global() -> Self {
        Self::Ephemeral {
            ttl: Some(CacheTtl::OneHour),
            scope: Some(CacheScope::Global),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum CacheTtl {
    #[serde(rename = "5m")]
    FiveMin,
    #[serde(rename = "1h")]
    OneHour,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum CacheScope {
    #[serde(rename = "global")]
    Global,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThinkingConfig {
    /// 模型自适应预算（部分新模型支持；不支持的模型会报错）
    Adaptive,
    /// 显式 token 预算
    Enabled { budget_tokens: u32 },
    /// **L1 **: explicit "no thinking". DeepSeek V4 (thinking model
    /// by default) and some Anthropic-compat backends accept this to
    /// suppress reasoning_content emission. Important for multi-turn
    /// flows where the client doesn't echo thinking blocks back — DS V4
    /// rejects with 400 in that case (see compare-vs-claude-ts/scripts/
    /// llm_quality_diff_batch.py for historical use).
    Disabled,
}

#[derive(Debug, Clone, Serialize)]
pub struct RequestMetadata {
    pub user_id: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Speed {
    Fast,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn minimal_request_serializes() {
        let req = MessagesRequest::minimal("claude-sonnet-4-6", "hello");
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["model"], "claude-sonnet-4-6");
        assert_eq!(v["max_tokens"], 1024);
        assert_eq!(v["stream"], true);
        assert_eq!(v["system"], json!([]));
        assert_eq!(v["messages"][0]["role"], "user");
        assert_eq!(v["messages"][0]["content"][0]["type"], "text");
        // betas 不进 body
        assert!(v.get("betas").is_none());
        // 可选字段缺省全省略
        assert!(v.get("temperature").is_none());
        assert!(v.get("thinking").is_none());
        assert!(v.get("tool_choice").is_none());
    }

    #[test]
    fn cache_control_serializes_5m_and_1h() {
        let cc5 = CacheControl::ephemeral_5m();
        let cc1 = CacheControl::ephemeral_1h();
        assert_eq!(
            serde_json::to_value(&cc5).unwrap(),
            json!({"type": "ephemeral", "ttl": "5m"})
        );
        assert_eq!(
            serde_json::to_value(&cc1).unwrap(),
            json!({"type": "ephemeral", "ttl": "1h"})
        );
        assert_eq!(
            serde_json::to_value(CacheControl::ephemeral_1h_global()).unwrap(),
            json!({"type": "ephemeral", "ttl": "1h", "scope": "global"})
        );
    }

    #[test]
    fn tool_def_with_cache_control() {
        let t = ToolDef {
            name: "Bash".into(),
            description: "run shell".into(),
            input_schema: json!({"type": "object"}),
            cache_control: Some(CacheControl::ephemeral_1h()),
            defer_loading: None,
            strict: None,
        };
        let v = serde_json::to_value(&t).unwrap();
        assert_eq!(v["name"], "Bash");
        assert_eq!(v["cache_control"]["type"], "ephemeral");
        assert!(v.get("defer_loading").is_none());
    }

    #[test]
    fn thinking_enabled_with_budget() {
        let t = ThinkingConfig::Enabled {
            budget_tokens: 8000,
        };
        assert_eq!(
            serde_json::to_value(&t).unwrap(),
            json!({"type": "enabled", "budget_tokens": 8000})
        );
    }

    #[test]
    fn tool_choice_variants() {
        assert_eq!(
            serde_json::to_value(ToolChoice::Auto {
                disable_parallel_tool_use: None
            })
            .unwrap(),
            json!({"type": "auto"})
        );
        assert_eq!(
            serde_json::to_value(ToolChoice::Tool {
                name: "Read".into()
            })
            .unwrap(),
            json!({"type": "tool", "name": "Read"})
        );
        assert_eq!(
            serde_json::to_value(ToolChoice::None).unwrap(),
            json!({"type": "none"})
        );
    }

    #[test]
    fn empty_stop_sequences_omitted() {
        let req = MessagesRequest::minimal("m", "u");
        let v = serde_json::to_value(&req).unwrap();
        assert!(v.get("stop_sequences").is_none());
    }

    #[test]
    fn thinking_disabled_serializes() {
        let t = ThinkingConfig::Disabled;
        assert_eq!(
            serde_json::to_value(&t).unwrap(),
            json!({"type": "disabled"})
        );
    }

    /// Full request snapshot — system blocks with cache breakpoints, tools,
    /// messages, and thinking enabled. Verifies the wire shape matches the
    /// Anthropic Messages API contract.
    #[test]
    fn full_request_snapshot() {
        let req = MessagesRequest {
            model: "claude-sonnet-4-6".into(),
            max_tokens: 4096,
            system: vec![
                SystemBlock::text_cached("Core instructions", CacheControl::ephemeral_5m()),
                SystemBlock::text("Dynamic context"),
            ],
            messages: vec![MessageParam {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "hello".into(),
                    cache_control: None,
                }],
            }],
            tools: vec![ToolDef {
                name: "Bash".into(),
                description: "Execute shell commands".into(),
                input_schema: json!({"type": "object", "properties": {"cmd": {"type": "string"}}}),
                cache_control: Some(CacheControl::ephemeral_5m()),
                defer_loading: None,
                strict: None,
            }],
            anthropic_tools: vec![BuiltinTool::WebSearch {
                name: "web_search".into(),
                allowed_domains: None,
                blocked_domains: None,
                max_uses: Some(8),
            }],
            tool_choice: Some(ToolChoice::Auto {
                disable_parallel_tool_use: None,
            }),
            stream: true,
            thinking: Some(ThinkingConfig::Enabled {
                budget_tokens: 8000,
            }),
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: vec![],
            metadata: None,
            betas: vec![],
            speed: None,
        };

        let v = serde_json::to_value(&req).unwrap();
        let obj = v.as_object().unwrap();

        // --- Top-level fields ---
        assert_eq!(obj["model"], "claude-sonnet-4-6");
        assert_eq!(obj["max_tokens"], 4096);
        assert_eq!(obj["stream"], true);

        // --- System blocks (two, first with cache_control) ---
        let sys = obj["system"].as_array().unwrap();
        assert_eq!(sys.len(), 2);
        assert_eq!(sys[0]["type"], "text");
        assert_eq!(sys[0]["text"], "Core instructions");
        assert_eq!(sys[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(sys[0]["cache_control"]["ttl"], "5m");
        assert_eq!(sys[1]["type"], "text");
        assert_eq!(sys[1]["text"], "Dynamic context");
        // Second block has no cache_control
        assert!(sys[1].get("cache_control").is_none());

        // --- Messages ---
        let msgs = obj["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"][0]["type"], "text");
        assert_eq!(msgs[0]["content"][0]["text"], "hello");

        // --- Tools (function tool + built-in web search) ---
        let tools = obj["tools"].as_array().unwrap();
        assert_eq!(
            tools.len(),
            2,
            "tools array should contain both function and built-in tools"
        );
        // Index 0: function tool
        assert_eq!(tools[0]["name"], "Bash");
        assert!(tools[0].get("input_schema").is_some());
        assert_eq!(tools[0]["cache_control"]["type"], "ephemeral");
        // Index 1: built-in web search
        assert_eq!(tools[1]["type"], "web_search_20250305");
        assert_eq!(tools[1]["name"], "web_search");
        assert_eq!(tools[1]["max_uses"], 8);
        assert!(tools[1].get("input_schema").is_none());

        // --- Thinking ---
        assert_eq!(obj["thinking"]["type"], "enabled");
        assert_eq!(obj["thinking"]["budget_tokens"], 8000);

        // --- Optional fields omitted ---
        assert!(obj.get("temperature").is_none());
        assert!(obj.get("top_p").is_none());
        assert!(obj.get("metadata").is_none());
        // betas never appears in body
        assert!(obj.get("betas").is_none());
        // empty stop_sequences is omitted
        assert!(obj.get("stop_sequences").is_none());

        // --- tool_choice ---
        assert_eq!(obj["tool_choice"]["type"], "auto");
    }
}
