//! 提供商插件包的管理面：`/api/plugins*`。
//!
//! ── 这个模块解决什么问题 ────────────────────────────────────
//! 各家的转发与账号实现都在独立的提供商插件里（`agent2api-providers`，
//! 不随本仓库发布）。插件是**用户安装**的，所以「装 / 卸 / 启停」必须有一个
//! 界面入口 —— 否则用户只能手工往 `{config_dir}/plugins/` 里摆文件。
//!
//! ── 端点（全部挂 protected：它们能 spawn 任意二进制）──────────
//! ```text
//!   GET  /api/plugins                 已安装包列表 + 宿主状态 + 当前平台
//!   POST /api/plugins/install         上传 zip（base64 或裸字节），装完即热加载
//!   POST /api/plugins/uninstall       {id}
//!   POST /api/plugins/enable          {id, enabled}
//!   POST /api/plugins/provider        {id, provider, enabled}
//! ```
//!
//! ── 为什么安装收 base64 而不是 multipart ──────────────────────
//! 管理面所有写接口的既有形态都是「JSON 体 + `Bytes` 提取」（见
//! `api::accounts` 的 `authFileB64`、`api::backup_api` 的上传）。multipart 要
//! 额外引一个解析器，而这里的调用方只有自家前端 —— 它读文件转 base64 是
//! 一行 `FileReader`。包约 5MB，base64 后 6.7MB，在 32MB 的管理面上限内。
//!
//! ── 装完为什么要热加载 ──────────────────────────────────────
//! 用户点「安装」时期待的是「马上能用」。不热加载就得重启网关（桌面端等于
//! 重开应用），而插件架构的整个卖点就是「加一家不用重新发版、装个包就行」。
//! 热加载失败不回滚安装：包已经落盘、注册表已经写了，重启后照样生效 ——
//! 这时错误只说明「这次没起来」，界面会显示宿主状态与原因。
//!
//! ── 安全边界 ────────────────────────────────────────────────
//! 装插件等于让用户在本机跑任意代码，所以这一组端点**必须**过鉴权
//! （挂 `protected`），与 `/api/backup/import` 同级。签名校验在
//! `plugin_host::install` 里做（ed25519，公钥编译期内置）：未签名允许装但界面
//! 标黄，签名无效一律拒绝 —— 信任由「用户选的文件 + 可验证的发布者」共同承担。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use axum::body::Bytes;
use axum::response::Response;
use base64::Engine;
use serde_json::{json, Value};

use crate::server::core::account_store::plugin_accounts;
use crate::server::core::plugin_host;
use crate::server::errors::management_error;
use crate::server::http::{ok_json, parse_body};

/// 上传包的大小上限（base64 解码后）。
///
/// 32MB 是管理面通用上限（`http::MAX_BODY_SIZE`）；这里再收一道 24MB：
/// 插件包是「一个二进制 + 一份清单」，正常 5–15MB，超过这个量级多半是传错了
/// 文件（整份仓库的 zip、带调试符号的包）。收窄的收益是明确报错，而不是
/// 解到一半撑爆磁盘。
const MAX_PACK_BYTES: usize = 24 * 1024 * 1024;

/// GET /api/plugins —— 列表 + 宿主状态。
///
/// 响应形状（前端契约）：
/// ```json
/// {
///   "packs": [{ id, name, version, enabled, signed, installedAt, entryPresent,
///               protocol, homepage,
///               providers: [{ id, label, enabled, loaded }] }],
///   "host": { "running": true, "providers": ["workbuddy"], "crashes": 0,
///             "disabled": false, "lastError": null },
///   "target": "x86_64-windows",
///   "protocol": 1,
///   "pluginsDir": "C:\\Users\\…\\.agent2api\\plugins"
/// }
/// ```
/// `host` 为 null 表示宿主没起来（没装包，或装了但启动失败）。
/// `target` / `protocol` 给界面做「你下载的包对不对」的提示。
pub async fn list() -> Response {
    let host = plugin_host::current().map(|host| {
        let status = host.status();
        json!({
            "running": status.running,
            "providers": status.providers,
            "crashes": status.crashes,
            "disabled": status.disabled,
            "lastError": status.last_error,
        })
    });
    let packs: Vec<Value> = plugin_host::list_packs()
        .into_iter()
        .filter_map(|pack| serde_json::to_value(pack).ok())
        .collect();
    ok_json(json!({
        "packs": packs,
        "host": host,
        // 已加载插件申报的管理面板（`["upstream_headers", "captcha_pool", …]`）。
        // 界面按它决定渲染哪些面板 —— 没装插件时是空数组，那些面板整块不出现。
        "adminPanels": plugin_host::admin_panels(),
        // 各家申报的账号行渲染参数（`{provider: AccountFeatures}`）。界面按它
        // 决定标识列读哪个键、要不要给余额入口 —— 没申报的家不在表里，界面
        // 走自己的兜底表（旧插件的兼容路径）。
        "accountFeatures": plugin_host::account_features(),
        "target": plugin_host::host_target(),
        "protocol": agent2api_provider_protocol::PROTOCOL_VERSION,
        "pluginsDir": plugin_host::plugins_dir_display(),
    }))
}

