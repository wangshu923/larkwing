//! 文件授权圈(§7.2「文件授权圈」,2026-07-30 用户拍板,推翻原「不设路径门禁」):
//! 只有明确允许的文件夹,模型才能读写;程序数据目录免授权。
//!
//! **闸的是模型的手脚,不是程序自己**:只拦「模型通过工具入参指定的路径」(fs 家族、
//! media_play 本地路径、各下载工具落盘 dir、send_file/read_image/pdf_to_png/web_render
//! 上传等);程序内部读写(TTS/媒体缓存、日志、收件区——全在数据根下)不经工具层,零影响。
//! 定位 = **安全带不是保险库**(§7.8 确认闸同口径):防模型犯错/被网页内容带偏乱动文件,
//! 不承诺对抗恶意;可逆三规(回收站/永不覆盖/快照撤销)仍是第二层保险。
//!
//! 三档 × 四动作(2026-07-30 用户拍板,§4.11;常量单源在此):
//!   只读 read     = 读(看内容/列目录/找文件)
//!   可存入 create = 读 + 新建(落新文件/建目录,不动已有内容)
//!   完全访问 full = 读 + 新建 + 修改(覆盖/编辑/移走)+ 删除(回收站)
//! 内置区:数据根恒 full(程序自己的,不进表、设置页只显示说明);系统「下载」「桌面」
//! = 出厂基线 create(升 full 落一条表记录、降档删记录回落基线)。
//! 授权即覆盖整棵子树(按路径组件前缀匹配,`D:\Movies2` 蹭不上 `D:\Movies`);入表自动
//! 合并被覆盖、档位不高于新条目的子孙记录(档位更高的子条目保留,绝不静默降级)。
//!
//! 撞圈 → 确认卡(复用 §7.8 Confirmer 四通道)「一直允许 / 仅这次 / 先不要」:
//! 一直允许 = 按所需最低档入表永久记住;仅这次 = 本回合内该目录临时放行
//! (`ToolCtx::grants`,回合结束即失效);拒绝也记本回合(同目录同档不再连环弹)。
//! 渠道文字/语音口头回「允许」恒按仅这次 —— 永久授权是改机器配置,只在有 UI 的地方给。
//! 接线纪律:**新收路径的工具一律在拿到路径后、动手前过 `ensure()`**(expand_home 同款)。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::store::Store;

/// 授权表的 settings 键(app 级,文件系统是这台电脑的、不按人分;
/// 专用命令收口读写,不进 `APP_SETTING_KEYS` 通用白名单 —— llm.model_overrides 同款)。
const SCOPES_KEY: &str = "fs.scopes";

// ---------------------------------------------------------------------------
// 档位与动作
// ---------------------------------------------------------------------------

/// 授权档位(设置页三态)。Ord 语义 = 权限包含关系(合并/覆盖判断用)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// 只读:看内容 / 列目录 / 找文件。
    Read,
    /// 可存入:读 + 新建(不动已有内容)。下载/桌面的出厂基线。
    Create,
    /// 完全访问:读 + 新建 + 修改 + 删除。
    Full,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Read => "read",
            Mode::Create => "create",
            Mode::Full => "full",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "read" => Some(Mode::Read),
            "create" => Some(Mode::Create),
            "full" => Some(Mode::Full),
            _ => None,
        }
    }

    fn allows(self, access: Access) -> bool {
        match access {
            Access::Read => true, // 任一档都含读
            Access::Create => self >= Mode::Create,
            Access::Modify | Access::Delete => self == Mode::Full,
        }
    }
}

/// 工具动作分类(接入点声明)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// 读内容 / 列目录 / 找文件 / 读出去(发送、上传)。
    Read,
    /// 新建落盘(下载 / 写新文件 / 建目录 / 复制目标),不动已有内容。
    Create,
    /// 动已有内容(覆盖 / 编辑 / 追加 / 从原地移走)。
    Modify,
    /// 删除(回收站)。
    Delete,
}

impl Access {
    /// 满足该动作所需的最低档(「一直允许」按它入表)。
    pub fn need_mode(self) -> Mode {
        match self {
            Access::Read => Mode::Read,
            Access::Create => Mode::Create,
            Access::Modify | Access::Delete => Mode::Full,
        }
    }

