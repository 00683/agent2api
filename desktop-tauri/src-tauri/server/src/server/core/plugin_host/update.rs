//! 插件包的**应用内更新**：读索引仓库的清单 → 去对应仓库的 GitHub Release
//! 查最新版 → 下载 → 安装。
//!
//! ── 为什么先读索引而不是直接查插件仓库 ──────────────────────
//! 插件不止一个。每个插件可能在自己的仓库发版，也可能和别的插件共用一个
//! 仓库（不同 tag 前缀）。索引仓库（[`INDEX_REPO`]）的 `index.json` 声明
//! 「有哪些插件、每个插件的包在哪」，网关按 id 找到条目，再按条目里的
//! `release.repo` 去查 Release。这样加一家插件不需要网关发版。
//!
//! ── 为什么单独一个模块（而不是塞进 `install.rs`）────────────────
//! `install.rs` 只做「拿到 zip 字节之后」的事（解包、验签、落盘），它对网络
//! 一无所知 —— 这条边界让「本地选文件安装」与「在线更新」共用同一段安装逻辑，
//! 也让安装可以在没有网络的机器上被测。
//!
//! ── 出网复用软件更新那条链 ────────────────────────────────────
//! `core::update::client::fetch_public` 已经处理了「直连优先、Clash 兜底」的
//! 候选出口与 GitHub 的请求头（含用户在「更新设置」里配的令牌，匿名限额
//! 60 次/小时 → 带令牌 5000）。插件更新打的是**同一个 GitHub**，没有理由
//! 另写一套出口逻辑 —— 用户在一个地方配好代理，两处都该生效。
//!
//! ── 资产名约定 ──────────────────────────────────────────────
//! `<id>-<version>-<target>.zip`（`scripts/pack.mjs`（Agent2Providers 仓库） 产出）。
//! 本模块按「包 id + 版本 + 当前平台」拼出期望的文件名去匹配 —— 不做模糊匹配，
//! 因为下错平台的包只会在 spawn 时失败，而错误信息指向「插件启动失败」。
//!
//! ── 为什么不做后台自动更新 ──────────────────────────────────
//! 插件是**能执行任意代码的二进制**。静默替换它等于让「某次自动更新」变成
//! 一次无人确认的代码执行 —— 而用户装的若是自建包，更新的还是别人维护的
//! 那一份。所以只有用户点「检查更新 / 升级」才动网络，装完也必须重新验签。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{json, Value};

use crate::server::core::update::client::{fetch_public, github_headers};
use crate::server::logging;

/// 插件索引仓库：`index.json` 在这里，它声明有哪些插件、各自的包在哪。
///
/// 网关的**唯一信任根**。想搭自己的插件源，照同一格式放一份 `index.json`
/// 即可。
pub const INDEX_REPO: &str = "Agent2plugin/Agent2plugin";

/// 索引文件在仓库里的路径。
const INDEX_PATH: &str = "index.json";

/// 兜底仓库：索引读不出来时，仍按老路子查这个仓库的最新 Release。
///
/// 索引仓库还没建、或者网络只放通了 raw.githubusercontent.com 却没放通
/// api.github.com 时，插件商店不该整个变成空白 —— 官方包的位置是已知的。
///
/// ── 为什么是构建仓库而不是主仓库 ────────────────────────────
/// 插件包的构建已搬到专用账号（`Agent2Builder/agent2api-providers-releases`），
/// 主仓库推 tag 只发一条极轻的派发请求、不再产出任何 Release。这个兜底若仍
/// 指向主仓库，索引一读不到就会「永远已是最新」—— 静默失效，比报错更难查。
///
/// 注意它只是**兜底**：正常情况下包的位置由索引 `index.json` 的
/// `source.repo` 决定（见 [`INDEX_REPO`]），换构建仓库改那里即可，不必发版。
pub const FALLBACK_REPO: &str = "Agent2Builder/agent2api-providers-releases";

