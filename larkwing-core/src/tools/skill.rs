//! 能力轴:技能(工作手册)三件套 —— 取(lookup)/ 教(write)/ 删(remove)。
//! 技能是 **agent 的**、恒全局(与用户无关;记忆才是与用户的约定、归人)。
//! 三层渐进披露:L1 索引常驻 system prompt(名称 + 何时用,恒常驻无折叠)→
//! L2 正文 = skill_lookup 按需取 → L3 附录节 = 带 section 再取一层。
//! **触发的唯一定义 = lookup 命中一次**(skill_hits 流水,技能页三数字由它现算)。
//! 三件是常驻基础工具(BASE_TOOLS),法条「技能」节点名它们。

use anyhow::Context;
use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolSpec};

/// 总条数 backstop(含内置):超了写入时如实退回让用户清理,**绝不静默折叠索引**
/// (用户拍板「路径不产生折叠」—— 启用技能的索引恒常驻)。
const SKILLS_MAX: i64 = 64;
/// 单条上限(超长退回不截断,§3.5):名称短小、时机一句话、正文 = 工作手册量级。
const NAME_MAX_CHARS: usize = 24;
const WHEN_MAX_CHARS: usize = 80;
const CONTENT_MAX_CHARS: usize = 4000;

fn arg_str<'a>(args: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(serde_json::Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------
// skill_lookup
// ---------------------------------------------------------------------------

pub(super) struct SkillLookup {
    spec: ToolSpec,
}

impl SkillLookup {
    pub(super) fn new() -> SkillLookup {
        SkillLookup {
            spec: ToolSpec {
                name: "skill_lookup",
                description: "取一份技能手册的全文。系统提示「技能(索引)」里列着每份手册的\
                              名称和适用时机——手头的活对得上哪条,就先取它照着做,再开始动手。\
                              正文末尾列了附录的,需要那部分细节再带 section 取。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "技能名称(用索引里列的名字)"
                        },
                        "section": {
                            "type": "string",
                            "description": "附录节名(可选;正文末尾列出的附录才有)"
                        }
                    },
                    "required": ["name"]
                }),
                timeout: std::time::Duration::from_secs(5),
                ui_key: "tool.skill_lookup",
            },
        }
    }
}

#[async_trait]
impl Tool for SkillLookup {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let name = arg_str(&args, "name").context("缺少 name 参数(技能名称)")?.to_string();
        let section = arg_str(&args, "section").map(str::to_string);
        let store = ctx.store.clone();
        let conv_id = ctx.conv_id;
        tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
            let Some((skill, sections)) = store.skills.find(&name)? else {
                return Ok(format!("没有叫「{name}」的技能(以索引里列的名字为准)"));
            };
            // 触发 = lookup 命中(唯一定义);记流水失败不挡取用
            if let Err(e) = store.skills.record_hit(skill.id, conv_id) {
                tracing::warn!(err = %e, skill = %skill.name, "技能触发流水记录失败");
            }
            match section {
                Some(sec) => match store.skills.get_section(skill.id, &sec)? {
                    Some(s) => Ok(format!("【{} · {}】\n{}", skill.name, s.name, s.content)),
                    None => Ok(if sections.is_empty() {
                        format!("「{}」没有附录;正文:\n{}", skill.name, skill.content)
                    } else {
                        format!(
                            "「{}」没有叫「{sec}」的附录(有:{})",
                            skill.name,
                            sections.join("、")
                        )
                    }),
                },
                None => {
                    let mut out = format!("【{}】\n{}", skill.name, skill.content);
                    if !sections.is_empty() {
                        out.push_str(&format!(
                            "\n\n(另有附录:{} —— 需要哪部分再带 section 取)",
                            sections.join("、")
                        ));
                    }
                    Ok(out)
                }
            }
        })
        .await
        .context("技能查询任务挂了")?
    }
}

// ---------------------------------------------------------------------------
// skill_write
// ---------------------------------------------------------------------------

pub(super) struct SkillWrite {
    spec: ToolSpec,
}

