//! 能力轴:任务需知(写/查/删)。家庭备忘 —— 跟任务/能力域走的环境知识
//! (资源在哪、目录、家里的惯例),与小本本(归人)机制同构、数据分账(PLAN §9)。
//! 三件套是**常驻基础工具**(每场景自动含,法条点名它们)。
//! 常驻预算在写入时执法:满额自动降非常驻并如实告知 —— 装配无条件全装,前缀字节稳定。

use anyhow::Context;
use async_trait::async_trait;

use crate::store::briefings::{scope_home, scope_user};

use super::{Tool, ToolCtx, ToolSpec};

/// 常驻区(进前缀)总预算,字数计(中文 ~1.5 字/token,约合 600-800 token)。
const RESIDENT_BUDGET_CHARS: usize = 1200;
/// 单条内容上限:超过则退回让模型精简/拆主题重写,**不静默截断**(§3.5)。
/// 媒体目录+一串 URL 容易破 300,放到与常驻预算同量级;真超大条目由预算逻辑自动降按需。
const CONTENT_MAX_CHARS: usize = 1200;
const DOMAIN_MAX_CHARS: usize = 24;

/// scope 参数("home" 默认 / "user" = 当前用户个人)→ 存储 scope 串。
fn scope_of(args: &serde_json::Value, ctx: &ToolCtx) -> String {
    match args.get("scope").and_then(serde_json::Value::as_str) {
        Some("user") => scope_user(ctx.user_id),
        _ => scope_home(),
    }
}

fn domain_of(args: &serde_json::Value) -> anyhow::Result<String> {
    let domain = args
        .get("domain")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .context("缺少 domain 参数")?
        .to_string();
    // 超长不静默截断(§3.5):主题词应短小,过长退回让模型换个短的。
    let n = domain.chars().count();
    if n > DOMAIN_MAX_CHARS {
        anyhow::bail!("主题词 {n} 字,太长了(上限 {DOMAIN_MAX_CHARS} 字),请换个更短的主题词。");
    }
    Ok(domain)
}

// ---------------------------------------------------------------------------
// briefing_write
// ---------------------------------------------------------------------------

pub(super) struct BriefingWrite {
    spec: ToolSpec,
}

impl BriefingWrite {
    pub(super) fn new() -> BriefingWrite {
        BriefingWrite {
            spec: ToolSpec {
                name: "briefing_write",
                description: "把「这个家的环境信息」记进家庭备忘:资源放在哪(电影/动画片目录、\
                              NAS 路径)、设备位置、家里的惯例。按主题(domain)整存整取:同一主题\
                              再次写入会**整体覆盖**旧内容,所以不管是更正还是补充新信息,都要把\
                              该主题已有内容连同新内容一起写全(只有旧信息确实作废了才整段替掉)。\
                              关于「人」的事(名字/喜好/忌口)不用这个,用 remember。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "domain": {
                            "type": "string",
                            "description": "主题词,短小固定,如 media(影音)、coding、appliance(设备)"
                        },
                        "content": {
                            "type": "string",
                            "description": "该主题的完整当前状态,简短陈述,如「电影在 \\\\nas\\film 和 D:\\Movies;动画片在 \\\\nas\\kids」"
                        },
                        "scope": {
                            "type": "string",
                            "enum": ["home", "user"],
                            "description": "归属:home=这个家(默认);user=只属于当前用户个人(如他自己的工作目录)"
                        }
                    },
                    "required": ["domain", "content"]
                }),
                timeout: std::time::Duration::from_secs(5),
                ui_key: "tool.briefing_write",
            },
        }
    }
}

#[async_trait]
impl Tool for BriefingWrite {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let domain = domain_of(&args)?;
        let content = args
            .get("content")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .context("缺少 content 参数")?
            .to_string();
        // 超长不静默截断(§3.5):退回错误,让模型自己精简或按主题拆开重写。
        let n = content.chars().count();
        if n > CONTENT_MAX_CHARS {
            anyhow::bail!(
                "这条备忘 {n} 字,超过 {CONTENT_MAX_CHARS} 字上限,没有写入。\
                 请精简,或按主题(domain)拆成多条分开写,再重试。"
            );
        }
        let scope = scope_of(&args, ctx);

