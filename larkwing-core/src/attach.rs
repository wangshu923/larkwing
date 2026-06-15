//! 媒体附件抽取(媒体输入 PLAN §9):纯函数、无状态。
//! 图交给视觉模型直读(不在此解码);文档抽成纯文字,由 engine 当轮注入提示。
//! 人格中立底座:这里只做格式 → 文字,不碰任何话术。

/// 是不是图(交视觉模型直读,不抽文字)。
pub fn is_image(mime: &str) -> bool {
    mime.starts_with("image/")
}

/// 文档抽文字:txt/md/源码直读;.docx 解 zip 取正文;PDF 暂未接解析器(返回 None)。
/// None = 抽不出文字,调用方按「读不出内容」兜底。
pub fn extract_doc_text(name: &str, mime: &str, bytes: &[u8]) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    // PDF:待接解析器(pdf-extract),先按读不出兜底,不阻塞其余格式
    if mime == "application/pdf" || lower.ends_with(".pdf") {
        return None;
    }
    if lower.ends_with(".docx")
        || mime == "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
    {
        return extract_docx(bytes);
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
    let text = strip_xml_text(&xml);
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// 极简 XML 正文抽取:</w:p>(段落结束)→ 换行;剥 <...> 标签;粗还原常见实体。
fn strip_xml_text(xml: &str) -> String {
    let with_breaks = xml.replace("</w:p>", "\n");
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
    fn pdf_is_deferred_to_none() {
        assert!(extract_doc_text("x.pdf", "application/pdf", b"%PDF-1.4 junk").is_none());
    }

    #[test]
    fn xml_strip_keeps_text_breaks_paragraphs_and_unescapes() {
        let xml = r#"<w:p><w:r><w:t>你好</w:t></w:r></w:p><w:p><w:r><w:t>世界 &amp; 朋友</w:t></w:r></w:p>"#;
        let out = strip_xml_text(xml);
        assert!(out.contains("你好"));
        assert!(out.contains("世界 & 朋友"), "实体还原 + 标签剥离");
        assert!(out.contains('\n'), "段落分隔成换行");
    }
}