impl SkillWrite {
    pub(super) fn new() -> SkillWrite {
        SkillWrite {
            spec: ToolSpec {
                name: "skill_write",
                description: "把用户教的「做某类事的完整做法」存成一份技能手册。**只在用户明确\
                              表示要记住这套做法时用**(「以后就这么办」「记住这个流程」),别把\
                              随手做完的事自作主张记成技能;记完把名称和做法要点复述一遍让用户\
                              确认。内容只写用户真实教的步骤。关于人的事实用 remember,环境信息\
                              (东西在哪)用 briefing_write,一句话的偏好也归 remember——这里只装\
                              多步骤的完整做法。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "技能名,短小好认(如「洗照片」「报销发票」)"
                        },
                        "when_to_use": {
                            "type": "string",
                            "description": "什么时候该用它,一句话写具体(触发时机,别写太泛)"
                        },
                        "content": {
                            "type": "string",
                            "description": "完整做法:步骤、要点、注意事项,只写用户真实教的内容"
                        }
                    },
                    "required": ["name", "when_to_use", "content"]
                }),
                timeout: std::time::Duration::from_secs(5),
                ui_key: "tool.skill_write",
            },
        }
    }
}

#[async_trait]
impl Tool for SkillWrite {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let name = arg_str(&args, "name").context("缺少 name 参数")?.to_string();
        let when = arg_str(&args, "when_to_use").context("缺少 when_to_use 参数")?.to_string();
        let content = arg_str(&args, "content").context("缺少 content 参数")?.to_string();
        // 超长一律退回、绝不静默截断(§3.5)
        let n = name.chars().count();
        if n > NAME_MAX_CHARS {
            anyhow::bail!("技能名 {n} 字太长了(上限 {NAME_MAX_CHARS} 字),换个短名。");
        }
        let n = when.chars().count();
        if n > WHEN_MAX_CHARS {
            anyhow::bail!(
                "「什么时候用」写了 {n} 字(上限 {WHEN_MAX_CHARS} 字),精简成一句话再写。"
            );
        }
        let n = content.chars().count();
        if n > CONTENT_MAX_CHARS {
            anyhow::bail!(
                "这份手册 {n} 字,超过 {CONTENT_MAX_CHARS} 字上限,没有写入。\
                 请把做法精简后重写(细节留最关键的)。"
            );
        }
        let store = ctx.store.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
            // 总量 backstop:满了如实退回(绝不静默折叠索引);覆盖已有条不占新额度
            let exists = store.skills.find(&name)?.is_some_and(|(s, _)| s.name == name);
            if !exists && store.skills.count()? >= SKILLS_MAX {
                anyhow::bail!(
                    "技能已有 {SKILLS_MAX} 条(上限),没有写入。请用户先在技能页清理不用的,再教新的。"
                );
            }
            let old = store.skills.upsert_user(&name, &when, &content)?;
            Ok(match old {
                Some(old) => format!(
                    "ok,已更新技能「{name}」。(此前的做法是:{old})"
                ),
                None => format!("ok,已记下技能「{name}」(用的时机:{when})"),
            })
        })
        .await
        .context("技能落库任务挂了")?
    }
}

// ---------------------------------------------------------------------------
// skill_remove
// ---------------------------------------------------------------------------

pub(super) struct SkillRemove {
    spec: ToolSpec,
}

impl SkillRemove {
    pub(super) fn new() -> SkillRemove {
        SkillRemove {
            spec: ToolSpec {
                name: "skill_remove",
                description: "删掉一份用户教的技能(用户明确说不要了/作废了才删)。\
                              内置技能删不掉,只能请用户在技能页把它停用。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "要删的技能名" }
                    },
                    "required": ["name"]
                }),
                timeout: std::time::Duration::from_secs(5),
                ui_key: "tool.skill_remove",
            },
        }
    }
}

#[async_trait]
impl Tool for SkillRemove {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let name = arg_str(&args, "name").context("缺少 name 参数")?.to_string();
        let store = ctx.store.clone();
        let target = name.clone();
        let removed = tokio::task::spawn_blocking(move || store.skills.remove_user(&target))
            .await
            .context("技能删除任务挂了")??;
        Ok(if removed { "ok".into() } else { format!("没有叫「{name}」的技能") })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::MediaRuntime;
    use crate::store::Store;
    use crate::tools::Tool;

