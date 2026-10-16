//! OrcaTerm 模型目录：静态兜底（上游没有可拉的清单接口）。
//!
//! ── 为什么是静态表（与 zcode 同一口径）────────────────────────
//! 模型清单来自**客户端内置**（桌面端代码缓存 2026-10-16 提取 + 逐个真实对证
//! 实测；Web 版面板的 `DescribeAIModelList` 对普通账号返回空数组，不是清单
//! 来源）。上游没有「模型清单」的 HTTP 接口 —— 「这家没有某接口」的排除依据：
//! 面板打开时的全部请求（DescribeWorkspaces / DescribeWorkspaceConversation
//! Mappings / DescribeAIModelList）都不携带模型列表，下拉项与桌面端前端打包
//! 内置一致。
//!
//! ── 对证结果（2026-10-16 实测，每条都真发过一次对话）────────
//!   - `TokenHub/glm-5.3-flash` / `glm-5.3` / `glm-5.2` / `kimi-k3` /
//!     `deepseek-v4-flash` / `deepseek-v4-pro`：可用（SSE 正常回流）；
//!   - `TokenHub/hy4-preview`：上游回 `LLM_ERROR: LLM(...) is not loaded`
//!     （面板上写着「限时免费」但网关侧未挂载）—— 不进清单。
//!
//! ── 条目形态 ────────────────────────────────────────────────
//! 对外的模型 id **不带** `TokenHub/` 前缀（`glm-5.3-flash` 这种短名，
//! 与别家的模型名天然同形，注册表顺序保证同名时优先已有各家）；发给上游时
//! 由 [`resolve_model`] 补前缀。也接受客户端直接点名带前缀的全名。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{Value, json};

use super::DEFAULT_MODEL;

/// 静态清单（对证可用的 6 个；`name` 是面板上的展示名）。
const MODELS: [(&str, &str); 6] = [
    ("glm-5.3-flash", "GLM-5.3-Flash"),
    ("glm-5.3", "GLM-5.3"),
    ("glm-5.2", "GLM-5.2"),
    ("kimi-k3", "Kimi-K3"),
    ("deepseek-v4-flash", "DeepSeek-V4-Flash"),
    ("deepseek-v4-pro", "DeepSeek-V4-Pro"),
];

/// 模型清单（聚合层 `models::list_item` 的口径：id + name）。
pub fn list() -> Vec<Value> {
    MODELS
        .iter()
        .map(|(id, name)| json!({ "id": id, "name": name }))
        .collect()
}

/// 把客户端请求的模型名归一成上游的 `TokenHub/<name>`。
///
/// 接受裸名（`glm-5.3-flash`）与全名（`TokenHub/glm-5.3-flash`）两种形态；
/// 未知模型**显式回退** [`DEFAULT_MODEL`]（上游对未知名回流内
/// `LLM_ERROR`，与其让它失败不如回退到实测可用的默认档）。
pub fn resolve_model(name: &str) -> String {
    let name = name.trim().trim_start_matches("TokenHub/").trim();
    if name.is_empty() {
        return DEFAULT_MODEL.to_string();
    }
    if MODELS.iter().any(|(id, _)| *id == name) {
        format!("TokenHub/{name}")
    } else {
        DEFAULT_MODEL.to_string()
    }
}