/// GitHub API 根（与 `core::update::version::GITHUB_API` 同值）。
///
/// 复制一份而不是 import：那个常量属于「本应用的更新」这条链，插件更新是
/// 另一条 —— 将来若两者要指向不同的 API 端点（企业代理），各自改各自的。
const GITHUB_API: &str = "https://api.github.com";

/// 索引文件的原始内容地址（raw.githubusercontent.com，不吃 API 限额）。
fn index_url(repo: &str) -> String {
    format!("https://raw.githubusercontent.com/{repo}/main/{INDEX_PATH}")
}

/// 查询/下载的超时。查询是小 JSON；下载走 `None`（见 `fetch_public` 的说明，
/// 大文件不可能在固定秒数内下完，卡死由 egress 的 read_timeout 兜底）。
const QUERY_TIMEOUT_MS: u64 = 30_000;

/// 下载体积上限（与 `api::plugins::MAX_PACK_BYTES` 同一量级）。
///
/// 24MB：插件包正常 5–15MB，超过这个量级多半是仓库被换成了别的东西。
/// 上限放在这里而不是复用 api 层的常量，是因为**下载**这条路径不经过那层
/// 校验（它直接调 `install_zip`），少了这道闸就是一个「磁盘被写满」的入口。
const MAX_DOWNLOAD_BYTES: usize = 24 * 1024 * 1024;

/// 一个可用的插件包资产。
#[derive(Clone, Debug)]
pub struct PackAsset {
    /// 版本号（从 tag 或资产名解析，去掉前缀 `v`）。
    pub version: String,
    /// 资产文件名。
    pub name: String,
    /// 下载地址。
    pub url: String,
    /// 资产大小（字节；0 = 上游没给）。
    pub size: u64,
    /// Release 页面地址（界面上「查看」用）。
    pub page: String,
}

/// 当前平台的资产名。
///
/// 与 `scripts/pack.mjs`（Agent2Providers 仓库） 的产出名逐字对应 —— 两处任一处改了命名，
/// 更新就会变成「找不到对应平台的包」。测试里有一条断言钉住这个形状。
pub fn asset_name(pack_id: &str, version: &str, target: &str) -> String {
    format!("{pack_id}-{version}-{target}.zip")
}

/// 查最新一版插件包（`None` = 仓库里没有可用的 Release）。
///
/// ── 只认正式版 ──────────────────────────────────────────────
/// `/releases/latest` 由 GitHub 排除预发布。插件包的 beta 通道没做：插件的
/// 版本节奏跟着网关走，而网关本身已经有 beta 通道 —— 再叠一层只会让「我到底
/// 跑的是哪个组合」变得难以回答。
pub async fn latest(pack_id: &str, target: &str) -> Result<Option<PackAsset>, String> {
    latest_in(FALLBACK_REPO, pack_id, target).await
}

/// 在指定仓库里查最新一版插件包。
///
/// `repo` 是 `owner/repo`。索引读得到时由调用方传明细里的 `release.repo`，
/// 读不到时走 [`FALLBACK_REPO`]。
pub async fn latest_in(repo: &str, pack_id: &str, target: &str) -> Result<Option<PackAsset>, String> {
    let url = format!("{GITHUB_API}/repos/{repo}/releases/latest");
    let response = fetch_public(&url, &github_headers(), Some(QUERY_TIMEOUT_MS)).await?;
    let status = response.status();
    if status.as_u16() == 404 {
        // 仓库还没有 Release：不是错误，界面显示「暂无可更新的版本」
        return Ok(None);
    }
    if !status.is_success() {
        return Err(format!("查询插件更新失败：GitHub 返回 HTTP {}", status.as_u16()));
    }
    let release: Value = response
        .json()
        .await
        .map_err(|e| format!("插件 Release 响应不是合法 JSON：{e}"))?;
    Ok(pick_asset(&release, pack_id, target))
}