    /// 确认卡 kind(前端动词字典 `confirm.act.*` 按它选;渠道话术同源)。
    pub fn kind(self) -> &'static str {
        match self {
            Access::Read => "fs_read",
            Access::Create => "fs_create",
            Access::Modify => "fs_modify",
            Access::Delete => "fs_delete",
        }
    }

    /// bail 话术里的中文动词(工具结果是喂模型的观察,不是 UI 文案,§6.6 豁免口径
    /// 同 fs 工具的「移动了 N 个」)。
    fn verb(self) -> &'static str {
        match self {
            Access::Read => "查看",
            Access::Create => "存入",
            Access::Modify => "修改",
            Access::Delete => "删除",
        }
    }
}

/// 一条授权记录(settings JSON 数组元素;path 存规范化后的绝对路径)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopeEntry {
    pub path: String,
    pub mode: Mode,
}

// ---------------------------------------------------------------------------
// 路径规范化与覆盖判定(纯函数,可测)
// ---------------------------------------------------------------------------

/// 词法归一:吃掉 `.` 与 `..`(不触盘)。`..` 到根就停(pop 无效无害)。
fn lexical_normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// 判定用的规范形:`~` 展开 → 绝对化 → canonicalize 存在的最深祖先(解 symlink、
/// 统一盘符写法;产出过 `datadir::simplify` 防 `\\?\` verbatim 毒,§8.1)→ 余下段
/// 词法拼接 → 归一 `.`/`..`。目标不存在(写新文件)也能得到稳定形。
pub(crate) fn normalize_for_check(raw: &str) -> PathBuf {
    let expanded = super::expand_home(raw.trim());
    let p = PathBuf::from(&expanded);
    let abs = if p.is_absolute() {
        p
    } else {
        std::env::current_dir().map(|c| c.join(&p)).unwrap_or(p)
    };
    let abs = lexical_normalize(&abs);
    // 找存在的最深祖先做 canonicalize,余下段原样接回
    let mut existing = abs.clone();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
                rest.push(name.to_os_string());
                existing = parent.to_path_buf();
            }
            _ => break,
        }
    }
    let base = existing
        .canonicalize()
        .map(|c| crate::datadir::simplify(&c))
        .unwrap_or(existing);
    let mut out = base;
    for seg in rest.iter().rev() {
        out.push(seg);
    }
    out
}

/// 路径组件折叠形(比较用)。Windows/macOS 文件系统默认不区分大小写 → 折小写;
/// 其余(Linux)保持原样。
fn comps_folded(p: &Path) -> Vec<String> {
    p.components()
        .map(|c| {
            let s = c.as_os_str().to_string_lossy();
            if cfg!(any(windows, target_os = "macos")) {
                s.to_lowercase()
            } else {
                s.into_owned()
            }
        })
        .collect()
}

/// `dir` 是否覆盖 `target`(含相等):按路径**组件**前缀比,`D:\Movies2` 蹭不上
/// `D:\Movies`;大小写敏感度随平台。
pub(crate) fn dir_covers(dir: &Path, target: &Path) -> bool {
    let d = comps_folded(dir);
    let t = comps_folded(target);
    !d.is_empty() && d.len() <= t.len() && d.iter().zip(t.iter()).all(|(a, b)| a == b)
}

/// 弹卡粒度 = 目录:存在的目录取本身,其余(文件/不存在的目标)取父。
fn confirm_dir_of(t: &Path) -> PathBuf {
    if t.is_dir() {
        t.to_path_buf()
    } else {
        t.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| t.to_path_buf())
    }
}

/// 同批弹卡目录归并:去等价重复、去被同批祖先覆盖的(一张卡列最少的目录)。
fn dedup_covered(dirs: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut kept: Vec<PathBuf> = Vec::new();
    'outer: for d in dirs {
        for k in &kept {
            if dir_covers(k, &d) {
                continue 'outer; // 已有等价/祖先
            }
        }
        kept.retain(|k| !dir_covers(&d, k)); // d 是已留项的祖先 → 换掉它们
        kept.push(d);
    }
    kept
}

