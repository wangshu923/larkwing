//! 能力轴:图里的二维码 → 文本(rxing 纯 Rust,全本地)。正交原语:只认码不办事——
//! 认出的链接交给 web_fetch/web_download、文本交模型自己用(下载页码、WiFi 码、
//! 名片码都是同一个动作;组合链缘起见 AGENT §7.8,词汇不进本原语)。缺省吃「刚发的
//! 那批图」(手机拍单据连发数张的形状,ChatRepo::recent_image_attachments),
//! 也收显式路径(桌面文件夹批量)。

use std::path::PathBuf;

use anyhow::Context;
use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolSpec};

/// 单次封顶(fs 批量纪律同款:超额如实告知,不静默截断)。
const QR_MAX_IMAGES: usize = 10;
/// 大图先缩到这个长边再认(相机原图十几 MP 白费时间);认不出再用原图重试一次。
const QR_DETECT_MAX_SIDE: u32 = 2000;
/// 「糊化」档:缩到这个长边让下采样平掉打印晕染/噪点(真图矩阵实验:800px 连原样都过)。
const QR_BLUR_SIDE: u32 = 800;

pub(super) struct QrDecode {
    spec: ToolSpec,
}

impl QrDecode {
    pub(super) fn new() -> QrDecode {
        QrDecode {
            spec: ToolSpec {
                name: "qr_decode",
                description: "认图片里的二维码,返回码里的文字/链接。不带参数 = 认用户\
                              刚发来的那批图;也可以传本机图片的绝对路径。PDF 里的码要先用 \
                              pdf_to_png 转成图再认。认出链接后按用户意图接着办(比如 \
                              web_fetch 打开、web_download 下载)。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "paths": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "图片文件的绝对路径,最多 10 个;省略 = 用户刚发的图"
                        }
                    }
                }),
                timeout: std::time::Duration::from_secs(60),
                ui_key: "tool.qr_decode",
            },
        }
    }
}

#[async_trait]
impl Tool for QrDecode {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let explicit: Vec<String> = args
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