/// 从 Release 里挑出当前平台的那个 zip。
///
/// 版本从 **tag** 取（`v0.2.0` → `0.2.0`），与资产名里的版本比对：两者不一致
/// 说明发布流程出了问题（tag 与文件名没对齐），此时按 tag 为准但记一条日志 ——
/// 挡住它只会让用户看到「没有可更新的版本」而不知道为什么。
fn pick_asset(release: &Value, pack_id: &str, target: &str) -> Option<PackAsset> {
    let tag = release.get("tag_name").and_then(Value::as_str).unwrap_or("");
    let version = tag.trim().trim_start_matches('v').to_string();
    if version.is_empty() {
        logging::verbose("[Plugin]", "插件 Release 没有 tag_name，跳过");
        return None;
    }
    let page = release
        .get("html_url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let assets = release.get("assets").and_then(Value::as_array)?;
    let wanted = asset_name(pack_id, &version, target);
    // 先按精确名找；找不到再按「以包 id 开头 + 含当前平台 + .zip 结尾」兜底 ——
    // 发布脚本若把版本号写进名字的方式变了（比如带上 build 号），精确匹配会
    // 全部落空，而那时按形状匹配仍能挑到唯一那一个。
    let found = assets
        .iter()
        .find(|item| asset_name_of(item) == wanted)
        .or_else(|| {
            assets.iter().find(|item| {
                let name = asset_name_of(item);
                name.starts_with(&format!("{pack_id}-"))
                    && name.contains(target)
                    && name.ends_with(".zip")
            })
        })?;
    let name = asset_name_of(found);
    if name != wanted {
        logging::verbose(
            "[Plugin]",
            &format!("插件资产名与约定不一致（期望 {wanted}，实际 {name}），按形状匹配采用"),
        );
    }
    Some(PackAsset {
        version,
        name,
        url: found
            .get("browser_download_url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        size: found.get("size").and_then(Value::as_u64).unwrap_or(0),
        page,
    })
}

fn asset_name_of(item: &Value) -> String {
    item.get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// 下载一个插件包（含体积上限）。
///
/// 先看 `Content-Length`（有就直接拒绝，省得白下 20MB），没有则边下边数 ——
/// 上游不给长度是常态，只靠前者等于没有上限。
pub async fn download(asset: &PackAsset) -> Result<Vec<u8>, String> {
    if asset.url.is_empty() {
        return Err("这个 Release 里的资产没有下载地址".to_string());
    }
    let response = fetch_public(&asset.url, &github_headers(), None).await?;
    if !response.status().is_success() {
        return Err(format!(
            "下载插件包失败：HTTP {}",
            response.status().as_u16()
        ));
    }
    if let Some(length) = response.content_length() {
        if length as usize > MAX_DOWNLOAD_BYTES {
            return Err(format!(
                "插件包太大（{:.1} MB），已中止下载",
                length as f64 / 1024.0 / 1024.0
            ));
        }
    }
    use futures::StreamExt;
    let mut stream = response.bytes_stream();
    let mut buffer: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("下载中断：{e}"))?;
        if buffer.len() + chunk.len() > MAX_DOWNLOAD_BYTES {
            return Err(format!(
                "插件包超过 {:.0} MB 上限，已中止下载",
                MAX_DOWNLOAD_BYTES as f64 / 1024.0 / 1024.0
            ));
        }
        buffer.extend_from_slice(&chunk);
    }
    if buffer.is_empty() {
        return Err("下载到的插件包是空的".to_string());
    }
    logging::log(
        "[Plugin]",
        &format!(
            "插件包已下载：{}（{:.1} MB）",
            asset.name,
            buffer.len() as f64 / 1024.0 / 1024.0
        ),
    );
    Ok(buffer)
}

/// 索引里的一个插件条目（`index.json` 的 `plugins[]`）。
#[derive(Clone, Debug)]
pub struct IndexEntry {
    /// 包 id（也是装好后的目录名）。
    pub id: String,
    /// 显示名。
    pub name: String,
    /// 一句话说明。
    pub description: String,
    /// 是否官方维护。
    pub official: bool,
    /// 明细文件路径（相对仓库根，形如 `plugins/<id>.json`）。
    pub manifest: String,
    /// 商店卡片图标路径（相对索引仓库根，形如 `icons/<id>.png`）；空 = 没有。
    pub icon: String,
    /// 包所在仓库（`owner/repo`），明细读不到时用它兜底。
    pub repo: String,
}

/// 一个插件的明细（`plugins/<id>.json`）。
#[derive(Clone, Debug)]
pub struct PluginDetail {
    /// 版本号。
    pub version: String,
    /// 插件协议版本，必须等于网关的 `PROTOCOL_VERSION`。
    pub protocol: u32,
    /// 包所在仓库。
    pub repo: String,
    /// Release tag 模板（`v{version}` 或 `demo-v{version}`）。
    pub tag_template: String,
    /// 资产名模板（`{id}-{version}-{target}.zip`）。
    pub asset_template: String,
    /// 主页。
    pub homepage: String,
    /// 这个包提供的 provider（`id` 与 `label`）。
    pub providers: Vec<(String, String)>,
}

/// 读索引仓库的 `index.json`，返回全部条目。
///
/// ── 读不到时返回空表而不是错误 ──────────────────────────────
/// 调用方（商店页）据此回落到 [`FALLBACK_REPO`] 的单包视图。把「索引不存在」
/// 当成硬错误会让一个刚搭起来、还没写 `index.json` 的索引仓库把整个插件
/// 商店变成报错页 —— 而老路子仍然能用。
pub async fn read_index() -> Vec<IndexEntry> {
    let url = index_url(INDEX_REPO);
    let response = match fetch_public(&url, &github_headers(), Some(QUERY_TIMEOUT_MS)).await {
        Ok(response) => response,
        Err(e) => {
            logging::verbose("[Plugin]", &format!("读插件索引失败（{url}）：{e}"));
            return Vec::new();
        }
    };
    if !response.status().is_success() {
        logging::verbose(
            "[Plugin]",
            &format!("读插件索引失败（{url}）：HTTP {}", response.status().as_u16()),
        );
        return Vec::new();
    }
    let text = match response.text().await {
        Ok(text) => text,
        Err(e) => {
            logging::verbose("[Plugin]", &format!("插件索引正文读不出来：{e}"));
            return Vec::new();
        }
    };
    parse_index(&text)
}

/// 解析 `index.json` 的正文。
///
/// 拆成独立函数是为了能在没有网络的地方测 —— 这份 JSON 是网关与索引仓库
/// 之间唯一的契约，解析规则出错的表现是「插件列表空了」，现场很难反推。
fn parse_index(text: &str) -> Vec<IndexEntry> {
    let Ok(document) = serde_json::from_str::<Value>(text) else {
        logging::verbose("[Plugin]", "插件索引不是合法 JSON");
        return Vec::new();
    };
    let Some(items) = document.get("plugins").and_then(Value::as_array) else {
        logging::verbose("[Plugin]", "插件索引缺少 plugins 数组");
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let id = item.get("id").and_then(Value::as_str)?.trim().to_string();
            if id.is_empty() {
                return None;
            }
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(&id)
                .to_string();
            let description = item
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let official = item.get("official").and_then(Value::as_bool).unwrap_or(false);
            let manifest = item
                .get("manifest")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let icon = item
                .get("icon")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let repo = item
                .get("source")
                .and_then(|source| source.get("repo"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Some(IndexEntry { id, name, description, official, manifest, icon, repo })
        })
        .collect()
}

/// 读一个插件的明细文件。
///
/// ── 明细始终在索引仓库里 ────────────────────────────────────
/// `manifest` 是相对**索引仓库**根的路径（`plugins/<id>.json`），不是相对
/// 插件自己的仓库 —— 索引仓库收的是「描述」，插件仓库收的是「产物」。这两
/// 件事分开才能让第三方只往索引里提一个 JSON 条目，不必把自己的仓库也
/// 改成某种布局。
///
/// 路径里的 `..` 一律拒绝 —— 这份 JSON 来自远端，不该有让网关去读仓库之外
/// 内容的能力。
pub async fn read_detail(manifest: &str) -> Option<PluginDetail> {
    if manifest.is_empty()
        || manifest.starts_with('/')
        || manifest.split('/').any(|part| part == "..")
    {
        return None;
    }
    let url = format!("https://raw.githubusercontent.com/{INDEX_REPO}/main/{manifest}");
    let response = fetch_public(&url, &github_headers(), Some(QUERY_TIMEOUT_MS))
        .await
        .ok()?;
    if !response.status().is_success() {
        logging::verbose(
            "[Plugin]",
            &format!("读插件明细失败（{url}）：HTTP {}", response.status().as_u16()),
        );
        return None;
    }
    let text = response.text().await.ok()?;
    parse_detail(&text)
}

/// 图标体积上限（2MB，与 `install.rs` 的 `ICON_MAX_BYTES` 同一量级）。
///
/// 商店列表一次要取全部插件的图标，几 MB 的图会让面板明显变慢。超过上限
/// 就不显示图标（回落字母徽章），而不是把列表拖慢。
const STORE_ICON_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// 从索引仓库读一个插件的商店图标，转成 data URI。
///
/// ── 为什么内联成 data URI ──────────────────────────────────
/// 界面拿到的 `icon` 字段直接塞进 `<img src>`。给 URL 的话要处理跨域、
/// 重定向与加载失败三件事；内联一次就把它们全省了，代价是列表响应变大
/// 几十 KB（图标本身是 256×256 的 PNG）。
///
/// 任何一步失败都返回 `None`（界面回落字母徽章），不报错 —— 图标是装饰。
pub async fn read_store_icon(path: &str) -> Option<String> {
    if path.is_empty()
        || path.starts_with('/')
        || path.split('/').any(|part| part == "..")
    {
        return None;
    }
    let url = format!("https://raw.githubusercontent.com/{INDEX_REPO}/main/{path}");
    let response = fetch_public(&url, &github_headers(), Some(QUERY_TIMEOUT_MS))
        .await
        .ok()?;
    if !response.status().is_success() {
        logging::verbose(
            "[Plugin]",
            &format!("读插件图标失败（{url}）：HTTP {}", response.status().as_u16()),
        );
        return None;
    }
    let mime = match path.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "webp" => "image/webp",
        "jpg" | "jpeg" => "image/jpeg",
        "svg" => "image/svg+xml",
        _ => return None,
    };
    let bytes = response.bytes().await.ok()?;
    if bytes.len() as u64 > STORE_ICON_MAX_BYTES {
        logging::verbose(
            "[Plugin]",
            &format!("插件图标超过上限（{} 字节），跳过：{path}", bytes.len()),
        );
        return None;
    }
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Some(format!("data:{mime};base64,{encoded}"))
}

/// 解析 `plugins/<id>.json` 的正文。
fn parse_detail(text: &str) -> Option<PluginDetail> {
    let document = serde_json::from_str::<Value>(text).ok()?;
    let version = document.get("version").and_then(Value::as_str)?.to_string();
    let protocol = document.get("protocol").and_then(Value::as_u64).unwrap_or(0) as u32;
    let release = document.get("release");
    let repo = release
        .and_then(|item| item.get("repo"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let tag_template = release
        .and_then(|item| item.get("tag"))
        .and_then(Value::as_str)
        .unwrap_or("v{version}")
        .to_string();
    let asset_template = release
        .and_then(|item| item.get("asset"))
        .and_then(Value::as_str)
        .unwrap_or("{id}-{version}-{target}.zip")
        .to_string();
    let homepage = document
        .get("homepage")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let providers = document
        .get("providers")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let id = item.get("id").and_then(Value::as_str)?.to_string();
                    let label = item
                        .get("label")
                        .and_then(Value::as_str)
                        .unwrap_or(&id)
                        .to_string();
                    Some((id, label))
                })
                .collect()
        })
        .unwrap_or_default();
    Some(PluginDetail { version, protocol, repo, tag_template, asset_template, homepage, providers })
}