/// 当前插件面快照：包列表 + 宿主状态。四个写端点共用同一份形状。
///
/// ── 为什么每次都全量返回 ────────────────────────────────────
/// 界面在每次操作后都要重画（包的启停、某一家是否 loaded、宿主是否在跑都可能
/// 因这一次操作而变），前端若只拿一个 `ok` 就得再发一次 GET。全量返回省一次
/// 往返，也让「操作结果」与「列表状态」不可能对不上（同一个快照）。
fn snapshot() -> Value {
    let packs: Vec<Value> = plugin_host::list_packs()
        .into_iter()
        .filter_map(|item| serde_json::to_value(item).ok())
        .collect();
    let host = plugin_host::current().map(|host| {
        let status = host.status();
        json!({
            "running": status.running,
            "providers": status.providers,
            "crashes": status.crashes,
            "disabled": status.disabled,
            "lastError": status.last_error,
        })
    });
    json!({
        "packs": packs,
        "host": host,
        // 与 `list` 同一字段：面板声明会随「装了 / 停了某个包」变化
        "adminPanels": plugin_host::admin_panels(),
    })
}

/// POST /api/plugins/install —— 上传 zip 并安装。
///
/// 请求体两种形态（都接受，按有没有 `packB64` 判）：
///   - `{"packB64": "UEsDB…"}`：base64 的 zip（前端 FileReader 的产物）；
///   - 裸字节体（`Content-Type: application/zip`）：`curl --data-binary` 那种。
///
/// 成功返回 `{pack, packs, host}`：`pack` 是刚装的那个，`packs` 是装完后的
/// 全量列表（前端直接替换，省一次往返），`host` 是热加载后的宿主状态。
///
/// 热加载失败**不**让整个请求失败：包已落盘，重启后生效。响应里
/// `hostError` 带原因，界面显示成一条黄色提示而不是红色错误。
///
/// ── 为什么装之前先关宿主（重要）─────────────────────────────
/// 升级 / 同版本重装要删掉旧的版本目录，而 Windows 上正在运行的 exe 删不掉
/// （`os error 5 拒绝访问`）。所以顺序必须是「先关 → 再装 → 再起」：
/// 先装后关会在删目录那一步失败，而那一步失败时旧版本已经被删了一半 ——
/// 用户会停在「包没了、进程也停了」的状态。
///
/// 先关还有第二个好处：关掉之后 `install_zip` 里的 rename 一定不会撞上
/// 文件占用。代价是装失败时要**重新拉起来**（下面 `Err` 分支），否则
/// 「上传一个坏包」会把正在用的插件停掉。
pub async fn install(body: Bytes) -> Response {
    let bytes = match pack_bytes(&body) {
        Ok(bytes) => bytes,
        Err(message) => return management_error(400, message),
    };
    if bytes.len() > MAX_PACK_BYTES {
        return management_error(
            400,
            format!(
                "插件包太大（{}），上限 {}。请确认选的是插件包 zip，而不是整个仓库的压缩包",
                human_size(bytes.len()),
                human_size(MAX_PACK_BYTES)
            ),
        );
    }
    let store = plugin_accounts::shared();
    // 先停宿主：见上面「为什么装之前先关宿主」
    if let Some(host) = plugin_host::current() {
        host.shutdown().await;
    }
    plugin_host::detach();
    plugin_host::clear_providers();
    let pack = match plugin_host::install_zip(&bytes) {
        Ok(pack) => pack,
        Err(message) => {
            // 装失败：把原来那个（或注册表里现有的）重新拉起来，
            // 别让一次失败的上传把在用的插件停掉
            if let Some(store) = store {
                let _ = plugin_host::reload_host(store).await;
            }
            return management_error(400, format!("安装失败：{message}"));
        }
    };
    // 热加载：拿进程级账号库句柄（bootstrap 时装过一次）。拿不到说明网关
    // 还没初始化完，那就不重载 —— 下次启动自然会读到新注册表。
    let mut host_error = None;
    if let Some(store) = store {
        if plugin_host::reload_host(store).await.is_none() {
            host_error = Some(
                "插件包已安装，但本次没能启动（详见日志）；重启应用后会自动加载".to_string(),
            );
        }
    }
    let mut body = snapshot();
    if let Some(object) = body.as_object_mut() {
        object.insert("pack".to_string(), serde_json::to_value(pack).unwrap_or(Value::Null));
        object.insert("hostError".to_string(), json!(host_error));
    }
    ok_json(body)
}

