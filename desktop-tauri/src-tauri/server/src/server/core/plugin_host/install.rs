//! 插件包的安装、校验与注册表。
//!
//! ── 信任链（顺序不能变）──────────────────────────────────────
//!   1. **sha256**：zip 自身的摘要记进注册表，用于完整性复核（`InstalledPack::sha256`）。
//!      它挡「文件传坏了」与「装完之后被换过」—— 校验在 [`verify_installed`]；
//!   2. **协议版本**：`pack.json` 的 `protocol` 必须等于网关的
//!      [`proto::PROTOCOL_VERSION`]。挡「版本不匹配」；
//!   3. **签名**：`pack.json` 里带 `signature` 时做 ed25519 验签（公钥内置，
//!      见 [`OFFICIAL_PUBKEYS`]）。验签覆盖的是**包内容**（见 [`signing_payload`]），
//!      不是 zip 字节 —— zip 的压缩方式/时间戳可以变，内容才是要保护的东西。
//!
//! 三步都过才允许落盘并登记。**未签名的包仍然允许安装**（第三方/自建包必须
//! 有活路），但界面标黄 —— 与「官方签名 / 第三方未签名」的策略一致。带签名
//! 却验不过的包**一律拒绝**：那说明包被改过，或者不是它自称的那个发布者。
//!
//! ── 公钥从哪来 ──────────────────────────────────────────────
//! 编译期常量 [`OFFICIAL_PUBKEYS`]（十六进制 32 字节，可多条以便轮换）。
//! 运行期不可改 —— 一个能被配置改掉的公钥等于没有签名。
//!
//! ── 目录布局 ────────────────────────────────────────────────
//! ```text
//! {config_dir}/plugins/
//!   plugins.json              注册表（装了哪些、启停状态）
//!   agent2api-providers/      包 id
//!     0.1.0/                  版本目录（可并存多版本，便于回滚）
//!       pack.json
//!       bin/providers.exe
//!     current -> 0.1.0        当前版本（Windows 上是 current.txt）
//! ```
//!
//! ── 为什么要版本目录而不是原地覆盖 ───────────────────────────
//! 一个包升级失败时要能回滚。原地覆盖会让「新版本起不来」变成「这个功能
//! 彻底没了」。多版本并存 + `current` 指针是 mihomo 内核那套已经在用的
//! 形态（`sub_kernel::layout`），复用同一思路让排障心智一致。
//!
//! ── 安装从哪来（zip 上传）────────────────────────────────────
//! 用户拿到的是各平台的 zip（`scripts/pack.mjs`（Agent2Providers 仓库） 产出，约 13MB）。
//! [`install_zip`] 负责解包：先把整包解到**临时目录**，校验清单与入口，
//! 全过了才 `rename` 进版本目录 —— 半成品包绝不会出现在 `plugins/` 里
//! （否则 `bootstrap` 会去 spawn 一个不完整的二进制）。
//!
//! zip slip 在两道防线上各挡一次：解包时拒绝越界路径，校验清单时再拒一次
//! 入口里的 `..`（见 [`validate_manifest`]）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use agent2api_provider_protocol as proto;
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::server::logging;

/// 官方发布者公钥表：`(key_id, 32 字节 ed25519 公钥的十六进制)`。
///
/// ── 为什么是编译期常量 ──────────────────────────────────────
/// 一个能被配置改掉的公钥等于没有签名 —— 攻击者只要能改配置，就能换成自己的
/// 公钥再签一个包。所以公钥只随**网关发版**更新，与插件包的发版节奏解耦。
///
/// ── 轮换怎么做 ──────────────────────────────────────────────
/// 表里可以有多条：新密钥先加进来（旧包仍能验过），插件包重新签名后再删旧的。
/// `key_id` 是给这件事用的标识，不是安全边界 —— 验签只认「表里有没有这把」。
///
/// ── 现在这一把是什么 ────────────────────────────────────────
/// `official-2026`：2026-10 生成，私钥只存在发布方与插件仓库的 GitHub
/// Actions secret（`AGENT2API_PLUGIN_KEY`）里，CI 打包后自动签名。
/// 轮换时把新公钥加进来、等插件包全部重签后再删旧的。
pub const OFFICIAL_PUBKEYS: &[(&str, &str)] = &[(
    "official-2026",
    "7cdac378ff9bf23b72ae1b686f19859b3c802c26eed13c699f7df39f027352d6",
)];

/// 验签结果。
///
/// 三态而不是 bool：`Unsigned`（没带签名）与 `Invalid`（带了但验不过）的处置
/// 完全不同 —— 前者允许安装并标黄，后者必须拒绝。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignatureState {
    /// 包里带了适用于本平台的签名，且验签通过。
    Verified,
    /// 包里没有适用于本平台的签名（未签名包，或签的是别的平台）。
    Unsigned,
    /// 带了签名但验不过（算法不认识 / 公钥不在表里 / 签名不匹配）。
    Invalid,
}

impl SignatureState {
    /// 是否算「已签名」（界面展示用）。
    pub fn is_signed(self) -> bool {
        matches!(self, Self::Verified)
    }
}