/// 把模板里的占位符替换掉（`{id}` / `{version}` / `{target}`）。
/// 一份 Release 里**所有平台**的资产（商店页展示用）。
#[derive(Clone, Debug)]
pub struct ReleaseAssets {
    /// 版本号（从 tag 解析，去掉前缀 `v`）。
    pub version: String,
    /// Release 页面地址。
    pub page: String,
    /// 资产文件名。
    pub name: String,
    /// 各平台：`(三元组, 下载地址, 字节数)`，按三元组字典序。
    pub platforms: Vec<(String, String, u64)>,
}

/// 包 id 对应的资产名前缀（`<id>-<version>-`）。
///
/// 从资产名里剥出平台段时要用它：资产名是 `<id>-<version>-<target>.zip`，
/// 而 id 与 version 里都可能含连字符，不能用「按 `-` 切分」反推。
fn asset_prefix(pack_id: &str, version: &str) -> String {
    format!("{pack_id}-{version}-")
}

/// 查最新一版的**全部平台**资产。
///
/// 与 [`latest`] 的区别：那个只挑当前平台一个，这个把整张清单给界面 ——
/// 商店页要让用户看到「有哪几个平台、各自多大」，也允许在弹窗里手动改选
/// （同一个人可能给另一台机器准备包）。
pub async fn release_assets(pack_id: &str) -> Result<Option<ReleaseAssets>, String> {
    release_assets_in(FALLBACK_REPO, pack_id).await
}

