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

/// 宽容解析 `pages`(§4.4 Quirks,与 `arg_bool` / `arg_u64` 同族:流式 JSON 里模型常把
/// 声明为数组的参数发成别的形)。认:数组(元素是数字或数字字符串)、单个数字、
/// 逗号分隔的字符串(顺带认 `3-5` 这种范围写法)。认不出返回空 —— 调用方据此**如实退回**,
/// 绝不当成「全转」。0 页不存在(页码从 1 起),丢掉。print_file 选页复用(pub(super))。
pub(super) fn parse_pages(v: &serde_json::Value) -> Vec<usize> {
    fn from_str(s: &str) -> Vec<usize> {
        let mut out = Vec::new();
        for part in s.split([',', '、', ';', ' ']).filter(|p| !p.trim().is_empty()) {
            let part = part.trim();
            if let Some((a, b)) = part.split_once('-') {
                if let (Ok(a), Ok(b)) = (a.trim().parse::<usize>(), b.trim().parse::<usize>()) {
                    if a >= 1 && b >= a {
                        out.extend(a..=b);
                        continue;
                    }
                }
            }
            if let Ok(n) = part.parse::<usize>() {
                if n >= 1 {
                    out.push(n);
                }
            }
        }
        out
    }
    match v {
        serde_json::Value::Array(a) => a
            .iter()
            .flat_map(|e| match e {
                serde_json::Value::Number(n) => n.as_u64().filter(|n| *n >= 1).map(|n| n as usize).into_iter().collect(),
                serde_json::Value::String(s) => from_str(s),
                _ => Vec::new(),
            })
            .collect(),
        serde_json::Value::Number(n) => n.as_u64().filter(|n| *n >= 1).map(|n| n as usize).into_iter().collect(),
        serde_json::Value::String(s) => from_str(s),
        _ => Vec::new(),
    }
}
/// 渲染目标宽(px):单据/文档在手机上看清楚够用,又不产出巨图。
const RENDER_TARGET_WIDTH: i32 = 1600;

/// pdfium C 库全局唯一实例纪律:并发绑定/释放(FPDF_InitLibrary/Destroy)会互相打;
/// 一次只跑一份渲染(轮内工具并发是常态,别赌运气)。
static PDFIUM_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// 绑定每进程只许一次(pdfium-render 0.9 二次 bind 报 AlreadyInitialized,e2e 实锤)
/// → 首次绑定后进程级缓存复用;库文件路径稳定(组件目录),不存在"换路径重绑"。
static PDFIUM: std::sync::OnceLock<pdfium_render::prelude::Pdfium> = std::sync::OnceLock::new();