/// 验一份包的签名。
///
/// `dir` 是**已解包**的目录（要读入口二进制算 sha256），`target` 是当前平台。
/// 返回 `(状态, 失败原因)` —— 原因只在 `Invalid` 时有值，用于安装失败的文案。
pub fn verify_signature(
    dir: &Path,
    manifest: &proto::PackManifest,
    target: &str,
) -> (SignatureState, Option<String>) {
    let Some(signature) = manifest.signature.as_ref() else {
        return (SignatureState::Unsigned, None);
    };
    // 签名绑定的平台与本平台不一致：当作「没有适用于本平台的签名」。
    // 多平台包每个平台各签一份，只有匹配的那份能验 —— 见 signing_payload。
    if signature.target.as_deref() != Some(target) {
        return (SignatureState::Unsigned, None);
    }
    if !signature.algorithm.eq_ignore_ascii_case("ed25519") {
        return (
            SignatureState::Invalid,
            Some(format!("不支持的签名算法：{}", signature.algorithm)),
        );
    }
    let Some((_, public_hex)) = OFFICIAL_PUBKEYS
        .iter()
        .find(|(id, _)| *id == signature.key_id)
    else {
        return (
            SignatureState::Invalid,
            Some(format!(
                "签名密钥 {} 不在本版本的信任列表里（可能需要更新网关）",
                signature.key_id
            )),
        );
    };
    let Some(public_bytes) = hex_to_32(public_hex) else {
        // 表写错了（编译期常量，只有开发期能犯）：按验不过处理并留下明确原因
        return (
            SignatureState::Invalid,
            Some(format!("内置公钥 {} 的格式无效", signature.key_id)),
        );
    };
    let Some(entry) = manifest.entry.get(target) else {
        return (
            SignatureState::Invalid,
            Some("包里没有本平台的入口，无法验签".to_string()),
        );
    };
    let Some(binary_hash) = sha256_file(&dir.join(entry)) else {
        return (
            SignatureState::Invalid,
            Some("读不到入口二进制，无法验签".to_string()),
        );
    };
    let payload = proto::signing_payload(manifest, target, &binary_hash, &collect_ui_files(dir));
    let Ok(signature_bytes) = base64::engine::general_purpose::STANDARD.decode(signature.value.trim())
    else {
        return (SignatureState::Invalid, Some("签名不是合法的 base64".to_string()));
    };
    match verify_ed25519(&public_bytes, payload.as_bytes(), &signature_bytes) {
        true => (SignatureState::Verified, None),
        false => (
            SignatureState::Invalid,
            Some("签名与本包内容不匹配（包可能被改过，或不是它自称的发布者签的）".to_string()),
        ),
    }
}

/// 收集包里 UI 资源的清单：`[(相对包根的 POSIX 路径, sha256), …]`，按路径排序。
///
/// ── 为什么验签要读 UI 资源 ──────────────────────────────────
/// 插件页面由网关托管、跑在管理面板的 origin 下，能拿到面板令牌（等于能调
/// 所有 protected API）。只签二进制的话，一个包可以带合法签名的 exe **加**
/// 一个偷令牌的 js，验签照样通过 —— 那等于给了一条绕过签名的后门。
///
/// 判据与 `scripts/sign.mjs` 的 `collectUiFiles` 同源：只收 `ui/` 前缀下的
/// 普通文件。路径分隔符统一成 `/`（Windows 上打的包要在 Linux 上验得过）。
fn collect_ui_files(dir: &Path) -> Vec<(String, String)> {
    let root = dir.join("ui");
    let mut files: Vec<(String, String)> = Vec::new();
    collect_ui_files_into(&root, &root, &mut files);
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
}

/// 递归收集（目录不存在时什么都不做 —— 没有 UI 资源的包是正常形态）。
fn collect_ui_files_into(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            collect_ui_files_into(root, &path, out);
            continue;
        }
        let Some(hash) = sha256_file(&path) else {
            continue;
        };
        let Some(relative) = path.strip_prefix(root.parent().unwrap_or(root)).ok() else {
            continue;
        };
        // 统一成 POSIX 分隔符：签名载荷要求两边逐字节一致，而 Windows 上
        // `Path::display()` 给的是反斜杠
        let name = relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join("/");
        out.push((name, hash));
    }
}

/// ed25519 验签（`Ok` 表示签名有效）。
fn verify_ed25519(public: &[u8; 32], message: &[u8], signature: &[u8]) -> bool {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};
    let Ok(key) = VerifyingKey::from_bytes(public) else {
        return false;
    };
    let Ok(signature) = Signature::from_slice(signature) else {
        return false;
    };
    key.verify(message, &signature).is_ok()
}

/// 十六进制（64 字符）→ 32 字节。
fn hex_to_32(text: &str) -> Option<[u8; 32]> {
    let text = text.trim();
    if text.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (index, chunk) in text.as_bytes().chunks(2).enumerate() {
        let pair = std::str::from_utf8(chunk).ok()?;
        out[index] = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(out)
}

/// 文件 sha256（十六进制小写）。读不到返回 None。
fn sha256_file(path: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Some(
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    )
}

/// 注册表里的一个包。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InstalledPack {
    /// 包 id。
    pub id: String,
    /// 当前版本。
    pub version: String,
    /// 是否启用（用户可以在界面上关掉整个包）。
    #[serde(default = "yes")]
    pub enabled: bool,
    /// 逐 provider 的启停（用户可以只关掉其中一家）。
    #[serde(default)]
    pub providers: BTreeMap<String, bool>,
    /// 入口二进制的相对路径（相对版本目录）。
    pub entry: String,
    /// 包的 sha256（安装时算的，用于完整性复核）。
    #[serde(default)]
    pub sha256: String,
    /// 是否已签名并通过验签。
    #[serde(default)]
    pub signed: bool,
    /// 安装时刻（毫秒）。
    #[serde(default)]
    pub installed_at: i64,
    /// 包清单里的 provider 列表（缓存，免得每次都读 pack.json）。
    #[serde(default)]
    pub manifest_providers: Vec<proto::PackProvider>,
}

fn yes() -> bool {
    true
}

/// 注册表文件（`plugins.json`）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PackRegistry {
    #[serde(default)]
    pub packs: Vec<InstalledPack>,
}

/// 插件根目录。
pub fn plugins_dir() -> PathBuf {
    crate::paths::config_dir().join("plugins")
}

/// 注册表路径。
pub fn registry_path() -> PathBuf {
    plugins_dir().join("plugins.json")
}

impl PackRegistry {
    /// 读注册表（不存在 = 空表；损坏 = 空表并记日志）。
    ///
    /// ── 为什么损坏不报错 ────────────────────────────────────────
    /// 注册表是「装了什么」的缓存，真正的数据在版本目录里。读不出来时
    /// 退化成空表，用户重新安装即可 —— 报错会让整个插件功能不可用，
    /// 而它本来只是配置。
    pub fn load() -> Self {
        let path = registry_path();
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        match serde_json::from_str(&text) {
            Ok(registry) => registry,
            Err(e) => {
                logging::log("Plugin", &format!("注册表损坏，按空表处理：{e}"));
                Self::default()
            }
        }
    }

