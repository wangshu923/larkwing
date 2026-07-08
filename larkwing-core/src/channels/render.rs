//! 渠道出站富格式(AGENT §7.7「富格式走输出后处理、不走 prompt」的兑现,2026-07-08):
//! 模型输出的 markdown → Telegram HTML(受控标签 + 严格转义)/ 钉钉走 msgtype 判定(mod 外用
//! `looks_markdown` + `md_title`)。纯函数、零 IO,单测友好。
//!
//! 立场:**探测保守 + 发送兜底**。`looks_markdown` 漏判 = 发纯文本(老行为,零风险);
//! Telegram HTML 由本模块只产受控标签、原始 HTML 一律转义成字面(模型写 `<b>` 也当文本),
//! 发送仍 400 时调用方降级纯文本重发(§3.5 不静默失败)。选 HTML 而非 MarkdownV2 =
//! 绕开 robot 踩过的 MarkdownV2 全字符转义地狱(HTML 只需转义 & < >)。

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

/// 回复里像有 markdown 结构吗?决定走富格式还是纯文本。**保守启发式**:短句寒暄/纯文本
/// 走老路是语义需要——钉钉 markdown 会把单换行折叠成一段,别让「好的\n已设好」变一行。
pub(crate) fn looks_markdown(text: &str) -> bool {
    if text.contains("```") || text.contains("**") || text.contains('`') {
        return true;
    }
    if text.contains("](") && text.contains('[') {
        return true; // [文字](链接)
    }
    text.lines().any(|l| {
        let t = l.trim_start();
        let heading = t.starts_with('#')
            && t.chars().take_while(|&c| c == '#').count() <= 6
            && t.trim_start_matches('#').starts_with(' ');
        let list = t.starts_with("- ") || t.starts_with("* ") || t.starts_with("> ");
        let ordered = t.len() > 2
            && t.as_bytes()[0].is_ascii_digit()
            && t[1..].starts_with(". ");
        let table = t.starts_with('|') && t.trim_end().ends_with('|');
        heading || list || ordered || table
    })
}

/// 钉钉 markdown 消息的 `title`(通知栏摘要,正文外的必填字段):首行删掉 markdown 记号字符取头
/// (摘要不求保真,别把 `**` 带进通知栏)。
pub(crate) fn md_title(text: &str) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let line: String = line.chars().filter(|c| !matches!(c, '*' | '`' | '#' | '>' | '|')).collect();
    let head: String = line.trim().trim_start_matches(['-', ' ']).chars().take(20).collect();
    if head.is_empty() { "…".into() } else { head }
}

/// HTML 文本转义(Telegram HTML 模式要求的三个)。
fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// 属性值转义(href):文本三件套 + 双引号。
fn esc_attr(s: &str) -> String {
    esc(s).replace('"', "&quot;")
}

/// 代码块语言名清洗(进 `class="language-…"` 属性):只留字母数字与 `_ + - #`,防属性注入。
fn clean_lang(lang: &str) -> String {
    lang.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-' | '#')).collect()
}

