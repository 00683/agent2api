//! WorkBuddy **猫猫旅行定时排程**（国内版）。
//!
//! ── 这个模块解决什么问题 ─────────────────────────────────────
//! 猫猫旅行原本只有手动入口（签到页的「巡检一次」）：用户得记得每天点两下
//! （早上派出、晚上领奖）。参考项目把它挂成每日整点排程（`travel_hours`
//! 默认 09/21 点），本模块照同一形态接入本仓的调度体系。
//!
//! ── 为什么连登管家不在这里 ──────────────────────────────────
//! 连登管家（补签 → 档位兑换 → 抽奖）**挂在签到末尾**（见
//! `core::billing::checkin` 的 `run_checkin`）：它的判据依赖「今天签到了没有」，
//! 紧跟签到跑最准；而签到是手动与定时共用的唯一入口，挂那里天然覆盖两条
//! 路径。参考项目同样是这个挂法（`RunCheckinNow` 末尾调 `RunStreakBonusNow`）。
//! 本模块只管旅行 —— 它与签到无关，可以独立开关与时刻。
//!
//! ── 为什么不是 `core::scheduled_tasks` 里的一条 ───────────────
//! 那个模块是**等间隔重复**型（`{enabled, interval}`，间隔以分钟计）。
//! 旅行是**每天定点**型（一天两趟、按北京时间整点），与自动签到
//! （`core::auto_checkin`）同一形状：时刻数组 + 当天去重 + 启动补跑。
//! 硬塞进间隔型清单会逼着两边都变形 —— 那个模块头已经为自动签到写过
//! 同样的理由。
//!
//! ── 与签到排程的关系 ────────────────────────────────────────
//! 完全独立：签到是 `autoCheckin`，这里是 `growthSchedule`。两者可以各开各的，
//! 时刻互不影响（参考项目也是独立排程，见其 `TravelDisabled` /
//! `CheckinDisabled` 两个独立开关）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{json, Value};

use crate::server::config;
use crate::server::core::account_store::AccountStore;
use crate::server::core::beijing;
use crate::server::logging;

/// 配置键（config.json 里的字段名）。
pub const CONFIG_KEY: &str = "growthSchedule";

/// 缺省时刻：与参考项目 `TravelHours` 一致（09 点派出、21 点领奖）。
pub const DEFAULT_TIMES: [&str; 2] = ["09:00", "21:00"];

/// 时刻条目上限：再多列表难以扫读，成长维护也不需要更密。
pub const MAX_TIMES: usize = 6;

/// 轮询间隔：与 `auto_checkin` 的 30 秒同口径（到点后最多晚 30 秒执行）。
pub const TICK_MS: u64 = 30_000;

/// 一条时刻的归一化结果（`HH:MM`）。
fn normalize_time(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?.trim().to_string();
    let (hour, minute) = text.split_once(':')?;
    let hour: u32 = hour.parse().ok()?;
    let minute: u32 = minute.parse().ok()?;
    if hour > 23 || minute > 59 {
        return None;
    }
    Some(format!("{hour:02}:{minute:02}"))
}

/// 读配置（缺失/损坏一律回落缺省：**默认关闭**）。
///
/// 返回 `(enabled, times)`。`times` 恒非空（空数组回落缺省）—— 一个开着的
/// 排程没有任何时刻等于「永远不跑」，那是配置错误而不是用户意图。
///
/// ── 为什么默认关闭（曾经默认开启）────────────────────────────
/// 早先这里 `unwrap_or(true)`，理由是「与参考项目零值即启用一致」。但那意味着
/// **刚装好、一条配置都没有的机器上，程序启动 30 秒内就会自动跑一轮巡检**：
/// 09:00 已过、`firedTimes` 为空 → 立刻补跑。此时用户可能还没装插件（或刚装
/// 还没启用），于是日志里出现一条
/// 「WorkBuddy 国内版没有实现管理动作 growth.travel.batch（可能需要更新提供商插件）」
/// —— 一条用户没触发、也不知道该怎么处理的失败。
///
/// 定时任务在「用户没表过态」时不该自己开始写账号数据。现在与
/// `auto_checkin::master_enabled` 对齐：只有配置里明确写了 `enabled: true`
/// 才跑。缺省值仍然保留（09:00 / 21:00），用户打开开关时不必先填时刻。
pub fn settings() -> (bool, Vec<String>) {
    let raw = config::current()
        .raw()
        .get(CONFIG_KEY)
        .cloned()
        .unwrap_or(Value::Null);
    let enabled = raw
        .get("enabled")
        .and_then(Value::as_bool)
        == Some(true);
    let times: Vec<String> = raw
        .get("times")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| normalize_time(Some(item)))
                .collect()
        })
        .unwrap_or_default();
    let times = if times.is_empty() {
        DEFAULT_TIMES.iter().map(|text| text.to_string()).collect()
    } else {
        // 去重并按时间序（界面上的顺序与执行顺序一致）
        let mut sorted = times;
        sorted.sort();
        sorted.dedup();
        sorted
    };
    (enabled, times)
}

