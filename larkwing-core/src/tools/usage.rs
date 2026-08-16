//! 能力轴:文件(占用分析)。只读直交原语:统计一棵目录树的磁盘占用(第一层子目录
//! 排行 + 全树最大文件)。「C 盘怎么满了」由模型对着最大子目录反复下钻组合出来
//! (§5 正交,不造 cleanup 任务工具;清理动作走既有 fs_trash,可逆)。
//! 引擎 `crate::usage`(纯同步可测),执行节奏 media/usage.rs(回合内 30s → 转后台)。

use std::path::PathBuf;

use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolSpec};
use crate::media::UsageOutcome;
use crate::usage as engine;

pub(super) struct FsUsage {
    spec: ToolSpec,
}

impl FsUsage {
    pub(super) fn new() -> FsUsage {
        FsUsage {
            spec: ToolSpec {
                name: "fs_usage",
                description: "统计一个文件夹(或整个盘)占了多少磁盘空间:整棵树算完,\
                              报回按占用排的子目录 + 全树最大的几个文件。查「哪儿占地方/\
                              磁盘怎么满了」就从用户说的盘或文件夹开始,对着报回来最大的\
                              子目录再调一次往下钻,几轮就能定位。只读、不动任何文件;\
                              大文件夹半分钟没扫完会自动转后台接着扫(任务条可见、可叫停,\
                              跑完自动回来汇报)。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "文件夹的绝对路径(支持 ~ 开头;盘根如 C:\\ 也行)"
                        }
                    },
                    "required": ["path"]
                }),
                // 回合内窗 30s + 大树盘点余量(archive 同口径)
                timeout: std::time::Duration::from_secs(60),
                ui_key: "tool.fs_usage",
            },
        }
    }
}

#[async_trait]
impl Tool for FsUsage {
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
            .ok_or_else(|| anyhow::anyhow!("缺少 path 参数(要看的文件夹)"))?;
        let path = PathBuf::from(path);
        anyhow::ensure!(path.is_absolute(), "path 要绝对路径,收到:{}", path.display());
        anyhow::ensure!(path.is_dir(), "path 要是已存在的文件夹,收到:{}", path.display());
        // 只读也是读(§7.2 授权圈):扫谁的树就要谁的 read
        super::guard::ensure(
            ctx,
            super::guard::Access::Read,
            &[path.to_string_lossy().into_owned()],
        )
        .await?;

        match ctx.media.disk_usage(path.clone(), (ctx.user_id, ctx.conv_id)).await? {
            UsageOutcome::Done(rep) => Ok(engine::render_report(&path, &rep)),
            UsageOutcome::Background { title } => Ok(format!(
                "这个文件夹有点大,转后台接着扫了(任务「{title}」)。跑完会自动回来汇报;\
                 进度看「此刻」背景或 task_status,用户要停就 task_cancel。"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::MediaRuntime;
    use crate::store::Store;

    fn ctx(tag: &str) -> (ToolCtx, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("lw-fsusage-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("t.db")).unwrap();
        let me = store.users.ensure_default_user().unwrap();
        (
            ToolCtx {
                user_id: me.id,
                conv_id: 1,
                media: MediaRuntime::detached(store.clone()),
                store,
                web: None,
                voice: None,
                confirm: None,
                grants: Default::default(),
            },
            dir,
        )
    }

    #[tokio::test]
    async fn reports_tree_usage_in_turn() {
        let (ctx, dir) = ctx("inturn");
        std::fs::create_dir_all(dir.join("media")).unwrap();
        std::fs::write(dir.join("media/movie.bin"), vec![0u8; 4096]).unwrap();
        std::fs::write(dir.join("note.txt"), b"hi").unwrap();
        let out = FsUsage::new()
            .run(serde_json::json!({ "path": dir.to_string_lossy() }), &ctx)
            .await
            .unwrap();
        assert!(out.contains("media"), "要点名大头子目录: {out}");
        assert!(out.contains("movie.bin"), "要点名最大文件: {out}");
    }

    #[tokio::test]
    async fn rejects_relative_and_missing_path() {
        let (ctx, dir) = ctx("rej");
        let tool = FsUsage::new();
        assert!(tool.run(serde_json::json!({}), &ctx).await.is_err(), "缺 path 要退回");
        assert!(
            tool.run(serde_json::json!({ "path": "rel/dir" }), &ctx).await.is_err(),
            "相对路径要退回"
        );
        let gone = dir.join("不存在");
        let err = tool
            .run(serde_json::json!({ "path": gone.to_string_lossy() }), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("已存在的文件夹"), "{err:#}");
    }
}