        let store = ctx.store.clone();
        let user_id = ctx.user_id;
        tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
            // 预算执法(写入时,不在装配时 —— 前缀永远全装、字节稳定):
            // 覆盖同主题时旧字数让位,新增才挤占。
            let existing = store
                .briefings
                .list_for(user_id)?
                .into_iter()
                .find(|b| b.scope == scope && b.domain == domain);
            // 覆盖时回显旧内容:让模型(和回头看的用户)当场发现「该留的没带上」,
            // 不静默吃数据(宪法 §3.5 不静默失败)。
            let old_content = existing.as_ref().map(|b| b.content.clone());
            let old_chars = existing
                .as_ref()
                .filter(|b| b.resident)
                .map(|b| b.content.chars().count() + b.domain.chars().count())
                .unwrap_or(0);
            let new_chars = content.chars().count() + domain.chars().count();
            let total = store.briefings.resident_chars(user_id)? - old_chars + new_chars;
            let resident = total <= RESIDENT_BUDGET_CHARS;
            store.briefings.upsert(&scope, &domain, &content, resident)?;
            let verb = if old_content.is_some() { "已更新" } else { "已记入" };
            let detail = match &old_content {
                Some(old) => format!("。(此前记的是:{old})"),
                None => String::new(),
            };
            Ok(if resident {
                format!("ok,{verb}家庭备忘(主题: {domain}){detail}")
            } else {
                format!(
                    "{verb}家庭备忘(主题: {domain}),但常驻区满了,这条记成了按需查询 —— \
                     用的时候需要先查一下备忘{detail}"
                )
            })
        })
        .await
        .context("备忘落库任务挂了")?
    }
}

// ---------------------------------------------------------------------------
// briefing_lookup
// ---------------------------------------------------------------------------

pub(super) struct BriefingLookup {
    spec: ToolSpec,
}

impl BriefingLookup {
    pub(super) fn new() -> BriefingLookup {
        BriefingLookup {
            spec: ToolSpec {
                name: "briefing_lookup",
                description: "翻家庭备忘:系统提示里「任务需知」没有、但像是家里登记过的事\
                              (某类资源在哪、某个约定),先用它查一遍再说不知道。\
                              不带参数 = 返回全部备忘。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "关键词(匹配主题名或内容);不传 = 全部"
                        }
                    }
                }),
                timeout: std::time::Duration::from_secs(5),
                ui_key: "tool.briefing_lookup",
            },
        }
    }
}

#[async_trait]
impl Tool for BriefingLookup {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let query = args
            .get("query")
            .and_then(serde_json::Value::as_str)
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty());
        let store = ctx.store.clone();
        let user_id = ctx.user_id;
        let hits = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<String>> {
            Ok(store
                .briefings
                .list_for(user_id)?
                .into_iter()
                .filter(|b| match &query {
                    Some(q) => {
                        b.domain.to_lowercase().contains(q) || b.content.to_lowercase().contains(q)
                    }
                    None => true,
                })
                .map(|b| format!("【{}】{}", b.domain, b.content))
                .collect())
        })
        .await
        .context("备忘查询任务挂了")??;
        if hits.is_empty() {
            Ok("家庭备忘里没有相关记录".into())
        } else {
            Ok(hits.join("\n"))
        }
    }
}

// ---------------------------------------------------------------------------
// briefing_remove
// ---------------------------------------------------------------------------

pub(super) struct BriefingRemove {
    spec: ToolSpec,
}

impl BriefingRemove {
    pub(super) fn new() -> BriefingRemove {
        BriefingRemove {
            spec: ToolSpec {
                name: "briefing_remove",
                description: "删掉家庭备忘里的一个主题(用户明确说不要了/作废了才删)。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "domain": { "type": "string", "description": "要删的主题词" },
                        "scope": {
                            "type": "string",
                            "enum": ["home", "user"],
                            "description": "归属,默认 home"
                        }
                    },
                    "required": ["domain"]
                }),
                timeout: std::time::Duration::from_secs(5),
                ui_key: "tool.briefing_remove",
            },
        }
    }
}

#[async_trait]
impl Tool for BriefingRemove {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let domain = domain_of(&args)?;
        let scope = scope_of(&args, ctx);
        let store = ctx.store.clone();
        let target = domain.clone();
        let removed =
            tokio::task::spawn_blocking(move || store.briefings.remove(&scope, &target))
                .await
                .context("备忘删除任务挂了")??;
        Ok(if removed { "ok".into() } else { format!("没有叫 {domain} 的备忘主题") })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::MediaRuntime;
    use crate::store::Store;
    use crate::tools::Tool;