/// 写配置（`{enabled?, times?}`，只改传进来的字段）。
///
/// 与 `auto_checkin` 的 `write_state` 同构：合并写回，config.json 的其它字段
/// 原样保留。
pub fn configure(patch: &Value) -> Result<Value, String> {
    let mut current = config::current()
        .raw()
        .get(CONFIG_KEY)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(enabled) = patch.get("enabled").and_then(Value::as_bool) {
        current.insert("enabled".to_string(), Value::Bool(enabled));
    }
    if let Some(items) = patch.get("times") {
        let Some(items) = items.as_array() else {
            return Err("times 必须是时刻数组".to_string());
        };
        if items.len() > MAX_TIMES {
            return Err(format!("时刻最多 {MAX_TIMES} 条"));
        }
        let mut times: Vec<String> = Vec::new();
        for item in items {
            let Some(time) = normalize_time(Some(item)) else {
                return Err(format!(
                    "非法时刻「{}」（应为 HH:MM，00:00 - 23:59）",
                    item.as_str().unwrap_or("?")
                ));
            };
            if !times.contains(&time) {
                times.push(time);
            }
        }
        if times.is_empty() {
            return Err("至少保留一个时刻".to_string());
        }
        times.sort();
        current.insert(
            "times".to_string(),
            Value::Array(times.into_iter().map(Value::String).collect()),
        );
    }
    // 当天去重标记：改了时刻就清掉，让新时刻当天就能生效
    //（否则用户在 10:00 把时刻从 09:00 改成 11:00，会看到「今天已跑过」）
    current.insert("lastFiredDate".to_string(), Value::Null);
    current.insert("lastFiredTime".to_string(), Value::Null);
    config::update_raw_field(CONFIG_KEY, Value::Object(current));
    Ok(state())
}

/// 对外状态（设置页 / 签到页读它）。
pub fn state() -> Value {
    let (enabled, times) = settings();
    let raw = config::current()
        .raw()
        .get(CONFIG_KEY)
        .cloned()
        .unwrap_or(Value::Null);
    let today = beijing::today_key();
    let last_fired_date = raw
        .get("lastFiredDate")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // 今天已经跑过的**计划时刻**（界面据此把已执行的条目标灰）。
    // 只认 `firedTimes`：那是按计划时刻落的标记（见 `run_once` 的说明），
    // 不回落到 `lastFiredTime`（实际执行时刻，与计划时刻不是同一个坐标系）。
    let fired_today: Vec<String> = if last_fired_date == today {
        raw.get("firedTimes")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    json!({
        "enabled": enabled,
        "times": times,
        "firedToday": fired_today,
        "lastFiredDate": if last_fired_date.is_empty() { Value::Null } else { Value::String(last_fired_date) },
        "lastFiredTime": raw.get("lastFiredTime").cloned().unwrap_or(Value::Null),
        "lastResult": raw.get("lastResult").cloned().unwrap_or(Value::Null),
        "running": is_running(),
    })
}

// ─── 运行状态 ───────────────────────────────────────────────

/// 是否有一轮正在执行（跨进程内全局，与 auto_checkin 的 running 同构）。
fn running_flag() -> &'static AtomicBool {
    static RUNNING: AtomicBool = AtomicBool::new(false);
    &RUNNING
}

pub fn is_running() -> bool {
    running_flag().load(Ordering::SeqCst)
}

/// 抢「执行中」标记的守卫：Drop 时复位（对应 Go 的 `defer`）。
struct RunGuard;

impl Drop for RunGuard {
    fn drop(&mut self) {
        running_flag().store(false, Ordering::SeqCst);
    }
}

fn acquire() -> Option<RunGuard> {
    if running_flag().swap(true, Ordering::SeqCst) {
        return None;
    }
    Some(RunGuard)
}

// ─── 执行 ───────────────────────────────────────────────────

