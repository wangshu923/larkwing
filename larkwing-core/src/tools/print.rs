//! 能力轴:打印(把文件送到打印机)。家庭刚需闭环的最后一环:老师微信发试卷 →
//! 收件区「存到电脑」→ **「打出来」**。正交原语:只管「文件 → 打印机」,不认识试卷/发票
//! (§5;组合归模型)。
//!
//! 一期收 **PDF + 图片**(按内容认:%PDF 魔数 / 图片格式嗅探,假后缀照认——0.2.27 教训);
//! Office 文档如实退回指路「先转成 PDF」(docx 没有干净的本地转换组件,不赌第三方应用)。
//!
//! 平台(§8.1):
//! - **Windows(目标)= 自绘 GDI 路,不赌第三方应用**:PDF 用 pdfium 组件按打印分辨率
//!   栅格化(pdf_to_png 同一份绑定单例),图片 image 解码;逐页 `StartDoc/StretchDIBits`
//!   等比留边、宽图自动横放。不走 ShellExecute "print" 动词——它依赖装了什么软件、会弹
//!   UI、无成败反馈(§3.5)。列打印机/默认打印机走注册表(免 EnumPrinters 的 buffer 舞蹈)。
//! - **Mac(dev)**:`lp`(CUPS)直送,开发机可验基本链;真验收在 Windows 真机(PLAN)。
//!
//! 不过确认闸(家庭信任圈内、后果轻,send_file 同级);物理灾难由份数闸挡(copies ≤ 20、
//! 总页次 ≤ 100,超了如实退回让用户明说)。送进打印队列即返回——队列之后的事(缺纸/卡纸)
//! 归打印机自己的提示。

use std::path::Path;

use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolSpec};

/// 一次最多打几个文件(照片场景批量;超了如实退回分批)。
const PRINT_MAX_FILES: usize = 10;
/// 份数上限(防「打 999 份」的物理灾难;超了如实退回,不静默钳)。
const PRINT_MAX_COPIES: u64 = 20;
/// 单个 PDF 一次最多打多少页(不给 pages 时全打的封顶;更厚的用 pages 分批)。
/// 只有 Windows 自绘路消费(mac 交给 lp,量闸随 copies 参数;§8.1)。
#[cfg_attr(not(windows), allow(dead_code))]
const PRINT_MAX_PAGES: usize = 50;
/// 页数 × 份数的总闸(GDI 路每份重渲染,别让一次调用占住打印机十分钟)。
#[cfg_attr(not(windows), allow(dead_code))]
const PRINT_MAX_SHEETS: usize = 100;
/// PDF 打印渲染宽(px):约 A4 300dpi,打印清晰够用又不爆内存(600dpi 一页 140MB)。
#[cfg(windows)]
const PRINT_RENDER_WIDTH: i32 = 2480;

pub(super) struct PrintFile {
    spec: ToolSpec,
}

impl PrintFile {
    pub(super) fn new() -> PrintFile {
        PrintFile {
            spec: ToolSpec {
                name: "print_file",
                description: "把文件用打印机打印出来(纸质的)。能直接打 PDF 和图片\
                              (png/jpg 等);Word/Excel 这类文档打不了,如实告诉用户先转成\
                              PDF。paths 传文件绝对路径(手机发来的文件先看聊天里报的\
                              「已存到本地」路径)。缺省用系统默认打印机;用户点名了哪台才传\
                              printer。要打某几页传 pages(只对单个 PDF 有效);多印几份传\
                              copies。送进打印队列就算完成,把「已发给哪台打印机」告诉用户。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "paths": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "要打印的文件绝对路径(PDF 或图片),最多 10 个"
                        },
                        "printer": {
                            "type": "string",
                            "description": "打印机名字(用户点名才传;省略 = 系统默认打印机)"
                        },
                        "copies": {
                            "type": "integer",
                            "description": "打几份(1–20;省略 = 1)"
                        },
                        "pages": {
                            "type": "array",
                            "items": { "type": "integer" },
                            "description": "只打这些页,从 1 数(只在打单个 PDF 时有效;省略 = 整份)"
                        }
                    },
                    "required": ["paths"]
                }),
                timeout: std::time::Duration::from_secs(300),
                ui_key: "tool.print_file",
            },
        }
    }
}