    fn ctx(tag: &str) -> ToolCtx {
        let dir = std::env::temp_dir().join(format!("lw-brieftool-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("t.db"));
        let store = Store::open(&dir.join("t.db")).unwrap();
        ToolCtx { user_id: 1, conv_id: 1, media: MediaRuntime::detached(store.clone()), store, web: None, confirm: None }
    }

    #[tokio::test]
    async fn write_lookup_remove_roundtrip() {
        let ctx = ctx("rt");
        let w = BriefingWrite::new();
        let out = w
            .run(
                serde_json::json!({"domain": "media", "content": "电影在 NAS 的 film 文件夹"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.contains("media"));

        let l = BriefingLookup::new();
        let hits = l.run(serde_json::json!({"query": "电影"}), &ctx).await.unwrap();
        assert!(hits.contains("【media】"));
        let all = l.run(serde_json::json!({}), &ctx).await.unwrap();
        assert!(all.contains("film"));
        let none = l.run(serde_json::json!({"query": "代码"}), &ctx).await.unwrap();
        assert!(none.contains("没有相关记录"));

        let r = BriefingRemove::new();
        assert_eq!(r.run(serde_json::json!({"domain": "media"}), &ctx).await.unwrap(), "ok");
        assert!(r
            .run(serde_json::json!({"domain": "media"}), &ctx)
            .await
            .unwrap()
            .contains("没有"));
    }

    #[tokio::test]
    async fn overwrite_echoes_old_content_not_silent() {
        let ctx = ctx("echo");
        let w = BriefingWrite::new();
        // 首次写:报「已记入」,无旧内容可回显。
        let first = w
            .run(serde_json::json!({"domain": "media", "content": "动画片在 NAS 的 kids"}), &ctx)
            .await
            .unwrap();
        assert!(first.starts_with("ok,已记入"), "首次=已记入: {first}");
        assert!(!first.contains("此前记的是"), "首次无可回显的旧内容: {first}");
        // 再写同主题:报「已更新」并回显被覆盖的旧内容 —— 不静默吃数据。
        let second = w
            .run(serde_json::json!({"domain": "media", "content": "电影在 D:\\Movies"}), &ctx)
            .await
            .unwrap();
        assert!(second.starts_with("ok,已更新"), "再写=已更新: {second}");
        assert!(second.contains("此前记的是:动画片在 NAS 的 kids"), "回显旧内容: {second}");
    }

    #[tokio::test]
    async fn budget_demotes_to_non_resident_at_write_time() {
        let ctx = ctx("budget");
        let w = BriefingWrite::new();
        let chunk = "九".repeat(280); // 每条 ~284 字,四条 ~1136,第五条破 1200
        for i in 0..4 {
            let out = w
                .run(serde_json::json!({"domain": format!("d{i}"), "content": chunk}), &ctx)
                .await
                .unwrap();
            assert!(out.starts_with("ok"), "前四条仍在预算内: {out}");
        }
        let out = w
            .run(serde_json::json!({"domain": "d4", "content": chunk}), &ctx)
            .await
            .unwrap();
        assert!(out.contains("按需查询"), "超额自动降非常驻: {out}");
        // 覆盖同主题不重复挤占:重写 d0 仍常驻
        let out = w
            .run(serde_json::json!({"domain": "d0", "content": "瘦身后的内容"}), &ctx)
            .await
            .unwrap();
        assert!(out.starts_with("ok"));
        // 降级条目 lookup 查得到
        let l = BriefingLookup::new();
        assert!(l.run(serde_json::json!({"query": "d4"}), &ctx).await.unwrap().contains("【d4】"));
    }

    #[tokio::test]
    async fn user_scope_is_separate() {
        let ctx = ctx("scope");
        let w = BriefingWrite::new();
        w.run(
            serde_json::json!({"domain": "coding", "content": "仓库在 ~/code", "scope": "user"}),
            &ctx,
        )
        .await
        .unwrap();
        let rows = ctx.store.briefings.list_for(1).unwrap();
        assert_eq!(rows[0].scope, "user:1");
    }

    #[tokio::test]
    async fn over_limit_rejects_not_truncates() {
        let ctx = ctx("overlimit");
        let w = BriefingWrite::new();
        let too_long = "九".repeat(CONTENT_MAX_CHARS + 50);
        // 超长 → 退回错误,不静默截断(§3.5)
        assert!(w
            .run(serde_json::json!({"domain": "media", "content": too_long}), &ctx)
            .await
            .is_err());
        // 拒绝写入后不留半截数据
        assert!(ctx.store.briefings.list_for(1).unwrap().is_empty());
    }

    #[tokio::test]
    async fn over_long_domain_rejects() {
        let ctx = ctx("longdomain");
        let w = BriefingWrite::new();
        let long_domain = "题".repeat(DOMAIN_MAX_CHARS + 5);
        // 主题词超长 → 退回错误,不静默截断(§3.5)
        assert!(w
            .run(serde_json::json!({"domain": long_domain, "content": "x"}), &ctx)
            .await
            .is_err());
    }
}
