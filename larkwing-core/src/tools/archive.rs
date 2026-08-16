//! 能力轴:文件(压缩包)。两个直交原语:`fs_unzip` 解压(zip/rar/7z,按内容认格式)、
//! `fs_zip` 打包(只产 zip)。下载链的最后一棒(BT/FTP/网页下回来的常是压缩包)与
//! send_file 的前一棒(打包一批再发)。执行在 media/archive.rs(回合内 30s 窗,超了
//! 自动转 bgtasks 后台);解压恒落**全新文件夹**(包名命名、重名加序号)→ 永不覆盖
//! 天然成立,清理 = 整个文件夹扔回收站(fs_trash),不进 fsops 逐文件记录。

use std::path::PathBuf;

use anyhow::Context;
use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolRisk, ToolSpec};
use crate::media::{ExtractOutcome, ZipOutcome};

/// 打包输入根的封顶(每个可以是整个文件夹;fs 批量纪律同族,超额如实退回)。
const ZIP_MAX_INPUTS: usize = 100;

pub(super) struct FsUnzip {
    spec: ToolSpec,
}

impl FsUnzip {
    pub(super) fn new() -> FsUnzip {
        FsUnzip {
            spec: ToolSpec {
                name: "fs_unzip",
                description: "解压压缩包(zip/rar/7z;按文件内容认格式,后缀不对也认)。\
                              解到压缩包旁边(或 dir 指定目录)一个以包名命名的**新文件夹**\
                              里,绝不覆盖已有文件。带密码的包把用户给的密码传 password;\
                              分卷 rar 传第一卷的路径。大包半分钟没解完会自动转后台接着解\
                              (任务条可见、可叫停,跑完自动回来汇报)。解出来的视频/音乐\
                              可以直接 media_play 放。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "archive": {
                            "type": "string",
                            "description": "压缩包的绝对路径(支持 ~ 开头)"
                        },
                        "dir": {
                            "type": "string",
                            "description": "解到哪个目录下(可选;缺省 = 压缩包旁边)"
                        },
                        "password": {
                            "type": "string",
                            "description": "压缩包密码(可选;只填用户给的,别自己猜)"
                        }
                    },
                    "required": ["archive"]
                }),
                // 回合内窗 30s + 大包盘点的余量(ffmpeg_run 同口径,组件下载不存在 = 短些)
                timeout: std::time::Duration::from_secs(120),
                ui_key: "tool.fs_unzip",
            },
        }
    }
}

#[async_trait]
impl Tool for FsUnzip {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn risk(&self) -> ToolRisk {
        ToolRisk::Mutating
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let archive = args
            .get("archive")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(super::expand_home)
            .ok_or_else(|| anyhow::anyhow!("缺少 archive 参数(压缩包绝对路径)"))?;
        let archive = PathBuf::from(archive);
        anyhow::ensure!(archive.is_absolute(), "archive 要绝对路径,收到:{}", archive.display());
        anyhow::ensure!(archive.is_file(), "压缩包不存在:{}", archive.display());
        let password = args
            .get("password")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        let dir = args
            .get("dir")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(super::expand_home);
        let base = match &dir {
            Some(d) => {
                let p = PathBuf::from(d);
                anyhow::ensure!(p.is_absolute(), "dir 要绝对路径,收到:{d}");
                p
            }
            None => archive
                .parent()
                .map(std::path::Path::to_path_buf)
                .ok_or_else(|| anyhow::anyhow!("压缩包没有上级目录,显式给 dir"))?,
        };
        // 目标 = base/<包名> 的全新文件夹(重名加序号,资源管理器「解压到 <名>\」口径)
        let stem = archive
            .file_stem()
            .map(|s| crate::files::sanitize_filename(&s.to_string_lossy()))
            .unwrap_or_else(|| "解压".to_string());
        let target = crate::files::dedupe_path(&base.join(stem));

        // 授权圈(§7.2):压缩包 = 读;新文件夹落点 = 存入。全部前置在动手前。
        super::guard::ensure(
            ctx,
            super::guard::Access::Read,
            &[archive.to_string_lossy().into_owned()],
        )
        .await?;
        super::guard::ensure(
            ctx,
            super::guard::Access::Create,
            &[target.to_string_lossy().into_owned()],
        )
        .await?;