#[async_trait]
impl Tool for PrintFile {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        // paths:数组为正形;单个字符串也认(流式 JSON quirk,arg_bool 同族)
        let paths: Vec<String> = match args.get("paths") {
            Some(serde_json::Value::Array(a)) => a
                .iter()
                .filter_map(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(super::expand_home)
                .collect(),
            Some(serde_json::Value::String(s)) if !s.trim().is_empty() => {
                vec![super::expand_home(s.trim())]
            }
            _ => Vec::new(),
        };
        anyhow::ensure!(!paths.is_empty(), "缺少 paths 参数(要打印的文件绝对路径)");
        anyhow::ensure!(
            paths.len() <= PRINT_MAX_FILES,
            "一次最多打 {PRINT_MAX_FILES} 个文件,收到 {} 个——分批来",
            paths.len()
        );
        for p in &paths {
            anyhow::ensure!(Path::new(p).is_absolute(), "要绝对路径,收到: {p}");
            anyhow::ensure!(Path::new(p).is_file(), "文件不存在: {p}");
        }
        let copies = super::arg_u64(&args, "copies", 1).max(1);
        anyhow::ensure!(
            copies <= PRINT_MAX_COPIES,
            "一次最多打 {PRINT_MAX_COPIES} 份,收到 {copies} 份——真要更多请用户明确说了再分批"
        );
        let pages: Vec<usize> = match args.get("pages") {
            None | Some(serde_json::Value::Null) => Vec::new(),
            Some(v) => {
                let got = super::pdf::parse_pages(v);
                anyhow::ensure!(
                    !got.is_empty(),
                    "pages 没看懂:{v}——要一串页码,比如 [1, 3];不指定就是整份都打"
                );
                got
            }
        };
        anyhow::ensure!(
            pages.is_empty() || paths.len() == 1,
            "pages 只在打单个 PDF 时有效——多个文件别带 pages,分开打"
        );

        let printer: Option<String> = args
            .get("printer")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        // 授权圈(§7.2):打印 = 读
        super::guard::ensure(ctx, super::guard::Access::Read, &paths).await?;

        // 按内容认格式(假后缀照认);认不出的如实退回,别送进队列打出乱码
        let mut kinds = Vec::with_capacity(paths.len());
        for p in &paths {
            kinds.push(sniff_kind(Path::new(p))?);
        }
        if !pages.is_empty() {
            anyhow::ensure!(
                matches!(kinds[0], FileKind::Pdf),
                "pages(指定页码)只对 PDF 有意义,这个文件是图片——去掉 pages 整张打"
            );
        }

        print_files(ctx, &paths, &kinds, printer.as_deref(), copies as u32, &pages).await
    }
}

/// 能打的两类(按内容判)。
#[derive(Debug, Clone, Copy, PartialEq)]
enum FileKind {
    Pdf,
    Image,
}

/// 按文件**内容**认格式:%PDF 魔数 → PDF;图片格式嗅探(不解码)→ 图片;
/// 其他如实退回(Office 文档指路转 PDF)。
fn sniff_kind(p: &Path) -> anyhow::Result<FileKind> {
    use std::io::Read;
    let mut head = [0u8; 5];
    let n = std::fs::File::open(p)
        .and_then(|mut f| f.read(&mut head))
        .map_err(|e| anyhow::anyhow!("读不了文件 {}:{e}", p.display()))?;
    if head[..n].starts_with(b"%PDF") {
        return Ok(FileKind::Pdf);
    }
    // 图片:只按内容嗅探。⚠️ 不能用 `ImageReader::open(p)` —— 它带着扩展名提示,内容认不出时
    // 会保留从后缀猜的格式(image 0.25 `with_guessed_format` 的文档化行为)→ 「课程表.png」
    // 内容是垃圾也被判成图片、真送进打印队列(复审实锤)。从裸 reader 起步,猜不出 = 不是图。
    let is_image = std::fs::File::open(p)
        .ok()
        .map(std::io::BufReader::new)
        .and_then(|f| image::ImageReader::new(f).with_guessed_format().ok())
        .and_then(|r| r.format())
        .is_some();
    anyhow::ensure!(
        is_image,
        "打不了这种文件:{}——能直接打的是 PDF 和图片;Word/Excel 这类文档先转成 PDF 再打",
        p.display()
    );
    Ok(FileKind::Image)
}