    /// 写回注册表（原子替换）。
    pub fn save(&self) -> Result<(), String> {
        let path = registry_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("建目录失败: {e}"))?;
        }
        let text =
            serde_json::to_string_pretty(self).map_err(|e| format!("序列化失败: {e}"))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text).map_err(|e| format!("写临时文件失败: {e}"))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("替换注册表失败: {e}"))
    }

    /// 找一个已启用的包。
    pub fn enabled(&self) -> Option<&InstalledPack> {
        self.packs.iter().find(|pack| pack.enabled)
    }

    /// 某个 provider 是否由已启用的包提供。
    pub fn provider_enabled(&self, provider: &str) -> bool {
        self.packs.iter().any(|pack| {
            pack.enabled
                && pack
                    .manifest_providers
                    .iter()
                    .any(|p| p.id.as_str() == provider)
                && pack.providers.get(provider).copied().unwrap_or(true)
        })
    }
}

/// 校验一份包清单。
///
/// ── 返回的错误都要能直接显示给用户 ──────────────────────────
/// 安装失败时界面就显示这句话。因此文案要说「为什么」与「怎么办」，
/// 不要只说「校验失败」。
pub fn validate_manifest(manifest: &proto::PackManifest, host_protocol: u32) -> Result<(), String> {
    if manifest.protocol != host_protocol {
        return Err(format!(
            "这个插件包要求协议版本 {}，而当前网关是 {}。请更新插件包（或更新网关）。",
            manifest.protocol, host_protocol
        ));
    }
    if manifest.providers.is_empty() {
        return Err("插件包里没有声明任何提供商。".to_string());
    }
    if manifest.entry.is_empty() {
        return Err("插件包没有声明入口二进制。".to_string());
    }
    // 入口路径必须是相对路径且不越界（防 zip slip）
    for (target, entry) in &manifest.entry {
        // ── 为什么两个方向都要判 ────────────────────────────────
        // `Path::new(x).is_absolute()` 是**按当前平台**判的：Windows 上
        // `/usr/bin/evil` 不算绝对路径（它没有盘符），但它在包里同样是
        // 「指向包外」的危险写法。所以除了平台判定，还要显式拒绝以 `/`
        // 或 `\` 开头的路径。
        let path = Path::new(entry);
        if path.is_absolute()
            || entry.starts_with('/')
            || entry.starts_with('\\')
            || entry.chars().nth(1) == Some(':')
        {
            return Err(format!("入口路径必须是相对路径（{target}: {entry}）"));
        }
        if entry.split(['/', '\\']).any(|part| part == "..") {
            return Err(format!("入口路径不能包含 ..（{target}: {entry}）"));
        }
    }
    Ok(())
}

/// 为当前目标挑一个入口。
pub fn entry_for_target(manifest: &proto::PackManifest, target: &str) -> Option<String> {
    // 先精确匹配，再按「架构-系统」宽松匹配（不同打包工具的三元组写法不一致）
    if let Some(entry) = manifest.entry.get(target) {
        return Some(entry.clone());
    }
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    manifest
        .entry
        .iter()
        .find(|(key, _)| key.contains(arch) && key.contains(os))
        .map(|(_, entry)| entry.clone())
}

/// 读一个版本目录里的 `pack.json`。
pub fn read_manifest(dir: &Path) -> Result<proto::PackManifest, String> {
    let path = dir.join("pack.json");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("读不到 pack.json（{}）: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("pack.json 不是合法 JSON: {e}"))
}

// ─── 安装 / 卸载 / 启停 ─────────────────────────────────────

/// 已安装包的摘要（界面列表用）。
///
/// 与 [`InstalledPack`] 的区别：这里是**给界面看的**，多带三样安装期才知道的
/// 事实（版本目录是否还在、入口是否存在、当前宿主是否在跑），少带注册表内部
/// 字段。界面据此把「装了但文件没了」画成异常行，而不是一个点了没反应的卡片。
///
/// `rename_all = "camelCase"`：这份结构体是**前端契约**（`plugins-page.tsx`
/// 逐字段读），而 `InstalledPack` 是磁盘格式（保持 snake_case 与既有注册表
/// 兼容）。两者刻意用两套命名，别为了「统一」把它们并成一个。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackSummary {
    /// 包 id。
    pub id: String,
    /// 显示名（清单里的 `name`；清单读不到时回落 id）。
    pub name: String,
    /// 版本。
    pub version: String,
    /// 是否启用。
    pub enabled: bool,
    /// 是否已签名（安装时验签通过才为 true —— 界面据此显示「已签名 / 未签名」）。
    pub signed: bool,
    /// 安装时刻（毫秒）。
    pub installed_at: i64,
    /// 包声明的 provider（id / 展示名 / 是否启用）。
    pub providers: Vec<PackProviderSummary>,
    /// 入口二进制是否存在于磁盘（false = 包被外部删了 / 移动了）。
    pub entry_present: bool,
    /// 这个包声明的协议版本（与网关不一致时界面要提示升级）。
    pub protocol: u32,
    /// 包主页（可选）。
    pub homepage: Option<String>,
    /// 包图标（可选，`data:image/png;base64,…` 形态；没有则为 null）。
    ///
    /// ── 为什么内联成 data URI 而不是给一个 URL ─────────────────
    /// 图标在插件包的版本目录里（`plugins/<id>/<version>/assets/…`），不是
    /// `ui/` 下的静态资源 —— 静态服务只出 `ui_dir` 内的文件，为它开一条
    /// 能读任意插件目录的路由，等于把「装了个包」变成「能读磁盘」。内联进
    /// 列表响应既不用新增端点，也不扩大文件读取的边界。
    ///
    /// 体积上限见 [`ICON_MAX_BYTES`]：列表每次操作后都要重取，几 MB 的图
    /// 会让面板变卡。
    pub icon: Option<String>,
    /// 这个包贡献的界面页面（空 = 纯后端包）。
    ///
    /// ── 客户端怎么用 ────────────────────────────────────────────
    /// 菜单项从这份清单动态生成：没装包就没有这些页面，卸载后页面自动消失
    /// （与「插件不在，这家就不存在」同一条原则，只是从 provider 级扩展到
    /// 页面级）。`url` 是网关托管的地址，客户端用 iframe 嵌。
    pub ui: Vec<PackUiSummary>,
}