/// POST /api/plugins/uninstall —— `{id}`。
///
/// 先关宿主再删目录：Windows 上正在运行的 exe 删不掉。关完之后如果还有别的
/// 启用的包，重新装配一次（单宿主模型下卸载一个包等于换一个宿主）。
pub async fn uninstall(body: Bytes) -> Response {
    let payload = parse_body(&body).unwrap_or(Value::Null);
    let id = payload.get("id").and_then(Value::as_str).unwrap_or("").trim();
    if id.is_empty() {
        return management_error(400, "缺少插件包 id");
    }
    // 先停：把宿主关掉，文件才释放
    if let Some(host) = plugin_host::current() {
        host.shutdown().await;
    }
    plugin_host::detach();
    plugin_host::clear_providers();
    if let Err(message) = plugin_host::uninstall(id) {
        // 删目录失败（文件被占用）时把宿主重新拉起来，别让用户停在
        // 「注册表还在、进程没了」的半死状态
        if let Some(store) = plugin_accounts::shared() {
            let _ = plugin_host::reload_host(store).await;
        }
        return management_error(400, message);
    }
    let mut host_error = None;
    if let Some(store) = plugin_accounts::shared() {
        if plugin_host::reload_host(store).await.is_none() && !plugin_host::list_packs().is_empty() {
            host_error = Some("卸载成功，但剩下的插件包没能启动（详见日志）".to_string());
        }
    }
    let mut body = snapshot();
    if let Some(object) = body.as_object_mut() {
        object.insert("ok".to_string(), json!(true));
        object.insert("hostError".to_string(), json!(host_error));
    }
    ok_json(body)
}

/// POST /api/plugins/enable —— `{id, enabled}`（整包启停）。
pub async fn set_enabled(body: Bytes) -> Response {
    let payload = parse_body(&body).unwrap_or(Value::Null);
    let id = payload.get("id").and_then(Value::as_str).unwrap_or("").trim();
    let enabled = payload
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if id.is_empty() {
        return management_error(400, "缺少插件包 id");
    }
    if let Err(message) = plugin_host::set_pack_enabled(id, enabled) {
        return management_error(400, message);
    }
    let mut host_error = None;
    if let Some(store) = plugin_accounts::shared() {
        if plugin_host::reload_host(store).await.is_none() && enabled {
            host_error = Some("已启用，但本次没能启动（详见日志）".to_string());
        }
    }
    let mut body = snapshot();
    if let Some(object) = body.as_object_mut() {
        object.insert("ok".to_string(), json!(true));
        object.insert("hostError".to_string(), json!(host_error));
    }
    ok_json(body)
}