/// 判定环境(纯参数化,单测不碰 OnceLock/settings)。
pub(crate) struct CheckEnv<'a> {
    /// 全权免闸区:程序数据根 + 系统临时目录(scratch 区,pdf 产物缺省落点)——整棵树恒 full。
    pub free: &'a [PathBuf],
    /// 出厂基线目录(下载/桌面),档位 = 可存入。
    pub baselines: &'a [PathBuf],
    /// 用户授权表。
    pub scopes: &'a [ScopeEntry],
    /// 本回合「仅这次」临时放行。
    pub grants: &'a [(PathBuf, Mode)],
}

/// 单条路径的放行判定(取所有覆盖来源里的最高档语义:任一来源允许即允许)。
pub(crate) fn allowed(env: &CheckEnv, target: &Path, access: Access) -> bool {
    if env.free.iter().any(|f| dir_covers(f, target)) {
        return true;
    }
    if Mode::Create.allows(access) && env.baselines.iter().any(|b| dir_covers(b, target)) {
        return true;
    }
    if env
        .scopes
        .iter()
        .any(|e| e.mode.allows(access) && dir_covers(Path::new(&e.path), target))
    {
        return true;
    }
    env.grants.iter().any(|(d, m)| m.allows(access) && dir_covers(d, target))
}

/// 入表合并(「一直允许」与设置页添加共用):同路径 = 改档;被新条目覆盖且档位
/// **不高于**新档的子孙 = 合并掉;档位更高的子条目保留(绝不静默降级)。
pub(crate) fn merged(mut list: Vec<ScopeEntry>, new_path: &Path, new_mode: Mode) -> Vec<ScopeEntry> {
    list.retain(|e| {
        let ep = PathBuf::from(&e.path);
        let same = dir_covers(new_path, &ep) && dir_covers(&ep, new_path);
        if same {
            return false; // 同一个目录:移除旧条目,下面按新档重加
        }
        !(dir_covers(new_path, &ep) && e.mode <= new_mode)
    });
    list.push(ScopeEntry { path: new_path.to_string_lossy().into_owned(), mode: new_mode });
    list.sort_by(|a, b| a.path.cmp(&b.path));
    list
}

// ---------------------------------------------------------------------------
// 存取(settings app 级)与内置区
// ---------------------------------------------------------------------------

/// 数据根(boot 时装配一次;没 set〔单测/eval〕= 不参与判定,靠授权表)。
static DATA_ROOT: OnceLock<PathBuf> = OnceLock::new();

pub fn set_data_root(root: PathBuf) {
    let _ = DATA_ROOT.set(root);
}

/// 出厂基线目录(下载/桌面)。每次现取(便宜),路径过同一套规范化。
pub(crate) fn baseline_dirs() -> Vec<PathBuf> {
    [dirs::download_dir(), dirs::desktop_dir()]
        .into_iter()
        .flatten()
        .map(|d| normalize_for_check(&d.to_string_lossy()))
        .collect()
}

/// 设置页展示:用户授权表(内置区不在内)。
pub fn list_scopes(store: &Store) -> Vec<ScopeEntry> {
    load_scopes(store)
}

/// 设置页展示:出厂基线目录的实际路径(下载, 桌面)。
pub fn builtin_baselines() -> (Option<String>, Option<String>) {
    let s = |d: Option<PathBuf>| d.map(|p| p.to_string_lossy().into_owned());
    (s(dirs::download_dir()), s(dirs::desktop_dir()))
}