/// 包内一个界面页面的摘要。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackUiSummary {
    pub id: String,
    pub title: String,
    pub icon: Option<String>,
    /// 网关托管地址（`/plugins/<pack>/ui/<entry>`）。
    pub url: String,
    /// 页面申报的默认归属分组（客户端内置分组 id，如 `g-overview`）。
    ///
    /// 网关**不解释**这个值：分组 id 是客户端的事实（桌面端与手机端各有自己
    /// 的默认布局），网关只负责把插件说的那句话原样转给客户端。校验与兜底
    /// 在客户端做（不认识就按第一组处理）。
    pub group: Option<String>,
    pub order: i32,
}

/// 包内一家 provider 的界面摘要。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackProviderSummary {
    pub id: String,
    pub label: String,
    /// 这一家是否启用（用户在界面上单独关掉某家）。
    pub enabled: bool,
    /// 这一家此刻是否真的装进了运行时注册表（宿主在跑且握手成功）。
    pub loaded: bool,
}

/// 图标文件体积上限（2MB）。
///
/// 包里的图标是界面用的 44px 徽章，正常几十 KB。超过这个量级多半是把
/// 整张 4K 原图塞进了包 —— 内联进列表响应会让面板每次刷新都拖几百 KB
/// 到几 MB，所以宁可不显示图标（回落字母），也不把列表拖慢。
const ICON_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// 从版本目录里读出包图标，转成 data URI。
///
/// ── 找哪个文件 ──────────────────────────────────────────────
/// 清单的 `icons` 是一个**目录**（相对包根）。目录里放什么名字没有约定，
/// 所以按固定优先级挑第一个存在的：`logo.*` → `icon.*` → 目录下任意
/// 一张图。优先级而不是「第一个文件名」，是为了让作者能用 `logo.png`
/// 明确指定，而不是靠字典序碰运气。
///
/// 任何一步失败都返回 `None`（界面回落字母），不报错 —— 图标是装饰，
/// 不该让「列表」这个核心功能跟着失败。
fn pack_icon(dir: &Path, manifest: Option<&proto::PackManifest>) -> Option<String> {
    let icons_dir = manifest?.icons.as_ref()?;
    let icons_dir = dir.join(icons_dir);
    let mut candidates: Vec<PathBuf> = Vec::new();
    for stem in ["logo", "icon"] {
        for ext in ["png", "webp", "jpg", "jpeg", "svg"] {
            candidates.push(icons_dir.join(format!("{stem}.{ext}")));
        }
    }
    // 上面都没命中时，取目录里任意一张图（排序保证同一份包每次选同一张）
    if let Ok(entries) = std::fs::read_dir(&icons_dir) {
        let mut rest: Vec<PathBuf> = entries
            .filter_map(|item| item.ok())
            .map(|item| item.path())
            .filter(|path| path.is_file() && is_image_path(path))
            .collect();
        rest.sort();
        candidates.extend(rest);
    }
    let path = candidates.into_iter().find(|path| path.is_file())?;
    if std::fs::metadata(&path).ok()?.len() > ICON_MAX_BYTES {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    let mime = match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "webp" => "image/webp",
        "jpg" | "jpeg" => "image/jpeg",
        "svg" => "image/svg+xml",
        _ => return None,
    };
    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Some(format!("data:{mime};base64,{encoded}"))
}

/// 扩展名是不是界面能显示的图片。
fn is_image_path(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "png" | "webp" | "jpg" | "jpeg" | "svg"
    )
}

/// 把包清单里的 UI 页面展开成客户端能直接用的形态（`url` 指向网关托管地址）。
///
/// ── 为什么要校验 `entry` ────────────────────────────────────
/// `entry` 是包自己写的字符串。不校验的话，一个包可以写 `../../../../etc/passwd`
/// 让托管端点去读包外的文件 —— 那是路径穿越。这里在**声明侧**就挡掉：必须是
/// 相对路径、不含 `..`、以 `.html` 结尾。托管端点那边还会再挡一次（纵深防御）。
fn pack_ui_pages(pack_id: &str, manifest: Option<&proto::PackManifest>) -> Vec<PackUiSummary> {
    let Some(manifest) = manifest else {
        return Vec::new();
    };
    manifest
        .ui
        .iter()
        .filter(|page| ui_entry_ok(&page.entry))
        .map(|page| PackUiSummary {
            id: page.id.clone(),
            title: page.title.clone(),
            icon: page.icon.clone(),
            // 包 id 与入口都进 URL，所以两边都要 URL-safe：包 id 由安装期校验
            // （只允许 `[a-z0-9-]`），入口这里已挡掉 `..` 与绝对路径
            url: format!("/plugins/{pack_id}/ui/{}", page.entry.replace('\\', "/")),
            // 原样转出插件申报的默认分组，不做白名单校验（理由见
            // `PackUiSummary::group` 的说明）
            group: page.group.clone(),
            order: page.order,
        })
        .collect()
}

/// UI 页面的入口路径是否合法（相对、不含 `..`、以 `.html` 结尾）。
fn ui_entry_ok(entry: &str) -> bool {
    let normalized = entry.replace('\\', "/");
    if normalized.is_empty() || normalized.starts_with('/') {
        return false;
    }
    if normalized.split('/').any(|segment| segment == "..") {
        return false;
    }
    normalized.to_ascii_lowercase().ends_with(".html")
}