/// 在指定仓库里查最新一版的全部平台资产。
pub async fn release_assets_in(repo: &str, pack_id: &str) -> Result<Option<ReleaseAssets>, String> {
    let url = format!("{GITHUB_API}/repos/{repo}/releases/latest");
    let response = fetch_public(&url, &github_headers(), Some(QUERY_TIMEOUT_MS)).await?;
    let status = response.status();
    if status.as_u16() == 404 {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(format!("查询插件版本失败：GitHub 返回 HTTP {}", status.as_u16()));
    }
    let release: Value = response
        .json()
        .await
        .map_err(|e| format!("插件 Release 响应不是合法 JSON：{e}"))?;
    let version = release
        .get("tag_name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .trim_start_matches('v')
        .to_string();
    if version.is_empty() {
        return Ok(None);
    }
    let page = release
        .get("html_url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let prefix = asset_prefix(pack_id, &version);
    let mut platforms: Vec<(String, String, u64)> = release
        .get("assets")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let name = item.get("name").and_then(Value::as_str)?;
                    // 只认 `<id>-<version>-<target>.zip`：别的附件（校验和、
                    // 说明文件）不该出现在平台清单里
                    let target = name
                        .strip_prefix(&prefix)?
                        .strip_suffix(".zip")?;
                    if target.is_empty() || target.contains('/') {
                        return None;
                    }
                    let url = item
                        .get("browser_download_url")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let size = item.get("size").and_then(Value::as_u64).unwrap_or(0);
                    Some((target.to_string(), url, size))
                })
                .collect()
        })
        .unwrap_or_default();
    platforms.sort_by(|a, b| a.0.cmp(&b.0));
    if platforms.is_empty() {
        return Ok(None);
    }
    Ok(Some(ReleaseAssets {
        version,
        page,
        name: pack_id.to_string(),
        platforms,
    }))
}

