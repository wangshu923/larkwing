//! 能力轴:文件(读 + 写,正交原语)。配合任务需知里的目录,模型自行组合出
//! "找到电影并播放""把这些歌按歌手归类""记个清单"——不造 local_media_search/organize_media
//! 这类任务形工具(宪法 §5 正交纪律)。
//! 读类:fs_list / fs_find(封顶条数/深度)/ fs_read_text。
//! 写类(PLAN §9 文件能力,2026-06-15):move/copy/mkdir/trash/write/append/edit/undo;
//! 底层执行 + 撤销/重做在 crate::files,记账在 store::fsops。功能性、不覆盖、可撤销,
//! **不做安全承诺**(用户准则)。

use std::path::{Path, PathBuf};

use anyhow::Context;
use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolRisk, ToolSpec};
use crate::files;
use crate::store::Store;

/// 单层列目录上限。
const LIST_MAX: usize = 200;
/// 递归找文件:深度与结果上限。
const FIND_MAX_DEPTH: usize = 4;
const FIND_MAX_RESULTS: usize = 50;

fn human_size(bytes: u64) -> String {
    if bytes >= 1 << 30 {
        format!("{:.1}GB", bytes as f64 / (1u64 << 30) as f64)
    } else if bytes >= 1 << 20 {
        format!("{:.0}MB", bytes as f64 / (1u64 << 20) as f64)
    } else {
        format!("{}KB", bytes >> 10)
    }
}

fn hidden(name: &str) -> bool {
    name.starts_with('.') || name == "$RECYCLE.BIN" || name == "System Volume Information"
}

// ---------------------------------------------------------------------------
// fs_list
// ---------------------------------------------------------------------------

pub(super) struct FsList {
    spec: ToolSpec,
}

impl FsList {
    pub(super) fn new() -> FsList {
        FsList {
            spec: ToolSpec {
                name: "fs_list",
                description: "列出一个文件夹里有什么(单层)。配合任务需知里登记的目录用,\
                              比如需知说电影在某个文件夹,就先列出来再挑。返回 名字/大小,\
                              文件夹名以 / 结尾。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "文件夹绝对路径,如 D:\\Movies 或 \\\\nas\\film 或 /Users/me/Movies"
                        }
                    },
                    "required": ["path"]
                }),
                timeout: std::time::Duration::from_secs(15),
                ui_key: "tool.fs_list",
            },
        }
    }
}

#[async_trait]
impl Tool for FsList {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value, _ctx: &ToolCtx) -> anyhow::Result<String> {
        let path = args
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .context("缺少 path 参数")?
            .to_string();
        tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
            let dir = Path::new(&path);
            anyhow::ensure!(dir.is_dir(), "{path} 不是文件夹或不存在");
            let mut dirs: Vec<String> = Vec::new();
            let mut files: Vec<String> = Vec::new();
            for entry in std::fs::read_dir(dir)?.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if hidden(&name) {
                    continue;
                }
                match entry.metadata() {
                    Ok(md) if md.is_dir() => dirs.push(format!("{name}/")),
                    Ok(md) => files.push(format!("{name} ({})", human_size(md.len()))),
                    Err(_) => files.push(name),
                }
            }
            dirs.sort();
            files.sort();
            let total = dirs.len() + files.len();
            let mut lines: Vec<String> = dirs.into_iter().chain(files).take(LIST_MAX).collect();
            if total > LIST_MAX {
                lines.push(format!("…(共 {total} 项,只列出前 {LIST_MAX} 项)"));
            }
            if lines.is_empty() {
                return Ok("(空文件夹)".into());
            }
            Ok(lines.join("\n"))
        })
        .await
        .context("列目录任务挂了")?
    }
}

// ---------------------------------------------------------------------------
// fs_find
// ---------------------------------------------------------------------------

pub(super) struct FsFind {
    spec: ToolSpec,
}