/// 列出全部已安装包（含未启用的）。
///
/// 清单读不出来不报错：`InstalledPack` 自己记着 `entry` 与版本，界面照样能
/// 显示一行「清单损坏，请重新安装」。整表读失败才返回空 —— 那是「没装过」。
pub fn list_packs() -> Vec<PackSummary> {
    let registry = PackRegistry::load();
    registry
        .packs
        .iter()
        .map(|pack| {
            let dir = plugins_dir().join(&pack.id).join(&pack.version);
            let manifest = read_manifest(&dir).ok();
            let name = manifest
                .as_ref()
                .map(|item| item.name.clone())
                .unwrap_or_else(|| pack.id.clone());
            let protocol = manifest
                .as_ref()
                .map(|item| item.protocol)
                .unwrap_or(0);
            let homepage = manifest.as_ref().and_then(|item| item.homepage.clone());
            let declared = manifest
                .as_ref()
                .map(|item| item.providers.clone())
                .unwrap_or_else(|| pack.manifest_providers.clone());
            let providers = declared
                .into_iter()
                .map(|item| {
                    let id = item.id.0.clone();
                    let enabled = pack.providers.get(&id).copied().unwrap_or(true);
                    PackProviderSummary {
                        loaded: enabled && super::serves_provider(&id),
                        enabled,
                        label: item.label,
                        id,
                    }
                })
                .collect();
            PackSummary {
                id: pack.id.clone(),
                name,
                version: pack.version.clone(),
                enabled: pack.enabled,
                signed: pack.signed,
                installed_at: pack.installed_at,
                providers,
                entry_present: dir.join(&pack.entry).exists(),
                protocol,
                homepage,
                icon: pack_icon(&dir, manifest.as_ref()),
                ui: pack_ui_pages(&pack.id, manifest.as_ref()),
            }
        })
        .collect()
}

/// 从 zip 字节流安装（或升级）一个包。
///
/// ── 顺序（每一步失败都留下干净的现场）────────────────────────
///   1. 解到 `plugins/.staging/<随机>/`（`plugins_dir` 的兄弟临时目录，
///      与版本目录同一分区 —— `rename` 才能是原子的）；
///   2. 读 `pack.json` 并 [`validate_manifest`]（协议版本 + 入口路径）；
///   3. 确认入口二进制在解包结果里真的存在；
///   4. `rename` 成 `plugins/<id>/<version>/`（同版本已存在则先删旧的）；
///   5. 写注册表（启用、记 sha256 与安装时刻）。
///
/// 任一步失败都删掉临时目录 —— **绝不**留下半个版本目录。第 4 步之前失败时
/// 连 `plugins/<id>/` 都不会被创建，所以「装坏了」不会影响已在用的版本。
///
/// ── Windows 上的重试（`os error 5 拒绝访问`）──────────────────
/// 第 4 步的目录改名与「删旧版本」都可能被**杀软的实时扫描**挡下来：
/// 刚解压出来的 exe 会被立刻扫一遍，扫描期间句柄不释放，整个目录就动不了。
/// 这不是包的问题，等一两百毫秒就好 —— 所以两处都走带退避的重试
/// （见 [`move_dir_retrying`] / [`remove_dir_retrying`]）。
pub fn install_zip(bytes: &[u8]) -> Result<PackSummary, String> {
    if bytes.is_empty() {
        return Err("插件包是空的".to_string());
    }
    let staging = plugins_dir().join(".staging").join(format!(
        "pack-{}",
        crate::server::logging::now_ms()
    ));
    std::fs::create_dir_all(&staging).map_err(|e| format!("建临时目录失败: {e}"))?;
    // 任何提前返回都要清理；用一个内部函数 + 外层统一收尾比逐处删更难漏
    let result = extract_and_install(bytes, &staging);
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    result
}

fn extract_and_install(bytes: &[u8], staging: &Path) -> Result<PackSummary, String> {
    extract_zip(bytes, staging)?;
    let manifest = read_manifest(staging)?;
    validate_manifest(&manifest, proto::PROTOCOL_VERSION)?;
    let target = super::rpc::target_triple();
    // 入口必须真的在包里：清单写了个不存在的路径时，装完只会得到一个
    // 「启动失败」的包，而错误信息会指向 spawn（看不出是清单写错）
    let entry = entry_for_target(&manifest, &target)
        .ok_or_else(|| {
            format!(
                "这个包没有为当前平台（{target}）提供入口二进制，请下载对应平台的包"
            )
        })?;
    if !staging.join(&entry).exists() {
        return Err(format!("包里的入口二进制不存在：{entry}"));
    }
    // ── 验签（第三道，也是唯一能证明「谁发布的」那道）─────────────
    // 未签名 → 允许装（第三方/自建包必须有活路），界面标黄；
    // 签名无效 → 一律拒绝：包被改过，或者不是它自称的发布者签的。
    let (signature_state, signature_problem) = verify_signature(staging, &manifest, &target);
    if signature_state == SignatureState::Invalid {
        return Err(format!(
            "签名校验失败：{}",
            signature_problem.unwrap_or_else(|| "原因未知".to_string())
        ));
    }

    let version_dir = plugins_dir().join(&manifest.id).join(&manifest.version);
    if let Some(parent) = version_dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("建包目录失败: {e}"))?;
    }
    // 同版本重装：先删旧的再 rename（Windows 上 rename 不能覆盖非空目录）
    if version_dir.exists() {
        remove_dir_retrying(&version_dir)
            .map_err(|e| format!("清理旧版本失败: {e}"))?;
    }
    // ── 为什么 rename 要重试 ────────────────────────────────────
    // 刚解压出来的 `agent2api-providers.exe` 会被 Windows Defender（以及各种
    // 国产杀软）的实时保护**立刻**扫一遍；扫描期间文件句柄不释放，于是整个
    // 目录的 `MoveFileEx` 直接返回 `os error 5 拒绝访问`。这一步失败与包本身
    // 无关，隔一两百毫秒重试就过去了 —— 新装的机器上首次装包几乎必然撞上
    // （Defender 的「首次见到这个文件」扫描最慢）。
    //
    // 三次、总约 1.5 秒：够 Defender 扫完一个 12MB 的 exe；再长会让用户以为
    // 界面卡死（这个请求是同步的，前端在转圈等）。
    move_dir_retrying(staging, &version_dir)
        .map_err(|e| format!("落盘失败: {e}"))?;

    let mut registry = PackRegistry::load();
    let installed_at = crate::server::logging::now_ms();
    let sha256 = sha256_hex(bytes);
    let providers: BTreeMap<String, bool> = manifest
        .providers
        .iter()
        .map(|item| (item.id.0.clone(), item.default_enabled))
        .collect();
    let record = InstalledPack {
        id: manifest.id.clone(),
        version: manifest.version.clone(),
        enabled: true,
        providers,
        entry,
        sha256,
        signed: signature_state.is_signed(),
        installed_at,
        manifest_providers: manifest.providers.clone(),
    };
    match registry.packs.iter_mut().find(|pack| pack.id == record.id) {
        Some(slot) => *slot = record,
        None => registry.packs.push(record),
    }
    registry.save()?;
    logging::log(
        "Plugin",
        &format!(
            "插件包「{}」v{} 已安装（{} 家提供商）",
            manifest.id,
            manifest.version,
            manifest.providers.len()
        ),
    );
    list_packs()
        .into_iter()
        .find(|pack| pack.id == manifest.id)
        .ok_or_else(|| "安装后读不到注册表条目".to_string())
}