// ───────────────────────────── Mac(dev):lp 直送 ─────────────────────────────

#[cfg(target_os = "macos")]
async fn print_files(
    _ctx: &ToolCtx,
    paths: &[String],
    kinds: &[FileKind],
    printer: Option<&str>,
    copies: u32,
    pages: &[usize],
) -> anyhow::Result<String> {
    let mut done = Vec::new();
    for (p, kind) in paths.iter().zip(kinds) {
        let mut c = tokio::process::Command::new("lp");
        if let Some(pr) = printer {
            c.arg("-d").arg(pr);
        }
        if copies > 1 {
            c.arg("-n").arg(copies.to_string());
        }
        if !pages.is_empty() && *kind == FileKind::Pdf {
            let list: Vec<String> = pages.iter().map(usize::to_string).collect();
            c.arg("-P").arg(list.join(","));
        }
        c.arg(p);
        let out = c.output().await.map_err(|e| anyhow::anyhow!("lp 启动失败:{e}"))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let printers = String::from_utf8_lossy(
                &tokio::process::Command::new("lpstat")
                    .arg("-p")
                    .output()
                    .await
                    .map(|o| o.stdout)
                    .unwrap_or_default(),
            )
            .trim()
            .to_string();
            anyhow::bail!(
                "没打出去 {p}:{err}{}",
                if printers.is_empty() {
                    String::new()
                } else {
                    format!("\n这台电脑的打印机:\n{printers}")
                }
            );
        }
        done.push(p.as_str());
    }
    // mac 是开发机:printer 名交给 lp 自己解析(错了上面已带 lpstat 清单退回)
    Ok(report(&done, copies, pages, printer.unwrap_or("系统默认打印机")))
}

// ───────────────────────── Windows(目标):自绘 GDI ─────────────────────────

#[cfg(windows)]
async fn print_files(
    ctx: &ToolCtx,
    paths: &[String],
    kinds: &[FileKind],
    printer: Option<&str>,
    copies: u32,
    pages: &[usize],
) -> anyhow::Result<String> {
    // 打印机:点名的先认(名对不上带清单退回,别送给不存在的设备);缺省用系统默认
    let printer = match printer {
        Some(p) => {
            let known = win::printers();
            // UNC 形(\\server\printer)的网络打印机交给 CreateDCW 自己判——按用户装的连接
            // 未必都在注册表清单里(复审实锤),名单只用来拦本机打印机的手误
            let unc = p.starts_with("\\\\");
            if !unc && !known.is_empty() && !known.iter().any(|k| k.eq_ignore_ascii_case(p)) {
                anyhow::bail!("没有叫「{p}」的打印机;装了这些:{}", known.join("、"));
            }
            p.to_string()
        }
        None => match win::default_printer() {
            Some(p) => p,
            None => {
                let known = win::printers();
                anyhow::bail!(
                    "这台电脑没有默认打印机{}",
                    if known.is_empty() {
                        "(一台打印机都没装)".to_string()
                    } else {
                        format!(";装了这些:{}(用 printer 点名一台)", known.join("、"))
                    }
                )
            }
        },
    };

    // 总页次闸(每份重渲染,别让一次调用占住打印机十分钟)
    let mut total_sheets = 0usize;
    for (p, kind) in paths.iter().zip(kinds) {
        total_sheets += match kind {
            FileKind::Image => copies as usize,
            FileKind::Pdf => {
                let n = if pages.is_empty() {
                    let lib = ctx.media.ensure_pdfium().await?;
                    let path = std::path::PathBuf::from(p);
                    tokio::task::spawn_blocking(move || {
                        super::pdf::with_pdfium(&lib, |pdfium| {
                            Ok(pdfium
                                .load_pdf_from_file(&path, None)
                                .map_err(|e| anyhow::anyhow!("打不开 PDF: {e:?}"))?
                                .pages()
                                .len() as usize)
                        })
                    })
                    .await??
                } else {
                    pages.len()
                };
                anyhow::ensure!(
                    n <= PRINT_MAX_PAGES,
                    "这份 PDF 要打 {n} 页,一次最多 {PRINT_MAX_PAGES} 页——用 pages 指定页码分批"
                );
                n * copies as usize
            }
        };
    }
    anyhow::ensure!(
        total_sheets <= PRINT_MAX_SHEETS,
        "这次要打 {total_sheets} 张纸(页数×份数),一次最多 {PRINT_MAX_SHEETS} 张——分批来"
    );

    let mut done = Vec::new();
    for (p, kind) in paths.iter().zip(kinds) {
        let (printer2, path, kind, pages2) =
            (printer.clone(), std::path::PathBuf::from(p), *kind, pages.to_vec());
        let lib = match kind {
            FileKind::Pdf => Some(ctx.media.ensure_pdfium().await?),
            FileKind::Image => None,
        };
        tokio::task::spawn_blocking(move || match kind {
            FileKind::Image => win::print_image(&printer2, &path, copies),
            FileKind::Pdf => win::print_pdf(&printer2, &lib.expect("PDF 必有组件"), &path, &pages2, copies),
        })
        .await??;
        done.push(p.as_str());
    }
    Ok(report(&done, copies, pages, &printer))
}

