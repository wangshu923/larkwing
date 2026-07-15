//! 能力轴:把本机图片附进工具结果里「亲眼看」。工具结果多媒体的**拉取式**消费者——
//! 模型自己决定要不要看、看哪张(§5 正交原语:用户发的照片、pdf_to_png 转出的页面图、
//! 收进本地的图都是同一个动作,组合不专设)。缺省吃「刚发的那批图」(qr_decode 同款,
//! ChatRepo::recent_image_attachments),也收显式绝对路径。图片只有视觉模型真能收到;
//! 非视觉主脑收到的是出向层的丢图留话(llm::tool_result_text),描述里已引导「看不了就别调」。

use std::path::PathBuf;

use anyhow::Context;
use async_trait::async_trait;
use base64::Engine;

use super::{Tool, ToolCtx, ToolOutput, ToolSpec};

/// 单次封顶:视觉输入按张计费,比 qr 的纯算法解码贵得多;超额如实退回,绝不静默截断。
const READ_IMAGE_MAX: usize = 5;
/// 附给模型前等比缩到这个最长边(px):看清内容够用,不喂相机原图(pdf_to_png 渲染宽同款)。
const VIEW_MAX_SIDE: u32 = 1600;
/// 重编码 JPEG 质量(无透明通道时;有透明走 PNG)。
const JPEG_QUALITY: u8 = 85;
/// 单文件输入上限:再大多半不是要「看」的图(误传视频/原始扫描件),如实退回。
const INPUT_MAX_BYTES: u64 = 30 * 1024 * 1024;

pub(super) struct ReadImage {
    spec: ToolSpec,
}

impl ReadImage {
    pub(super) fn new() -> ReadImage {
        ReadImage {
            spec: ToolSpec {
                name: "read_image",
                description: "把本机图片附进结果里亲眼看画面(前提:你是能看图的模型;\
                              看不了图就别调,如实告诉用户即可)。不带参数 = 看用户刚发来的\
                              那批图;也可以传图片的绝对路径(消息里「已存到本地」的图、\
                              pdf_to_png 转出的页面图都行)。一次最多 5 张。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "paths": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "图片文件的绝对路径,最多 5 个;省略 = 用户刚发的图"
                        }
                    }
                }),
                timeout: std::time::Duration::from_secs(60),
                ui_key: "tool.read_image",
            },
        }
    }

    /// 核心:定位目标图 → 解码/缩放/重编码 → (文本, data URL 列表)。run/run_output 共享。
    async fn look(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx,
    ) -> anyhow::Result<(String, Vec<String>)> {
        let explicit: Vec<String> = args
            .get("paths")
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();

        // (显示名, 文件路径):缺省走「刚发的那批图」(相对名 → attachments 目录)
        let mut batch_note = String::new();
        let targets: Vec<(String, PathBuf)> = if explicit.is_empty() {
            let atts_dir = ctx.media.attachments_dir();
            // 多要一张探测「这批比封顶还多」,超了如实说明(绝不静默截断)
            let mut recent = ctx
                .store
                .chat
                .recent_image_attachments(ctx.conv_id, READ_IMAGE_MAX + 1)
                .unwrap_or_default();
            anyhow::ensure!(
                !recent.is_empty(),
                "这个对话里没找到最近发的图片;让用户把图发过来,或给我图片的绝对路径"
            );
            if recent.len() > READ_IMAGE_MAX {
                recent.truncate(READ_IMAGE_MAX);
                batch_note = format!(
                    "(这批图不止 {READ_IMAGE_MAX} 张,只看了最新 {READ_IMAGE_MAX} 张;\
                     更早的用 paths 点名)\n"
                );
            }
            recent
                .into_iter()
                .enumerate()
                .map(|(i, f)| {
                    // 防目录穿越:只取末段文件名(qr_decode 同款兜底)
                    let name = std::path::Path::new(&f)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or(f.clone());
                    (format!("第{}张图", i + 1), atts_dir.join(name))
                })
                .collect()
        } else {
            anyhow::ensure!(
                explicit.len() <= READ_IMAGE_MAX,
                "一次最多看 {READ_IMAGE_MAX} 张,收到 {} 张——分批来",
                explicit.len()
            );
            explicit
                .into_iter()
                .map(|p| {
                    let name = std::path::Path::new(&p)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| p.clone());
                    (name, PathBuf::from(p))
                })
                .collect()
        };

        // 解码/缩放/重编码是 CPU 活,挪出 tokio 工作线程
        let prepared = tokio::task::spawn_blocking(move || {
            targets
                .into_iter()
                .map(|(name, path)| (name, prepare_image(&path)))
                .collect::<Vec<_>>()
        })
        .await
        .context("图片处理任务没跑完")?;

        let mut lines = String::new();
        let mut images: Vec<String> = Vec::new();
        for (name, res) in prepared {
            match res {
                Ok(p) => {
                    images.push(p.data_url);
                    let dims = if p.scaled {
                        format!("原图 {}x{},已缩到 {}x{}", p.orig_w, p.orig_h, p.w, p.h)
                    } else {
                        format!("{}x{}", p.w, p.h)
                    };
                    lines.push_str(&format!("- {name}:第 {} 张附图({dims})\n", images.len()));
                }
                Err(e) => lines.push_str(&format!("- {name}: 读不了({e:#})\n")),
            }
        }
        let header = if images.is_empty() {
            "没读出能看的图:\n".to_string()
        } else {
            format!("已附上 {} 张图,与下面的顺序一一对应:\n", images.len())
        };
        let text = format!("{batch_note}{header}{lines}");
        Ok((text.trim_end().to_string(), images))
    }
}

