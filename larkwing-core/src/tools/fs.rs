//! 能力轴:文件(只读两原语)。配合任务需知里的目录,模型自行组合出
//! "找到电影并播放"——不造 local_media_search 这类任务形工具(宪法 §5 正交纪律)。
//! 只读、封顶(条数/深度),写删等真需求出现再议。

use std::path::{Path, PathBuf};

use anyhow::Context;
use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolSpec};

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
