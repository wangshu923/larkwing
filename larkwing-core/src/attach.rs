//! 媒体附件抽取(媒体输入 PLAN §9):纯函数、无状态。
//! 图交给视觉模型直读(不在此解码);文档抽成纯文字,由 engine 当轮注入提示。
//! 人格中立底座:这里只做格式 → 文字,不碰任何话术。

/// 是不是图(交视觉模型直读,不抽文字)。
pub fn is_image(mime: &str) -> bool {
    mime.starts_with("image/")
}

/// 文档抽文字(0.2.0「文档支持」):txt/md/源码直读;OOXML(docx/pptx/xlsx)解 zip 取正文,
/// xlsx 转 CSV 保住行列结构;PDF 抽文字层(扫描件无文字层 → None,栅格化转图是 stretch)。
/// 老二进制 .doc/.ppt/.xls 不支持。None = 抽不出文字,调用方按「读不出内容」兜底。
pub fn extract_doc_text(name: &str, mime: &str, bytes: &[u8]) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    // PDF:文字层(pdf-extract);扫描件没有文字层 → 抽空 → None(按读不出兜底)
    if mime == "application/pdf" || lower.ends_with(".pdf") {
        return extract_pdf(bytes);
    }
    if lower.ends_with(".docx")
        || mime == "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
    {
        return extract_docx(bytes);
    }
    // PowerPoint:各页文字(ppt/slides/slideN.xml)
    if lower.ends_with(".pptx")
        || mime == "application/vnd.openxmlformats-officedocument.presentationml.presentation"
    {
        return extract_pptx(bytes);
    }
    // Excel:转 CSV(每 sheet 一段,保住行列结构,模型最好懂)
    if lower.ends_with(".xlsx")
        || mime == "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
    {
        return extract_xlsx(bytes);
    }
    // 文本类:text/*、json/xml/csv、常见源码,或没 mime 但看着就是 UTF-8 文本
    if mime.starts_with("text/") || is_texty(&lower) || looks_utf8(bytes) {
        let s = String::from_utf8_lossy(bytes);
        let s = s.trim();
        return (!s.is_empty()).then(|| s.to_string());
    }
    None
}

fn is_texty(lower: &str) -> bool {
    const EXT: &[&str] = &[
        ".txt", ".md", ".markdown", ".json", ".xml", ".csv", ".log", ".rs", ".py", ".js", ".ts",
        ".vue", ".html", ".css", ".toml", ".yaml", ".yml", ".sh", ".c", ".cpp", ".h", ".java", ".go",
    ];
    EXT.iter().any(|e| lower.ends_with(e))
}

/// 粗判 UTF-8 文本:前 4KB 无 NUL 且能解 UTF-8 → 当文本兜(没 mime/扩展名时)。
fn looks_utf8(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(4096)];
    !head.contains(&0) && std::str::from_utf8(head).is_ok()
}

/// .docx = zip,正文在 word/document.xml:</w:p> 当段落换行,剥标签留文本,粗还原实体。
/// 不求富格式,够喂模型即可;读不出返回 None。
fn extract_docx(bytes: &[u8]) -> Option<String> {
    use std::io::Read;
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).ok()?;
    let mut xml = String::new();
    zip.by_name("word/document.xml").ok()?.read_to_string(&mut xml).ok()?;
    let text = strip_xml_text(&xml, "</w:p>");
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// .pptx = zip,正文分散在 ppt/slides/slideN.xml(每页一份);文本在 <a:t>,段落 </a:p>。
/// 按页号排序逐页抽,页间空行分隔;够喂模型即可。读不出返回 None。
fn extract_pptx(bytes: &[u8]) -> Option<String> {
    use std::io::Read;
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).ok()?;
    // 收集 slide 文件名(ppt/slides/slideN.xml,排除 _rels)并按页号自然排序
    let mut slides: Vec<String> = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().map(|f| f.name().to_string()))
        .filter(|n| {
            n.starts_with("ppt/slides/slide") && n.ends_with(".xml") && !n.contains("_rels")
        })
        .collect();
    slides.sort_by_key(|n| slide_index(n));
    let mut out = String::new();
    for name in slides {
        let mut xml = String::new();
        if zip.by_name(&name).ok()?.read_to_string(&mut xml).is_err() {
            continue;
        }
        let page = strip_xml_text(&xml, "</a:p>");
        let page = page.trim();
        if !page.is_empty() {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(page);
        }
    }
    let out = out.trim();
    (!out.is_empty()).then(|| out.to_string())
}

/// 从 "ppt/slides/slide12.xml" 取页号 12(取不到给大数,排末尾)。
fn slide_index(name: &str) -> u32 {
    name.trim_start_matches("ppt/slides/slide")
        .trim_end_matches(".xml")
        .parse()
        .unwrap_or(u32::MAX)
}