/// POST /api/plugins/provider —— `{id, provider, enabled}`（包内单家启停）。
pub async fn set_provider(body: Bytes) -> Response {
    let payload = parse_body(&body).unwrap_or(Value::Null);
    let id = payload.get("id").and_then(Value::as_str).unwrap_or("").trim();
    let provider = payload
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let enabled = payload
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if id.is_empty() || provider.is_empty() {
        return management_error(400, "缺少插件包 id 或提供商 id");
    }
    if let Err(message) = plugin_host::set_provider_enabled(id, provider, enabled) {
        return management_error(400, message);
    }
    // 单家启停只影响「这个 id 要不要进注册表」，重新装配最省事且语义一致
    let mut host_error = None;
    if let Some(store) = plugin_accounts::shared() {
        if plugin_host::reload_host(store).await.is_none() {
            host_error = Some("设置已保存，但插件没能重新启动（详见日志）".to_string());
        }
    }
    let mut body = snapshot();
    if let Some(object) = body.as_object_mut() {
        object.insert("ok".to_string(), json!(true));
        object.insert("hostError".to_string(), json!(host_error));
    }
    ok_json(body)
}

/// GET /api/plugins/update —— 查插件仓库有没有新版本。
///
/// 返回形状见 `plugin_host::update::check` 的文档。`available=false` 时
/// `reason` 说明原因（已是最新 / 仓库没发过版 / 网络失败）—— 界面把前两种
/// 显示成中性提示、网络失败显示成错误，靠 `reason` 区分不了，所以**网络失败
/// 会带 `error: true`**：那是「查不到」而不是「没有更新」。
pub async fn check_update() -> Response {
    let packs = plugin_host::list_packs();
    // 以第一个包为准：插件仓库当前只发一个包。多包时这里要按 id 循环，
    // 但「哪个包该更新」本身需要界面参与（用户可能只想更新其中一个），
    // 所以先按单包做，多包时再补 `id` 参数。
    let Some(pack) = packs.first() else {
        return ok_json(json!({
            "available": false,
            "current": Value::Null,
            "latest": Value::Null,
            "asset": Value::Null,
            "reason": "还没有安装任何插件包",
            "error": false,
        }));
    };
    let result = plugin_host::update::check(
        &pack.id,
        &pack.version,
        &plugin_host::host_target(),
    )
    .await;
    ok_json(result)
}

/// POST /api/plugins/update —— 下载并安装最新版。
///
/// ── 为什么下载与安装是**同一个请求** ────────────────────────
/// 插件包十几 MB，分两步（先下到磁盘、再点安装）只是把同一件事拆开给用户看。
/// 这个请求会跑十几秒，前端显示「更新中…」即可。
///
/// ── 装的是哪一个 ────────────────────────────────────────────
/// 服务端自己查一次「最新版」再下载 —— 不接受前端传 URL。让前端指定下载地址
/// 等于把「装什么」的决定权交给一个可能被 XSS 影响的上下文，而这一层装的是
/// **可执行文件**。
pub async fn run_update() -> Response {
    let packs = plugin_host::list_packs();
    let Some(pack) = packs.first() else {
        return management_error(400, "还没有安装任何插件包，请先用「安装插件包」选文件安装");
    };
    let target = plugin_host::host_target();
    let asset = match plugin_host::update::latest(&pack.id, &target).await {
        Ok(Some(asset)) => asset,
        Ok(None) => {
            return management_error(404, "插件仓库还没有发布任何版本");
        }
        Err(message) => return management_error(502, message),
    };
    let newer = crate::server::core::update::compare_versions(&asset.version, &pack.version)
        .map(|order| order > 0)
        .unwrap_or(false);
    if !newer {
        return management_error(400, format!("已经是最新版本（v{}）", pack.version));
    }
    let bytes = match plugin_host::update::download(&asset).await {
        Ok(bytes) => bytes,
        Err(message) => return management_error(502, message),
    };
    // 与 `install` 同一顺序：先关宿主再落盘（Windows 上运行中的 exe 删不掉，
    // 而升级要先删旧版本目录）。下载失败已经在上一步返回，那时宿主没动过。
    let store = plugin_accounts::shared();
    if let Some(host) = plugin_host::current() {
        host.shutdown().await;
    }
    plugin_host::detach();
    plugin_host::clear_providers();
    let installed = match plugin_host::install_zip(&bytes) {
        Ok(pack) => pack,
        // 签名/校验失败也走这里：更新包必须重新过一遍信任链，
        // 「旧版本验过了」不代表新版本可信
        Err(message) => {
            // 新包没装成，把原来的重新拉起来（旧版本目录没被动过）
            if let Some(store) = store {
                let _ = plugin_host::reload_host(store).await;
            }
            return management_error(400, format!("更新失败：{message}"));
        }
    };
    let mut host_error = None;
    if let Some(store) = store {
        if plugin_host::reload_host(store).await.is_none() {
            host_error = Some("插件包已更新，但本次没能启动（详见日志）；重启应用后会自动加载".to_string());
        }
    }
    let mut body = snapshot();
    if let Some(object) = body.as_object_mut() {
        object.insert(
            "pack".to_string(),
            serde_json::to_value(installed).unwrap_or(Value::Null),
        );
        object.insert("hostError".to_string(), json!(host_error));
        object.insert("updatedFrom".to_string(), json!(pack.version));
    }
    ok_json(body)
}

