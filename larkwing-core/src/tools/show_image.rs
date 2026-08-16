//! 能力轴:给用户看图(展示式)。`read_image` 的镜像姊妹 —— read_image = 模型自己看
//! (拉取式,走视觉输入计费),show_image = 把本机图片**亮在聊天里给用户看**(图卡走 UI,
//! 一个字节都不喂模型,零视觉费)。找照片给人过目、qr_encode 出的码亮屏让手机扫、
//! pdf_to_png 转完给人确认效果、讲到哪张图让用户看见说的是哪张 —— 都是它。
//!
//! 机制:字节进用户发图同一个内容寻址仓(`attachments/`,files::save_image_blob 单源,
//! 同图只存一份);live 过桥 = `TurnEvent::Shown`,落库 = tool 行 payload.attachments
//! (重开会话由行派生同一张图卡)—— 两路接线在 engine/turn.rs,这里只产 `ToolOutput.shown`。
//! 渠道回合(手机对话)没有聊天图卡这块展示面 → 如实退回指路 send_file(§3.5 不装展示了)。

use std::path::PathBuf;

use anyhow::Context;
use async_trait::async_trait;

use super::{ShownImage, Tool, ToolCtx, ToolOutput, ToolSpec};

/// 单次封顶(§4.11,2026-08-13 用户拍板「≤6 张」;超额如实退回)。
const SHOW_MAX_IMAGES: usize = 6;
/// 读入上限:超了不像要亮的图,别把大文件整个吞进内存(read_image 同口径)。
const INPUT_MAX_BYTES: u64 = 30 * 1024 * 1024;
/// 原样直存的上限:web 安全格式且不超它 = 原字节进仓(gif 动图不重编、无损);
/// 超了(或格式 WebView 不认)重编码出展示副本。
const PASSTHROUGH_MAX_BYTES: u64 = 10 * 1024 * 1024;
/// 重编码展示副本的最长边(超尺寸等比缩;聊天图卡用不上原图分辨率)。
const REENCODE_MAX_SIDE: u32 = 2000;
const JPEG_QUALITY: u8 = 85;

pub(super) struct ShowImage {
    spec: ToolSpec,
}

impl ShowImage {
    pub(super) fn new() -> ShowImage {
        ShowImage {
            spec: ToolSpec {
                name: "show_image",
                description: "把本机图片亮在聊天对话里给用户看(出图卡)。找到的照片给用户\
                              过目、qr_encode 生成的二维码亮出来给手机扫、pdf_to_png 转出的\
                              页面给用户确认、讲某张图的内容时让用户看见你说的是哪张,都用它;\
                              传图片的绝对路径,最多 6 张。这是给用户看的 —— 你自己要看图\
                              内容用 read_image。手机渠道的对话没有这块展示面,要给手机上的\
                              人看图用 send_file 发过去。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "paths": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "图片文件的绝对路径(支持 ~ 开头),最多 6 个"
                        }
                    },
                    "required": ["paths"]
                }),
                timeout: std::time::Duration::from_secs(60),
                ui_key: "tool.show_image",
            },
        }
    }

    async fn prepare(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx,
    ) -> anyhow::Result<(String, Vec<ShownImage>)> {
        let paths: Vec<String> = args
            .get("paths")
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(super::expand_home) // 「~/xxx」宽容展开(§4.4)
                    .collect()
            })
            .unwrap_or_default();
        anyhow::ensure!(!paths.is_empty(), "缺少 paths 参数(要亮出来的图片的绝对路径)");
        anyhow::ensure!(
            paths.len() <= SHOW_MAX_IMAGES,
            "一次最多亮 {SHOW_MAX_IMAGES} 张,收到 {} 张——挑最要紧的,或分批来",
            paths.len()
        );
        // 渠道回合没有聊天图卡这块展示面(手机上看不到),如实退回指路(§3.5)。
        // 会话查不到(极端/测试夹具)不拦——真回合会话恒存在。
        if let Ok(Some(conv)) = ctx.store.chat.get_conversation(ctx.conv_id) {
            anyhow::ensure!(
                matches!(conv.channel.as_str(), "ui" | "system"),
                "这个对话在手机渠道上,聊天图卡只在电脑端显示——要给 TA 看图,用 send_file 发过去"
            );
        }
        // 亮图也是读图(§7.2 授权圈)
        super::guard::ensure(ctx, super::guard::Access::Read, &paths).await?;

        let atts_dir = ctx.media.attachments_dir();
        // 解码/缩放/重编码是 CPU 活,挪出 tokio 工作线程
        let prepared = tokio::task::spawn_blocking(move || {
            paths
                .into_iter()
                .map(|p| {
                    let path = PathBuf::from(&p);
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| p.clone());
                    (name, prepare_display(&path, &atts_dir))
                })
                .collect::<Vec<_>>()
        })
        .await
        .context("图片处理任务没跑完")?;

        let mut shown = Vec::new();
        let mut misses = Vec::new();
        for (name, res) in prepared {
            match res {
                Ok((mime, file)) => shown.push(ShownImage { name, mime, file }),
                Err(e) => misses.push(format!("- {name}: {e:#}")),
            }
        }
        if shown.is_empty() {
            anyhow::bail!("一张都没亮出来:\n{}", misses.join("\n"));
        }
        let mut text = format!(
            "已把 {} 张图亮在对话里:{}。",
            shown.len(),
            shown.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join("、")
        );
        if !misses.is_empty() {
            text.push_str(&format!("\n没亮出来的:\n{}", misses.join("\n")));
        }
        Ok((text, shown))
    }
}