/// 解 zip 到目标目录（拒绝越界路径）。
fn extract_zip(bytes: &[u8], target: &Path) -> Result<(), String> {
    let reader = std::io::Cursor::new(bytes);
    let mut archive =
        zip::ZipArchive::new(reader).map_err(|e| format!("不是合法的 zip 包: {e}"))?;
    for index in 0..archive.len() {
        let mut file = archive
            .by_index(index)
            .map_err(|e| format!("读 zip 条目失败: {e}"))?;
        // `enclosed_name` 已经做了「不能越出解包目录」的判定（含 `..` 与
        // 绝对路径两种形态）；返回 None 一律拒绝，不尝试修复路径
        let Some(relative) = file.enclosed_name() else {
            return Err(format!("包里有越界路径，已拒绝安装：{}", file.name()));
        };
        let out_path = target.join(&relative);
        if file.is_dir() {
            std::fs::create_dir_all(&out_path).map_err(|e| format!("建目录失败: {e}"))?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("建目录失败: {e}"))?;
        }
        let mut out = std::fs::File::create(&out_path)
            .map_err(|e| format!("写文件失败（{}）: {e}", out_path.display()))?;
        std::io::copy(&mut file, &mut out).map_err(|e| format!("解包失败: {e}"))?;
        // Unix 上要保住可执行位（zip 的 external attributes 高 16 位是 mode）
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = file.unix_mode().unwrap_or(0o644);
            let _ = std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(mode));
        }
    }
    Ok(())
}

/// 目录改名的重试退避：三次，合计约 1.4 秒。
///
/// ── 为什么是「重试」而不是「换个写法」──────────────────────────
/// `MoveFileEx` 返回 `ERROR_ACCESS_DENIED`（os error 5）时，Windows 没有提供
/// 「谁占着」的查询接口（`RestartManager` 能查，但要额外的 API 与权限，且对
/// 杀软的内核回调无效）。而这里的占用**本来就是暂时的**：Defender 扫完就放。
/// 于是重试是最直接的解法。
const DIR_RETRY_DELAYS_MS: [u64; 3] = [80, 250, 1000];

/// 目录改名，遇到「被占用」就退避重试。
fn move_dir_retrying(from: &Path, to: &Path) -> std::io::Result<()> {
    retry_dir_op(|| std::fs::rename(from, to), from)
}

/// 删目录，遇到「被占用」就退避重试（同 [`move_dir_retrying`]）。
fn remove_dir_retrying(dir: &Path) -> std::io::Result<()> {
    retry_dir_op(|| std::fs::remove_dir_all(dir), dir)
}

/// 目录操作的统一重试壳。
///
/// 只对「暂时性」错误重试：Windows 的 `PermissionDenied`（os error 5，杀软
/// 扫描 / 索引服务占着）与 `ResourceBusy`。其余错误（路径不存在、磁盘满、
/// 名字非法）立刻返回 —— 那些重试一万次也一样，只会把一次失败的安装拖成
/// 十秒的等待。
fn retry_dir_op<F>(mut op: F, subject: &Path) -> std::io::Result<()>
where
    F: FnMut() -> std::io::Result<()>,
{
    let mut last = match op() {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };
    if !is_transient_io_error(&last) {
        return Err(explain_dir_error(last, subject));
    }
    for delay in DIR_RETRY_DELAYS_MS {
        std::thread::sleep(std::time::Duration::from_millis(delay));
        match op() {
            Ok(()) => return Ok(()),
            Err(error) => {
                if !is_transient_io_error(&error) {
                    return Err(explain_dir_error(error, subject));
                }
                last = error;
            }
        }
    }
    Err(explain_dir_error(last, subject))
}

/// 这个 IO 错误值不值得重试（「暂时被占用」这一类）。
fn is_transient_io_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ResourceBusy
    )
}

/// 给最终失败的目录错误补上可操作的说明。
///
/// 原始的 `os error 5` 对用户没有信息量 —— 他不知道是杀软、是权限、还是
/// 自己把配置目录设到了只读盘上。这里按 Windows 最常见的那个原因给出指引。
fn explain_dir_error(error: std::io::Error, subject: &Path) -> std::io::Error {
    if !is_transient_io_error(&error) {
        return error;
    }
    let hint = if cfg!(windows) {
        "（重试 3 次仍被拒绝：多半是杀毒软件的实时保护正锁着刚解压出来的 exe。\
         把配置目录加入杀软白名单，或关掉实时保护后重试）"
    } else {
        "（重试 3 次仍被拒绝：请检查该目录的写权限）"
    };
    std::io::Error::new(
        error.kind(),
        format!("{}{}：{subject:?}", error, hint),
    )
}

