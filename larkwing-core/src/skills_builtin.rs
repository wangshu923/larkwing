//! 出厂技能(数据):`assets/skills.json` 编译期内嵌,boot 时经 `SkillRepo::sync_builtins`
//! 刷进库(内容以出厂为准、enabled 保留用户状态)。§5「X = 数据」:加/改一份出厂技能 =
//! 改 JSON,代码零改。内容纪律同 few-shot(§6.5):不嵌具体可复用事实、路径一律不写死;
//! 技能是给模型看的工作手册,工具名可以出现。

use serde::Deserialize;

use crate::store::skills::BuiltinSkill;

#[derive(Deserialize)]
struct RawSection {
    name: String,
    content: String,
}

#[derive(Deserialize)]
struct RawSkill {
    slug: String,
    name: String,
    when_to_use: String,
    content: String,
    #[serde(default)]
    sections: Vec<RawSection>,
}

/// 解析出厂技能清单。数据坏 = 编译者错误,panic 让 CI/开机即炸(场景 validate 同款态度)。
pub fn builtin_skills() -> Vec<BuiltinSkill> {
    let raw: Vec<RawSkill> = serde_json::from_str(include_str!("../assets/skills.json"))
        .expect("assets/skills.json 解析失败(出厂技能数据坏了)");
    raw.into_iter()
        .map(|r| BuiltinSkill {
            slug: r.slug,
            name: r.name,
            when_to_use: r.when_to_use,
            content: r.content,
            sections: r.sections.into_iter().map(|s| (s.name, s.content)).collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// 出厂数据结构守卫:解析得开、键唯一、字段非空、长度在限内(与工具层写入上限同量级,
    /// 出厂内容不受工具闸约束但不该无界)。
    #[test]
    fn builtin_skills_are_wellformed() {
        let skills = builtin_skills();
        assert!(!skills.is_empty(), "出厂技能不该为空(技能页首屏靠它示范)");
        let mut slugs = HashSet::new();
        let mut names = HashSet::new();
        for s in &skills {
            assert!(slugs.insert(s.slug.clone()), "slug 重复: {}", s.slug);
            assert!(names.insert(s.name.clone()), "name 重复: {}", s.name);
            assert!(!s.name.trim().is_empty() && !s.when_to_use.trim().is_empty());
            assert!(!s.content.trim().is_empty());
            assert!(s.name.chars().count() <= 24, "{}: 名称超 24 字", s.slug);
            assert!(s.when_to_use.chars().count() <= 80, "{}: 时机描述超 80 字", s.slug);
            assert!(s.content.chars().count() <= 4000, "{}: 正文超 4000 字", s.slug);
            for (name, content) in &s.sections {
                assert!(!name.trim().is_empty() && !content.trim().is_empty());
                assert!(content.chars().count() <= 10_000, "{}/{name}: 附录节超长", s.slug);
            }
        }
    }
}
