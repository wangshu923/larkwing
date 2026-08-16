//! 能力轴:文件(读 + 写,正交原语)。配合任务需知里的目录,模型自行组合出
//! "找到电影并播放""把这些歌按歌手归类""记个清单"——不造 local_media_search/organize_media
//! 这类任务形工具(宪法 §5 正交纪律)。
//! 读类:fs_list / fs_find(封顶条数/深度)/ fs_read_text。三者都是「一页 + 报总数 +
//! 给续读起点(offset)」同一形态 —— 截断了得有路把后面看完,不然模型只能瞎猜。
//! 写类(PLAN §9 文件能力,2026-06-15):move/copy/mkdir/trash/write/append/edit/undo;
//! 底层执行 + 撤销/重做在 crate::files,记账在 store::fsops。功能性、不覆盖、可撤销,
//! **不做安全承诺**(用户准则)。

use std::path::{Path, PathBuf};

use anyhow::Context;
use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolRisk, ToolSpec};
use crate::files;
use crate::store::Store;

/// 单层列目录:一页多少项(超了报总数 + 给续读起点,fs_read_text 的 offset 同款形态)。
const LIST_MAX: usize = 200;
/// 递归找文件:深度上限 + 一页多少条。
const FIND_MAX_DEPTH: usize = 4;
const FIND_MAX_RESULTS: usize = 50;
/// 递归找文件的**扫描**上限(失控 backstop):要给出总数就不能再「凑够一页就停」,
/// 但也不能对着大盘无限走(工具 30s 超时)——扫到这个数就收手,并如实说「可能还有」。
const FIND_SCAN_MAX: usize = 1000;

