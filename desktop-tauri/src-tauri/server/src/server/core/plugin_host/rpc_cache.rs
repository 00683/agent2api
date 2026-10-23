//! 插件 provider 的模型清单缓存。
//!
//! ── 为什么需要这个缓存（而不是每次问插件）──────────────────────
//! `ProviderAdapter::list_models` 是**同步**方法，而 RPC 只能异步。
//! 在 trait 改成异步之前（P1 待办），清单只能由后台任务填充、这里同步读。
//!
//! ── 三档数据，优先级从高到低 ────────────────────────────────
//!   1. 远程刷新拿回来的清单（`models.list` / `models.refresh` 的结果）；
//!   2. 握手时 `provider.info` 带回来的内置兜底（`default_models`）；
//!   3. 空 —— 调用方按「这家没有模型」处理（`/v1/models` 不广告它）。
//!
//! ── 为什么进程级而不是每适配器一份 ──────────────────────────
//! 与网关既有的 `catalog_cache` 同一形态：清单是进程级共享状态，重启后
//! 由 `catalog_cache` 的持久化层读回。这里只做内存层，持久化仍走既有模块
//! （插件 provider 的清单与内置家一样落 `catalog_cache`）。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde_json::Value;

/// 一家 provider 的清单状态。
#[derive(Clone, Default)]
struct Entry {
    /// 远程刷新拿回来的清单。
    remote: Vec<Value>,
    /// 握手时带回来的内置兜底。
    fallback: Vec<Value>,
}

fn table() -> &'static Mutex<HashMap<String, Entry>> {
    static TABLE: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 记下握手时带回来的内置兜底。
pub fn set_defaults(provider: &str, models: Vec<Value>) {
    if let Ok(mut map) = table().lock() {
        map.entry(provider.to_string()).or_default().fallback = models;
    }
}

/// 记下一次远程刷新的结果。
///
/// ── 为什么空清单不覆盖已有值 ────────────────────────────────
/// 「刷新成功但上游返回 0 条」与「刷新失败」在语义上是两件事，但都不该
/// 把用户手上能用的清单清空 —— 与网关既有口径一致（Trae 那家刻意保留
/// 空清单，其余家失败时保留旧清单）。这里按「空 = 不改」处理。
pub fn set_remote(provider: &str, models: Vec<Value>) {
    if models.is_empty() {
        return;
    }
    if let Ok(mut map) = table().lock() {
        map.entry(provider.to_string()).or_default().remote = models;
    }
}

/// 读清单（远程优先，其次内置兜底）。
pub fn models_for(provider: &str) -> Option<Vec<Value>> {
    let map = table().lock().ok()?;
    let entry = map.get(provider)?;
    if !entry.remote.is_empty() {
        return Some(entry.remote.clone());
    }
    if !entry.fallback.is_empty() {
        return Some(entry.fallback.clone());
    }
    None
}

/// 只读内置兜底。
pub fn default_models_for(provider: &str) -> Option<Vec<Value>> {
    let map = table().lock().ok()?;
    let entry = map.get(provider)?;
    if entry.fallback.is_empty() {
        None
    } else {
        Some(entry.fallback.clone())
    }
}

/// 清空一家（卸载包时调）。
pub fn forget(provider: &str) {
    if let Ok(mut map) = table().lock() {
        map.remove(provider);
    }
}

/// 清空全部（卸载 / 重装整个包时调）。
///
/// 与逐家 [`forget`] 的区别：卸载时我们**不知道**包里有哪些 id（清单可能
/// 已经读不出来了），逐家清会漏；整表清空后由下一次握手重新填 —— 那是唯一
/// 完整的做法。
pub fn clear_all() {
    if let Ok(mut map) = table().lock() {
        map.clear();
    }
}