impl FsFind {
    pub(super) fn new() -> FsFind {
        FsFind {
            spec: ToolSpec {
                name: "fs_find",
                description: "在一个目录树里按 glob 模式找文件(不分大小写,递归几层)。\
                              pattern 支持 * 和 ?:如 *佩奇*.mp4、*.mp3;含 / 时按相对路径匹配\
                              (如 kids/*.mp4);纯关键词(无通配符)自动当 *关键词* 用。\
                              知道想找什么时比逐层 fs_list 快,返回绝对路径列表。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "root": {
                            "type": "string",
                            "description": "从哪个文件夹开始找(绝对路径)"
                        },
                        "pattern": {
                            "type": "string",
                            "description": "glob 模式或关键词,如「*佩奇*.mp4」「晴天」"
                        }
                    },
                    "required": ["root", "pattern"]
                }),
                timeout: std::time::Duration::from_secs(30),
                ui_key: "tool.fs_find",
            },
        }
    }
}

/// 匹配口径:pattern 含 `/` → 对 root 起的相对路径(统一 `/` 分隔)匹配;否则只对文件名。
/// 不分大小写;无通配符的纯关键词包成 `*关键词*`(模型省心)。
struct Matcher {
    pattern: glob::Pattern,
    against_path: bool,
}

impl Matcher {
    fn new(raw: &str) -> anyhow::Result<Matcher> {
        let raw = raw.trim();
        let wrapped;
        let effective = if raw.contains(['*', '?', '[']) {
            raw
        } else {
            wrapped = format!("*{raw}*");
            &wrapped
        };
        Ok(Matcher {
            pattern: glob::Pattern::new(effective)
                .with_context(|| format!("glob 模式不合法: {effective}"))?,
            against_path: effective.contains('/'),
        })
    }

    fn hit(&self, name: &str, rel_path: &str) -> bool {
        let opts = glob::MatchOptions {
            case_sensitive: false,
            require_literal_separator: false,
            require_literal_leading_dot: false,
        };
        let target = if self.against_path { rel_path } else { name };
        self.pattern.matches_with(target, opts)
    }
}

fn walk(dir: &Path, root: &Path, matcher: &Matcher, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > FIND_MAX_DEPTH || out.len() >= FIND_MAX_RESULTS {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if out.len() >= FIND_MAX_RESULTS {
            return;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if hidden(&name) {
            continue;
        }
        let path = entry.path();
        let is_dir = entry.metadata().map(|m| m.is_dir()).unwrap_or(false);
        if is_dir {
            walk(&path, root, matcher, depth + 1, out);
        } else {
            let rel = path
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| name.clone());
            if matcher.hit(&name, &rel) {
                out.push(path);
            }
        }
    }
}

#[async_trait]
impl Tool for FsFind {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value, _ctx: &ToolCtx) -> anyhow::Result<String> {
        let root = args
            .get("root")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .context("缺少 root 参数")?
            .to_string();
        let raw = args
            .get("pattern")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .context("缺少 pattern 参数")?
            .to_string();
        let matcher = Matcher::new(&raw)?;
        tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
            let dir = Path::new(&root);
            anyhow::ensure!(dir.is_dir(), "{root} 不是文件夹或不存在");
            let mut out = Vec::new();
            walk(dir, dir, &matcher, 0, &mut out);
            if out.is_empty() {
                return Ok(format!("在 {root} 里没找到匹配「{raw}」的文件"));
            }
            let truncated = out.len() >= FIND_MAX_RESULTS;
            let mut lines: Vec<String> =
                out.into_iter().map(|p| p.to_string_lossy().to_string()).collect();
            if truncated {
                lines.push(format!("…(已达 {FIND_MAX_RESULTS} 条上限,可换更具体的模式)"));
            }
            Ok(lines.join("\n"))
        })
        .await
        .context("找文件任务挂了")?
    }
}