/// pdfium 的**唯一**取用口(pdf_to_png 与 print_file 共用):锁全局闸 → 进程级单例绑定 →
/// 把实例交给闭包。所有要碰 pdfium 的代码一律走这里 —— 各自另开 OnceLock = 第二次 bind
/// 必炸 AlreadyInitialized(上面那条纪律的模块间版本)。阻塞调用,须在 spawn_blocking 里。
pub(super) fn with_pdfium<R>(
    lib: &Path,
    f: impl FnOnce(&pdfium_render::prelude::Pdfium) -> anyhow::Result<R>,
) -> anyhow::Result<R> {
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
    f(pdfium)
}

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
            .map(super::expand_home) // 「~/xxx」宽容展开(§4.4)
            .context("缺少 path 参数(PDF 的绝对路径)")?;
        let pdf = PathBuf::from(&path);
        anyhow::ensure!(pdf.is_absolute(), "path 需要绝对路径,收到: {path}");
        anyhow::ensure!(pdf.is_file(), "文件不存在: {path}");
        // ⚠️ **给了 pages 却没认出来,绝不能当成「全转」(2026-08-22 修)**:原先是
        // `as_array` 拿不到就 `unwrap_or_default()` = 空 = 全转 —— 模型把参数发成
        // `pages: 3` 或 `pages: "1,3"`(流式 JSON 的常态,arg_bool/arg_u64 早就为此宽容过),
        // 于是「只转第 3 页」被**悄悄变成整本转**、还报成功。宽容认几种写法;认不出就如实退回。
        let pages: Vec<usize> = match args.get("pages") {
            None | Some(serde_json::Value::Null) => Vec::new(), // 没给 = 全转(本来的语义)
            Some(v) => {
                let got = parse_pages(v);
                anyhow::ensure!(
                    !got.is_empty(),
                    "pages 没看懂:{v}——要一串页码,比如 [1, 3, 5];不指定就是整本都转"
                );
                got
            }
        };
        anyhow::ensure!(
            pages.len() <= PDF_MAX_PAGES,
            "一次最多转 {PDF_MAX_PAGES} 页,收到 {} 页——分批来",
            pages.len()
        );
        let out_dir = match args.get("dir").and_then(serde_json::Value::as_str).map(str::trim) {
            Some(d) if !d.is_empty() => {
                let p = PathBuf::from(super::expand_home(d)); // 「~/xxx」宽容展开(§4.4)
                anyhow::ensure!(p.is_absolute(), "dir 需要绝对路径,收到: {d}");
                p
            }
            _ => pdf.parent().map(Path::to_path_buf).unwrap_or_else(std::env::temp_dir),
        };
        // 授权圈(§7.2):读源 PDF + 往产物目录落新文件;授权过了才建目标文件夹
        super::guard::ensure(ctx, super::guard::Access::Read, std::slice::from_ref(&path)).await?;
        super::guard::ensure(
            ctx,
            super::guard::Access::Create,
            &[out_dir.to_string_lossy().into_owned()],
        )
        .await?;
        std::fs::create_dir_all(&out_dir)
            .with_context(|| format!("建不了目标文件夹 {}", out_dir.display()))?;

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
    with_pdfium(lib, |pdfium| render_pages_with(pdfium, pdf, pages, out_dir))
}

fn render_pages_with(
    pdfium: &pdfium_render::prelude::Pdfium,
    pdf: &Path,
    pages: &[usize],
    out_dir: &Path,
) -> anyhow::Result<Vec<PathBuf>> {
    use pdfium_render::prelude::*;
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

    /// **`pages` 认不出来时绝不能当「全转」(2026-08-22)**:模型把数组参数发成
    /// 单个数字 / 逗号串是流式 JSON 的常态(arg_bool/arg_u64 早为此宽容过)。原先
    /// `as_array` 拿不到就落空 = 全转,「只转第 3 页」被悄悄变成整本转还报成功。
    #[test]
    fn pages_accepts_loose_forms_and_refuses_to_guess() {
        use serde_json::json;
        // 正常形
        assert_eq!(parse_pages(&json!([1, 3, 5])), vec![1, 3, 5]);
        // 模型常发的几种松散形
        assert_eq!(parse_pages(&json!(3)), vec![3], "单个数字");
        assert_eq!(parse_pages(&json!("3")), vec![3], "数字字符串");
        assert_eq!(parse_pages(&json!(["1", "2"])), vec![1, 2], "数组里是字符串");
        assert_eq!(parse_pages(&json!("1,3")), vec![1, 3], "逗号串");
        assert_eq!(parse_pages(&json!("2-4")), vec![2, 3, 4], "范围写法");
        assert_eq!(parse_pages(&json!("1, 3-4")), vec![1, 3, 4], "混着来");
        // 页码从 1 起,0 和负数不是页
        assert_eq!(parse_pages(&json!([0, 2])), vec![2]);
        // 真认不出 → 空(调用方据此如实退回,**不是**当成全转)
        assert!(parse_pages(&json!("封面那几页")).is_empty());
        assert!(parse_pages(&json!({"from": 1})).is_empty());
        assert!(parse_pages(&json!(true)).is_empty());
    }

    fn ctx(tag: &str) -> (ToolCtx, PathBuf) {
        let dir = std::env::temp_dir().join(format!("lw-pdf-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("t.db"));
        let store = Store::open(&dir.join("t.db")).unwrap();
        let me = store.users.ensure_default_user().unwrap();
        let media = MediaRuntime::new(dir.clone(), store.clone(), crate::bus::Bus::new());
        (ToolCtx { user_id: me.id, conv_id: 1, media, store, web: None, voice: None, confirm: None, grants: Default::default(), agent: None }, dir)
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