pub(super) use crate::files::human_size; // 单源在 files.rs(media 下载话术也用),这里留旧名转发

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
                              比如需知说电影在某个文件夹,就先列出来再挑。返回 名字/大小/\
                              改动日期,文件夹名以 / 结尾。要看更细的属性(拍摄时间/时长)\
                              用 fs_stat。一次最多列 200 项,东西多会报总数并给续读起点,\
                              要接着看就带 offset 再调一次。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "文件夹绝对路径(盘符 D:\\…、UNC \\\\nas\\…、Unix /…;支持 ~ 开头 = 用户主目录)"
                        },
                        "offset": {
                            "type": "integer",
                            "description": "从第几项开始列(0 起,缺省 0)。上一次结果说「继续列带 offset=N」就填那个 N"
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

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let path = args
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(super::expand_home) // 「~/xxx」宽容展开(§4.4)
            .context("缺少 path 参数")?;
        super::guard::ensure(ctx, super::guard::Access::Read, std::slice::from_ref(&path)).await?;
        let offset = super::arg_u64(&args, "offset", 0) as usize;
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
                    Ok(md) => {
                        // 大小 · 改动日期(排序/找最近的显示级信息;细属性归 fs_stat)
                        let mut meta = human_size(md.len());
                        if let Ok(t) = md.modified() {
                            meta.push_str(&format!(" · {}", fmt_date(t)));
                        }
                        files.push(format!("{name} ({meta})"));
                    }
                    Err(_) => files.push(name),
                }
            }
            dirs.sort();
            files.sort();
            let total = dirs.len() + files.len();
            if total == 0 {
                return Ok("(空文件夹)".into());
            }
            // 分页:排好序 = 顺序稳定,offset 跨调用对得上(文件夹没被改动的话)。
            anyhow::ensure!(
                offset < total,
                "这个文件夹一共 {total} 项,offset={offset} 超出末尾——已经列完了"
            );
            let mut lines: Vec<String> =
                dirs.into_iter().chain(files).skip(offset).take(LIST_MAX).collect();
            let end = offset + lines.len();
            // 装不下、或本来就是翻页请求 → 报总数与位置(翻页时不报,模型分不清是不是到底了)
            if total > LIST_MAX || offset > 0 {
                // 总数一并给:模型据此判断是接着翻(offset)还是换个更细的路径
                lines.push(if end < total {
                    format!(
                        "…(共 {total} 项,这是第 {}-{end} 项;继续列带 offset={end})",
                        offset + 1
                    )
                } else {
                    format!("…(共 {total} 项,这是第 {}-{end} 项,已到末尾)", offset + 1)
                });
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
                              pattern 支持 * 和 ?:如 *动画*.mp4、*.mp3;含 / 时按相对路径匹配\
                              (如 某子目录/*.mp4);纯关键词(无通配符)自动当 *关键词* 用。\
                              知道想找什么时比逐层 fs_list 快,返回绝对路径列表(按路径排序)。\
                              一次最多返回 50 条,命中多会报总数并给续读起点:要接着看就带 \
                              offset 再调一次,总数大得离谱就换更具体的模式或更小的起点。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "root": {
                            "type": "string",
                            "description": "从哪个文件夹开始找(绝对路径;支持 ~ 开头 = 用户主目录)"
                        },
                        "pattern": {
                            "type": "string",
                            "description": "glob 模式或关键词(拿用户说的词组模式,别自己编)"
                        },
                        "offset": {
                            "type": "integer",
                            "description": "从第几条开始返回(0 起,缺省 0)。上一次结果说「继续带 offset=N」就填那个 N"
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

/// 收集命中(深度 ≤ FIND_MAX_DEPTH,总量 ≤ FIND_SCAN_MAX)。停在**扫描**上限而不是一页,
/// 才有总数可报、offset 才翻得动;扫满 = 调用方如实说「可能还有」。
fn walk(dir: &Path, root: &Path, matcher: &Matcher, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > FIND_MAX_DEPTH || out.len() >= FIND_SCAN_MAX {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if out.len() >= FIND_SCAN_MAX {
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

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let root = args
            .get("root")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(super::expand_home) // 「~/xxx」宽容展开(§4.4)
            .context("缺少 root 参数")?;
        super::guard::ensure(ctx, super::guard::Access::Read, std::slice::from_ref(&root)).await?;
        let raw = args
            .get("pattern")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .context("缺少 pattern 参数")?
            .to_string();
        let matcher = Matcher::new(&raw)?;
        let offset = super::arg_u64(&args, "offset", 0) as usize;
        tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
            let dir = Path::new(&root);
            anyhow::ensure!(dir.is_dir(), "{root} 不是文件夹或不存在");
            let mut out = Vec::new();
            walk(dir, dir, &matcher, 0, &mut out);
            if out.is_empty() {
                return Ok(format!("在 {root} 里没找到匹配「{raw}」的文件"));
            }
            // 排序 = 分页顺序稳定(read_dir 的原始顺序不保证,offset 会翻乱)
            out.sort();
            let total = out.len();
            let capped = total >= FIND_SCAN_MAX;
            anyhow::ensure!(
                offset < total,
                "一共找到 {total} 条,offset={offset} 超出末尾——已经列完了"
            );
            let mut lines: Vec<String> = out
                .into_iter()
                .skip(offset)
                .take(FIND_MAX_RESULTS)
                .map(|p| {
                    // 尾捎改动日期(「找最近下载的那个」直接可判;细属性归 fs_stat)
                    match std::fs::metadata(&p).and_then(|m| m.modified()) {
                        Ok(t) => format!("{} ({})", p.to_string_lossy(), fmt_date(t)),
                        Err(_) => p.to_string_lossy().to_string(),
                    }
                })
                .collect();
            let end = offset + lines.len();
            // 总数一并给:模型据此判断该翻页(offset)还是该换更具体的模式
            if capped {
                lines.push(format!(
                    "…(扫到 {FIND_SCAN_MAX} 条就停了、可能还有,这是第 {}-{end} 条;\
                     继续带 offset={end},或换更具体的模式/更小的起点)",
                    offset + 1
                ));
            } else if total > FIND_MAX_RESULTS || offset > 0 {
                lines.push(if end < total {
                    format!("…(共 {total} 条,这是第 {}-{end} 条;继续带 offset={end})", offset + 1)
                } else {
                    format!("…(共 {total} 条,这是第 {}-{end} 条,已到末尾)", offset + 1)
                });
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

/// 取一个**路径**字段:非空 + `~` 前缀宽容展开(`tools::expand_home`,§4.4)。
fn arg_path(v: &serde_json::Value, key: &str) -> anyhow::Result<String> {
    Ok(super::expand_home(&arg_str(v, key)?))
}

/// 取 `key` 下的 `[{src, dst}, …]`(src/dst 都是路径,`~` 展开)。
fn arg_pairs(args: &serde_json::Value, key: &str) -> anyhow::Result<Vec<(String, String)>> {
    let arr = args.get(key).and_then(|v| v.as_array()).with_context(|| format!("缺少 {key}(应为数组)"))?;
    let mut out = Vec::with_capacity(arr.len());
    for it in arr {
        out.push((arg_path(it, "src")?, arg_path(it, "dst")?));
    }
    anyhow::ensure!(!out.is_empty(), "{key} 是空的");
    Ok(out)
}

/// 取 `key` 下的**路径**字符串数组(空项跳过,`~` 展开)。
fn arg_paths(args: &serde_json::Value, key: &str) -> anyhow::Result<Vec<String>> {
    let arr = args.get(key).and_then(|v| v.as_array()).with_context(|| format!("缺少 {key}(应为数组)"))?;
    let out: Vec<String> = arr
        .iter()
        .filter_map(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(super::expand_home)
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
                description: "读一个文件的内容拿来看(总结文档、念清单、看说明书、看账单表之类)。\
                              支持纯文本/源码,以及 Word(.docx)、PowerPoint(.pptx)、\
                              Excel(.xlsx,会转成表格文本)、PDF(文字版;扫描成图的 PDF 读不出字)。\
                              老格式 .doc/.xls/.ppt 读不了;太长会分段,结果末尾标注\
                              「继续读带 offset=N」,带上 offset 再调就接着读。只看大小/时间/\
                              拍摄信息这类属性、不读内容的,用 fs_stat。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "文件绝对路径" },
                        "offset": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "长文续读:从第几个字开始(上次结果末尾给出的数);默认 0 从头读"
                        }
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

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let path = arg_path(&args, "path")?;
        super::guard::ensure(ctx, super::guard::Access::Read, std::slice::from_ref(&path)).await?;
        let offset = super::arg_u64(&args, "offset", 0) as usize;
        tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
            let p = Path::new(&path);
            anyhow::ensure!(p.is_file(), "{path} 不是文件或不存在");
            // 文档类(docx/pptx/xlsx/pdf)走抽取器(同聊天上传那条路);纯文本直读。
            let lower = path.to_ascii_lowercase();
            let full = if lower.ends_with(".docx")
                || lower.ends_with(".pptx")
                || lower.ends_with(".xlsx")
                || lower.ends_with(".pdf")
            {
                let bytes = std::fs::read(p).map_err(|_| anyhow::anyhow!("读不了这个文件"))?;
                crate::attach::extract_doc_text(&path, "", &bytes).ok_or_else(|| {
                    anyhow::anyhow!("这个文档没抽出文字(可能是扫描成图的 PDF,或是老格式 .doc/.xls/.ppt)")
                })?
            } else {
                std::fs::read_to_string(p)
                    .map_err(|_| anyhow::anyhow!("这看起来不是文本文件,读不了内容"))?
            };
            // 续读 = 换切片起点重抽一遍(文件本身就是持久层,不另建抽取缓存);
            // 字符计数口径与 web_fetch 一致(CJK 安全)。
            let total = full.chars().count();
            if total == 0 {
                anyhow::ensure!(offset == 0, "这是个空文件,没有第 {offset} 字");
                return Ok("(空文件)".into());
            }
            anyhow::ensure!(
                offset == 0 || offset < total,
                "全文约 {total} 字,offset={offset} 超出末尾——已经读完了"
            );
            let slice: String = full.chars().skip(offset).take(READ_TEXT_MAX_CHARS).collect();
            let end = offset + slice.chars().count();
            let mut out = if offset > 0 {
                format!("(从第 {offset} 字接着读,全文约 {total} 字)\n{slice}")
            } else {
                slice
            };
            if end < total {
                out.push_str(&format!("\n…(未完:全文约 {total} 字,读到第 {end} 字;继续读带 offset={end})"));
            }
            Ok(out)
        })
        .await
        .context("读文件任务挂了")?
    }
}

// ---------------------------------------------------------------------------
// fs_stat(批量看属性,只读;2026-08-13 用户拍板独立成原语——内容读取天然单文件、
// 属性读取天然整批〔照片按拍摄时间归档〕,塞进 fs_read_text 会出双形参数)
// ---------------------------------------------------------------------------

/// fs_stat 单次封顶(fs 批量纪律同口径,§7.2 量约束:超额如实退回)。
const STAT_MAX: usize = 300;

pub(super) struct FsStat {
    spec: ToolSpec,
}

impl FsStat {
    pub(super) fn new() -> FsStat {
        FsStat {
            spec: ToolSpec {
                name: "fs_stat",
                description: "看文件的属性、不读内容:大小、修改/创建时间;照片(jpg/heic/\
                              png…)带 EXIF 拍摄时间、相机、分辨率;mp4/mov 视频带时长。\
                              批量原生——paths 一次传一整批(照片按拍摄时间归类,先拿它批量\
                              查时间再分组),最多 300 个,结果顺序与传入一致。读内容用 \
                              fs_read_text,列文件夹用 fs_list。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "paths": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "文件绝对路径(支持 ~ 开头),最多 300 个"
                        }
                    },
                    "required": ["paths"]
                }),
                timeout: std::time::Duration::from_secs(60),
                ui_key: "tool.fs_stat",
            },
        }
    }
}

#[async_trait]
impl Tool for FsStat {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let paths = arg_paths(&args, "paths")?;
        anyhow::ensure!(
            paths.len() <= STAT_MAX,
            "一次最多看 {STAT_MAX} 个,收到 {}——分批来",
            paths.len()
        );
        super::guard::ensure(ctx, super::guard::Access::Read, &paths).await?;
        tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
            Ok(paths.iter().map(|p| stat_line(Path::new(p))).collect::<Vec<_>>().join("\n"))
        })
        .await
        .context("看属性的任务没跑完")?
    }
}

/// 单文件一行:有啥报啥、缺啥不装(§3.5);读不了如实点名,不砸整批。
fn stat_line(p: &Path) -> String {
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string());
    let md = match std::fs::metadata(p) {
        Ok(md) => md,
        Err(_) => return format!("- {name}: 不存在或读不了"),
    };
    if md.is_dir() {
        let m = md.modified().map(fmt_time).unwrap_or_default();
        return format!("- {name}/ — 文件夹 · 改于 {m}(里面有什么用 fs_list 看)");
    }
    let mut parts = vec![human_size(md.len())];
    if let Ok(t) = md.modified() {
        parts.push(format!("改于 {}", fmt_time(t)));
    }
    if let Ok(t) = md.created() {
        parts.push(format!("建于 {}", fmt_time(t)));
    }
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "heic" | "heif" => {
            if let Some(x) = photo_bits(p) {
                parts.push(x);
            }
        }
        // BMFF 才有免 ffmpeg 的轻量时长(moov);mkv/mp3 等不报,不为一行属性拉子进程
        "mp4" | "m4v" | "mov" => {
            if let Some(d) = crate::media::probe_local(p).and_then(|pr| pr.duration_seconds) {
                parts.push(format!("时长 {}", fmt_dur(d)));
            }
        }
        _ => {}
    }
    format!("- {name} — {}", parts.join(" · "))
}

/// 照片三样(EXIF 拍摄时间 / 相机 / 分辨率),有啥报啥;全没有 → None(基本行照报)。
fn photo_bits(p: &Path) -> Option<String> {
    let mut bits: Vec<String> = Vec::new();
    if let Ok(f) = std::fs::File::open(p) {
        let mut br = std::io::BufReader::new(f);
        if let Ok(ex) = exif::Reader::new().read_from_container(&mut br) {
            let dt = ex
                .get_field(exif::Tag::DateTimeOriginal, exif::In::PRIMARY)
                .or_else(|| ex.get_field(exif::Tag::DateTime, exif::In::PRIMARY))
                .map(|f| tidy_exif_dt(&f.display_value().to_string()));
            if let Some(dt) = dt {
                bits.push(format!("拍摄 {dt}"));
            }
            let cam = [exif::Tag::Make, exif::Tag::Model]
                .iter()
                .filter_map(|t| ex.get_field(*t, exif::In::PRIMARY))
                .map(|f| f.display_value().to_string().trim_matches('"').trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            if !cam.is_empty() {
                bits.push(cam);
            }
        }
    }
    // 分辨率:读头不解码(HEIC 这类解不了的就省略,EXIF 半边照报)
    if let Ok((w, h)) = image::image_dimensions(p) {
        bits.push(format!("{w}×{h}"));
    }
    if bits.is_empty() {
        None
    } else {
        Some(bits.join(","))
    }
}

/// EXIF 日期形「2024:01:15 13:20:11」→「2024-01-15 13:20」(不是这个形就原样)。
fn tidy_exif_dt(raw: &str) -> String {
    let s = raw.trim().trim_matches('"');
    let b = s.as_bytes();
    if b.len() >= 16 && b[4] == b':' && b[7] == b':' {
        let mut t = b[..16].to_vec();
        t[4] = b'-';
        t[7] = b'-';
        if let Ok(v) = String::from_utf8(t) {
            return v;
        }
    }
    s.to_string()
}

fn fmt_time(t: std::time::SystemTime) -> String {
    chrono::DateTime::<chrono::Local>::from(t).format("%Y-%m-%d %H:%M").to_string()
}

fn fmt_date(t: std::time::SystemTime) -> String {
    chrono::DateTime::<chrono::Local>::from(t).format("%Y-%m-%d").to_string()
}

fn fmt_dur(secs: f64) -> String {
    let s = secs.round().max(0.0) as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
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
        // 移动 = 源头拿走(改动已有)+ 目标落新(§7.2 授权圈:两侧各按其档判)
        let srcs: Vec<String> = moves.iter().map(|(s, _)| s.clone()).collect();
        let dsts: Vec<String> = moves.iter().map(|(_, d)| d.clone()).collect();
        super::guard::ensure(ctx, super::guard::Access::Modify, &srcs).await?;
        super::guard::ensure(ctx, super::guard::Access::Create, &dsts).await?;
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
        // 复制 = 源头只读 + 目标落新
        let srcs: Vec<String> = copies.iter().map(|(s, _)| s.clone()).collect();
        let dsts: Vec<String> = copies.iter().map(|(_, d)| d.clone()).collect();
        super::guard::ensure(ctx, super::guard::Access::Read, &srcs).await?;
        super::guard::ensure(ctx, super::guard::Access::Create, &dsts).await?;
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
        super::guard::ensure(ctx, super::guard::Access::Create, &paths).await?;
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
        super::guard::ensure(ctx, super::guard::Access::Delete, &paths).await?;
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
        let path = arg_path(&args, "path")?;
        // 新文件 = 存入;覆盖已有 = 修改(档位要求更高,§7.2)
        let access = if Path::new(&path).exists() {
            super::guard::Access::Modify
        } else {
            super::guard::Access::Create
        };
        super::guard::ensure(ctx, access, std::slice::from_ref(&path)).await?;
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
        let path = arg_path(&args, "path")?;
        // 追加到已有文件 = 修改;文件不存在(顺手新建)= 存入
        let access = if Path::new(&path).exists() {
            super::guard::Access::Modify
        } else {
            super::guard::Access::Create
        };
        super::guard::ensure(ctx, access, std::slice::from_ref(&path)).await?;
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
        let path = arg_path(&args, "path")?;
        super::guard::ensure(ctx, super::guard::Access::Modify, std::slice::from_ref(&path)).await?;
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
        // 先读记录、过授权圈,再执行(记录里的路径当初操作时授过,但授权可能已被撤 → 同闸)
        let row = tokio::task::spawn_blocking({
            let store = store.clone();
            move || store.fsops.latest(user_id, "applied")
        })
        .await
        .context("撤销任务挂了")??;
        let Some(row) = row else {
            return Ok("最近没有可以撤销的文件操作".into());
        };
        let items: Vec<files::FsOpItem> =
            serde_json::from_str(&row.ops).context("操作记录读不出来")?;
        let touched: Vec<String> = items
            .iter()
            .flat_map(|it| match it {
                files::FsOpItem::Move { src, dst } | files::FsOpItem::Copy { src, dst } => {
                    vec![src.clone(), dst.clone()]
                }
                files::FsOpItem::Mkdir { path }
                | files::FsOpItem::Trash { path }
                | files::FsOpItem::Write { path, .. }
                | files::FsOpItem::Append { path, .. }
                | files::FsOpItem::Edit { path, .. } => vec![path.clone()],
            })
            .collect();
        super::guard::ensure(ctx, super::guard::Access::Modify, &touched).await?;
        tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
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

    #[test]
    fn arg_path_expands_home_prefix() {
        let home = dirs::home_dir().unwrap();
        let v = serde_json::json!({"path": "~/某文件夹"});
        assert_eq!(arg_path(&v, "path").unwrap(), home.join("某文件夹").to_string_lossy());
        let v = serde_json::json!({"path": "/abs/x"});
        assert_eq!(arg_path(&v, "path").unwrap(), "/abs/x", "非 ~ 形原样");
        let v = serde_json::json!({"paths": ["~/a", "D:\\b"]});
        let got = arg_paths(&v, "paths").unwrap();
        assert_eq!(got[0], home.join("a").to_string_lossy());
        assert_eq!(got[1], "D:\\b");
    }

    fn ctx_and_dir(tag: &str) -> (ToolCtx, PathBuf) {
        let dir = std::env::temp_dir().join(format!("lw-fs-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("kids")).unwrap();
        std::fs::write(dir.join("电影A.mp4"), vec![0u8; 2048]).unwrap();
        std::fs::write(dir.join("kids/小猪佩奇01.mp4"), b"x").unwrap();
        std::fs::write(dir.join(".hidden"), b"x").unwrap();
        let store = Store::open(&dir.join("t.db")).unwrap();
        let ctx =
            ToolCtx { user_id: 1, conv_id: 1, media: MediaRuntime::detached(store.clone()), store, web: None, voice: None, confirm: None, grants: Default::default() };
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

    /// 东西多:报总数 + 给续读起点,带 offset 能把后面看完(原先第 201 项起永远拿不到)。
    #[tokio::test]
    async fn list_paginates_with_offset_and_reports_total() {
        let (ctx, dir) = ctx_and_dir("list-page");
        let big = dir.join("big");
        std::fs::create_dir_all(&big).unwrap();
        for i in 0..LIST_MAX + 30 {
            std::fs::write(big.join(format!("f{i:04}.txt")), b"x").unwrap();
        }
        let path = big.to_string_lossy().to_string();

        let p1 = FsList::new()
            .run(serde_json::json!({"path": &path}), &ctx)
            .await
            .unwrap();
        assert!(p1.contains("f0000.txt"), "首页从头列: {}", &p1[..80.min(p1.len())]);
        assert!(!p1.contains("f0200.txt"), "首页只到第 200 项");
        assert!(
            p1.contains(&format!("共 {} 项", LIST_MAX + 30)) && p1.contains("继续列带 offset=200"),
            "要报总数 + 续读起点: {}",
            p1.lines().last().unwrap()
        );

        // 第二页:字符串形 offset 也认(quirks 同 arg_bool);到末尾不再给起点
        let p2 = FsList::new()
            .run(serde_json::json!({"path": &path, "offset": "200"}), &ctx)
            .await
            .unwrap();
        assert!(p2.contains("f0200.txt") && p2.contains("f0229.txt"), "第二页接着列: {p2}");
        assert!(p2.contains("已到末尾") && !p2.contains("继续列带"), "末页不再给起点: {p2}");

        // 越界如实退回(fs_read_text 同款措辞)
        assert!(FsList::new()
            .run(serde_json::json!({"path": &path, "offset": 999}), &ctx)
            .await
            .is_err());
    }

    /// 命中多:同样报总数 + 翻页;且结果按路径排序 —— 顺序稳定 offset 才对得上。
    #[tokio::test]
    async fn find_paginates_with_offset_and_reports_total() {
        let (ctx, dir) = ctx_and_dir("find-page");
        let many = dir.join("many");
        std::fs::create_dir_all(&many).unwrap();
        for i in 0..FIND_MAX_RESULTS + 12 {
            std::fs::write(many.join(format!("song{i:03}.mp3")), b"x").unwrap();
        }
        let root = many.to_string_lossy().to_string();
        let total = FIND_MAX_RESULTS + 12;

        let p1 = FsFind::new()
            .run(serde_json::json!({"root": &root, "pattern": "*.mp3"}), &ctx)
            .await
            .unwrap();
        assert!(p1.contains("song000.mp3"), "排序后从第一个起: {p1}");
        assert!(
            p1.contains(&format!("共 {total} 条")) && p1.contains("继续带 offset=50"),
            "要报总数 + 续读起点: {}",
            p1.lines().last().unwrap()
        );
        assert_eq!(p1.lines().count(), FIND_MAX_RESULTS + 1, "一页 50 条 + 一行说明");

        let p2 = FsFind::new()
            .run(serde_json::json!({"root": &root, "pattern": "*.mp3", "offset": 50}), &ctx)
            .await
            .unwrap();
        assert!(p2.contains("song050.mp3") && p2.contains("song061.mp3"), "第二页接着给: {p2}");
        assert!(!p2.contains("song049.mp3"), "不重复上一页");
        assert!(p2.contains("已到末尾"), "末页如实说: {p2}");
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

    #[tokio::test]
    async fn read_text_paginates_long_files_with_offset() {
        let (ctx, dir) = ctx_and_dir("read");
        std::fs::write(dir.join("长文.txt"), "甲".repeat(45_000)).unwrap();
        std::fs::write(dir.join("短.txt"), "短短的一行").unwrap();
        std::fs::write(dir.join("空.txt"), "").unwrap();
        let path = dir.join("长文.txt").to_string_lossy().to_string();

        // 首段:截断在 40k 并给出续读起点
        let p1 = FsReadText::new().run(serde_json::json!({"path": path}), &ctx).await.unwrap();
        assert!(p1.contains("继续读带 offset=40000"));
        // 第二段:接着读到尾,无「未完」;字符串形 offset 也认(quirks 同 arg_bool)
        let p2 = FsReadText::new()
            .run(serde_json::json!({"path": path, "offset": "40000"}), &ctx)
            .await
            .unwrap();
        assert!(p2.contains("从第 40000 字接着读,全文约 45000 字"));
        assert!(!p2.contains("未完"));
        assert_eq!(p2.chars().filter(|c| *c == '甲').count(), 5_000);
        // 超出末尾 = 明确报错(带总长,模型能自纠)
        let err = FsReadText::new()
            .run(serde_json::json!({"path": path, "offset": 99_999}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("超出末尾"));
        // 短文件输出与从前逐字节相同;空文件仍是「(空文件)」
        let s = FsReadText::new()
            .run(serde_json::json!({"path": dir.join("短.txt").to_string_lossy()}), &ctx)
            .await
            .unwrap();
        assert_eq!(s, "短短的一行");
        let e = FsReadText::new()
            .run(serde_json::json!({"path": dir.join("空.txt").to_string_lossy()}), &ctx)
            .await
            .unwrap();
        assert_eq!(e, "(空文件)");
    }

    #[tokio::test]
    async fn list_and_find_carry_modified_dates() {
        let (ctx, dir) = ctx_and_dir("dates");
        let out = FsList::new()
            .run(serde_json::json!({"path": dir.to_string_lossy()}), &ctx)
            .await
            .unwrap();
        assert!(
            out.lines().any(|l| l.starts_with("电影A.mp4 (2KB · 20")),
            "fs_list 行要捎改动日期: {out}"
        );
        let found = FsFind::new()
            .run(serde_json::json!({"root": dir.to_string_lossy(), "pattern": "佩奇"}), &ctx)
            .await
            .unwrap();
        assert!(found.contains("小猪佩奇01.mp4 (20"), "fs_find 行要捎改动日期: {found}");
    }

    #[tokio::test]
    async fn stat_reports_batch_with_misses_and_dirs() {
        let (ctx, dir) = ctx_and_dir("stat");
        let out = FsStat::new()
            .run(
                serde_json::json!({"paths": [
                    dir.join("电影A.mp4").to_string_lossy(),
                    dir.join("kids").to_string_lossy(),
                    dir.join("没有的.txt").to_string_lossy(),
                ]}),
                &ctx,
            )
            .await
            .unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3, "结果顺序与传入一致、一行一个: {out}");
        assert!(lines[0].contains("电影A.mp4 — 2KB · 改于 20"), "{out}");
        assert!(lines[1].contains("kids/ — 文件夹"), "{out}");
        assert!(lines[2].contains("不存在"), "{out}");
    }

    #[tokio::test]
    async fn stat_caps_batch_honestly() {
        let (ctx, dir) = ctx_and_dir("stat-cap");
        let many: Vec<String> =
            (0..STAT_MAX + 1).map(|i| dir.join(format!("f{i}")).to_string_lossy().into_owned()).collect();
        let err =
            FsStat::new().run(serde_json::json!({ "paths": many }), &ctx).await.unwrap_err();
        assert!(err.to_string().contains("最多看"), "{err:#}");
    }

    /// EXIF 半边:手工拼一张带 EXIF(拍摄时间/相机)的 JPEG——SOI 后插 APP1 段,
    /// TIFF 载荷用 exif crate 的实验 Writer 生成,读回验证「拍摄 …」归一化格式。
    #[tokio::test]
    async fn stat_reads_photo_exif_and_dimensions() {
        let (ctx, dir) = ctx_and_dir("stat-exif");
        let dt = exif::Field {
            tag: exif::Tag::DateTimeOriginal,
            ifd_num: exif::In::PRIMARY,
            value: exif::Value::Ascii(vec![b"2024:01:15 13:20:11".to_vec()]),
        };
        let make = exif::Field {
            tag: exif::Tag::Make,
            ifd_num: exif::In::PRIMARY,
            value: exif::Value::Ascii(vec![b"TestCam".to_vec()]),
        };
        let mut writer = exif::experimental::Writer::new();
        writer.push_field(&dt);
        writer.push_field(&make);
        let mut tiff = std::io::Cursor::new(Vec::new());
        writer.write(&mut tiff, false).unwrap();

        let mut jpeg = Vec::new();
        {
            let mut cur = std::io::Cursor::new(&mut jpeg);
            let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cur, 80);
            image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
                8,
                8,
                image::Rgb([100, 100, 100]),
            ))
            .write_with_encoder(enc)
            .unwrap();
        }
        let mut payload = b"Exif\0\0".to_vec();
        payload.extend_from_slice(tiff.get_ref());
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&jpeg[..2]); // SOI
        bytes.extend_from_slice(&[0xFF, 0xE1]);
        bytes.extend_from_slice(&(((payload.len() + 2) as u16).to_be_bytes()));
        bytes.extend_from_slice(&payload);
        bytes.extend_from_slice(&jpeg[2..]);
        let photo = dir.join("照片.jpg");
        std::fs::write(&photo, bytes).unwrap();

        let out = FsStat::new()
            .run(serde_json::json!({"paths": [photo.to_string_lossy()]}), &ctx)
            .await
            .unwrap();
        assert!(out.contains("拍摄 2024-01-15 13:20"), "EXIF 日期要归一化: {out}");
        assert!(out.contains("TestCam"), "相机要报出来: {out}");
        assert!(out.contains("8×8"), "分辨率要报出来: {out}");
    }
}