#[async_trait]
impl Tool for ReadImage {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    // run 只取文本(无图降级路);turn loop 实际走 run_output 把图当 parts 带回。
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        Ok(self.look(args, ctx).await?.0)
    }

    async fn run_output(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx,
    ) -> anyhow::Result<ToolOutput> {
        let (text, images) = self.look(args, ctx).await?;
        Ok(ToolOutput { text, images })
    }
}

struct Prepared {
    data_url: String,
    w: u32,
    h: u32,
    orig_w: u32,
    orig_h: u32,
    scaled: bool,
}

/// 单图:解码 → 超尺寸等比缩 → 统一重编码(有透明 PNG / 无透明 JPEG,两种各家视觉 API
/// 都收;顺带把 webp/bmp 等归一,不赌下游认不认)→ data URL。
fn prepare_image(path: &std::path::Path) -> anyhow::Result<Prepared> {
    let meta =
        std::fs::metadata(path).with_context(|| format!("打不开 {}", path.display()))?;
    anyhow::ensure!(meta.is_file(), "{} 不是文件", path.display());
    anyhow::ensure!(
        meta.len() <= INPUT_MAX_BYTES,
        "文件 {} MB,超过 {} MB——不像要看的图",
        meta.len() / 1024 / 1024,
        INPUT_MAX_BYTES / 1024 / 1024
    );
    let img = image::open(path).with_context(|| format!("打不开图片 {}", path.display()))?;
    let (orig_w, orig_h) = (img.width(), img.height());
    let scaled = orig_w.max(orig_h) > VIEW_MAX_SIDE;
    let img = if scaled { img.thumbnail(VIEW_MAX_SIDE, VIEW_MAX_SIDE) } else { img };
    let (w, h) = (img.width(), img.height());

    let mut buf = std::io::Cursor::new(Vec::new());
    let mime = if img.color().has_alpha() {
        img.write_to(&mut buf, image::ImageFormat::Png).context("PNG 编码失败")?;
        "image/png"
    } else {
        let rgb = image::DynamicImage::ImageRgb8(img.to_rgb8());
        let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, JPEG_QUALITY);
        rgb.write_with_encoder(enc).context("JPEG 编码失败")?;
        "image/jpeg"
    };
    let b64 = base64::engine::general_purpose::STANDARD.encode(buf.get_ref());
    Ok(Prepared { data_url: format!("data:{mime};base64,{b64}"), w, h, orig_w, orig_h, scaled })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::MediaRuntime;
    use crate::store::Store;

    fn ctx(tag: &str) -> (ToolCtx, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("lw-readimg-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("t.db"));
        let store = Store::open(&dir.join("t.db")).unwrap();
        let me = store.users.ensure_default_user().unwrap();
        let media = MediaRuntime::new(dir.clone(), store.clone(), crate::bus::Bus::new());
        (ToolCtx { user_id: me.id, conv_id: 1, media, store, web: None, confirm: None }, dir)
    }

    #[tokio::test]
    async fn attaches_explicit_paths_and_reports_misses() {
        let (ctx, dir) = ctx("explicit");
        let p = dir.join("pic.png");
        image::GrayImage::from_pixel(200, 100, image::Luma([128u8])).save(&p).unwrap();

        let out = ReadImage::new()
            .run_output(
                serde_json::json!({ "paths": [p.to_string_lossy(), dir.join("ghost.png").to_string_lossy()] }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out.images.len(), 1);
        assert!(out.images[0].starts_with("data:image/jpeg;base64,"), "无透明重编码成 JPEG");
        assert!(out.text.contains("pic.png"), "{}", out.text);
        assert!(out.text.contains("200x100"), "{}", out.text);
        assert!(out.text.contains("读不了"), "缺失文件要如实点名: {}", out.text);
    }

    #[tokio::test]
    async fn oversize_image_is_scaled_down_for_the_model() {
        let (ctx, dir) = ctx("scale");
        let p = dir.join("big.png");
        image::GrayImage::from_pixel(3200, 800, image::Luma([200u8])).save(&p).unwrap();

        let out = ReadImage::new()
            .run_output(serde_json::json!({ "paths": [p.to_string_lossy()] }), &ctx)
            .await
            .unwrap();
        assert!(out.text.contains("已缩到"), "{}", out.text);
        // 解回 data URL 验证真缩了
        let b64 = out.images[0].split(',').nth(1).unwrap();
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
        let img = image::load_from_memory(&bytes).unwrap();
        assert!(img.width() <= VIEW_MAX_SIDE && img.height() <= VIEW_MAX_SIDE, "{}x{}", img.width(), img.height());
    }

    #[tokio::test]
    async fn defaults_to_recent_batch_and_caps_honestly() {
        let (ctx, dir) = ctx("recent");
        let atts = ctx.media.attachments_dir();
        std::fs::create_dir_all(&atts).unwrap();
        let conv = ctx.store.chat.create_conversation(ctx.user_id, "companion").unwrap();
        let pay = |f: &str| {
            format!(r#"{{"attachments":[{{"kind":"image","name":"p.png","mime":"image/png","file":"{f}"}}]}}"#)
        };
        // 一批 6 张(> 封顶 5):只看最新 5 且如实说明
        for i in 0..6 {
            let f = format!("r{i}.png");
            image::GrayImage::from_pixel(32, 32, image::Luma([100u8])).save(atts.join(&f)).unwrap();
            ctx.store.chat.append_message_full(conv.id, "user", "图", Some(&pay(&f))).unwrap();
        }

        let mut c2 = ctx;
        c2.conv_id = conv.id;
        let out = ReadImage::new().run_output(serde_json::json!({}), &c2).await.unwrap();
        assert_eq!(out.images.len(), READ_IMAGE_MAX);
        assert!(out.text.contains("只看了最新"), "{}", out.text);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn too_many_explicit_paths_is_honest_error() {
        let (ctx, _dir) = ctx("cap");
        let paths: Vec<String> = (0..6).map(|i| format!("/tmp/x{i}.png")).collect();
        let err = ReadImage::new()
            .run(serde_json::json!({ "paths": paths }), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("最多看 5 张"), "{err:#}");
    }

    #[tokio::test]
    async fn no_recent_image_is_honest_error() {
        let (mut ctx, _dir) = ctx("empty");
        let conv = ctx.store.chat.create_conversation(ctx.user_id, "companion").unwrap();
        ctx.conv_id = conv.id;
        let err = ReadImage::new().run(serde_json::json!({}), &ctx).await.unwrap_err();
        assert!(err.to_string().contains("没找到最近发的图片"), "{err:#}");
    }
}