// ===========================================================================
// 写类原语(PLAN §9 文件能力):move/copy/mkdir/trash/write/append/edit/undo。
// 都是「能力轴正交原语」(宪法 §5),不造任务形工具;模型 + 需知目录自行组合出
// 「整理音乐」「把这几个文件归一起」「记个清单」等。底层执行在 crate::files,
// 这里只做参数解析 + 批量汇总 + 落操作记录(store::fsops)。
// 量是一等约束(用户提醒):批量原生 + 结果只汇总只点名失败(token 不随条数爆)。
// ===========================================================================

/// 单次工具调用的条数上限(防单次参数过大/单轮过久;超出部分如实告知"再喊我接着弄")。
const BATCH_MAX: usize = 300;
/// fs_read_text 返回上限(字符):够模型读文档/清单,超了截断并标注。
const READ_TEXT_MAX_CHARS: usize = 40_000;

/// 顶层或数组项里取一个非空字符串字段。
fn arg_str(v: &serde_json::Value, key: &str) -> anyhow::Result<String> {
    v.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .with_context(|| format!("缺少 {key} 参数"))
}

/// 取 `key` 下的 `[{src, dst}, …]`。
fn arg_pairs(args: &serde_json::Value, key: &str) -> anyhow::Result<Vec<(String, String)>> {
    let arr = args.get(key).and_then(|v| v.as_array()).with_context(|| format!("缺少 {key}(应为数组)"))?;
    let mut out = Vec::with_capacity(arr.len());
    for it in arr {
        out.push((arg_str(it, "src")?, arg_str(it, "dst")?));
    }
    anyhow::ensure!(!out.is_empty(), "{key} 是空的");
    Ok(out)
}

/// 取 `key` 下的字符串数组(空项跳过)。
fn arg_paths(args: &serde_json::Value, key: &str) -> anyhow::Result<Vec<String>> {
    let arr = args.get(key).and_then(|v| v.as_array()).with_context(|| format!("缺少 {key}(应为数组)"))?;
    let out: Vec<String> = arr
        .iter()
        .filter_map(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    anyhow::ensure!(!out.is_empty(), "{key} 是空的");
    Ok(out)
}

/// 把一批执行结果落库(成功项进 fsops 一行)+ 汇总成给模型的短文本(只点名失败)。
fn finish_batch(
    store: &Store,
    user_id: i64,
    kind: &str,
    verb: &str,
    results: Vec<Result<files::FsOpItem, String>>,
    overflow: usize,
) -> anyhow::Result<String> {
    let mut items = Vec::new();
    let mut fails = Vec::new();
    for r in results {
        match r {
            Ok(it) => items.push(it),
            Err(e) => fails.push(e),
        }
    }
    let n = items.len();
    if n > 0 {
        let json = serde_json::to_string(&items).context("序列化操作记录失败")?;
        store.fsops.record(user_id, kind, &json, n as i64).context("操作记录落库失败")?;
    }
    let mut msg = format!("{verb}了 {n} 个");
    if !fails.is_empty() {
        let shown: Vec<String> = fails.iter().take(8).cloned().collect();
        msg.push_str(&format!(";{} 个没成功:{}", fails.len(), shown.join(" | ")));
    }
    if overflow > 0 {
        msg.push_str(&format!(";另有 {overflow} 个这次没处理(一次太多),需要的话再喊我接着弄"));
    }
    Ok(msg)
}

// ---------------------------------------------------------------------------
// fs_read_text(只读,Safe)
// ---------------------------------------------------------------------------

pub(super) struct FsReadText {
    spec: ToolSpec,
}

impl FsReadText {
    pub(super) fn new() -> FsReadText {
        FsReadText {
            spec: ToolSpec {
                name: "fs_read_text",
                description: "读一个文本文件的内容拿来看(总结文档、念清单、看说明书之类)。\
                              只读文本;二进制或太大的会被拒或只给前一部分。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "文件绝对路径" }
                    },
                    "required": ["path"]
                }),
                timeout: std::time::Duration::from_secs(15),
                ui_key: "tool.fs_read_text",
            },
        }
    }
}