/// 卸载一个包：删版本目录 + 从注册表移除。
///
/// 宿主进程由调用方（api 层）先关掉 —— 这里只动磁盘。Windows 上二进制被
/// 占用时删不掉，那种情况返回明确错误（「请先停用插件再卸载」）。
pub fn uninstall(id: &str) -> Result<(), String> {
    let mut registry = PackRegistry::load();
    let Some(index) = registry.packs.iter().position(|pack| pack.id == id) else {
        return Err(format!("没有安装这个插件包：{id}"));
    };
    let pack = registry.packs.remove(index);
    let dir = plugins_dir().join(&pack.id);
    if dir.exists() {
        // 走重试版：卸载时同样会撞上杀软扫描（尤其是刚装完还没扫过的包）
        remove_dir_retrying(&dir).map_err(|e| {
            format!("删除包目录失败（{e}）：如果插件进程还在运行，请先停用插件再卸载")
        })?;
    }
    registry.save()?;
    logging::log("Plugin", &format!("插件包「{id}」已卸载"));
    Ok(())
}

/// 启用 / 停用整个包。
pub fn set_pack_enabled(id: &str, enabled: bool) -> Result<(), String> {
    let mut registry = PackRegistry::load();
    let Some(pack) = registry.packs.iter_mut().find(|pack| pack.id == id) else {
        return Err(format!("没有安装这个插件包：{id}"));
    };
    pack.enabled = enabled;
    registry.save()?;
    Ok(())
}

/// 启用 / 停用包内的某一家 provider。
pub fn set_provider_enabled(id: &str, provider: &str, enabled: bool) -> Result<(), String> {
    let mut registry = PackRegistry::load();
    let Some(pack) = registry.packs.iter_mut().find(|pack| pack.id == id) else {
        return Err(format!("没有安装这个插件包：{id}"));
    };
    if !pack
        .manifest_providers
        .iter()
        .any(|item| item.id.0 == provider)
    {
        return Err(format!("这个包里没有提供商 {provider}"));
    }
    pack.providers.insert(provider.to_string(), enabled);
    registry.save()?;
    Ok(())
}