/// 检查是否有比 `current` 更新的版本。
///
/// 返回的 JSON 直接进 HTTP 响应（形状是前端契约）：
/// ```json
/// { "available": true, "current": "0.1.0", "latest": "0.2.0",
///   "asset": { "name": "...", "size": 123, "url": "...", "page": "..." },
///   "reason": null, "error": false }
/// ```
/// 三档语义（`error` 区分「查不到」与「没有更新」）：
///   · `available=true` —— 有新版本；
///   · `available=false, error=false` —— 已是最新 / 仓库还没发版（中性）；
///   · `available=false, error=true` —— 查询本身失败（网络 / 限额），界面报红。
pub async fn check(pack_id: &str, current: &str, target: &str) -> Value {
    match latest(pack_id, target).await {
        Ok(None) => json!({
            "available": false,
            "current": current,
            "latest": Value::Null,
            "asset": Value::Null,
            "reason": "插件仓库还没有发布任何版本",
            "error": false,
        }),
        Ok(Some(asset)) => {
            // 版本比较复用软件更新那份实现：语义化版本 + 预发布段的规则只有一处
            let newer = crate::server::core::update::compare_versions(&asset.version, current)
                .map(|order| order > 0)
                .unwrap_or(false);
            json!({
                "available": newer,
                "current": current,
                "latest": asset.version,
                "asset": {
                    "name": asset.name,
                    "size": asset.size,
                    "url": asset.url,
                    "page": asset.page,
                },
                "reason": if newer { Value::Null } else { json!("已经是最新版本") },
                "error": false,
            })
        }
        Err(message) => json!({
            "available": false,
            "current": current,
            "latest": Value::Null,
            "asset": Value::Null,
            "reason": message,
            "error": true,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 资产名必须与打包脚本的产出名逐字一致（两处任一处改了命名，
    /// 更新就会变成「找不到对应平台的包」）。
    #[test]
    fn asset_name_matches_pack_script() {
        assert_eq!(
            asset_name("agent2api-providers", "0.1.0", "x86_64-pc-windows-msvc"),
            "agent2api-providers-0.1.0-x86_64-pc-windows-msvc.zip"
        );
    }

    #[test]
    fn picks_asset_by_exact_name() {
        let release = json!({
            "tag_name": "v0.2.0",
            "html_url": "https://github.com/x/y/releases/tag/v0.2.0",
            "assets": [
                { "name": "agent2api-providers-0.2.0-x86_64-pc-windows-msvc.zip",
                  "browser_download_url": "https://example.com/a.zip", "size": 100 },
                { "name": "agent2api-providers-0.2.0-aarch64-apple-darwin.zip",
                  "browser_download_url": "https://example.com/b.zip", "size": 200 }
            ]
        });
        let asset = pick_asset(&release, "agent2api-providers", "x86_64-pc-windows-msvc")
            .expect("应当挑到 Windows 包");
        assert_eq!(asset.version, "0.2.0");
        assert_eq!(asset.size, 100);
        assert_eq!(asset.url, "https://example.com/a.zip");
    }

    /// 找不到当前平台时返回 None（而不是错挑一个别的平台）。
    #[test]
    fn skips_other_platforms() {
        let release = json!({
            "tag_name": "v0.2.0",
            "assets": [
                { "name": "agent2api-providers-0.2.0-aarch64-apple-darwin.zip",
                  "browser_download_url": "https://example.com/b.zip" }
            ]
        });
        assert!(pick_asset(&release, "agent2api-providers", "x86_64-pc-windows-msvc").is_none());
    }

    /// tag 带 `v` 前缀也要认。
    #[test]
    fn strips_v_prefix_from_tag() {
        let release = json!({
            "tag_name": "v1.2.3",
            "assets": [
                { "name": "agent2api-providers-1.2.3-x86_64-unknown-linux-gnu.zip",
                  "browser_download_url": "https://example.com/a.zip" }
            ]
        });
        let asset = pick_asset(&release, "agent2api-providers", "x86_64-unknown-linux-gnu")
            .expect("应当挑到 Linux 包");
        assert_eq!(asset.version, "1.2.3");
    }
}