#[async_trait]
impl Tool for FsReadText {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value, _ctx: &ToolCtx) -> anyhow::Result<String> {
        let path = arg_str(&args, "path")?;
        tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
            let p = Path::new(&path);
            anyhow::ensure!(p.is_file(), "{path} 不是文件或不存在");
            let full = std::fs::read_to_string(p)
                .map_err(|_| anyhow::anyhow!("这看起来不是文本文件,读不了内容"))?;
            let mut out: String = full.chars().take(READ_TEXT_MAX_CHARS).collect();
            if out.len() < full.len() {
                out.push_str("\n…(文件较长,只读了前一部分)");
            }
            if out.is_empty() {
                return Ok("(空文件)".into());
            }
            Ok(out)
        })
        .await
        .context("读文件任务挂了")?
    }
}

// ---------------------------------------------------------------------------
// fs_move / fs_copy(批量,Mutating)
// ---------------------------------------------------------------------------

pub(super) struct FsMove {
    spec: ToolSpec,
}

impl FsMove {
    pub(super) fn new() -> FsMove {
        FsMove {
            spec: ToolSpec {
                name: "fs_move",
                description: "移动或改名文件/文件夹,可一次批量(整理文件夹就用它)。每条 src=现在的位置,\
                              dst=去处:dst 是已存在的文件夹就移进去(保留原名),是完整新路径就按它(=顺便改名)。\
                              同名不覆盖(自动加「 (2)」)。改错了可以让我撤销。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "moves": {
                            "type": "array",
                            "description": "一批移动,每条 {src, dst}",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "src": { "type": "string", "description": "源绝对路径" },
                                    "dst": { "type": "string", "description": "目标文件夹或完整新路径" }
                                },
                                "required": ["src", "dst"]
                            }
                        }
                    },
                    "required": ["moves"]
                }),
                timeout: std::time::Duration::from_secs(60),
                ui_key: "tool.fs_move",
            },
        }
    }
}

#[async_trait]
impl Tool for FsMove {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Mutating
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let moves = arg_pairs(&args, "moves")?;
        let store = ctx.store.clone();
        let user_id = ctx.user_id;
        tokio::task::spawn_blocking(move || {
            let overflow = moves.len().saturating_sub(BATCH_MAX);
            let results: Vec<_> = moves
                .into_iter()
                .take(BATCH_MAX)
                .map(|(src, dst)| {
                    files::move_one(Path::new(&src), Path::new(&dst))
                        .map_err(|e| format!("{src} → {dst}:{e:#}"))
                })
                .collect();
            finish_batch(&store, user_id, "move", "移动", results, overflow)
        })
        .await
        .context("移动任务挂了")?
    }
}

pub(super) struct FsCopy {
    spec: ToolSpec,
}

impl FsCopy {
    pub(super) fn new() -> FsCopy {
        FsCopy {
            spec: ToolSpec {
                name: "fs_copy",
                description: "复制文件/文件夹(原件保留),可批量。dst 规则同移动:是文件夹就复制进去,\
                              是完整路径就按这个名存。不覆盖同名。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "copies": {
                            "type": "array",
                            "description": "一批复制,每条 {src, dst}",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "src": { "type": "string" },
                                    "dst": { "type": "string" }
                                },
                                "required": ["src", "dst"]
                            }
                        }
                    },
                    "required": ["copies"]
                }),
                timeout: std::time::Duration::from_secs(60),
                ui_key: "tool.fs_copy",
            },
        }
    }
}

#[async_trait]
impl Tool for FsCopy {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Mutating
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let copies = arg_pairs(&args, "copies")?;
        let store = ctx.store.clone();
        let user_id = ctx.user_id;
        tokio::task::spawn_blocking(move || {
            let overflow = copies.len().saturating_sub(BATCH_MAX);
            let results: Vec<_> = copies
                .into_iter()
                .take(BATCH_MAX)
                .map(|(src, dst)| {
                    files::copy_one(Path::new(&src), Path::new(&dst))
                        .map_err(|e| format!("{src} → {dst}:{e:#}"))
                })
                .collect();
            finish_batch(&store, user_id, "copy", "复制", results, overflow)
        })
        .await
        .context("复制任务挂了")?
    }
}