    fn ctx(tag: &str) -> ToolCtx {
        let dir = std::env::temp_dir().join(format!("lw-skilltool-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("t.db"));
        let store = Store::open(&dir.join("t.db")).unwrap();
        ToolCtx { user_id: 1, conv_id: 7, media: MediaRuntime::detached(store.clone()), store, web: None, confirm: None, grants: Default::default() }
    }

    #[tokio::test]
    async fn teach_lookup_remove_roundtrip_and_hit_recorded() {
        let ctx = ctx("rt");
        let w = SkillWrite::new();
        let out = w
            .run(
                serde_json::json!({
                    "name": "洗照片",
                    "when_to_use": "用户要把手机照片整理成相册时",
                    "content": "1. 先看收件区有哪些图;2. 按日期建文件夹;3. 移进去。"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.contains("洗照片"), "{out}");

        // lookup 命中 → 返回正文 + 记一次触发
        let l = SkillLookup::new();
        let got = l.run(serde_json::json!({"name": "洗照片"}), &ctx).await.unwrap();
        assert!(got.contains("按日期建文件夹"), "{got}");
        let rows = ctx.store.skills.list_with_stats().unwrap();
        assert_eq!(rows[0].total_hits, 1, "lookup 命中 = 触发一次");

        // 查无如实说、不记触发
        let miss = l.run(serde_json::json!({"name": "修水管"}), &ctx).await.unwrap();
        assert!(miss.contains("没有叫"), "{miss}");
        assert_eq!(ctx.store.skills.list_with_stats().unwrap()[0].total_hits, 1);

        // 覆盖回显旧内容
        let out = w
            .run(
                serde_json::json!({"name": "洗照片", "when_to_use": "整理照片时", "content": "改成按人分。"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.contains("此前的做法"), "{out}");

        // 删除
        let r = SkillRemove::new();
        assert_eq!(r.run(serde_json::json!({"name": "洗照片"}), &ctx).await.unwrap(), "ok");
        assert!(r
            .run(serde_json::json!({"name": "洗照片"}), &ctx)
            .await
            .unwrap()
            .contains("没有"));
    }

    #[tokio::test]
    async fn over_limit_rejects_not_truncates() {
        let ctx = ctx("limits");
        let w = SkillWrite::new();
        let long_content = "步".repeat(CONTENT_MAX_CHARS + 1);
        assert!(w
            .run(serde_json::json!({"name": "x", "when_to_use": "y", "content": long_content}), &ctx)
            .await
            .is_err());
        let long_name = "名".repeat(NAME_MAX_CHARS + 1);
        assert!(w
            .run(serde_json::json!({"name": long_name, "when_to_use": "y", "content": "z"}), &ctx)
            .await
            .is_err());
        let long_when = "机".repeat(WHEN_MAX_CHARS + 1);
        assert!(w
            .run(serde_json::json!({"name": "x", "when_to_use": long_when, "content": "z"}), &ctx)
            .await
            .is_err());
        assert!(ctx.store.skills.list_with_stats().unwrap().is_empty(), "拒绝写入不留半截");
    }

    #[tokio::test]
    async fn builtin_name_collision_and_remove_guard() {
        let ctx = ctx("builtin");
        ctx.store
            .skills
            .sync_builtins(&[crate::store::skills::BuiltinSkill {
                slug: "a".into(),
                name: "放歌放视频".into(),
                when_to_use: "点播时".into(),
                content: "出厂做法".into(),
                sections: vec![("附录甲".into(), "细节内容".into())],
            }])
            .unwrap();
        let w = SkillWrite::new();
        let err = w
            .run(serde_json::json!({"name": "放歌放视频", "when_to_use": "x", "content": "y"}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("内置"), "{err}");
        let r = SkillRemove::new();
        let err = r.run(serde_json::json!({"name": "放歌放视频"}), &ctx).await.unwrap_err();
        assert!(err.to_string().contains("内置"), "{err}");

        // 附录:正文带导航行,带 section 取节,错节名报有哪些
        let l = SkillLookup::new();
        let body = l.run(serde_json::json!({"name": "放歌放视频"}), &ctx).await.unwrap();
        assert!(body.contains("另有附录:附录甲"), "{body}");
        let sec = l
            .run(serde_json::json!({"name": "放歌放视频", "section": "附录甲"}), &ctx)
            .await
            .unwrap();
        assert!(sec.contains("细节内容"), "{sec}");
        let bad = l
            .run(serde_json::json!({"name": "放歌放视频", "section": "没有的"}), &ctx)
            .await
            .unwrap();
        assert!(bad.contains("附录甲"), "错节名要报有哪些: {bad}");
    }

    #[tokio::test]
    async fn backstop_rejects_when_full() {
        let ctx = ctx("backstop");
        for i in 0..SKILLS_MAX {
            ctx.store.skills.upsert_user(&format!("技{i}"), "某时", "某法").unwrap();
        }
        let w = SkillWrite::new();
        let err = w
            .run(serde_json::json!({"name": "再来一条", "when_to_use": "x", "content": "y"}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("上限"), "{err}");
        // 覆盖已有条不受 backstop 影响
        let ok = w
            .run(serde_json::json!({"name": "技0", "when_to_use": "x", "content": "新法"}), &ctx)
            .await
            .unwrap();
        assert!(ok.starts_with("ok"), "{ok}");
    }
}