#[async_trait]
impl Tool for ShowImage {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    // run 只取文本;turn loop 实际走 run_output 把 shown 带给 UI(payload + 事件)。
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        Ok(self.prepare(args, ctx).await?.0)
    }

    async fn run_output(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx,
    ) -> anyhow::Result<ToolOutput> {
        let (text, shown) = self.prepare(args, ctx).await?;
        Ok(ToolOutput { text, shown, ..Default::default() })
    }
}

/// 单图:web 安全格式且不大 → 原字节直存(gif 动图保真);否则解码 → 等比缩 → 重编码
/// (有透明 PNG / 无透明 JPEG,read_image 同款口径)。返回 (mime, attachments/ 相对名)。
fn prepare_display(
    path: &std::path::Path,
    atts_dir: &std::path::Path,
) -> anyhow::Result<(String, String)> {
    let meta = std::fs::metadata(path).with_context(|| format!("打不开 {}", path.display()))?;
    anyhow::ensure!(meta.is_file(), "{} 不是文件", path.display());
    anyhow::ensure!(
        meta.len() <= INPUT_MAX_BYTES,
        "文件 {} MB,超过 {} MB——不像要亮的图",
        meta.len() / 1024 / 1024,
        INPUT_MAX_BYTES / 1024 / 1024
    );
    let bytes = std::fs::read(path).with_context(|| format!("读不了 {}", path.display()))?;
    let format = image::guess_format(&bytes)
        .with_context(|| format!("认不出图片格式 {}", path.display()))?;
    let web_safe = matches!(
        format,
        image::ImageFormat::Png
            | image::ImageFormat::Jpeg
            | image::ImageFormat::Gif
            | image::ImageFormat::WebP
            | image::ImageFormat::Bmp
    );
    let (mime, out_bytes) = if web_safe && meta.len() <= PASSTHROUGH_MAX_BYTES {
        (format_mime(format), bytes)
    } else {
        // 超大 / WebView 不认的格式:重编码出展示副本(动图会变静帧,罕见形态如实接受)
        let img = image::load_from_memory(&bytes)
            .with_context(|| format!("打不开图片 {}", path.display()))?;
        let img = if img.width().max(img.height()) > REENCODE_MAX_SIDE {
            img.thumbnail(REENCODE_MAX_SIDE, REENCODE_MAX_SIDE)
        } else {
            img
        };
        let mut buf = std::io::Cursor::new(Vec::new());
        if img.color().has_alpha() {
            img.write_to(&mut buf, image::ImageFormat::Png).context("PNG 编码失败")?;
            ("image/png", buf.into_inner())
        } else {
            let rgb = image::DynamicImage::ImageRgb8(img.to_rgb8());
            let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, JPEG_QUALITY);
            rgb.write_with_encoder(enc).context("JPEG 编码失败")?;
            ("image/jpeg", buf.into_inner())
        }
    };
    let file = crate::files::save_image_blob(atts_dir, &out_bytes, mime)
        .ok_or_else(|| anyhow::anyhow!("图片写不进展示仓"))?;
    Ok((mime.to_string(), file))
}