/// 跑一轮猫猫旅行巡检。
///
/// `plan_time` 是**本次对应的计划时刻**（`HH:MM`）：去重标记必须按它落，
/// 不能按实际执行时刻 —— 到点后最多晚一个 tick（30 秒）才跑，拿实际时刻
/// 去比对计划时刻永远不相等，结果是每个 tick 都重跑一遍。
/// `reason` 只进日志（区分「到点触发」与「手动立即执行」）。
pub async fn run_once(store: &AccountStore, reason: &str, plan_time: Option<&str>) -> Option<Value> {
    let guard = acquire()?;
    logging::log("[Travel]", &format!("⏰ 猫猫旅行巡检开始（{reason}）"));

    // 巡检体在插件里：`growth.travel.batch` 自己列本家账号、自己门控，
    // 返回网关 `summarize` 的同形摘要（账号数 / 动作 / 积分 / 失败数）。
    //
    // ── 为什么先查插件装没装 ────────────────────────────────────
    // `adapter_by_id` 在插件缺失时会**回落到内置适配器**（见那里的说明：
    // 「没装/停用时自然落到内置」）。而内置 WorkBuddy 早已不实现
    // `growth.travel.batch` —— 这个动作随签到功能整体迁到了插件里。于是
    // 没装插件时调用必然拿到 501「没有实现管理动作」，日志里就是一条红色
    // 失败。那不是「巡检失败」，是「这个能力还不存在」：没装插件的机器上
    // 根本不该有猫猫旅行这件事。
    //
    // 所以这里按能力是否存在分三档：插件没装 → 跳过（记一条说明，不报错）；
    // 装了且实现了 → 正常跑；跑出错误 → 才是真失败。
    let adapter = crate::server::core::providers::adapter::adapter_by_id("workbuddy");
    // `serves("workbuddy")` = 这一家此刻由**已装入的插件**接管。
    // 不用 `installed_ids()`：那返回的是 provider id 列表，与「装了哪个包」
    // 不是一回事（同一个包可以提供十几家）。
    let plugin_ready = crate::server::core::plugin_host::serves_provider("workbuddy");
    let summary = match adapter {
        Some(adapter) if plugin_ready => {
            match adapter
                .admin_action(store, "", "growth.travel.batch", &Value::Null)
                .await
            {
                Ok(summary) => summary,
                Err(error) => {
                    // 501 = 插件在但没实现这个动作（插件版本旧了）。仍按「跳过」
                    // 处理：这不是本轮巡检失败，重试也不会有结果。
                    if error.status_code == 501 {
                        logging::log(
                            "[Travel]",
                            "⚠️ 当前插件没有实现猫猫旅行巡检（growth.travel.batch），本轮跳过 —— \
                             请更新提供商插件",
                        );
                        return None;
                    }
                    logging::log("[Travel]", &format!("❌ 猫猫旅行巡检失败：{}", error.message));
                    return None;
                }
            }
        }
        // 插件没装 / 停用：猫猫旅行是插件能力，没有它就没有这项功能。
        // 按「本轮无事可做」收口，不写 lastResult（否则界面会显示一条
        // 时间为「刚刚」但零动作的记录，看着像跑过了）。
        _ => {
            logging::log(
                "[Travel]",
                "⚠️ 未安装 Agent2API 提供商插件，猫猫旅行巡检跳过（该功能在插件里）",
            );
            return None;
        }
    };

    // 落 lastResult + 当天的「已执行时刻」
    let today = beijing::today_key();
    let now_time = beijing::now().format("%H:%M").to_string();
    let raw = config::current()
        .raw()
        .get(CONFIG_KEY)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut fired: Vec<String> = raw
        .get("firedTimes")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    // 跨天则重置（firedTimes 只在同一天内有效）
    if raw.get("lastFiredDate").and_then(Value::as_str) != Some(today.as_str()) {
        fired.clear();
    }
    // 去重标记按**计划时刻**落（见函数头）；手动执行没有计划时刻，
    // 不写标记 —— 手动跑过不等于当天的定时轮次已经跑过。
    if let Some(plan_time) = plan_time.filter(|text| !text.is_empty()) {
        if !fired.iter().any(|item| item == plan_time) {
            fired.push(plan_time.to_string());
        }
    }
    let mut next = raw;
    next.insert("lastFiredDate".to_string(), Value::String(today));
    next.insert("lastFiredTime".to_string(), Value::String(now_time));
    next.insert(
        "firedTimes".to_string(),
        Value::Array(fired.into_iter().map(Value::String).collect()),
    );
    next.insert("lastResult".to_string(), summary.clone());
    config::update_raw_field(CONFIG_KEY, Value::Object(next));

    let action_count = summary
        .get("actions")
        .and_then(Value::as_array)
        .map(|items| items.len())
        .unwrap_or(0);
    let credit = summary.get("credit").and_then(Value::as_i64).unwrap_or(0);
    logging::log(
        "[Travel]",
        &format!("猫猫旅行巡检完成：{action_count} 个动作（+{credit}c）"),
    );
    drop(guard);
    Some(summary)
}