/// GET /api/plugins/release —— 商店页的「有什么可装」。
///
/// 与 `/api/plugins/update` 的区别：**不要求已经装过包**。那个端点的第一行是
/// `packs.first()`，没装就直接报错；商店页恰恰要在「一个都没装」时工作，
/// 所以它查的是仓库本身。
///
/// 响应形状（前端契约）：
/// ```json
/// { "available": true, "version": "0.1.0", "page": "https://…",
///   "current": "x86_64-pc-windows-msvc",
///   "platforms": [{ "target": "…", "size": 13189856, "url": "…", "current": true }],
///   "reason": null, "error": false }
/// ```
/// `available=false` 时 `reason` 说明原因，`error` 区分「仓库没发版」（中性）
/// 与「查不到」（网络/限额，界面报红）—— 与 `check` 的三档语义一致。
///
/// ── 为什么要读索引仓库 ──────────────────────────────────────
/// 插件不止一个，而且每个可能在自己的仓库发版。`index.json` 声明有哪些插件、
/// 各自的包在哪，本端点遍历它、按条目去对应仓库查 Release，一次返回整张
/// 清单 —— 商店页据此渲染成多张卡。
///
/// ── 索引读不到时回落 ────────────────────────────────────────
/// 回落到「官方包」单条视图（`FALLBACK_REPO`），而不是把商店页变成报错。
/// 索引仓库还没建、或者只放通了 raw 没放通 api 的环境仍然能用。
///
/// 响应形状（前端契约）：
/// ```json
/// { "available": true, "current": "x86_64-pc-windows-msvc", "plugins": [
///     { "id": "agent2api-providers", "name": "…", "description": "…",
///       "official": true, "version": "0.1.0", "page": "https://…",
///       "platforms": [{ "target": "…", "size": 13189856, "url": "…", "current": true }] }
///   ], "reason": null, "error": false }
/// ```
pub async fn release() -> Response {
    let current = plugin_host::host_target();
    let entries = plugin_host::update::read_index().await;

    // ── 索引读不到 = 离线或索引仓库不可达 ──────────────────────
    // 这里**不再回落到硬编码的官方包**：离线时给用户看一张点不动的卡，
    // 比诚实地说「连不上插件仓库」更糟 —— 前者让人以为「仓库里没东西」，
    // 后者指向真正该做的事（检查网络 / 代理）。
    if entries.is_empty() {
        return ok_json(json!({
            "available": false,
            "current": current,
            "plugins": [],
            "reason": "连不上插件仓库，请检查网络或代理后重试",
            "error": true,
        }));
    }

    let mut plugins: Vec<Value> = Vec::new();
    let mut last_error: Option<String> = None;
    for entry in &entries {
        // 明细给出真正的发布仓库与版本；读不到时用索引条目里的 repo
        let detail = plugin_host::update::read_detail(&entry.manifest).await;
        let (repo, version_hint) = match &detail {
            Some(detail) if !detail.repo.is_empty() => {
                (detail.repo.clone(), detail.version.clone())
            }
            _ => (entry.repo.clone(), String::new()),
        };
        if repo.is_empty() {
            continue;
        }
        // 图标与 Release 查询互不依赖，顺序取即可
        let icon = plugin_host::update::read_store_icon(&entry.icon).await;
        match plugin_host::update::release_assets_in(&repo, &entry.id).await {
            Ok(Some(assets)) => plugins.push(plugin_entry_json(
                entry,
                &assets.version,
                Some(&assets),
                &current,
                icon,
            )),
            Ok(None) => {
                // 这个插件还没发版：仍要出现在列表里，只是没有可装平台
                plugins.push(plugin_entry_json(entry, &version_hint, None, &current, icon))
            }
            Err(message) => {
                last_error = Some(message);
                plugins.push(plugin_entry_json(entry, &version_hint, None, &current, icon))
            }
        }
    }

    if plugins.is_empty() {
        let has_error = last_error.is_some();
        return ok_json(json!({
            "available": false,
            "current": current,
            "plugins": [],
            "reason": last_error.unwrap_or_else(|| "插件索引里没有任何条目".to_string()),
            "error": has_error,
        }));
    }
    // 官方的排前面，其余按 id 字典序（索引里的顺序可能被人随意写）
    plugins.sort_by(|a, b| {
        let official =
            |value: &Value| value.get("official").and_then(Value::as_bool).unwrap_or(false);
        let id = |value: &Value| {
            value.get("id").and_then(Value::as_str).unwrap_or("").to_string()
        };
        official(b).cmp(&official(a)).then_with(|| id(a).cmp(&id(b)))
    });
    ok_json(json!({
        "available": true,
        "current": current,
        "plugins": plugins,
        "reason": Value::Null,
        "error": false,
    }))
}