// ---------------------------------------------------------------------------
// fs_mkdir / fs_trash(批量,Mutating)
// ---------------------------------------------------------------------------

pub(super) struct FsMkdir {
    spec: ToolSpec,
}

impl FsMkdir {
    pub(super) fn new() -> FsMkdir {
        FsMkdir {
            spec: ToolSpec {
                name: "fs_mkdir",
                description: "新建文件夹(可多层、可批量)。整理前先把分类文件夹建好再往里移。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "paths": {
                            "type": "array",
                            "description": "要新建的文件夹绝对路径(可多个)",
                            "items": { "type": "string" }
                        }
                    },
                    "required": ["paths"]
                }),
                timeout: std::time::Duration::from_secs(15),
                ui_key: "tool.fs_mkdir",
            },
        }
    }
}

#[async_trait]
impl Tool for FsMkdir {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Mutating
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let paths = arg_paths(&args, "paths")?;
        let store = ctx.store.clone();
        let user_id = ctx.user_id;
        tokio::task::spawn_blocking(move || {
            let overflow = paths.len().saturating_sub(BATCH_MAX);
            let results: Vec<_> = paths
                .into_iter()
                .take(BATCH_MAX)
                .map(|p| files::mkdir_one(Path::new(&p)).map_err(|e| format!("{p}:{e:#}")))
                .collect();
            finish_batch(&store, user_id, "mkdir", "新建", results, overflow)
        })
        .await
        .context("建文件夹任务挂了")?
    }
}

pub(super) struct FsTrash {
    spec: ToolSpec,
}

impl FsTrash {
    pub(super) fn new() -> FsTrash {
        FsTrash {
            spec: ToolSpec {
                name: "fs_trash",
                description: "把文件/文件夹删到系统回收站(之后能在回收站找回,不是永久删除),可批量。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "paths": {
                            "type": "array",
                            "description": "要删的绝对路径(可多个)",
                            "items": { "type": "string" }
                        }
                    },
                    "required": ["paths"]
                }),
                timeout: std::time::Duration::from_secs(60),
                ui_key: "tool.fs_trash",
            },
        }
    }
}

#[async_trait]
impl Tool for FsTrash {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Mutating
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let paths = arg_paths(&args, "paths")?;
        let store = ctx.store.clone();
        let user_id = ctx.user_id;
        tokio::task::spawn_blocking(move || {
            let overflow = paths.len().saturating_sub(BATCH_MAX);
            let results: Vec<_> = paths
                .into_iter()
                .take(BATCH_MAX)
                .map(|p| files::trash_one(Path::new(&p)).map_err(|e| format!("{p}:{e:#}")))
                .collect();
            finish_batch(&store, user_id, "trash", "删除", results, overflow)
        })
        .await
        .context("删除任务挂了")?
    }
}

// ---------------------------------------------------------------------------
// fs_write_text / fs_append / fs_edit(单文件文本管理,Mutating)
// ---------------------------------------------------------------------------

pub(super) struct FsWriteText {
    spec: ToolSpec,
}

impl FsWriteText {
    pub(super) fn new() -> FsWriteText {
        FsWriteText {
            spec: ToolSpec {
                name: "fs_write_text",
                description: "新建或整体写入一个文本文件(给完整内容)。已存在会被这份新内容替换 —— \
                              适合保存一份清单/便条/整理结果。要往现有文件加内容用 fs_append,改某处用 fs_edit。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "文件绝对路径" },
                        "content": { "type": "string", "description": "要写入的完整文本" }
                    },
                    "required": ["path", "content"]
                }),
                timeout: std::time::Duration::from_secs(15),
                ui_key: "tool.fs_write_text",
            },
        }
    }
}