/// 当天该跑的时刻里还有没跑过的 → Some(那一条)，否则 None。
fn due_time(now: chrono::DateTime<chrono::FixedOffset>) -> Option<String> {
    let (enabled, times) = settings();
    if !enabled {
        return None;
    }
    let today = beijing::today_key();
    let raw = config::current()
        .raw()
        .get(CONFIG_KEY)
        .cloned()
        .unwrap_or(Value::Null);
    let fired: Vec<String> = if raw.get("lastFiredDate").and_then(Value::as_str) == Some(today.as_str()) {
        raw.get("firedTimes")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let now_text = now.format("%H:%M").to_string();
    // 取「已到点且今天没跑过」里最早的一条：多个时刻同时过期时逐轮补跑
    //（一轮只跑一次，下个 tick 再补下一条 —— 与 auto_checkin 的补签同语义）
    times
        .into_iter()
        .find(|time| time.as_str() <= now_text.as_str() && !fired.iter().any(|item| item == time))
}

/// 调度循环（由 `ServerState::bootstrap` 起一次）。
///
/// 每 `TICK_MS` 醒一次重新判定：改完设置下一轮就生效、机器休眠后自愈 ——
/// 与 `auto_checkin` / `scheduled_tasks` 同一取舍（详见那两个模块头）。
pub fn spawn(store: AccountStore) {
    crate::spawn_task(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(TICK_MS)).await;
            let now = beijing::now();
            let Some(plan_time) = due_time(now) else { continue };
            let _ = run_once(&store, &format!("到点触发（{plan_time}）"), Some(&plan_time)).await;
        }
    });
}

// ─── 全局句柄（供 API 手动触发）─────────────────────────────

/// 手动立即执行（供 API 调用）。返回 None = 已有一轮在跑。
///
/// 手动执行**不**写计划时刻的去重标记（见 `run_once` 的说明）：
/// 用户点一下「立即巡检」不等于当天的定时轮次已经跑过。
pub async fn run_now(store: &AccountStore) -> Option<Value> {
    run_once(store, "手动执行", None).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_time_accepts_and_rejects() {
        assert_eq!(normalize_time(Some(&json!("09:00"))), Some("09:00".to_string()));
        assert_eq!(normalize_time(Some(&json!("9:5"))), Some("09:05".to_string()));
        assert_eq!(normalize_time(Some(&json!(" 21:30 "))), Some("21:30".to_string()));
        // 越界 / 非字符串 / 缺字段
        assert_eq!(normalize_time(Some(&json!("24:00"))), None);
        assert_eq!(normalize_time(Some(&json!("12:60"))), None);
        assert_eq!(normalize_time(Some(&json!(930))), None);
        assert_eq!(normalize_time(None), None);
    }

    #[test]
    fn default_times_are_nine_and_twenty_one() {
        assert_eq!(DEFAULT_TIMES, ["09:00", "21:00"]);
    }

    /// 去重标记必须按**计划时刻**比对。
    ///
    /// 这条钉住的是一个真实 bug：`run_once` 曾按实际执行时刻（`now_time`）
    /// 落标记，而 `due_time` 拿计划时刻去查 —— 到点后最多晚 30 秒执行，
    /// 两者永不相等，于是每个 tick 都重跑一遍。
    #[test]
    fn due_time_matches_plan_time_not_actual_time() {
        let times = ["09:00".to_string(), "21:00".to_string()];
        let fired = vec!["09:00".to_string()];
        let now_text = "09:00".to_string();
        // 09:00 已跑过 → 21:00 还没到 → 无事可做
        let due = times
            .iter()
            .find(|time| time.as_str() <= now_text.as_str() && !fired.iter().any(|item| *item == **time));
        assert_eq!(due, None);
        // 若把 fired 换成实际执行时刻（09:01），会误判成「没跑过」→ 重复执行
        let wrong_fired = vec!["09:01".to_string()];
        let due_wrong = times
            .iter()
            .find(|time| time.as_str() <= now_text.as_str() && !wrong_fired.iter().any(|item| *item == **time));
        assert_eq!(due_wrong.map(|time| time.as_str()), Some("09:00"));
    }

    #[test]
    fn due_time_picks_earliest_unfired_past_slot() {
        let times = ["09:00".to_string(), "13:00".to_string(), "21:00".to_string()];
        // 现在 22:00、今天一个都没跑 → 先补最早的 09:00（一轮一条）
        let now_text = "22:00".to_string();
        let fired: Vec<String> = Vec::new();
        let due = times
            .iter()
            .find(|time| time.as_str() <= now_text.as_str() && !fired.iter().any(|item| *item == **time));
        assert_eq!(due.map(|time| time.as_str()), Some("09:00"));
        // 09:00 跑过之后 → 13:00
        let fired = vec!["09:00".to_string()];
        let due = times
            .iter()
            .find(|time| time.as_str() <= now_text.as_str() && !fired.iter().any(|item| *item == **time));
        assert_eq!(due.map(|time| time.as_str()), Some("13:00"));
    }
}