/// .xlsx → CSV(用 calamine 读,正确处理共享串/类型/日期)。每个 sheet 一段:`# 表名` 标题 +
/// CSV 行(RFC4180 转义);空表跳过。多 sheet 用空行分隔。读不出 / 全空返回 None。
fn extract_xlsx(bytes: &[u8]) -> Option<String> {
    use calamine::{Data, Reader, Xlsx};
    let mut wb: Xlsx<_> = calamine::open_workbook_from_rs(std::io::Cursor::new(bytes)).ok()?;
    let mut out = String::new();
    for name in wb.sheet_names() {
        let Ok(range) = wb.worksheet_range(&name) else { continue };
        if range.is_empty() {
            continue;
        }
        let mut block = String::new();
        for row in range.rows() {
            let line: Vec<String> = row
                .iter()
                .map(|cell| match cell {
                    Data::Empty => String::new(),
                    Data::String(s) => s.clone(),
                    Data::Float(f) => f.to_string(),
                    Data::Int(i) => i.to_string(),
                    Data::Bool(b) => b.to_string(),
                    other => other.to_string(),
                })
                .collect();
            // 整行全空不输出(尾部空行常见)
            if line.iter().all(|c| c.is_empty()) {
                continue;
            }
            block.push_str(&line.iter().map(|c| csv_escape(c)).collect::<Vec<_>>().join(","));
            block.push('\n');
        }
        if block.trim().is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format!("# {name}\n{block}"));
    }
    let out = out.trim();
    (!out.is_empty()).then(|| out.to_string())
}

/// CSV 字段转义(RFC4180):含逗号/引号/换行 → 包双引号 + 内部引号翻倍。
fn csv_escape(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// PDF 文字层抽取(pdf-extract)。扫描件无文字层 → 抽空 → None;解析失败也 None(按读不出兜底,
/// 不 panic、不阻塞)。栅格化转图喂视觉 = 0.2.0 stretch,这里只做文字层。
fn extract_pdf(bytes: &[u8]) -> Option<String> {
    // pdf-extract 内部可能对畸形 PDF panic → catch_unwind 兜住(只此一处不可信输入解析)
    let parsed = std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(bytes)).ok()?;
    let text = parsed.ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// 极简 XML 正文抽取:`para_end`(段落结束 tag)→ 换行;剥 <...> 标签;粗还原常见实体。
fn strip_xml_text(xml: &str, para_end: &str) -> String {
    let with_breaks = xml.replace(para_end, "\n");
    let mut out = String::with_capacity(with_breaks.len() / 2);
    let mut in_tag = false;
    for c in with_breaks.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_recognized_docs_not() {
        assert!(is_image("image/jpeg"));
        assert!(is_image("image/png"));
        assert!(!is_image("application/pdf"));
        assert!(!is_image("text/plain"));
    }

    #[test]
    fn plain_text_extracts_by_mime_and_extension() {
        assert_eq!(
            extract_doc_text("a.txt", "text/plain", b"hello\nworld").as_deref(),
            Some("hello\nworld")
        );
        // 没 mime,靠扩展名
        assert_eq!(extract_doc_text("note.md", "", b"# Title").as_deref(), Some("# Title"));
        // 全空白 → None
        assert!(extract_doc_text("a.txt", "text/plain", b"   ").is_none());
    }

    #[test]
    fn junk_pdf_reads_to_none() {
        // 畸形 PDF(无文字层 / 解析失败)→ None,不 panic
        assert!(extract_doc_text("x.pdf", "application/pdf", b"%PDF-1.4 junk").is_none());
    }

    #[test]
    fn xml_strip_keeps_text_breaks_paragraphs_and_unescapes() {
        let xml = r#"<w:p><w:r><w:t>你好</w:t></w:r></w:p><w:p><w:r><w:t>世界 &amp; 朋友</w:t></w:r></w:p>"#;
        let out = strip_xml_text(xml, "</w:p>");
        assert!(out.contains("你好"));
        assert!(out.contains("世界 & 朋友"), "实体还原 + 标签剥离");
        assert!(out.contains('\n'), "段落分隔成换行");
    }

    #[test]
    fn pptx_strip_uses_drawingml_paragraph_break() {
        // pptx 文本在 <a:t>、段落 </a:p>:剥标签留文本、段落成换行
        let xml = r#"<a:p><a:r><a:t>第一行</a:t></a:r></a:p><a:p><a:r><a:t>第二行</a:t></a:r></a:p>"#;
        let out = strip_xml_text(xml, "</a:p>");
        assert!(out.contains("第一行") && out.contains("第二行"));
        assert!(out.contains('\n'), "幻灯片段落分隔成换行");
    }

    #[test]
    fn slide_index_sorts_by_page_number() {
        // 自然排序:slide2 在 slide10 之前(字典序会把 10 排前,这里要数值序)
        assert!(slide_index("ppt/slides/slide2.xml") < slide_index("ppt/slides/slide10.xml"));
        assert_eq!(slide_index("ppt/slides/slide7.xml"), 7);
    }

    #[test]
    fn csv_escape_quotes_only_when_needed() {
        assert_eq!(csv_escape("普通"), "普通");
        assert_eq!(csv_escape("a,b"), "\"a,b\"");
        assert_eq!(csv_escape("说\"引号\""), "\"说\"\"引号\"\"\"");
        assert_eq!(csv_escape("换\n行"), "\"换\n行\"");
    }
}