#[async_trait]
impl Tool for FsWriteText {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Mutating
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let path = arg_str(&args, "path")?;
        let content = args
            .get("content")
            .and_then(serde_json::Value::as_str)
            .context("缺少 content 参数")?
            .to_string();
        let store = ctx.store.clone();
        let user_id = ctx.user_id;
        tokio::task::spawn_blocking(move || {
            let r =
                files::write_text(Path::new(&path), &content).map_err(|e| format!("{path}:{e:#}"));
            finish_batch(&store, user_id, "write", "写入", vec![r], 0)
        })
        .await
        .context("写文件任务挂了")?
    }
}

pub(super) struct FsAppend {
    spec: ToolSpec,
}

impl FsAppend {
    pub(super) fn new() -> FsAppend {
        FsAppend {
            spec: ToolSpec {
                name: "fs_append",
                description: "往文本文件末尾追加内容(文件不存在就新建)。适合「清单加一行」「日记记一笔」,\
                              只发新增的部分、不必重写全文。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "文件绝对路径" },
                        "text": { "type": "string", "description": "要追加到末尾的文本(需要换行自己带 \\n)" }
                    },
                    "required": ["path", "text"]
                }),
                timeout: std::time::Duration::from_secs(15),
                ui_key: "tool.fs_append",
            },
        }
    }
}

#[async_trait]
impl Tool for FsAppend {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Mutating
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let path = arg_str(&args, "path")?;
        let text = args
            .get("text")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .context("缺少 text 参数")?
            .to_string();
        let store = ctx.store.clone();
        let user_id = ctx.user_id;
        tokio::task::spawn_blocking(move || {
            let r =
                files::append_text(Path::new(&path), &text).map_err(|e| format!("{path}:{e:#}"));
            finish_batch(&store, user_id, "append", "追加", vec![r], 0)
        })
        .await
        .context("追加任务挂了")?
    }
}

pub(super) struct FsEdit {
    spec: ToolSpec,
}

impl FsEdit {
    pub(super) fn new() -> FsEdit {
        FsEdit {
            spec: ToolSpec {
                name: "fs_edit",
                description: "改文本文件里的某处:把 find 这段原文换成 replace。find 必须在文件里只出现一次\
                              (给一段独特的原文),否则会让你换更准的再来。适合改清单里的一项、更正一句话。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "文件绝对路径" },
                        "find": { "type": "string", "description": "要被替换的原文(需在文件里唯一)" },
                        "replace": { "type": "string", "description": "换成的新内容(可为空 = 删掉那段)" }
                    },
                    "required": ["path", "find", "replace"]
                }),
                timeout: std::time::Duration::from_secs(15),
                ui_key: "tool.fs_edit",
            },
        }
    }
}

#[async_trait]
impl Tool for FsEdit {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Mutating
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let path = arg_str(&args, "path")?;
        let find = arg_str(&args, "find")?;
        let replace = args.get("replace").and_then(serde_json::Value::as_str).unwrap_or("").to_string();
        let store = ctx.store.clone();
        let user_id = ctx.user_id;
        tokio::task::spawn_blocking(move || {
            let r = files::edit_text(Path::new(&path), &find, &replace)
                .map_err(|e| format!("{path}:{e:#}"));
            finish_batch(&store, user_id, "edit", "修改", vec![r], 0)
        })
        .await
        .context("改文件任务挂了")?
    }
}

// ---------------------------------------------------------------------------
// fs_undo(撤销最近一批,Mutating)
// ---------------------------------------------------------------------------

pub(super) struct FsUndo {
    spec: ToolSpec,
}

impl FsUndo {
    pub(super) fn new() -> FsUndo {
        FsUndo {
            spec: ToolSpec {
                name: "fs_undo",
                description: "撤销最近一次文件操作(把刚才的移动/改名/复制/删除/写入退回去)。\
                              用户说「撤销」「还原」「弄错了退回去」时用。",
                parameters: serde_json::json!({ "type": "object", "properties": {} }),
                timeout: std::time::Duration::from_secs(60),
                ui_key: "tool.fs_undo",
            },
        }
    }
}