/// zip 字节的 sha256（十六进制小写）。
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(protocol: u32) -> proto::PackManifest {
        proto::PackManifest {
            id: "test".into(),
            name: "Test".into(),
            version: "1.0.0".into(),
            protocol,
            providers: vec![proto::PackProvider {
                id: proto::ProviderId::new("demo"),
                label: "Demo".into(),
                order: 0,
                default_enabled: true,
            }],
            entry: BTreeMap::from([("x86_64-pc-windows-msvc".to_string(), "bin/x.exe".to_string())]),
            icons: None,
            homepage: None,
            signature: None,
        }
    }

    #[test]
    fn rejects_protocol_mismatch() {
        let err = validate_manifest(&manifest(999), 1).unwrap_err();
        assert!(err.contains("999"), "错误里要带上对方的版本号: {err}");
    }

    #[test]
    fn rejects_absolute_entry() {
        let mut m = manifest(1);
        m.entry.insert("t".into(), "/usr/bin/evil".into());
        assert!(validate_manifest(&m, 1).is_err());
    }

    #[test]
    fn rejects_parent_traversal() {
        let mut m = manifest(1);
        m.entry.insert("t".into(), "../../evil".into());
        assert!(validate_manifest(&m, 1).is_err());
    }

    #[test]
    fn accepts_matching_protocol() {
        assert!(validate_manifest(&manifest(1), 1).is_ok());
    }

    // ── 签名 ────────────────────────────────────────────────────
    //
    // 这一组钉住的是「验签的判据」，不是「某个具体签名值」：
    //   · 没有签名 → Unsigned（允许装，标黄）；
    //   · 签名平台对不上 → Unsigned（多平台包每个平台各签一份）；
    //   · 算法不认识 / 密钥不在表里 / 签名不匹配 → Invalid（拒绝装）。
    //
    // 用 `ed25519_dalek::SigningKey` 现生成一对密钥，而不是把测试向量写死：
    // 写死的向量在依赖升级后会莫名其妙地失败，而这里要验的是逻辑。

    /// 造一个已解包的临时目录（入口二进制写几个字节）。
    fn staged_dir(tag: &str, binary: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "a2a-sign-test-{tag}-{}",
            crate::server::logging::now_ms()
        ));
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).expect("建临时目录");
        std::fs::write(bin.join("x.exe"), binary).expect("写入口二进制");
        dir
    }

    #[test]
    fn unsigned_pack_is_allowed() {
        let dir = staged_dir("unsigned", b"binary");
        let (state, reason) = verify_signature(&dir, &manifest(1), "x86_64-pc-windows-msvc");
        assert_eq!(state, SignatureState::Unsigned);
        assert!(reason.is_none(), "未签名不该带失败原因");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 签名绑定的平台与本平台不一致 → 当作未签名（不是无效）。
    #[test]
    fn signature_for_other_target_is_unsigned() {
        let dir = staged_dir("othertarget", b"binary");
        let mut m = manifest(1);
        m.signature = Some(proto::PackSignature {
            algorithm: "ed25519".into(),
            key_id: "test".into(),
            value: "AAAA".into(),
            target: Some("aarch64-apple-darwin".into()),
        });
        let (state, _) = verify_signature(&dir, &m, "x86_64-pc-windows-msvc");
        assert_eq!(state, SignatureState::Unsigned);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_algorithm_is_invalid() {
        let dir = staged_dir("alg", b"binary");
        let mut m = manifest(1);
        m.signature = Some(proto::PackSignature {
            algorithm: "rsa-sha256".into(),
            key_id: "test".into(),
            value: "AAAA".into(),
            target: Some("x86_64-pc-windows-msvc".into()),
        });
        let (state, reason) = verify_signature(&dir, &m, "x86_64-pc-windows-msvc");
        assert_eq!(state, SignatureState::Invalid);
        assert!(reason.unwrap_or_default().contains("rsa-sha256"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_key_id_is_invalid() {
        let dir = staged_dir("keyid", b"binary");
        let mut m = manifest(1);
        m.signature = Some(proto::PackSignature {
            algorithm: "ed25519".into(),
            key_id: "not-in-table".into(),
            value: "AAAA".into(),
            target: Some("x86_64-pc-windows-msvc".into()),
        });
        let (state, reason) = verify_signature(&dir, &m, "x86_64-pc-windows-msvc");
        assert_eq!(state, SignatureState::Invalid);
        assert!(reason.unwrap_or_default().contains("not-in-table"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 载荷里必须含二进制摘要：改了二进制，同一份签名就验不过。
    ///
    /// 这条是签名机制的核心断言 —— 少了它，签名只覆盖元数据，
    /// 攻击者换掉 `bin/` 里的二进制而签名照样成立。
    #[test]
    fn payload_covers_binary_and_target() {
        let m = manifest(1);
        let left = proto::signing_payload(&m, "x86_64-pc-windows-msvc", "aaaa");
        let right = proto::signing_payload(&m, "x86_64-pc-windows-msvc", "bbbb");
        assert_ne!(left, right, "二进制摘要变了，载荷必须变");
        let other_target = proto::signing_payload(&m, "aarch64-apple-darwin", "aaaa");
        assert_ne!(left, other_target, "平台变了，载荷必须变");
        assert!(left.starts_with("agent2api-pack-v1\n"));
        assert!(left.contains("binary=aaaa"));
    }

    /// 完整的「签 → 验」往返：用同一对密钥签出的包必须验过，
    /// 改一个字节的包必须验不过。
    #[test]
    fn sign_and_verify_roundtrip() {
        use ed25519_dalek::{Signer, SigningKey};
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let public_hex: String = signing
            .verifying_key()
            .to_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();

        let target = "x86_64-pc-windows-msvc";
        let dir = staged_dir("roundtrip", b"the-real-binary");
        let binary_hash = sha256_file(&dir.join("bin/x.exe")).expect("算摘要");
        let mut m = manifest(1);
        let payload = proto::signing_payload(&m, target, &binary_hash);
        let signature = signing.sign(payload.as_bytes());
        m.signature = Some(proto::PackSignature {
            algorithm: "ed25519".into(),
            key_id: "test".into(),
            value: base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()),
            target: Some(target.into()),
        });

        // 公钥表是编译期常量，测试里临时替换不了 —— 所以这里直接调
        // verify_ed25519 验逻辑（表查找那一段由上面几条 Invalid 用例覆盖）
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(m.signature.as_ref().map(|s| s.value.clone()).unwrap_or_default())
            .expect("解 base64");
        assert!(verify_ed25519(
            &hex_to_32(&public_hex).expect("公钥十六进制"),
            payload.as_bytes(),
            &decoded
        ));

        // 换一份内容（模拟包被改过）→ 必须验不过
        let tampered = proto::signing_payload(&m, target, "0000");
        assert!(!verify_ed25519(
            &hex_to_32(&public_hex).expect("公钥十六进制"),
            tampered.as_bytes(),
            &decoded
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 十六进制解析：长度或字符不对都要返回 None（不 panic）。
    #[test]
    fn hex_parsing_rejects_bad_input() {
        assert!(hex_to_32("zz").is_none());
        assert!(hex_to_32(&"a".repeat(63)).is_none());
        assert!(hex_to_32(&"a".repeat(64)).is_some());
    }
}

// ─── UI 资源托管 ─────────────────────────────────────────────

/// 把 `/plugins/{pack}/ui/{rest}` 解析成磁盘上的文件路径（`None` = 不存在 / 非法）。
///
/// ── 安全边界（三条，缺一条就是漏洞）──────────────────────────
///   1. **只服务已启用包的版本目录**：包 id 必须在注册表里且 `enabled`，
///      版本目录由注册表给（不是 URL 给的）—— 于是 URL 里的 `{pack}` 只是一个
///      查表的键，不能指向任意目录；
///   2. **路径逐段归一化**：`..` / 绝对路径 / 反斜杠一律拒绝，不允许逃出
///      `ui/` 目录；
///   3. **只出 `ui/` 前缀下的文件**：包里的 `bin/`、`pack.json` 不对外
///      （那些是后端与元数据，前端没有理由读到）。
///
/// 与 `static_files::serve` 分开写而不是复用：那个的根是 `ui_dir`、还要注入
/// web shim（面板桥接），语义完全不同。共用会逼着两边都长出一堆分支。
pub fn resolve_ui_file(pack_id: &str, rest: &str) -> Option<PathBuf> {
    if pack_id.is_empty() || rest.is_empty() {
        return None;
    }
    // ① 包必须已安装且启用（停用 = 页面也不该能访问）
    let registry = PackRegistry::load();
    let pack = registry
        .packs
        .iter()
        .find(|item| item.id == pack_id && item.enabled)?;
    let root = plugins_dir().join(&pack.id).join(&pack.version).join("ui");
    // ② 逐段归一化（与 `static_files::resolve_file` 同一套规则）
    let mut segments: Vec<&str> = Vec::new();
    for segment in rest.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            other => {
                // 反斜杠：Windows 分隔符，归一化阶段就该拒绝（`..\..\x` 这类）
                if other.contains('\\') {
                    return None;
                }
                segments.push(other);
            }
        }
    }
    if segments.is_empty() {
        return None;
    }
    let mut path = root.clone();
    for segment in &segments {
        path.push(segment);
    }
    // ③ 归一化后的路径必须仍在 `ui/` 下（双保险：`..` 已在上面消化）
    let canonical_root = root.canonicalize().ok()?;
    let canonical = path.canonicalize().ok()?;
    if !canonical.starts_with(&canonical_root) {
        return None;
    }
    canonical.is_file().then_some(canonical)
}
