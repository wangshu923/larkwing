//! 能力轴:PDF → 逐页 PNG(栅格化)。pdfium 动态库走组件用时下载(§6.9,包里不带);
//! 正交原语:只转格式不办事——转出的图交给 qr_decode 认码、send_file 发手机、或用户
//! 自己看(单据 PDF 转 PNG 正是这条链,缘起见 AGENT §7.8)。也是 §9 扫描件 OCR 的「栅格化」半边,以后接
//! 视觉模型零新件。
//! 产物守 fs 三规①:同名自动 ` (N)`,永不覆盖(files::dedupe_path)。

use std::path::{Path, PathBuf};

use anyhow::Context;
use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolRisk, ToolSpec};

/// 单次封顶(fs 批量纪律:超额如实退回,让模型带 pages 分批,绝不静默截断)。
const PDF_MAX_PAGES: usize = 20;
/// 渲染目标宽(px):单据/文档在手机上看清楚够用,又不产出巨图。
const RENDER_TARGET_WIDTH: i32 = 1600;

/// pdfium C 库全局唯一实例纪律:并发绑定/释放(FPDF_InitLibrary/Destroy)会互相打;
/// 一次只跑一份渲染(轮内工具并发是常态,别赌运气)。
static PDFIUM_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// 绑定每进程只许一次(pdfium-render 0.9 二次 bind 报 AlreadyInitialized,e2e 实锤)
/// → 首次绑定后进程级缓存复用;库文件路径稳定(组件目录),不存在"换路径重绑"。
static PDFIUM: std::sync::OnceLock<pdfium_render::prelude::Pdfium> = std::sync::OnceLock::new();

pub(super) struct PdfToPng {
    spec: ToolSpec,
}

impl PdfToPng {
    pub(super) fn new() -> PdfToPng {
        PdfToPng {
            spec: ToolSpec {
                name: "pdf_to_png",
                description: "把 PDF 转成逐页 PNG 图片(用户要「能直接看的图」、要发到手机上看、\
                              或要认 PDF 里的二维码时用)。首次使用会自动准备渲染组件。\
                              产物存在 PDF 旁边(或指定文件夹),同名不覆盖;转完把图片路径\
                              告诉用户。你自己要看 PDF 页面内容(且你能看图)时,转完接 \
                              read_image 看转出的图。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "PDF 文件的绝对路径" },
                        "pages": {
                            "type": "array",
                            "items": { "type": "integer" },
                            "description": "只转这些页(从 1 数);省略 = 全部(最多 20 页)"
                        },
                        "dir": {
                            "type": "string",
                            "description": "图片存到哪个文件夹(绝对路径);省略 = PDF 所在文件夹"
                        }
                    },
                    "required": ["path"]
                }),
                timeout: std::time::Duration::from_secs(300),
                ui_key: "tool.pdf_to_png",
            },
        }
    }
}

#[async_trait]
impl Tool for PdfToPng {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn risk(&self) -> ToolRisk {
        ToolRisk::Mutating
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let path = args
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .context("缺少 path 参数(PDF 的绝对路径)")?;
        let pdf = PathBuf::from(path);
        anyhow::ensure!(pdf.is_absolute(), "path 需要绝对路径,收到: {path}");
        anyhow::ensure!(pdf.is_file(), "文件不存在: {path}");
        let pages: Vec<usize> = args
            .get("pages")
            .and_then(serde_json::Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_u64()).map(|n| n as usize).collect())
            .unwrap_or_default();
        anyhow::ensure!(
            pages.len() <= PDF_MAX_PAGES,
            "一次最多转 {PDF_MAX_PAGES} 页,收到 {} 页——分批来",
            pages.len()
        );
        let out_dir = match args.get("dir").and_then(serde_json::Value::as_str).map(str::trim) {
            Some(d) if !d.is_empty() => {
                let p = PathBuf::from(d);
                anyhow::ensure!(p.is_absolute(), "dir 需要绝对路径,收到: {d}");
                std::fs::create_dir_all(&p)
                    .with_context(|| format!("建不了目标文件夹 {}", p.display()))?;
                p
            }
            _ => pdf.parent().map(Path::to_path_buf).unwrap_or_else(std::env::temp_dir),
        };