#[async_trait]
impl Tool for FsUndo {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Mutating
    }

    async fn run(&self, _args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let store = ctx.store.clone();
        let user_id = ctx.user_id;
        tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
            let Some(row) = store.fsops.latest(user_id, "applied")? else {
                return Ok("最近没有可以撤销的文件操作".into());
            };
            let items: Vec<files::FsOpItem> =
                serde_json::from_str(&row.ops).context("操作记录读不出来")?;
            let r = files::undo_batch(&items);
            store.fsops.set_state(row.id, "undone")?;
            let mut msg = format!("撤销好了,还原了 {} 项", r.done);
            if r.skipped > 0 {
                msg.push_str(&format!(",有 {} 项没能还原(可能文件又被动过,或回收站已清空)", r.skipped));
            }
            Ok(msg)
        })
        .await
        .context("撤销任务挂了")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::MediaRuntime;
    use crate::store::Store;

    fn ctx_and_dir(tag: &str) -> (ToolCtx, PathBuf) {
        let dir = std::env::temp_dir().join(format!("lw-fs-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("kids")).unwrap();
        std::fs::write(dir.join("电影A.mp4"), vec![0u8; 2048]).unwrap();
        std::fs::write(dir.join("kids/小猪佩奇01.mp4"), b"x").unwrap();
        std::fs::write(dir.join(".hidden"), b"x").unwrap();
        let store = Store::open(&dir.join("t.db")).unwrap();
        let ctx =
            ToolCtx { user_id: 1, conv_id: 1, media: MediaRuntime::detached(store.clone()), store };
        (ctx, dir)
    }

    #[tokio::test]
    async fn list_shows_dirs_first_and_skips_hidden() {
        let (ctx, dir) = ctx_and_dir("list");
        let out = FsList::new()
            .run(serde_json::json!({"path": dir.to_string_lossy()}), &ctx)
            .await
            .unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "kids/");
        assert!(lines.iter().any(|l| l.starts_with("电影A.mp4 (2KB")));
        assert!(!out.contains(".hidden"));
        // 不存在的路径 = 错误观察
        assert!(FsList::new()
            .run(serde_json::json!({"path": dir.join("nope").to_string_lossy()}), &ctx)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn find_supports_glob_keyword_and_path_patterns() {
        let (ctx, dir) = ctx_and_dir("find");
        let root = dir.to_string_lossy().to_string();

        // 纯关键词 = 自动包成 *关键词*
        let kw = FsFind::new()
            .run(serde_json::json!({"root": root, "pattern": "佩奇"}), &ctx)
            .await
            .unwrap();
        assert!(kw.contains("小猪佩奇01.mp4"));

        // 显式 glob(大小写不敏感:MP4 也命中 .mp4)
        let g = FsFind::new()
            .run(serde_json::json!({"root": root, "pattern": "*佩奇*.MP4"}), &ctx)
            .await
            .unwrap();
        assert!(g.contains("小猪佩奇01.mp4"));

        // 含 / 的模式按相对路径匹配(限定子目录)
        let p = FsFind::new()
            .run(serde_json::json!({"root": root, "pattern": "kids/*.mp4"}), &ctx)
            .await
            .unwrap();
        assert!(p.contains("小猪佩奇01.mp4"));
        assert!(!p.contains("电影A.mp4"), "根目录的不在 kids/ 模式里");

        let none = FsFind::new()
            .run(serde_json::json!({"root": root, "pattern": "*海绵宝宝*"}), &ctx)
            .await
            .unwrap();
        assert!(none.contains("没找到"));

        // 坏 glob = 清晰错误
        assert!(FsFind::new()
            .run(serde_json::json!({"root": root, "pattern": "[bad"}), &ctx)
            .await
            .is_err());
    }
}