#[cfg(all(not(windows), not(target_os = "macos")))]
async fn print_files(
    _ctx: &ToolCtx,
    _paths: &[String],
    _kinds: &[FileKind],
    _printer: Option<&str>,
    _copies: u32,
    _pages: &[usize],
) -> anyhow::Result<String> {
    anyhow::bail!("这个系统上还不支持打印(如实告诉用户)")
}

/// 汇报(喂模型的观察):打了什么、几份、送到哪台。
fn report(done: &[&str], copies: u32, pages: &[usize], printer: &str) -> String {
    let names: Vec<String> = done
        .iter()
        .map(|p| {
            Path::new(p)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| (*p).to_string())
        })
        .collect();
    let mut out = format!("已发到打印机「{printer}」:{}", names.join("、"));
    if !pages.is_empty() {
        let list: Vec<String> = pages.iter().map(usize::to_string).collect();
        out.push_str(&format!("(第 {} 页)", list.join("、")));
    }
    if copies > 1 {
        out.push_str(&format!(" × {copies} 份"));
    }
    out.push_str("。已进打印队列;缺纸/卡纸这类事看打印机自己的提示。");
    out
}

// ───────────────────────── Windows GDI 机器件 ─────────────────────────

#[cfg(windows)]
mod win {
    use anyhow::{Context, Result};
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ};
    use winreg::RegKey;

    /// 已装打印机清单:注册表 Print\Printers 的子键名(EnumPrinters 的底层数据源,
    /// 免 unsafe buffer 舞蹈)。
    pub(super) fn printers() -> Vec<String> {
        let mut out = Vec::new();
        // 本机打印机
        let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
        if let Ok(k) = hklm
            .open_subkey_with_flags(r"SYSTEM\CurrentControlSet\Control\Print\Printers", KEY_READ)
        {
            out.extend(k.enum_keys().flatten());
        }
        // 按用户装的网络打印机连接:HKCU\Printers\Connections 的子键名把 `\` 编成 `,`
        // (`,,server,share` = `\\server\share`),还原后才与用户嘴里的名字对得上(复审实锤)
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        if let Ok(k) = hkcu.open_subkey_with_flags(r"Printers\Connections", KEY_READ) {
            out.extend(k.enum_keys().flatten().map(|n| n.replace(',', "\\")));
        }
        out
    }

    /// 系统默认打印机:HKCU Windows\Device 值,形如「HP LaserJet,winspool,Ne01:」取首段。
    pub(super) fn default_printer() -> Option<String> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let k = hkcu
            .open_subkey_with_flags(r"Software\Microsoft\Windows NT\CurrentVersion\Windows", KEY_READ)
            .ok()?;
        let device: String = k.get_value("Device").ok()?;
        let name = device.split(',').next()?.trim();
        (!name.is_empty()).then(|| name.to_string())
    }

    /// 打一张图片(单页 × copies)。
    pub(super) fn print_image(printer: &str, path: &std::path::Path, copies: u32) -> Result<()> {
        let img = image::open(path)
            .with_context(|| format!("图片解码失败 {}", path.display()))?
            .to_rgba8();
        let doc = doc_name(path);
        print_job(printer, &doc, copies, 1, |_page| Ok(img.clone()))
    }

    /// 打一份 PDF(选页 × copies;逐页渲染逐页送,不整本驻内存)。
    pub(super) fn print_pdf(
        printer: &str,
        lib: &std::path::Path,
        path: &std::path::Path,
        pages: &[usize],
        copies: u32,
    ) -> Result<()> {
        use pdfium_render::prelude::*;
        let doc_label = doc_name(path);
        crate::tools::pdf::with_pdfium(lib, |pdfium| {
            let doc = pdfium
                .load_pdf_from_file(path, None)
                .map_err(|e| anyhow::anyhow!("打不开 PDF(损坏或带密码?): {e:?}"))?;
            let total = doc.pages().len() as usize;
            anyhow::ensure!(total > 0, "这份 PDF 一页都没有");
            let selected: Vec<usize> = if pages.is_empty() {
                (1..=total).collect()
            } else {
                for &p in pages {
                    anyhow::ensure!(p >= 1 && p <= total, "页码 {p} 超出范围(共 {total} 页)");
                }
                pages.to_vec()
            };
            let cfg = PdfRenderConfig::new().set_target_width(super::PRINT_RENDER_WIDTH);
            print_job(printer, &doc_label, copies, selected.len(), |i| {
                let pno = selected[i];
                let page = doc
                    .pages()
                    .get((pno - 1) as i32)
                    .map_err(|e| anyhow::anyhow!("取第 {pno} 页失败: {e:?}"))?;
                // 拆成显式绑定:bitmap 借着 page,链式 + `?` 会让临时值活过 page(E0597,
                // Windows 交叉 check 实锤);pdf.rs 渲染同形
                let bitmap = page
                    .render_with_config(&cfg)
                    .map_err(|e| anyhow::anyhow!("第 {pno} 页渲染失败: {e:?}"))?;
                let img = bitmap
                    .as_image()
                    .map_err(|e| anyhow::anyhow!("第 {pno} 页位图转换失败: {e:?}"))?
                    .to_rgba8();
                Ok(img)
            })
        })
    }

    fn doc_name(path: &std::path::Path) -> String {
        path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or("打印".into())
    }

    /// 一个打印任务:StartDoc → (copies × 每页:渲染回调 → StretchDIBits)→ EndDoc。
    /// 页面回调按页号(0 起)现取位图 —— PDF 逐页渲染不整本驻内存(份数多时重渲染,
    /// 换内存上界;总量由 PRINT_MAX_SHEETS 闸)。
    fn print_job(
        printer: &str,
        doc: &str,
        copies: u32,
        page_count: usize,
        mut render: impl FnMut(usize) -> Result<image::RgbaImage>,
    ) -> Result<()> {
        use windows::core::{HSTRING, PCWSTR};
        use windows::Win32::Graphics::Gdi::{DeleteDC, GetDeviceCaps, HDC, HORZRES, VERTRES};
        use windows::Win32::Storage::Xps::{AbortDoc, EndDoc, EndPage, StartDocW, StartPage, DOCINFOW};

        /// DC 守卫:任何一步失败,drop 时先 AbortDoc(文档已 StartDoc 未 EndDoc 时显式让 spooler
        /// 丢掉半截文档——只 DeleteDC 是否丢弃取决于驱动,复审待核项)再 DeleteDC。
        struct Dc(HDC, std::cell::Cell<bool>);
        impl Drop for Dc {
            fn drop(&mut self) {
                unsafe {
                    if self.1.get() {
                        let _ = AbortDoc(self.0);
                    }
                    let _ = DeleteDC(self.0);
                }
            }
        }

        let driver = HSTRING::from("WINSPOOL");
        let device = HSTRING::from(printer);
        let hdc = unsafe {
            windows::Win32::Graphics::Gdi::CreateDCW(
                PCWSTR(driver.as_ptr()),
                PCWSTR(device.as_ptr()),
                PCWSTR::null(),
                None,
            )
        };
        anyhow::ensure!(!hdc.is_invalid(), "连不上打印机「{printer}」(名字对吗?在线吗?)");
        let dc = Dc(hdc, std::cell::Cell::new(false));
        let (pw, ph) =
            unsafe { (GetDeviceCaps(Some(dc.0), HORZRES), GetDeviceCaps(Some(dc.0), VERTRES)) };
        anyhow::ensure!(pw > 0 && ph > 0, "打印机「{printer}」没报出纸张尺寸");

        let doc_w = HSTRING::from(doc);
        let di = DOCINFOW {
            cbSize: std::mem::size_of::<DOCINFOW>() as i32,
            lpszDocName: PCWSTR(doc_w.as_ptr()),
            lpszOutput: PCWSTR::null(),
            lpszDatatype: PCWSTR::null(),
            fwType: 0,
        };
        unsafe {
            anyhow::ensure!(StartDocW(dc.0, &di) > 0, "打印任务没能开始(队列拒绝了)");
            dc.1.set(true); // 文档已开:中途任何失败 → drop 时 AbortDoc
            for _ in 0..copies {
                for i in 0..page_count {
                    let img = render(i)?;
                    anyhow::ensure!(StartPage(dc.0) > 0, "打印页开始失败");
                    blit_page(dc.0, &img, pw, ph)?;
                    anyhow::ensure!(EndPage(dc.0) > 0, "打印页收尾失败");
                }
            }
            anyhow::ensure!(EndDoc(dc.0) > 0, "打印任务收尾失败");
            dc.1.set(false); // 正常收尾,别再 AbortDoc
        }
        Ok(())
    }

    /// 把一页位图铺到打印 DC:透明合白底 → 宽图自动横放 → 等比 96% 居中。
    fn blit_page(
        hdc: windows::Win32::Graphics::Gdi::HDC,
        img: &image::RgbaImage,
        pw: i32,
        ph: i32,
    ) -> Result<()> {
        use windows::Win32::Graphics::Gdi::{
            StretchDIBits, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, SRCCOPY,
        };

        // 宽图 + 纵向纸 → 转 90° 横放(不动打印机方向设置,转位图更稳)
        let rotated;
        let mut img = if img.width() > img.height() && pw < ph {
            rotated = image::imageops::rotate90(img);
            &rotated
        } else {
            img
        };
        // 等比 96% 落纸;比纸大的先缩到落纸尺寸再送 DIB(50MP 照片原样送 = 200MB × 份数进 spool,
        // 且 iw*ih*4 按 i32 算会溢出,复审实锤);比纸小的交给 StretchDIBits 放大
        let s = f64::min(pw as f64 / img.width() as f64, ph as f64 / img.height() as f64) * 0.96;
        let resized;
        if s < 1.0 {
            let w = ((img.width() as f64 * s).round() as u32).max(1);
            let h = ((img.height() as f64 * s).round() as u32).max(1);
            resized = image::imageops::resize(img, w, h, image::imageops::FilterType::Triangle);
            img = &resized;
        }
        let (iw, ih) = (img.width() as i32, img.height() as i32);

        // RGBA → BGRA + 透明像素合白底(纸是白的;透明区 RGB 常是 0 → 不合成会打出黑块)
        let mut buf = Vec::with_capacity(iw as usize * ih as usize * 4);
        for px in img.pixels() {
            let a = px[3] as u32;
            let f = |c: u8| ((c as u32 * a + 255 * (255 - a)) / 255) as u8;
            buf.extend([f(px[2]), f(px[1]), f(px[0]), 255]);
        }

        let (dw, dh) = if s < 1.0 { (iw, ih) } else { ((iw as f64 * s) as i32, (ih as f64 * s) as i32) };
        let (dx, dy) = ((pw - dw) / 2, (ph - dh) / 2);

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: iw,
                biHeight: -ih, // 负高 = top-down(image 的行序)
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let copied = unsafe {
            StretchDIBits(
                hdc,
                dx,
                dy,
                dw,
                dh,
                0,
                0,
                iw,
                ih,
                Some(buf.as_ptr() as *const core::ffi::c_void),
                &bmi,
                DIB_RGB_COLORS,
                SRCCOPY,
            )
        };
        anyhow::ensure!(copied != 0, "页面画不上去(StretchDIBits 失败)");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::MediaRuntime;
    use crate::store::Store;
    use std::path::PathBuf;

    fn ctx(tag: &str) -> (ToolCtx, PathBuf) {
        let dir = std::env::temp_dir().join(format!("lw-print-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("t.db"));
        let store = Store::open(&dir.join("t.db")).unwrap();
        let me = store.users.ensure_default_user().unwrap();
        let media = MediaRuntime::new(dir.clone(), store.clone(), crate::bus::Bus::new());
        (
            ToolCtx {
                user_id: me.id,
                conv_id: 1,
                media,
                store,
                web: None,
                voice: None,
                confirm: None,
                grants: Default::default(),
                agent: None,
            },
            dir,
        )
    }

    #[test]
    fn sniff_recognizes_pdf_and_image_by_content_not_name() {
        let dir = std::env::temp_dir().join(format!("lw-print-sniff-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // 假后缀:内容是 PDF、名字是 .txt(0.2.27「按内容认」同款)
        let pdf = dir.join("其实是pdf.txt");
        std::fs::write(&pdf, b"%PDF-1.4\n...").unwrap();
        assert!(matches!(sniff_kind(&pdf).unwrap(), FileKind::Pdf));
        // 真 PNG(1x1)
        let png = dir.join("图.png");
        image::RgbaImage::from_pixel(1, 1, image::Rgba([255, 0, 0, 255])).save(&png).unwrap();
        assert!(matches!(sniff_kind(&png).unwrap(), FileKind::Image));
        // 假图:名字是 .png、内容是垃圾 → 不能凭后缀放行(复审实锤:原先 ImageReader::open
        // 带着扩展名提示,内容认不出时保留后缀猜的格式,垃圾会被真送进打印队列)
        let fake = dir.join("课程表.png");
        std::fs::write(&fake, b"not an image at all").unwrap();
        assert!(sniff_kind(&fake).is_err(), "垃圾内容不能凭 .png 后缀判成图片");
        // Office 文档(zip 魔数)→ 如实退回指路
        let docx = dir.join("作业.docx");
        std::fs::write(&docx, b"PK\x03\x04...").unwrap();
        let e = sniff_kind(&docx).unwrap_err();
        assert!(e.to_string().contains("先转成 PDF"), "{e:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rejects_bad_args_before_touching_printer() {
        let (ctx, dir) = ctx("args");
        let tool = PrintFile::new();
        // 缺 paths / 相对路径 / 不存在
        assert!(tool.run(serde_json::json!({}), &ctx).await.is_err());
        assert!(tool.run(serde_json::json!({"paths": ["a.pdf"]}), &ctx).await.is_err());
        assert!(tool
            .run(serde_json::json!({"paths": [dir.join("无.pdf").to_string_lossy()]}), &ctx)
            .await
            .is_err());
        // 份数超闸:如实退回,不静默钳
        let png = dir.join("p.png");
        image::RgbaImage::from_pixel(1, 1, image::Rgba([0, 0, 0, 255])).save(&png).unwrap();
        let e = tool
            .run(serde_json::json!({"paths": [png.to_string_lossy()], "copies": 99}), &ctx)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("最多打"), "{e:#}");
        // pages 配图片 / 配多文件:退回
        let e = tool
            .run(serde_json::json!({"paths": [png.to_string_lossy()], "pages": [1]}), &ctx)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("PDF"), "{e:#}");
        let png2 = dir.join("p2.png");
        image::RgbaImage::from_pixel(1, 1, image::Rgba([0, 0, 0, 255])).save(&png2).unwrap();
        let e = tool
            .run(
                serde_json::json!({"paths": [png.to_string_lossy(), png2.to_string_lossy()], "pages": [1]}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(e.to_string().contains("单个"), "{e:#}");
    }

    #[test]
    fn report_reads_naturally() {
        let r = report(&["/a/试卷.pdf", "/b/照片.png"], 2, &[1, 3], "HP LaserJet");
        assert!(r.contains("HP LaserJet"));
        assert!(r.contains("试卷.pdf、照片.png"));
        assert!(r.contains("第 1、3 页"));
        assert!(r.contains("× 2 份"));
    }
}