        // 组件就位(已下载即秒回;首次下载进度冒 HUD 卡)
        let lib = ctx.media.ensure_pdfium().await.context("PDF 渲染组件没准备好")?;

        let outputs = tokio::task::spawn_blocking(move || render_pages(&lib, &pdf, &pages, &out_dir))
            .await
            .context("PDF 渲染任务没跑完")??;

        let mut out = format!("转好 {} 页:\n", outputs.len());
        for p in &outputs {
            out.push_str(&format!("- {}\n", p.display()));
        }
        Ok(out.trim_end().to_string())
    }
}

/// 渲染主体(阻塞线程里跑):绑定 pdfium → 选页 → 逐页出 PNG。
/// 返回产物路径(与请求页序一致)。
fn render_pages(
    lib: &Path,
    pdf: &Path,
    pages: &[usize],
    out_dir: &Path,
) -> anyhow::Result<Vec<PathBuf>> {
    use pdfium_render::prelude::*;

    let _gate = PDFIUM_GATE.lock().unwrap_or_else(|p| p.into_inner());
    let pdfium: &Pdfium = match PDFIUM.get() {
        Some(p) => p,
        None => {
            let bindings = Pdfium::bind_to_library(lib.to_string_lossy().as_ref())
                .map_err(|e| anyhow::anyhow!("加载 PDF 渲染组件失败: {e:?}"))?;
            let _ = PDFIUM.set(Pdfium::new(bindings)); // gate 之内,无竞争
            PDFIUM.get().expect("刚 set 过")
        }
    };
    let doc = pdfium
        .load_pdf_from_file(pdf, None)
        .map_err(|e| anyhow::anyhow!("打不开 PDF(损坏或带密码?): {e:?}"))?;
    let total = doc.pages().len() as usize;
    anyhow::ensure!(total > 0, "这份 PDF 一页都没有");

    // 选页:显式页号(1 起)校验范围;缺省全转但封顶如实退回
    let selected: Vec<usize> = if pages.is_empty() {
        anyhow::ensure!(
            total <= PDF_MAX_PAGES,
            "这份 PDF 有 {total} 页,一次最多转 {PDF_MAX_PAGES} 页——用 pages 指定页码分批转"
        );
        (1..=total).collect()
    } else {
        for &p in pages {
            anyhow::ensure!(p >= 1 && p <= total, "页码 {p} 超出范围(共 {total} 页)");
        }
        pages.to_vec()
    };

    let stem = pdf.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or("页面".into());
    let cfg = PdfRenderConfig::new().set_target_width(RENDER_TARGET_WIDTH);
    let mut outputs = Vec::with_capacity(selected.len());
    let single = selected.len() == 1 && total == 1;
    for &pno in &selected {
        let page = doc
            .pages()
            .get((pno - 1) as i32)
            .map_err(|e| anyhow::anyhow!("取第 {pno} 页失败: {e:?}"))?;
        let bitmap = page
            .render_with_config(&cfg)
            .map_err(|e| anyhow::anyhow!("第 {pno} 页渲染失败: {e:?}"))?;
        // 单页文档不带 -pN 后缀(单页单据「一份一张图」的直觉名)
        let name = if single { format!("{stem}.png") } else { format!("{stem}-p{pno}.png") };
        let dest = crate::files::dedupe_path(&out_dir.join(name));
        bitmap
            .as_image()
            .map_err(|e| anyhow::anyhow!("第 {pno} 页位图转换失败: {e:?}"))?
            .save(&dest)
            .with_context(|| format!("图片写盘失败 {}", dest.display()))?;
        outputs.push(dest);
    }
    Ok(outputs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::MediaRuntime;
    use crate::store::Store;

    fn ctx(tag: &str) -> (ToolCtx, PathBuf) {
        let dir = std::env::temp_dir().join(format!("lw-pdf-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("t.db"));
        let store = Store::open(&dir.join("t.db")).unwrap();
        let me = store.users.ensure_default_user().unwrap();
        let media = MediaRuntime::new(dir.clone(), store.clone(), crate::bus::Bus::new());
        (ToolCtx { user_id: me.id, conv_id: 1, media, store, web: None }, dir)
    }

    /// 最小合法单页 PDF(程序算 xref 偏移,pdfium 能开):e2e 用。
    fn tiny_pdf() -> Vec<u8> {
        let stream = "0 0 1 RG 10 10 180 80 re S";
        let objs = [
            "<</Type/Catalog/Pages 2 0 R>>".to_string(),
            "<</Type/Pages/Kids[3 0 R]/Count 1>>".to_string(),
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 100]/Contents 4 0 R>>".to_string(),
            format!("<</Length {}>>\nstream\n{stream}\nendstream", stream.len()),
        ];
        let mut out = String::from("%PDF-1.4\n");
        let mut offsets = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.push_str(&format!("{} 0 obj\n{o}\nendobj\n", i + 1));
        }
        let xref_at = out.len();
        out.push_str(&format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1));
        for off in &offsets {
            out.push_str(&format!("{off:010} 00000 n \n"));
        }
        out.push_str(&format!(
            "trailer\n<</Size {}/Root 1 0 R>>\nstartxref\n{xref_at}\n%%EOF",
            objs.len() + 1
        ));
        out.into_bytes()
    }

    #[tokio::test]
    async fn rejects_bad_args_before_touching_component() {
        let (ctx, dir) = ctx("args");
        let tool = PdfToPng::new();
        // 缺 path / 相对路径 / 不存在 / pages 超封顶,全都在下载组件之前就退回
        assert!(tool.run(serde_json::json!({}), &ctx).await.is_err());
        assert!(tool.run(serde_json::json!({"path": "a.pdf"}), &ctx).await.is_err());
        assert!(tool
            .run(serde_json::json!({"path": dir.join("nope.pdf").to_string_lossy()}), &ctx)
            .await
            .is_err());
        std::fs::write(dir.join("x.pdf"), tiny_pdf()).unwrap();
        let too_many: Vec<usize> = (1..=21).collect();
        let err = tool
            .run(
                serde_json::json!({"path": dir.join("x.pdf").to_string_lossy(), "pages": too_many}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("最多转"), "{err:#}");
    }

    /// 真网下载 pdfium + 真渲染(手动验:`cargo test -p larkwing-core --lib tools::pdf -- --ignored`)。
    #[tokio::test]
    #[ignore = "要下载 pdfium(几 MB 真网),开发机手动跑"]
    async fn e2e_downloads_pdfium_and_renders_png() {
        let (ctx, dir) = ctx("e2e");
        let pdf = dir.join("样张.pdf");
        std::fs::write(&pdf, tiny_pdf()).unwrap();
        let out = PdfToPng::new()
            .run(serde_json::json!({"path": pdf.to_string_lossy()}), &ctx)
            .await
            .unwrap();
        assert!(out.contains("转好 1 页"), "{out}");
        let png = dir.join("样张.png");
        assert!(png.is_file(), "单页产物不带 -pN 后缀: {out}");
        let img = image::open(&png).unwrap();
        assert_eq!(img.width() as i32, super::RENDER_TARGET_WIDTH);
        // 同名再转 → (2),永不覆盖
        let out2 = PdfToPng::new()
            .run(serde_json::json!({"path": pdf.to_string_lossy()}), &ctx)
            .await
            .unwrap();
        assert!(out2.contains("样张 (2).png"), "{out2}");
    }
}