        // (显示名, 文件路径):缺省走「刚发的那批图」(相对名 → attachments 目录)
        let targets: Vec<(String, PathBuf)> = if explicit.is_empty() {
            let atts_dir = ctx.media.attachments_dir();
            let recent = ctx
                .store
                .chat
                .recent_image_attachments(ctx.conv_id, QR_MAX_IMAGES)
                .unwrap_or_default();
            anyhow::ensure!(
                !recent.is_empty(),
                "这个对话里没找到最近发的图片;让用户把带二维码的图发过来,或给我图片的绝对路径"
            );
            recent
                .into_iter()
                .enumerate()
                .map(|(i, f)| {
                    // 防目录穿越:只取末段文件名(attachment_url 同款兜底)
                    let name = std::path::Path::new(&f)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or(f.clone());
                    (format!("第{}张图", i + 1), atts_dir.join(name))
                })
                .collect()
        } else {
            anyhow::ensure!(
                explicit.len() <= QR_MAX_IMAGES,
                "一次最多认 {QR_MAX_IMAGES} 张,收到 {} 张——分批来",
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

        // 解码是 CPU 活(图片解码 + 网格检测),挪出 tokio 工作线程
        let results = tokio::task::spawn_blocking(move || {
            targets
                .into_iter()
                .map(|(name, path)| (name, decode_image(&path)))
                .collect::<Vec<_>>()
        })
        .await
        .context("二维码识别任务没跑完")?;

        let mut out = String::new();
        let mut hits = 0usize;
        for (name, res) in &results {
            match res {
                Ok(codes) if !codes.is_empty() => {
                    hits += 1;
                    for c in codes {
                        out.push_str(&format!("- {name}: {c}\n"));
                    }
                }
                Ok(_) => out.push_str(&format!("- {name}: 没认出二维码(图里可能没有,或太糊)\n")),
                Err(e) => out.push_str(&format!("- {name}: 读不了({e:#})\n")),
            }
        }
        out.insert_str(0, &format!("认了 {} 张图,{hits} 张有码:\n", results.len()));
        Ok(out.trim_end().to_string())
    }
}

/// 单图解码:预处理阶梯,便宜→重,认出即停(2026-07-10 真图矩阵实验定的配方,
/// 见 tests::probe_matrix)。热敏打印单据照(墨迹晕染/光照不均)是分水岭:原样喂
/// rxing 都认不出,**Otsu 全局二值化在各尺度全过、缩到 800px 连原样都过**——
/// ① 工作尺寸(≤2000px)原样:干净数字图秒过;② 同尺寸 Otsu:治晕染/灰底;
/// ③ 缩 800px(原样→Otsu):下采样平掉墨迹糊;④ 原尺寸兜底(缩图可能毁掉
/// 大图里的小码)。rqrr 已换 rxing(ZXing 纯 Rust 移植,TryHarder)。
fn decode_image(path: &std::path::Path) -> anyhow::Result<Vec<String>> {
    let img = image::open(path).with_context(|| format!("打不开图片 {}", path.display()))?;
    let oversize = img.width().max(img.height()) > QR_DETECT_MAX_SIDE;
    let work =
        if oversize { img.thumbnail(QR_DETECT_MAX_SIDE, QR_DETECT_MAX_SIDE) } else { img.clone() };

    let work_luma = work.to_luma8();
    let attempt = decode_luma(&work_luma);
    if !attempt.is_empty() {
        return Ok(attempt);
    }
    let attempt = decode_luma(&otsu_binarize(&work_luma));
    if !attempt.is_empty() {
        return Ok(attempt);
    }
    let small = img.thumbnail(QR_BLUR_SIDE, QR_BLUR_SIDE).to_luma8();
    let attempt = decode_luma(&small);
    if !attempt.is_empty() {
        return Ok(attempt);
    }
    let attempt = decode_luma(&otsu_binarize(&small));
    if !attempt.is_empty() || !oversize {
        return Ok(attempt);
    }
    // 原尺寸兜底:大图里占比很小的码,缩放会抹掉细节
    let full = img.to_luma8();
    let attempt = decode_luma(&full);
    if !attempt.is_empty() {
        return Ok(attempt);
    }
    Ok(decode_luma(&otsu_binarize(&full)))
}

fn decode_luma(luma: &image::GrayImage) -> Vec<String> {
    use rxing::{BarcodeFormat, DecodeHints};
    let mut hints = DecodeHints {
        TryHarder: Some(true), // 准确优先(工具不在实时路径上,几百毫秒花得起)
        PossibleFormats: Some(std::collections::HashSet::from([BarcodeFormat::QR_CODE])),
        ..Default::default()
    };
    match rxing::helpers::detect_multiple_in_luma_with_hints(
        luma.as_raw().clone(),
        luma.width(),
        luma.height(),
        &mut hints,
    ) {
        Ok(results) => results
            .into_iter()
            .map(|r| r.getText().to_string())
            .filter(|c| !c.trim().is_empty())
            .collect(),
        Err(_) => Vec::new(), // NotFound 等 = 没认出,统一交上层「没认出二维码」话术
    }
}

/// Otsu 全局二值化(墨迹晕染的模块钉回纯黑白;全局阈值对单据这类均匀光照够用。
/// 矩阵实验里 1%/99% 对比度拉伸全败、Otsu 全胜 → 只留 Otsu)。
fn otsu_binarize(luma: &image::GrayImage) -> image::GrayImage {
    let mut hist = [0u64; 256];
    for p in luma.pixels() {
        hist[p.0[0] as usize] += 1;
    }
    let total: u64 = (luma.width() * luma.height()) as u64;
    let sum_all: u64 = hist.iter().enumerate().map(|(i, n)| i as u64 * n).sum();
    let (mut best_t, mut best_var, mut w0, mut sum0) = (127u8, 0f64, 0u64, 0u64);
    for t in 0..256usize {
        w0 += hist[t];
        if w0 == 0 {
            continue;
        }
        let w1 = total - w0;
        if w1 == 0 {
            break;
        }
        sum0 += t as u64 * hist[t];
        let m0 = sum0 as f64 / w0 as f64;
        let m1 = (sum_all - sum0) as f64 / w1 as f64;
        let var = w0 as f64 * w1 as f64 * (m0 - m1) * (m0 - m1);
        if var > best_var {
            best_var = var;
            best_t = t as u8;
        }
    }
    image::GrayImage::from_fn(luma.width(), luma.height(), |x, y| {
        image::Luma([if luma.get_pixel(x, y).0[0] > best_t { 255 } else { 0 }])
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::MediaRuntime;
    use crate::store::Store;

    fn ctx(tag: &str) -> (ToolCtx, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("lw-qr-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("t.db"));
        let store = Store::open(&dir.join("t.db")).unwrap();
        let me = store.users.ensure_default_user().unwrap();
        let media = MediaRuntime::new(dir.clone(), store.clone(), crate::bus::Bus::new());
        (ToolCtx { user_id: me.id, conv_id: 1, media, store, web: None, confirm: None }, dir)
    }

    fn write_qr_png(path: &std::path::Path, content: &str) {
        let code = qrcode::QrCode::new(content.as_bytes()).unwrap();
        let img: image::GrayImage =
            code.render::<image::Luma<u8>>().min_dimensions(240, 240).build();
        img.save(path).unwrap();
    }

    #[tokio::test]
    async fn decodes_explicit_paths_and_reports_misses() {
        let (ctx, dir) = ctx("explicit");
        let qr = dir.join("code.png");
        write_qr_png(&qr, "https://inv.example.com/dl?id=42");
        // 一张白图(无码)作反例
        image::GrayImage::from_pixel(64, 64, image::Luma([255u8])).save(dir.join("blank.png")).unwrap();

        let tool = QrDecode::new();
        let out = tool
            .run(
                serde_json::json!({ "paths": [qr.to_string_lossy(), dir.join("blank.png").to_string_lossy()] }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.contains("https://inv.example.com/dl?id=42"), "{out}");
        assert!(out.contains("没认出二维码"), "{out}");
        assert!(out.contains("1 张有码"), "{out}");
    }

    #[tokio::test]
    async fn defaults_to_recent_image_attachments_batch() {
        let (ctx, dir) = ctx("recent");
        let atts = ctx.media.attachments_dir();
        std::fs::create_dir_all(&atts).unwrap();
        write_qr_png(&atts.join("a1.png"), "INV-A");
        write_qr_png(&atts.join("a2.png"), "INV-B");

        let conv = ctx.store.chat.create_conversation(ctx.user_id, "companion").unwrap();
        let pay = |f: &str| {
            format!(r#"{{"attachments":[{{"kind":"image","name":"p.png","mime":"image/png","file":"{f}"}}]}}"#)
        };
        // 旧的一批之前有条纯文本 → 区块边界之外的图不掺和
        ctx.store.chat.append_message(conv.id, "user", "早上好").unwrap();
        ctx.store
            .chat
            .append_message_full(conv.id, "user", "单子一", Some(&pay("a1.png")))
            .unwrap();
        ctx.store
            .chat
            .append_message_full(conv.id, "user", "单子二", Some(&pay("a2.png")))
            .unwrap();
        ctx.store.chat.append_message(conv.id, "user", "帮我认一下").unwrap();

        let mut c2 = ctx;
        c2.conv_id = conv.id;
        let out = QrDecode::new().run(serde_json::json!({}), &c2).await.unwrap();
        assert!(out.contains("INV-A") && out.contains("INV-B"), "{out}");

        let _ = std::fs::remove_dir_all(dir);
    }

    /// 真实照片 probe(斜角/光照不均的手机拍摄件是解码器的分水岭;rqrr 折在这里):
    /// `LW_QR_PROBE=<图片路径> cargo test -p larkwing-core --lib tools::qr -- --ignored --nocapture`
    #[test]
    #[ignore = "吃 LW_QR_PROBE 指的真图,手动跑"]
    fn probe_real_photo() {
        let path = std::env::var("LW_QR_PROBE").expect("设 LW_QR_PROBE=图片路径");
        let out = decode_image(std::path::Path::new(&path)).unwrap();
        println!("decoded: {out:?}");
        assert!(!out.is_empty(), "真图没认出来");
    }

    /// 配方矩阵实验(找到能过真图的预处理组合;定配方用,不是回归测试):
    /// 单/多码 × 缩放档 × 预处理(原样/对比度拉伸/Otsu 二值)。
    #[test]
    #[ignore = "实验件:吃 LW_QR_PROBE,打印矩阵结果"]
    fn probe_matrix() {
        use rxing::{BarcodeFormat, DecodeHints};
        let path = std::env::var("LW_QR_PROBE").expect("设 LW_QR_PROBE=图片路径");
        let img = image::open(&path).unwrap();
        let scales: [u32; 5] = [0, 2000, 1600, 1200, 800]; // 0 = 原尺寸
        for scale in scales {
            let base = if scale == 0 { img.clone() } else { img.thumbnail(scale, scale) };
            let luma = base.to_luma8();
            for prep in ["raw", "otsu"] {
                let l = match prep {
                    "otsu" => otsu_binarize(&luma),
                    _ => luma.clone(),
                };
                for mode in ["multi", "single"] {
                    let mut hints = DecodeHints {
                        TryHarder: Some(true),
                        PossibleFormats: Some(std::collections::HashSet::from([
                            BarcodeFormat::QR_CODE,
                        ])),
                        ..Default::default()
                    };
                    let got: Vec<String> = if mode == "multi" {
                        rxing::helpers::detect_multiple_in_luma_with_hints(
                            l.as_raw().clone(), l.width(), l.height(), &mut hints,
                        )
                        .map(|rs| rs.into_iter().map(|r| r.getText().to_string()).collect())
                        .unwrap_or_default()
                    } else {
                        rxing::helpers::detect_in_luma_with_hints(
                            l.as_raw().clone(), l.width(), l.height(), None, &mut hints,
                        )
                        .map(|r| vec![r.getText().to_string()])
                        .unwrap_or_default()
                    };
                    println!(
                        "scale={scale:<5} prep={prep:<8} mode={mode:<7} -> {}",
                        if got.is_empty() { "×".into() } else { format!("✓ {got:?}") }
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn no_recent_image_is_honest_error() {
        let (mut ctx, _dir) = ctx("empty");
        let conv = ctx.store.chat.create_conversation(ctx.user_id, "companion").unwrap();
        ctx.conv_id = conv.id;
        let err = QrDecode::new().run(serde_json::json!({}), &ctx).await.unwrap_err();
        assert!(err.to_string().contains("没找到最近发的图片"));
    }
}