pub(crate) fn load_scopes(store: &Store) -> Vec<ScopeEntry> {
    store
        .settings
        .get(None, SCOPES_KEY)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_scopes(store: &Store, list: &[ScopeEntry]) -> anyhow::Result<()> {
    let json = serde_json::to_string(list)?;
    store.settings.set(None, SCOPES_KEY, &json)?;
    Ok(())
}

/// 添加/改档一条授权(规范化 + 合并),返回合并后的表。设置页与「一直允许」共用。
pub fn upsert_scope(store: &Store, raw_path: &str, mode: Mode) -> anyhow::Result<Vec<ScopeEntry>> {
    let path = normalize_for_check(raw_path);
    anyhow::ensure!(path.is_absolute(), "只能授权绝对路径的文件夹");
    let list = merged(load_scopes(store), &path, mode);
    save_scopes(store, &list)?;
    Ok(list)
}

/// 删一条授权(按等价路径匹配),返回删后的表。
pub fn remove_scope(store: &Store, raw_path: &str) -> anyhow::Result<Vec<ScopeEntry>> {
    let path = normalize_for_check(raw_path);
    let mut list = load_scopes(store);
    list.retain(|e| {
        let ep = PathBuf::from(&e.path);
        !(dir_covers(&path, &ep) && dir_covers(&ep, &path))
    });
    save_scopes(store, &list)?;
    Ok(list)
}

// ---------------------------------------------------------------------------
// 回合内临时放行(「仅这次」)与已拒记录
// ---------------------------------------------------------------------------

/// 回合级授权缓存(挂 `ToolCtx::grants`,回合结束随 ToolCtx 丢弃;delegate 子回合
/// 共享父回合这一份——「本回合」含派生的子回合与其转后台续跑段,不二次弹卡):
/// allows = 「仅这次」放行;denies = 已拒(同目录、所需档 ≥ 拒过的档,不再连环弹)。
/// 轮内工具并发(join_all)→ Mutex。
#[derive(Clone, Default)]
pub struct Grants {
    inner: Arc<Mutex<GrantsInner>>,
}

#[derive(Default)]
struct GrantsInner {
    allows: Vec<(PathBuf, Mode)>,
    denies: Vec<(PathBuf, Mode)>,
}

impl Grants {
    fn allows_snapshot(&self) -> Vec<(PathBuf, Mode)> {
        self.inner.lock().expect("grants poisoned").allows.clone()
    }

    fn add_allow(&self, dir: PathBuf, mode: Mode) {
        self.inner.lock().expect("grants poisoned").allows.push((dir, mode));
    }

    fn add_deny(&self, dir: PathBuf, mode: Mode) {
        self.inner.lock().expect("grants poisoned").denies.push((dir, mode));
    }

    /// 这个目录、这一档,本回合是不是已经被拒过(拒过 read 连 full 也别再问;
    /// 拒过 full 不挡后续 read 请求 —— 用户拒的是「修改」,读也许愿意)。
    fn denied_before(&self, dir: &Path, need: Mode) -> bool {
        self.inner
            .lock()
            .expect("grants poisoned")
            .denies
            .iter()
            .any(|(d, m)| need >= *m && dir_covers(d, dir))
    }
}

// ---------------------------------------------------------------------------
// ensure:接入点唯一入口
// ---------------------------------------------------------------------------

/// 工具边界的授权检查:`paths` 里任何不在圈内的路径 → 归并成一张确认卡问用户;
/// 「一直允许」入表、「仅这次」记回合、拒/超时/无通道 = bail(错误观察喂模型,
/// 话术自带「如实说、别绕」引导)。全部在圈内 = 零打扰直接 Ok。
pub(crate) async fn ensure(
    ctx: &super::ToolCtx,
    access: Access,
    raw_paths: &[String],
) -> anyhow::Result<()> {
    let scopes = load_scopes(&ctx.store);
    let grants = ctx.grants.allows_snapshot();
    let baselines = baseline_dirs();
    // 免闸区 = 数据根 + 系统临时目录(scratch 区,任何本地程序都能写,不算用户文件;
    // unix 的 /tmp 与 env temp_dir 可能是两处〔mac = /var/folders/…〕,都收)
    let mut free: Vec<PathBuf> = Vec::new();
    if let Some(root) = DATA_ROOT.get() {
        free.push(root.clone());
    }
    free.push(normalize_for_check(&std::env::temp_dir().to_string_lossy()));
    #[cfg(unix)]
    free.push(normalize_for_check("/tmp"));
    let env = CheckEnv { free: &free, baselines: &baselines, scopes: &scopes, grants: &grants };
    let mut need: Vec<PathBuf> = Vec::new();
    for raw in raw_paths {
        if raw.trim().is_empty() {
            continue;
        }
        let t = normalize_for_check(raw);
        if !allowed(&env, &t, access) {
            need.push(confirm_dir_of(&t));
        }
    }
    if need.is_empty() {
        return Ok(());
    }
    let dirs = dedup_covered(need);
    let list = dirs.iter().map(|d| d.to_string_lossy()).collect::<Vec<_>>().join("、");
    let verb = access.verb();
    let need_mode = access.need_mode();
    // 本回合已经拒过 → 不再弹卡,直接按拒收(防换个工具/换个说法连环弹)
    if dirs.iter().any(|d| ctx.grants.denied_before(d, need_mode)) {
        anyhow::bail!(
            "用户这次已经拒绝过{verb} {list},这步没做。按用户的意思来,如实说明即可,\
             不要换别的路径或工具绕过。"
        );
    }
    let Some(confirmer) = ctx.confirm.as_ref() else {
        anyhow::bail!(
            "要{verb} {list} 需要用户允许,但这里没有确认通道——这步没做。\
             如实告诉用户:可以在设置里「能碰的文件夹」添加这个文件夹后再试。"
        );
    };
    // 确认路由到回合来源(web_render 确认闸同款):桌面 = 卡片;渠道 = 推回那个 chat。
    let origin = ctx
        .store
        .chat
        .get_conversation(ctx.conv_id)
        .ok()
        .flatten()
        .map(|c| c.channel)
        .unwrap_or_else(|| "ui".into());
    let timeout = if matches!(origin.as_str(), "ui" | "system") {
        crate::confirm::DESKTOP_TIMEOUT
    } else {
        crate::confirm::CHANNEL_TIMEOUT
    };
    let decision = confirmer
        .ask(
            crate::confirm::ConfirmAsk {
                user_id: ctx.user_id,
                conv_id: ctx.conv_id,
                origin,
                host: String::new(),
                action: list.clone(),
                kind: access.kind().into(),
            },
            timeout,
        )
        .await;
    use crate::confirm::ConfirmDecision::*;
    match decision {
        Allowed { always: true, .. } => {
            for d in &dirs {
                upsert_scope(&ctx.store, &d.to_string_lossy(), need_mode)?;
            }
            Ok(())
        }
        Allowed { .. } => {
            for d in dirs {
                ctx.grants.add_allow(d, need_mode);
            }
            Ok(())
        }
        Denied { via } if via == "unreachable" => anyhow::bail!(
            "要{verb} {list} 需要用户确认,但确认请求没能送到用户那(渠道断线/没有收件地址)\
             ——这步没做,如实说明;用户可以在电脑上让我继续。"
        ),
        Denied { .. } => {
            for d in dirs {
                ctx.grants.add_deny(d, need_mode);
            }
            anyhow::bail!(
                "用户没有允许{verb} {list},这步没做。如实告知即可,\
                 不要换别的路径或工具绕过。"
            )
        }
        TimedOut => anyhow::bail!(
            "等了一会儿没等到用户确认,{verb} {list} 这步没做。可以稍后再试,\
             或请用户在设置里「能碰的文件夹」添加。"
        ),
        NoUi => anyhow::bail!(
            "要{verb} {list} 需要用户允许,但现在没有可用的确认界面——这步没做,\
             如实告诉用户即可。"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, mode: Mode) -> ScopeEntry {
        ScopeEntry { path: path.into(), mode }
    }

    #[test]
    fn mode_allows_matrix() {
        use Access::*;
        // 只读:只放读
        assert!(Mode::Read.allows(Read));
        assert!(!Mode::Read.allows(Create));
        assert!(!Mode::Read.allows(Modify));
        assert!(!Mode::Read.allows(Delete));
        // 可存入:读 + 新建
        assert!(Mode::Create.allows(Read));
        assert!(Mode::Create.allows(Create));
        assert!(!Mode::Create.allows(Modify));
        assert!(!Mode::Create.allows(Delete));
        // 完全访问:全放
        assert!(Mode::Full.allows(Read));
        assert!(Mode::Full.allows(Create));
        assert!(Mode::Full.allows(Modify));
        assert!(Mode::Full.allows(Delete));
        // 所需最低档
        assert_eq!(Read.need_mode(), Mode::Read);
        assert_eq!(Create.need_mode(), Mode::Create);
        assert_eq!(Modify.need_mode(), Mode::Full);
        assert_eq!(Delete.need_mode(), Mode::Full);
    }

    #[test]
    fn dir_covers_is_component_wise() {
        assert!(dir_covers(Path::new("/a/b"), Path::new("/a/b")), "相等 = 覆盖");
        assert!(dir_covers(Path::new("/a/b"), Path::new("/a/b/c/d.txt")));
        assert!(!dir_covers(Path::new("/a/b"), Path::new("/a/bcd")), "字符串前缀蹭不上");
        assert!(!dir_covers(Path::new("/a/b/c"), Path::new("/a/b")), "子不覆盖父");
        assert!(!dir_covers(Path::new(""), Path::new("/a")), "空路径不覆盖一切");
        // 大小写:win/mac 不区分(开发机 mac 即验),重复分隔符由 components 天然折叠
        if cfg!(any(windows, target_os = "macos")) {
            assert!(dir_covers(Path::new("/A/B"), Path::new("/a/b/c")));
        }
        assert!(dir_covers(Path::new("/e//nas"), Path::new("/e/nas/movies")), "双斜杠折叠");
    }

    #[cfg(windows)]
    #[test]
    fn dir_covers_windows_forms() {
        assert!(dir_covers(Path::new("D:\\Movies"), Path::new("d:\\movies\\a.mp4")));
        assert!(!dir_covers(Path::new("D:\\Movies"), Path::new("D:\\Movies2\\a.mp4")));
        assert!(dir_covers(Path::new("D:/Movies"), Path::new("D:\\Movies\\x")), "正反斜杠混写");
        assert!(dir_covers(Path::new("\\\\nas\\share"), Path::new("\\\\nas\\share\\电影")));
    }

    #[test]
    fn allowed_matrix_by_area() {
        use Access::*;
        let free = vec![PathBuf::from("/data/larkwing")];
        let baselines = vec![PathBuf::from("/home/u/Downloads"), PathBuf::from("/home/u/Desktop")];
        let scopes =
            vec![entry("/mnt/nas", Mode::Full), entry("/docs/ro", Mode::Read), entry("/inbox/sv", Mode::Create)];
        let grants: Vec<(PathBuf, Mode)> = vec![];
        let env =
            CheckEnv { free: &free, baselines: &baselines, scopes: &scopes, grants: &grants };
        // 数据根:全放
        for a in [Read, Create, Modify, Delete] {
            assert!(allowed(&env, Path::new("/data/larkwing/media/inbox/x.pdf"), a));
        }
        // 基线(下载/桌面):读 + 新建放,修改/删除不放
        assert!(allowed(&env, Path::new("/home/u/Downloads/a.mp3"), Read));
        assert!(allowed(&env, Path::new("/home/u/Downloads/a.mp3"), Create));
        assert!(!allowed(&env, Path::new("/home/u/Downloads/a.mp3"), Modify));
        assert!(!allowed(&env, Path::new("/home/u/Desktop/b.txt"), Delete));
        // 授权表三档
        for a in [Read, Create, Modify, Delete] {
            assert!(allowed(&env, Path::new("/mnt/nas/movies/x.mkv"), a), "full 全放");
        }
        assert!(allowed(&env, Path::new("/docs/ro/a.txt"), Read));
        assert!(!allowed(&env, Path::new("/docs/ro/a.txt"), Create), "只读不许新建");
        assert!(allowed(&env, Path::new("/inbox/sv/new.txt"), Create));
        assert!(!allowed(&env, Path::new("/inbox/sv/old.txt"), Modify), "可存入不许改");
        // 圈外:全不放
        for a in [Read, Create, Modify, Delete] {
            assert!(!allowed(&env, Path::new("/etc/passwd"), a));
        }
        // 回合内「仅这次」
        let grants = vec![(PathBuf::from("/tmp/once"), Mode::Create)];
        let env = CheckEnv { grants: &grants, ..env };
        assert!(allowed(&env, Path::new("/tmp/once/sub/x.txt"), Create));
        assert!(!allowed(&env, Path::new("/tmp/once/x.txt"), Modify), "仅这次也按档");
    }

    #[test]
    fn merged_absorbs_covered_lower_entries_and_keeps_higher() {
        // 授权父目录(read):同档/低档子孙合并掉,更高档子条目保留(绝不降级)
        let list = vec![
            entry("/e/nas/movies", Mode::Read),
            entry("/e/nas/music", Mode::Full),
            entry("/other", Mode::Read),
        ];
        let out = merged(list, Path::new("/e/nas"), Mode::Read);
        let paths: Vec<&str> = out.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["/e/nas", "/e/nas/music", "/other"]);
        // 升档父目录:full 吃掉全部子孙
        let out2 = merged(out, Path::new("/e/nas"), Mode::Full);
        let paths2: Vec<&str> = out2.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths2, vec!["/e/nas", "/other"]);
        assert_eq!(out2[0].mode, Mode::Full);
        // 同路径重加 = 改档(不重复)
        let out3 = merged(out2, Path::new("/e/nas"), Mode::Read);
        assert_eq!(out3.iter().filter(|e| e.path == "/e/nas").count(), 1);
        assert_eq!(out3.iter().find(|e| e.path == "/e/nas").unwrap().mode, Mode::Read);
    }

    #[test]
    fn dedup_covered_merges_batch_dirs() {
        let dirs = vec![
            PathBuf::from("/a/b/c"),
            PathBuf::from("/a/b"),
            PathBuf::from("/a/b/d"),
            PathBuf::from("/x"),
            PathBuf::from("/a/b"), // 重复
        ];
        let out = dedup_covered(dirs);
        assert_eq!(out, vec![PathBuf::from("/a/b"), PathBuf::from("/x")]);
    }

    #[test]
    fn normalize_handles_dots_and_nonexistent_tail() {
        let tmp = std::env::temp_dir();
        let raw = format!("{}/lw-guard-none/../lw-guard-x/./sub", tmp.to_string_lossy());
        let n = normalize_for_check(&raw);
        assert!(n.to_string_lossy().ends_with("lw-guard-x/sub") || n.to_string_lossy().ends_with("lw-guard-x\\sub"));
        assert!(!n.to_string_lossy().contains(".."), "`..` 必须被吃掉");
        // `~` 展开(expand_home 同源)
        let home = dirs::home_dir().unwrap();
        assert!(dir_covers(&home, &normalize_for_check("~/somewhere/deep")));
    }

    #[test]
    fn scopes_persist_merge_and_remove_via_store() {
        let dir = std::env::temp_dir().join(format!("lw-guard-store-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("t.db"));
        let store = Store::open(&dir.join("t.db")).unwrap();
        assert!(load_scopes(&store).is_empty());
        // 用真实存在的临时目录(normalize 会 canonicalize)
        let nas = dir.join("nas");
        let movies = nas.join("movies");
        std::fs::create_dir_all(&movies).unwrap();
        upsert_scope(&store, &movies.to_string_lossy(), Mode::Read).unwrap();
        upsert_scope(&store, &nas.to_string_lossy(), Mode::Create).unwrap();
        let list = load_scopes(&store);
        assert_eq!(list.len(), 1, "read 子目录被 create 父目录合并:{list:?}");
        assert_eq!(list[0].mode, Mode::Create);
        let left = remove_scope(&store, &nas.to_string_lossy()).unwrap();
        assert!(left.is_empty());
    }

    fn e2e_ctx(tag: &str, reply: Option<crate::confirm::ConfirmReply>) -> super::super::ToolCtx {
        let dir = std::env::temp_dir().join(format!("lw-guard-e2e-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("t.db"));
        let store = Store::open(&dir.join("t.db")).unwrap();
        store.users.ensure_default_user().unwrap();
        let confirm = reply.map(|r| {
            let bus = crate::bus::Bus::new();
            let confirmer = crate::confirm::Confirmer::new(bus.clone(), store.clone());
            let mut rx = bus.subscribe();
            let c2 = confirmer.clone();
            tokio::spawn(async move {
                while let Ok(ev) = rx.recv().await {
                    if let crate::bus::AppEvent::Confirm(card) = ev {
                        if card.state == "pending" {
                            c2.resolve(card.id, r, "desktop");
                        }
                    }
                }
            });
            confirmer
        });
        super::super::ToolCtx {
            user_id: 1,
            conv_id: 1,
            media: crate::media::MediaRuntime::detached(store.clone()),
            store,
            web: None,
            voice: None,
            confirm,
            grants: Default::default(),
            agent: None,
        }
    }

    /// 圈外测试路径:主目录下的假目录(home 不在免闸区;不真建、纯判定)。
    fn outside_dir(tag: &str) -> String {
        dirs::home_dir()
            .unwrap()
            .join(format!("lw-guard-outside-{tag}"))
            .join("sub")
            .to_string_lossy()
            .into_owned()
    }

    #[tokio::test]
    async fn ensure_allow_always_persists_scope() {
        let ctx = e2e_ctx("always", Some(crate::confirm::ConfirmReply::AllowAlways));
        let p = outside_dir("always");
        ensure(&ctx, Access::Create, &[p.clone()]).await.expect("一直允许 → 放行");
        // 入表 = 下次不问(免确认通道也过)
        let list = load_scopes(&ctx.store);
        assert_eq!(list.len(), 1, "{list:?}");
        assert_eq!(list[0].mode, Mode::Create);
        // 同目录更高档仍要问(表里只有 create):没有确认者的话会拒
        let ctx2 = e2e_ctx("always2", None);
        // 换一个 store 不共享;直接用原 ctx 验二次调用零弹卡(confirmer 只答一次也无妨:
        // 已在圈内根本不会 ask)
        ensure(&ctx, Access::Read, &[p.clone()]).await.expect("已入表,读也过");
        ensure(&ctx, Access::Create, &[p]).await.expect("已入表,再存也过");
        drop(ctx2);
    }

    #[tokio::test]
    async fn ensure_once_grants_turn_and_deny_remembers() {
        // 仅这次:放行且不落表
        let ctx = e2e_ctx("once", Some(crate::confirm::ConfirmReply::AllowOnce));
        let p = outside_dir("once");
        ensure(&ctx, Access::Read, &[p.clone()]).await.expect("仅这次 → 放行");
        assert!(load_scopes(&ctx.store).is_empty(), "仅这次不落表");
        ensure(&ctx, Access::Read, &[p]).await.expect("本回合内同目录同档不再问(grants)");

        // 拒:bail + 本回合内同目录不再弹(直接拒,话术带「已拒绝过」)
        let ctx = e2e_ctx("deny", Some(crate::confirm::ConfirmReply::Deny));
        let p = outside_dir("deny");
        let err = ensure(&ctx, Access::Delete, &[p.clone()]).await.unwrap_err();
        assert!(err.to_string().contains("没有允许"), "{err:#}");
        let err2 = ensure(&ctx, Access::Delete, &[p]).await.unwrap_err();
        assert!(err2.to_string().contains("已经拒绝过"), "{err2:#}");

        // 没有确认通道 = 指路设置页
        let ctx = e2e_ctx("noconfirm", None);
        let err = ensure(&ctx, Access::Read, &[outside_dir("nc")]).await.unwrap_err();
        assert!(err.to_string().contains("能碰的文件夹"), "{err:#}");
    }

    #[test]
    fn grants_once_and_deny_memory() {
        let g = Grants::default();
        g.add_allow(PathBuf::from("/tmp/a"), Mode::Create);
        assert_eq!(g.allows_snapshot().len(), 1);
        g.add_deny(PathBuf::from("/tmp/b"), Mode::Read);
        // 拒过 read:read 与更高档请求都不再弹
        assert!(g.denied_before(Path::new("/tmp/b/sub"), Mode::Read));
        assert!(g.denied_before(Path::new("/tmp/b"), Mode::Full));
        // 拒过 full 不挡 read 请求
        let g2 = Grants::default();
        g2.add_deny(PathBuf::from("/tmp/c"), Mode::Full);
        assert!(!g2.denied_before(Path::new("/tmp/c"), Mode::Read));
        assert!(g2.denied_before(Path::new("/tmp/c"), Mode::Full));
    }
}