        match ctx
            .media
            .extract_archive(archive, target, password, (ctx.user_id, ctx.conv_id))
            .await?
        {
            ExtractOutcome::Done(rep, dest) => Ok(format!(
                "解压好了:{} 个文件({}),放在 {}。{}原压缩包没动。",
                rep.files,
                crate::files::human_size(rep.bytes),
                dest.display(),
                rep.skipped_note()
            )),
            ExtractOutcome::Background { title } => Ok(format!(
                "这个包半分钟内没解完,已转后台接着解({title}),任务条上有进度、可以叫停;\
                 **解完会自动回来一条结果汇报**(成没成、放哪了),到时再转述。现在告诉用户\
                 已经开工、解完会说一声就好。"
            )),
        }
    }
}

pub(super) struct FsZip {
    spec: ToolSpec,
}

impl FsZip {
    pub(super) fn new() -> FsZip {
        FsZip {
            spec: ToolSpec {
                name: "fs_zip",
                description: "把几个文件/文件夹打成一个 zip 包(文件夹整棵装进去)。\
                              output 是包的文件名(不带目录,自动补 .zip),dir 缺省 = \
                              第一个输入旁边;绝不覆盖已有文件(重名自动加序号)。打包好\
                              可以接 send_file 发到手机。只会打 zip,不做 rar/7z。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "files": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "要打包的文件/文件夹绝对路径(支持 ~ 开头),最多 100 个"
                        },
                        "output": {
                            "type": "string",
                            "description": "包的文件名(只是名字,不带目录;不带扩展名会自动补 .zip)"
                        },
                        "dir": {
                            "type": "string",
                            "description": "包放到哪个目录(可选;缺省 = 第一个输入旁边)"
                        }
                    },
                    "required": ["files", "output"]
                }),
                timeout: std::time::Duration::from_secs(120),
                ui_key: "tool.fs_zip",
            },
        }
    }
}