/// markdown → Telegram HTML(受控标签子集:b/i/s/code/pre/a/blockquote)。
/// 结构映射:标题→加粗行;列表→"• "/"N. "前缀行;表格→`<pre>` 里 " | " 对齐文本
/// (TG 不支持表格标签);原始 HTML 转义成字面文本;换行字符 TG 原样保留,无需 <br>。
pub(crate) fn to_telegram_html(md: &str) -> String {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(md, opts);

    let mut out = String::with_capacity(md.len() + 64);
    // 有序列表当前计数栈(None = 无序);深度 = 嵌套缩进。
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut in_table = false;
    let mut first_cell = true;
    let mut quote_depth = 0usize;
    let mut image_url: Vec<String> = Vec::new();
    // 当前代码块开的是 <pre><code…>(带语言)还是裸 <pre>,闭合要成对。
    let mut code_open_with_lang = false;

    for ev in parser {
        match ev {
            Event::Start(tag) => match tag {
                // 表格整体进 <pre>(等宽最像表);格内强调标签丢弃只留文本
                Tag::Table(_) => {
                    in_table = true;
                    out.push_str("<pre>");
                }
                Tag::TableHead | Tag::TableRow => first_cell = true,
                Tag::TableCell => {
                    if !first_cell {
                        out.push_str(" | ");
                    }
                    first_cell = false;
                }
                Tag::Strong if !in_table => out.push_str("<b>"),
                Tag::Emphasis if !in_table => out.push_str("<i>"),
                Tag::Strikethrough if !in_table => out.push_str("<s>"),
                Tag::Heading { .. } if !in_table => out.push_str("<b>"),
                Tag::BlockQuote(_) => {
                    quote_depth += 1;
                    out.push_str("<blockquote>");
                }
                Tag::CodeBlock(kind) => match kind {
                    CodeBlockKind::Fenced(lang) if !lang.is_empty() => {
                        code_open_with_lang = true;
                        out.push_str(&format!("<pre><code class=\"language-{}\">", clean_lang(&lang)));
                    }
                    _ => {
                        code_open_with_lang = false;
                        out.push_str("<pre>");
                    }
                },
                Tag::List(start) => lists.push(start),
                Tag::Item => {
                    if !out.is_empty() && !out.ends_with('\n') {
                        out.push('\n');
                    }
                    // 嵌套层缩进两空格;标记按栈顶类型
                    out.push_str(&"  ".repeat(lists.len().saturating_sub(1)));
                    match lists.last_mut() {
                        Some(Some(n)) => {
                            out.push_str(&format!("{n}. "));
                            *n += 1;
                        }
                        _ => out.push_str("• "),
                    }
                }
                Tag::Link { dest_url, .. } if !in_table => {
                    out.push_str(&format!("<a href=\"{}\">", esc_attr(&dest_url)));
                }
                Tag::Image { dest_url, .. } => image_url.push(dest_url.to_string()),
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Table => {
                    in_table = false;
                    while out.ends_with('\n') {
                        out.pop();
                    }
                    out.push_str("</pre>\n");
                }
                TagEnd::TableHead | TagEnd::TableRow => out.push('\n'),
                TagEnd::Strong if !in_table => out.push_str("</b>"),
                TagEnd::Emphasis if !in_table => out.push_str("</i>"),
                TagEnd::Strikethrough if !in_table => out.push_str("</s>"),
                TagEnd::Heading(_) if !in_table => out.push_str("</b>\n"),
                TagEnd::BlockQuote(_) => {
                    quote_depth = quote_depth.saturating_sub(1);
                    while out.ends_with('\n') {
                        out.pop();
                    }
                    out.push_str("</blockquote>\n");
                }
                TagEnd::CodeBlock => {
                    out.push_str(if code_open_with_lang { "</code></pre>\n" } else { "</pre>\n" });
                }
                TagEnd::Paragraph => out.push_str(if quote_depth > 0 || !lists.is_empty() { "\n" } else { "\n\n" }),
                TagEnd::List(_) => {
                    lists.pop();
                    if lists.is_empty() {
                        if !out.ends_with('\n') {
                            out.push('\n');
                        }
                        out.push('\n');
                    }
                }
                TagEnd::Item => {
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                }
                TagEnd::Link if !in_table => out.push_str("</a>"),
                TagEnd::Image => {
                    if let Some(u) = image_url.pop() {
                        out.push_str(&format!("({})", esc(&u)));
                    }
                }
                _ => {}
            },
            Event::Text(t) => out.push_str(&esc(&t)),
            Event::Code(t) => {
                if in_table {
                    out.push_str(&esc(&t));
                } else {
                    out.push_str(&format!("<code>{}</code>", esc(&t)));
                }
            }
            // 原始 HTML 一律转义成字面文本:模型写 <b> 也当文字,绝不透传不受控标签
            Event::Html(h) | Event::InlineHtml(h) => out.push_str(&esc(&h)),
            Event::SoftBreak | Event::HardBreak => out.push('\n'),
            Event::Rule => out.push_str("———\n"),
            Event::TaskListMarker(done) => out.push_str(if done { "[x] " } else { "[ ] " }),
            Event::FootnoteReference(r) => out.push_str(&esc(&r)),
            _ => {}
        }
    }
    // 收尾:3+ 连换行折成 2,去首尾空白
    let mut s = out;
    while s.contains("\n\n\n") {
        s = s.replace("\n\n\n", "\n\n");
    }
    s.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_chat_is_not_markdown() {
        assert!(!looks_markdown("好的,已设好提醒"));
        assert!(!looks_markdown("三点吃药\n记得带水"));
        // 常见结构都认得出
        assert!(looks_markdown("**重点**记一下"));
        assert!(looks_markdown("- 鸡蛋\n- 牛奶"));
        assert!(looks_markdown("1. 先热锅"));
        assert!(looks_markdown("# 今日安排"));
        assert!(looks_markdown("看[这里](https://a.b)"));
        assert!(looks_markdown("用 `cargo test` 跑"));
        assert!(looks_markdown("| 名 | 价 |\n| a | 1 |"));
    }

    #[test]
    fn bold_italic_code_link_render_to_controlled_tags() {
        let html = to_telegram_html("**粗** *斜* `code` [去](https://e.com/a?b=1&c=2)");
        assert_eq!(
            html,
            "<b>粗</b> <i>斜</i> <code>code</code> <a href=\"https://e.com/a?b=1&amp;c=2\">去</a>"
        );
    }

    #[test]
    fn raw_html_is_escaped_to_literal_text() {
        // 模型写原始 HTML → 当字面文本,绝不透传(防不受控标签 400 / 注入)
        let html = to_telegram_html("hi <script>x</script> <b>粗</b>");
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(!html.contains("<b>粗"), "原始 <b> 也转义:{html}");
    }

    #[test]
    fn heading_list_and_codeblock_shapes() {
        let html = to_telegram_html("# 标题\n\n- 甲\n- 乙\n\n1. 一\n2. 二\n\n```rust\nlet x = 1 < 2;\n```");
        assert!(html.contains("<b>标题</b>"), "{html}");
        assert!(html.contains("• 甲\n• 乙"), "{html}");
        assert!(html.contains("1. 一\n2. 二"), "{html}");
        assert!(html.contains("<pre><code class=\"language-rust\">"), "{html}");
        assert!(html.contains("let x = 1 &lt; 2;"), "代码内转义:{html}");
        assert!(html.trim_end().ends_with("</code></pre>"), "{html}");
    }

    #[test]
    fn table_degrades_into_pre_text() {
        let html = to_telegram_html("| 名 | 价 |\n| --- | --- |\n| 苹果 | 3 |\n| 梨 | 2 |");
        assert!(html.starts_with("<pre>"), "{html}");
        assert!(html.contains("名 | 价"), "{html}");
        assert!(html.contains("苹果 | 3"), "{html}");
        assert!(!html.contains("<b>"), "格内不产标签:{html}");
    }

    #[test]
    fn dingtalk_title_strips_markdown_head() {
        assert_eq!(md_title("# 今天的安排\n- 上午…"), "今天的安排");
        assert_eq!(md_title("**到点啦**,该吃药了"), "到点啦,该吃药了");
        assert_eq!(md_title("\n\n好的"), "好的");
        // 超长首行截 20 字
        assert_eq!(md_title(&"长".repeat(50)).chars().count(), 20);
    }
}