fn format_mime(f: image::ImageFormat) -> &'static str {
    match f {
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::Gif => "image/gif",
        image::ImageFormat::WebP => "image/webp",
        image::ImageFormat::Bmp => "image/bmp",
        _ => "image/png",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::MediaRuntime;
    use crate::store::Store;

    fn ctx(tag: &str) -> (ToolCtx, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("lw-show-{}-{tag}", std::process::id()));
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

    fn write_png(path: &std::path::Path) {
        image::GrayImage::from_pixel(32, 32, image::Luma([128u8])).save(path).unwrap();
    }

    #[tokio::test]
    async fn shows_images_and_lands_blob_in_shared_store() {
        let (ctx, dir) = ctx("ok");
        let pic = dir.join("照片.png");
        write_png(&pic);
        let out = ShowImage::new()
            .run_output(serde_json::json!({ "paths": [pic.to_string_lossy()] }), &ctx)
            .await
            .unwrap();
        assert_eq!(out.shown.len(), 1);
        assert!(out.images.is_empty(), "亮图绝不喂模型(零视觉费)");
        assert!(out.text.contains("照片.png"), "{}", out.text);
        let blob = ctx.media.attachments_dir().join(&out.shown[0].file);
        assert!(blob.is_file(), "字节要进内容寻址仓: {}", blob.display());
        assert_eq!(out.shown[0].mime, "image/png");
    }

    #[tokio::test]
    async fn partial_failure_is_reported_not_fatal() {
        let (ctx, dir) = ctx("partial");
        let pic = dir.join("有的.png");
        write_png(&pic);
        let gone = dir.join("没有的.png");
        let out = ShowImage::new()
            .run_output(
                serde_json::json!({ "paths": [pic.to_string_lossy(), gone.to_string_lossy()] }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out.shown.len(), 1);
        assert!(out.text.contains("没亮出来的"), "{}", out.text);
    }

    #[tokio::test]
    async fn rejects_empty_overflow_and_all_missing() {
        let (ctx, dir) = ctx("rej");
        let tool = ShowImage::new();
        assert!(tool.run(serde_json::json!({}), &ctx).await.is_err(), "缺 paths 要退回");
        let many: Vec<String> = (0..7).map(|i| format!("{}/p{i}.png", dir.display())).collect();
        let err = tool.run(serde_json::json!({ "paths": many }), &ctx).await.unwrap_err();
        assert!(err.to_string().contains("最多亮"), "{err:#}");
        let err = tool
            .run(serde_json::json!({ "paths": [dir.join("无.png").to_string_lossy()] }), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("一张都没亮出来"), "{err:#}");
    }

    #[tokio::test]
    async fn channel_conversation_routes_to_send_file() {
        let (mut ctx, dir) = ctx("channel");
        let conv = ctx
            .store
            .chat
            .create_conversation_full(ctx.user_id, "companion", "telegram")
            .unwrap();
        ctx.conv_id = conv.id;
        let pic = dir.join("p.png");
        write_png(&pic);
        let err = ShowImage::new()
            .run(serde_json::json!({ "paths": [pic.to_string_lossy()] }), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("send_file"), "要指路 send_file: {err:#}");
    }

    #[tokio::test]
    async fn oversize_or_exotic_gets_display_copy() {
        let (ctx, dir) = ctx("reenc");
        // TIFF:WebView 不认的格式 → 应重编码成 PNG/JPEG 展示副本
        let tif = dir.join("扫描.tiff");
        image::GrayImage::from_pixel(2600, 100, image::Luma([200u8]))
            .save_with_format(&tif, image::ImageFormat::Tiff)
            .unwrap();
        let out = ShowImage::new()
            .run_output(serde_json::json!({ "paths": [tif.to_string_lossy()] }), &ctx)
            .await
            .unwrap();
        assert_eq!(out.shown.len(), 1);
        assert!(
            out.shown[0].mime == "image/jpeg" || out.shown[0].mime == "image/png",
            "要出 web 安全副本,收到 {}",
            out.shown[0].mime
        );
        // 重编码顺带等比缩到 ≤2000
        let blob = ctx.media.attachments_dir().join(&out.shown[0].file);
        let img = image::open(&blob).unwrap();
        assert!(img.width() <= REENCODE_MAX_SIDE);
    }
}
