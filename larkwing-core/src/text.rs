//! 文本小工具的单一真相源。目前只有一件事:**按字符截断**。
//!
//! 为什么值得单独一个模块:这套截断散在五处各写一份(`llm/openai_compat`、`llm/anthropic_compat`、
//! `web`、`engine/turn`、`engine/mod`),而它有一条必须保住的性质 —— **绝不按字节切**
//! (多字节边界会 panic,或切出半个汉字;§7.8 ①的教训)。同一条性质散五份 = 五处都得记得。
//! 现在算法只此一份,各调用点只提供自己的「尾缀话术」(那才是站点自己的产品决定)。

/// 按**字符**截断,尾缀由「还剩多少字」算出来。
///
/// 装得下(`≤ max`)→ **原样返回,不产任何尾缀**;装不下 → 前 `max` 个字符 + `suffix(剩余字数)`。
/// `max` 数的是 **char 个数**(不是字节),故对中文 / emoji 都不会切出半个字符。
pub(crate) fn clip_fmt(s: &str, max: usize, suffix: impl FnOnce(usize) -> String) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}{}", suffix(n - max))
}

/// `clip_fmt` 的常量尾缀简写(尾缀不需要知道剩余字数时用)。`suffix = ""` = 纯截断不加话术。
pub(crate) fn clip(s: &str, max: usize, suffix: &str) -> String {
    clip_fmt(s, max, |_| suffix.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_limit_returns_as_is_without_suffix() {
        assert_eq!(clip("短的", 10, "…(截了)"), "短的");
        assert_eq!(clip("一二三四五", 5, "…(截了)"), "一二三四五", "正好装下不加尾缀");
        assert_eq!(clip("", 0, "…(截了)"), "");
    }

    #[test]
    fn over_limit_cuts_on_char_boundary_and_appends_suffix() {
        assert_eq!(clip("一二三四五六", 3, "…(截了)"), "一二三…(截了)");
        // 空尾缀 = 纯截断(openai/anthropic 两家错误消息的老行为)
        assert_eq!(clip("一二三四五六", 3, ""), "一二三");
    }

    /// 头号性质:多字节字符**绝不**被切成半个(按字节切会 panic 或吐乱码)。
    #[test]
    fn never_splits_multibyte_chars() {
        let out = clip("🎵🎶🎼🎹", 2, "");
        assert_eq!(out, "🎵🎶");
        assert_eq!(out.chars().count(), 2);
        // 中英混排同样按「字符」而不是字节数
        assert_eq!(clip("a中b文c", 3, ""), "a中b");
    }

    #[test]
    fn clip_fmt_reports_remaining_char_count() {
        assert_eq!(clip_fmt("一二三四五六", 4, |rest| format!("…(还有 {rest} 字)")), "一二三四…(还有 2 字)");
        // 剩余量按字符算,不是字节
        assert_eq!(clip_fmt("🎵🎵🎵", 1, |rest| format!("+{rest}")), "🎵+2");
    }
}
