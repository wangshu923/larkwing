//! 能力轴:本机文件 → 家里人的手机(渠道出站文件,机器件在 channels/outbound)。
//! 正交原语:只管「送过去」——找文件归 fs_find、转格式归 pdf_to_png,模型自己组合
//! (§7.8 组合链的最后一棒:PDF→PNG→发手机)。目标缺省 = **说话人**(ToolCtx.user_id =
//! 渠道归人后的 mem_user):桌面喊「发我手机」= 主人,家人在手机上让它取文件 = TA 自己;
//! `to` 填家人名字 = 发给那位家人(人际路由,2026-07-11 用户拍板放开跨人)。

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;

use crate::channels::outbound;

use super::{Tool, ToolCtx, ToolSpec};

/// 单次封顶(逐个上传,别一口气几十个把渠道打骨折;超额如实退回)。
const SEND_MAX_FILES: usize = 5;

pub(super) struct SendFile {
    spec: ToolSpec,
    net: crate::net::Client,
}

impl SendFile {
    pub(super) fn new() -> SendFile {
        SendFile {
            spec: ToolSpec {
                name: "send_file",
                description: "把本机的文件/图片发到家里人的手机上(走已连接的 \
                              Telegram/钉钉)。不填 to = 发给说这句话的人(「发我手机」\
                              「传给我」);「把XX发给妈妈」这类就把 to 填成那位家人的名字。\
                              发之前文件得已经在本机(要下载先 web_download,要转图先 \
                              pdf_to_png)。对方没连手机会明说。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "paths": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "要发的文件绝对路径,最多 5 个"
                        },
                        "note": {
                            "type": "string",
                            "description": "随文件带一句说明(可选,随第一个文件发)"
                        },
                        "to": {
                            "type": "string",
                            "description": "发给哪位家人(名字要跟家人页一致);不填 = 说这句话的人自己"
                        }
                    },
                    "required": ["paths"]
                }),
                timeout: Duration::from_secs(300),
                ui_key: "tool.send_file",
            },
            // 上传客户端:大文件慢网,超时给足(spec 300s 内);UA 无所谓,走 net 代理选路(§4.6)
            net: crate::net::Client::new(|b| {
                b.connect_timeout(Duration::from_secs(10)).timeout(Duration::from_secs(280))
            }),
        }
    }
}

#[async_trait]
impl Tool for SendFile {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let paths: Vec<PathBuf> = args
            .get("paths")
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from)
                    .collect()
            })
            .unwrap_or_default();
        anyhow::ensure!(!paths.is_empty(), "缺少 paths 参数(要发的文件绝对路径)");
        anyhow::ensure!(
            paths.len() <= SEND_MAX_FILES,
            "一次最多发 {SEND_MAX_FILES} 个,收到 {} 个——分批发",
            paths.len()
        );
        for p in &paths {
            anyhow::ensure!(p.is_absolute(), "需要绝对路径,收到: {}", p.display());
        }
        let note = args
            .get("note")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());

        // 目标 = 说话人的手机;to 填了 = 那位家人的手机(一次解析,逐文件复用);
        // 查无此人 / 没连手机的明白话直接当观察退回
        let recipient = args
            .get("to")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|name| outbound::find_member(&ctx.store, name))
            .transpose()?;
        let target_user = recipient.as_ref().map_or(ctx.user_id, |u| u.id);
        let target = outbound::resolve_target(&ctx.store, target_user)?;

        let mut sent: Vec<String> = Vec::new();
        let mut failed: Vec<String> = Vec::new();
        for (i, p) in paths.iter().enumerate() {
            let cap = if i == 0 { note } else { None };
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.display().to_string());
            match outbound::send_file(&self.net, &target, p, cap).await {
                Ok(()) => sent.push(name),
                Err(e) => failed.push(format!("{name}({e:#})")),
            }
        }
        // 全军覆没 = 错误观察(模型换路/如实说);部分失败 = 汇总 + 点名失败(fs 批量纪律)
        anyhow::ensure!(
            !sent.is_empty(),
            "一个都没发出去(经 {}):{}",
            target.channel_name(),
            failed.join(";")
        );
        let whose = recipient.as_ref().map(|u| format!("{}的", u.name)).unwrap_or_default();
        let mut out = format!(
            "已经 {} 发到{whose}手机 {} 个:{}",
            target.channel_name(),
            sent.len(),
            sent.join("、")
        );
        if !failed.is_empty() {
            out.push_str(&format!("\n没发出去 {} 个:{}", failed.len(), failed.join(";")));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::MediaRuntime;
    use crate::store::Store;

    fn ctx(tag: &str) -> ToolCtx {
        let dir = std::env::temp_dir().join(format!("lw-sendfile-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("t.db"));
        let store = Store::open(&dir.join("t.db")).unwrap();
        let me = store.users.ensure_default_user().unwrap();
        ToolCtx {
            user_id: me.id,
            conv_id: 1,
            media: MediaRuntime::detached(store.clone()),
            store,
            web: None,
        }
    }

    #[tokio::test]
    async fn rejects_bad_args_and_reports_unlinked_phone() {
        let ctx = ctx("args");
        let tool = SendFile::new();
        assert!(tool.run(serde_json::json!({}), &ctx).await.is_err(), "缺 paths");
        assert!(
            tool.run(serde_json::json!({"paths": ["rel.png"]}), &ctx).await.is_err(),
            "相对路径退回"
        );
        let six: Vec<String> = (0..6).map(|i| format!("/tmp/f{i}.png")).collect();
        let err = tool.run(serde_json::json!({"paths": six}), &ctx).await.unwrap_err();
        assert!(err.to_string().contains("最多发"), "{err:#}");
        // 没绑手机 → 明白话观察(§3.5)
        let err = tool
            .run(serde_json::json!({"paths": ["/tmp/nonexistent-x.png"]}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("还没连上手机"), "{err:#}");
    }

    #[tokio::test]
    async fn to_resolves_family_member_honestly() {
        let ctx = ctx("to");
        let tool = SendFile::new();
        // 查无此人 → 带现有名单的明白话
        let err = tool
            .run(serde_json::json!({"paths": ["/tmp/a.png"], "to": "二舅"}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("没有叫"), "{err:#}");
        // 有这个人但没连手机 → 明白话
        ctx.store.users.create("妈妈").unwrap();
        let err = tool
            .run(serde_json::json!({"paths": ["/tmp/a.png"], "to": "妈妈"}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("还没连上手机"), "{err:#}");
    }
}