/// 单个插件在商店页里的形状。
fn plugin_entry_json(
    entry: &plugin_host::update::IndexEntry,
    version: &str,
    assets: Option<&plugin_host::update::ReleaseAssets>,
    current: &str,
    icon: Option<String>,
) -> Value {
    let platforms: Vec<Value> = assets
        .map(|assets| {
            assets
                .platforms
                .iter()
                .map(|(target, url, size)| {
                    json!({
                        "target": target,
                        "size": size,
                        "url": url,
                        "current": target == current,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    json!({
        "id": entry.id,
        "name": entry.name,
        "description": entry.description,
        "official": entry.official,
        "version": if version.is_empty() { Value::Null } else { json!(version) },
        "page": assets.map(|assets| assets.page.clone()),
        "platforms": platforms,
        "icon": icon,
    })
}

/// POST /api/plugins/install-from-release —— 从仓库下载并安装（商店页的「安装」）。
///
/// 请求体：`{"target": "x86_64-pc-windows-msvc", "id": "agent2api-providers"`}，
/// 两者都可省略：`target` 缺省用当前平台，`id` 缺省用官方包。
///
/// ── 为什么不接受前端传 URL ──────────────────────────────────
/// 与 `run_update` 同一理由：这一层装的是**可执行文件**，让前端指定下载地址
/// 等于把「装什么」的决定权交给一个可能被 XSS 影响的上下文。前端只能选
/// **平台三元组**，URL 由服务端从仓库查出来的清单里取 —— 查不到就拒绝。
///
/// ── `id` 只用来在索引里定位条目 ──────────────────────────────
/// 前端传的 id 不是下载地址，服务端拿它去 `index.json` 里找条目、读明细、
/// 再用明细里的 `release.repo` 查 Release。索引里没有这个 id 就拒绝 ——
/// 不能凭 id 拼出一个仓库名去下载。
///
/// ── 与 `run_update` 的差别 ──────────────────────────────────
/// 那个是「升级已装的包」（要求已安装、要求版本更新）；这个是「首次安装」，
/// 不要求任何已装包、不比较版本（重装同版本是合法的：换平台、修复装坏的包）。
pub async fn install_from_release(body: Bytes) -> Response {
    let parsed = parse_body(&body).ok();
    let wanted = parsed
        .as_ref()
        .and_then(|value| value.get("target").and_then(Value::as_str))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(plugin_host::host_target);
    let wanted_id = parsed
        .as_ref()
        .and_then(|value| value.get("id").and_then(Value::as_str))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    // ── 定位要装的是哪一个包 ──────────────────────────────────
    // 索引读得到 → 按 id 找条目；读不到或没给 id → 走官方包兜底。
    let (pack_id, repo) = match &wanted_id {
        Some(id) => {
            let entries = plugin_host::update::read_index().await;
            let Some(entry) = entries.iter().find(|entry| &entry.id == id) else {
                return management_error(404, format!("插件索引里没有这个包：{id}"));
            };
            let detail = plugin_host::update::read_detail(&entry.manifest).await;
            let repo = detail
                .as_ref()
                .map(|detail| detail.repo.clone())
                .filter(|repo| !repo.is_empty())
                .unwrap_or_else(|| entry.repo.clone());
            if repo.is_empty() {
                return management_error(500, format!("插件 {id} 没有声明发布仓库"));
            }
            (id.clone(), repo)
        }
        None => ("agent2api-providers".to_string(), plugin_host::update::FALLBACK_REPO.to_string()),
    };

    let release = match plugin_host::update::release_assets_in(&repo, &pack_id).await {
        Ok(Some(release)) => release,
        Ok(None) => return management_error(404, "插件仓库还没有发布任何版本"),
        Err(message) => return management_error(502, message),
    };
    let Some((_, url, _)) = release
        .platforms
        .iter()
        .find(|(target, _, _)| *target == wanted)
    else {
        return management_error(
            404,
            format!("这个版本没有为 {wanted} 提供安装包，请在商店页换一个平台"),
        );
    };
    let asset = plugin_host::update::PackAsset {
        version: release.version.clone(),
        name: plugin_host::update::asset_name(&pack_id, &release.version, &wanted),
        url: url.clone(),
        size: 0,
        page: release.page.clone(),
    };
    let bytes = match plugin_host::update::download(&asset).await {
        Ok(bytes) => bytes,
        Err(message) => return management_error(502, message),
    };
    // 与 install / run_update 同一顺序：先关宿主再落盘（Windows 上运行中的
    // exe 删不掉，而重装要先删旧版本目录）。
    let store = plugin_accounts::shared();
    if let Some(host) = plugin_host::current() {
        host.shutdown().await;
    }
    plugin_host::detach();
    plugin_host::clear_providers();
    let installed = match plugin_host::install_zip(&bytes) {
        Ok(pack) => pack,
        Err(message) => {
            if let Some(store) = store {
                let _ = plugin_host::reload_host(store).await;
            }
            return management_error(400, format!("安装失败：{message}"));
        }
    };
    let mut host_error = None;
    if let Some(store) = store {
        if plugin_host::reload_host(store).await.is_none() {
            host_error =
                Some("插件包已安装，但本次没能启动（详见日志）；重启应用后会自动加载".to_string());
        }
    }
    let mut out = snapshot();
    if let Some(object) = out.as_object_mut() {
        object.insert(
            "pack".to_string(),
            serde_json::to_value(installed).unwrap_or(Value::Null),
        );
        object.insert("hostError".to_string(), json!(host_error));
    }
    ok_json(out)
}

/// 从请求体里取出 zip 字节：`{"packB64": "…"}` 或裸字节体。
fn pack_bytes(body: &[u8]) -> Result<Vec<u8>, String> {
    if body.is_empty() {
        return Err("请求体是空的：请选择插件包 zip 后重试".to_string());
    }
    // zip 的魔数 `PK\x03\x04`（空包是 `PK\x05\x06`）：裸字节体走这条
    if body.starts_with(b"PK") {
        return Ok(body.to_vec());
    }
    let payload = match parse_body(body) {
        Ok(value) => value,
        Err(_) => {
            return Err("请求体既不是 zip 也不是 JSON：请选择插件包 zip 后重试".to_string())
        }
    };
    let encoded = payload
        .get("packB64")
        .or_else(|| payload.get("fileB64"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if encoded.trim().is_empty() {
        return Err("缺少 packB64（插件包 zip 的 base64 内容）".to_string());
    }
    // 去掉可能存在的 data URL 前缀（`data:application/zip;base64,`）
    let encoded = encoded
        .split_once("base64,")
        .map(|(_, rest)| rest)
        .unwrap_or(encoded);
    base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .map_err(|e| format!("插件包内容不是合法的 base64：{e}"))
}

/// 字节数 → 人类可读（错误文案用）。
fn human_size(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}