#[async_trait]
impl Tool for FsZip {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn risk(&self) -> ToolRisk {
        ToolRisk::Mutating
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let inputs: Vec<PathBuf> = args
            .get("files")
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| PathBuf::from(super::expand_home(s)))
                    .collect()
            })
            .unwrap_or_default();
        anyhow::ensure!(!inputs.is_empty(), "缺少 files 参数(要打包的文件/文件夹绝对路径)");
        anyhow::ensure!(
            inputs.len() <= ZIP_MAX_INPUTS,
            "一次最多打包 {ZIP_MAX_INPUTS} 个输入,收到 {} 个——分几包",
            inputs.len()
        );
        for p in &inputs {
            anyhow::ensure!(p.is_absolute(), "要绝对路径,收到:{}", p.display());
            anyhow::ensure!(p.exists(), "不存在:{}", p.display());
        }

        let output = args
            .get("output")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("缺少 output 参数(包的文件名)"))?;
        anyhow::ensure!(
            !output.contains('/') && !output.contains('\\'),
            "output 只要文件名;目录用 dir 参数"
        );
        let mut clean = crate::files::sanitize_filename(output);
        match std::path::Path::new(&clean).extension().and_then(|e| e.to_str()) {
            Some(ext) if ext.eq_ignore_ascii_case("zip") => {}
            Some(ext) => anyhow::bail!("只会打 zip,output 别用 .{ext} 结尾"),
            None => clean.push_str(".zip"),
        }

        let dir = args
            .get("dir")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(super::expand_home);
        let dest_dir = match &dir {
            Some(d) => {
                let p = PathBuf::from(d);
                anyhow::ensure!(p.is_absolute(), "dir 要绝对路径,收到:{d}");
                p
            }
            None => inputs[0]
                .parent()
                .map(std::path::Path::to_path_buf)
                .ok_or_else(|| anyhow::anyhow!("第一个输入没有上级目录,显式给 dir"))?,
        };
        let dest = dest_dir.join(&clean);

        // 授权圈:输入 = 读;成品落点 = 存入
        let reads: Vec<String> =
            inputs.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        super::guard::ensure(ctx, super::guard::Access::Read, &reads).await?;
        super::guard::ensure(
            ctx,
            super::guard::Access::Create,
            &[dest.to_string_lossy().into_owned()],
        )
        .await?;
        if dir.is_some() {
            // 授权后才建目录(ffmpeg_run 同款次序)
            std::fs::create_dir_all(&dest_dir)
                .with_context(|| format!("建不出输出目录 {}", dest_dir.display()))?;
        }

        match ctx.media.create_zip(inputs, dest, (ctx.user_id, ctx.conv_id)).await? {
            ZipOutcome::Done { path, files, bytes, note } => Ok(format!(
                "打包好了:{}({},{files} 个文件)。{note}重名时已自动加序号,原件没动。",
                path.display(),
                crate::files::human_size(bytes)
            )),
            ZipOutcome::Background { title } => Ok(format!(
                "这包半分钟内没打完,已转后台接着打({title}),任务条上有进度、可以叫停;\
                 **打完会自动回来一条结果汇报**,到时再转述。现在告诉用户已经开工就好。"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::MediaRuntime;
    use crate::store::Store;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn ctx(tag: &str) -> (ToolCtx, PathBuf) {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "lw-toolarch-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("t.db"));
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
    async fn unzip_validates_args_and_detects_junk() {
        let (ctx, dir) = ctx("unzip-args");
        let tool = FsUnzip::new();
        assert!(tool.run(serde_json::json!({}), &ctx).await.is_err(), "缺 archive");
        assert!(
            tool.run(serde_json::json!({"archive": "rel.zip"}), &ctx).await.is_err(),
            "相对路径退回"
        );
        // 内容不是压缩包 → 明白话
        let junk = dir.join("假包.zip");
        std::fs::write(&junk, b"hello world").unwrap();
        let err = tool
            .run(serde_json::json!({"archive": junk.to_string_lossy()}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("认不出"), "{err:#}");
    }

    #[tokio::test]
    async fn unzip_end_to_end_new_folder_and_password_talk() {
        let (ctx, dir) = ctx("unzip-e2e");
        // 造一个真 zip:素材/a.txt
        let src = dir.join("素材");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.txt"), b"hi").unwrap();
        let plan = crate::archive::plan_zip(std::slice::from_ref(&src)).unwrap();
        let pack = dir.join("剧集.zip");
        crate::archive::create_zip(&plan, &pack, &crate::archive::Progress::default()).unwrap();

        let tool = FsUnzip::new();
        let out = tool
            .run(serde_json::json!({"archive": pack.to_string_lossy()}), &ctx)
            .await
            .unwrap();
        assert!(out.contains("解压好了") && out.contains("剧集"), "{out}");
        assert_eq!(std::fs::read(dir.join("剧集/素材/a.txt")).unwrap(), b"hi");
        // 再解一次 → 新文件夹加序号,永不覆盖
        let out2 = tool
            .run(serde_json::json!({"archive": pack.to_string_lossy()}), &ctx)
            .await
            .unwrap();
        assert!(out2.contains("剧集 (2)"), "{out2}");

        // 带密码的包没给密码 → 当场要密码(不做一半才发现)
        let locked = dir.join("锁着.7z");
        sevenz_rust2::compress_to_path_encrypted(&src, &locked, "口令".into()).unwrap();
        let err = tool
            .run(serde_json::json!({"archive": locked.to_string_lossy()}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("密码"), "{err:#}");
        let out3 = tool
            .run(
                serde_json::json!({"archive": locked.to_string_lossy(), "password": "口令"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out3.contains("解压好了"), "{out3}");
    }

    #[tokio::test]
    async fn zip_end_to_end_and_arg_validation() {
        let (ctx, dir) = ctx("zip-e2e");
        let a = dir.join("甲.txt");
        let b = dir.join("乙.txt");
        std::fs::write(&a, b"AA").unwrap();
        std::fs::write(&b, b"BBB").unwrap();

        let tool = FsZip::new();
        assert!(tool.run(serde_json::json!({"files": []}), &ctx).await.is_err(), "空 files");
        let err = tool
            .run(serde_json::json!({"files": [a.to_string_lossy()]}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("output"), "{err:#}");
        let err = tool
            .run(
                serde_json::json!({"files": [a.to_string_lossy()], "output": "包.rar"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("只会打 zip"), "{err:#}");

        let out = tool
            .run(
                serde_json::json!({
                    "files": [a.to_string_lossy(), b.to_string_lossy()],
                    "output": "行李"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.contains("打包好了") && out.contains("行李.zip"), "{out}");
        assert!(dir.join("行李.zip").is_file());
        // 解回来对账
        let unz = FsUnzip::new();
        let out = unz
            .run(serde_json::json!({"archive": dir.join("行李.zip").to_string_lossy()}), &ctx)
            .await
            .unwrap();
        assert!(out.contains("2 个文件"), "{out}");
        assert_eq!(std::fs::read(dir.join("行李/甲.txt")).unwrap(), b"AA");
    }
}
