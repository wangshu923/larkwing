//! 能力轴:JS 才出内容的网页(SPA)→ 可读内容 / 可点下载 / 会话式浏览。
//! web_fetch 的「真浏览器」档:静态抓取回来是空壳时交给壳层 WebView 窗真渲染
//! (webrender 接缝,壳层注入;没注入 = 如实说没有渲染组件)。
//! L2 会话式浏览(2026-07-10 用户拍板,DOM/文本编号快照路线):每步回**编号元素**
//! 快照(文本版 Set-of-Marks),窗口跨调用存活(session,TTL 3 分钟)——看 → 点编号 →
//! 返回 → 再看连续走。**动作只有看/点/返回,输入/填表仍不做**(等 Tool::risk 确认闸门,
//! §7.8);截图混合档等 ToolResult 支持图后再加(当前纯文本,§6.3)。

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;

use crate::webrender::RenderRequest;

use super::{Tool, ToolCtx, ToolRisk, ToolSpec};

/// 单次渲染预算(开窗→回传→可能的下载全含;壳层超时自己收摊关窗)。
const RENDER_TIMEOUT: Duration = Duration::from_secs(40);

pub(super) struct WebRender {
    spec: ToolSpec,
}

impl WebRender {
    pub(super) fn new() -> WebRender {
        WebRender {
            spec: ToolSpec {
                name: "web_render",
                description: "用真浏览器打开并操作网页(要跑 JS 才显示内容的页面:web_fetch \
                              抓回来是空壳、说「动态加载」时用这个)。每次返回渲染后的正文和\
                              带编号的可点元素(如 [3] 按钮「下载」),并给一个 session 号——\
                              窗口保持 3 分钟,可以连续操作:带 session + click_ref 点编号元素、\
                              back 返回上一页、再带 url 则跳新地址。点出的下载自动存到本机并\
                              返回路径。浏览窗在屏幕右下角对用户可见:遇到要登录/扫码/验证码的\
                              页面,直接请用户在那个小窗里操作,完成后你再带 session 继续。\
                              你自己只能看/点/返回,不能填表输入。比 web_fetch 慢得多,\
                              先试 web_fetch 不行再用它。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "http(s) 网页链接;开新会话必填,续用 session 时可省(省略 = 停在当前页操作)"
                        },
                        "session": {
                            "type": "string",
                            "description": "继续上次结果里的会话号(3 分钟内有效);过期就带 url 重开"
                        },
                        "click_ref": {
                            "type": "integer",
                            "description": "点上次快照里的编号元素(如 [3] → 3);编号只在同一页有效"
                        },
                        "click_text": {
                            "type": "string",
                            "description": "按文字点第一个包含这段文字的按钮/链接(没有编号可用时的退路)"
                        },
                        "back": {
                            "type": "boolean",
                            "description": "返回上一页(优先于点击)"
                        },
                        "dir": {
                            "type": "string",
                            "description": "点出的下载存到哪个文件夹(绝对路径);省略 = 系统「下载」文件夹"
                        }
                    }
                }),
                timeout: Duration::from_secs(90),
                ui_key: "tool.web_render",
            },
        }
    }
}

#[async_trait]
impl Tool for WebRender {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn risk(&self) -> ToolRisk {
        ToolRisk::Mutating // 可能落盘下载文件
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let url = args
            .get("url")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);
        let session = args
            .get("session")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);
        if let Some(u) = &url {
            anyhow::ensure!(
                u.starts_with("http://") || u.starts_with("https://"),
                "url 需要 http(s) 链接,收到: {u}"
            );
        }
        anyhow::ensure!(
            url.is_some() || session.is_some(),
            "缺参数:开新页面给 url,继续上次的窗给 session"
        );
        let click_ref = args.get("click_ref").and_then(serde_json::Value::as_u64).map(|n| n as u32);
        let click_text = args
            .get("click_text")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);
        let back = super::arg_bool(&args, "back", false);
        let download_dir = match args.get("dir").and_then(serde_json::Value::as_str).map(str::trim)
        {
            Some(d) if !d.is_empty() => {
                let p = PathBuf::from(d);
                anyhow::ensure!(p.is_absolute(), "dir 需要绝对路径,收到: {d}");
                p
            }
            _ => crate::files::default_download_dir(),
        };
        let renderer = ctx
            .web
            .clone()
            .context("这台机器没有接网页渲染组件(桌面壳层才有)——退回 web_fetch,或让用户手动打开页面下载后给我文件")?;

        let wanted_click = click_ref
            .map(|n| format!("[{n}]"))
            .or_else(|| click_text.clone().map(|t| format!("「{t}」")));
        let outcome = renderer
            .render(RenderRequest {
                url: url.clone().unwrap_or_default(),
                session,
                click_ref,
                click_text,
                back,
                download_dir,
                timeout: RENDER_TIMEOUT,
            })
            .await?;

        let mut out = String::new();
        if let Some(path) = &outcome.download {
            out.push_str(&format!(
                "点击{}触发了下载,已存到 {}\n",
                wanted_click.as_deref().unwrap_or("?"),
                path.display()
            ));
        }
        match &outcome.page {
            Some(page) => {
                out.push_str(&format!(
                    "《{}》\n\n{}",
                    page.title,
                    crate::web::clip(&page.text, PAGE_MAX_CHARS)
                ));
                if !page.elements.is_empty() {
                    out.push_str(
                        "\n\n【可点元素】(点哪个就带 session + click_ref=编号 再调;编号只在本页有效)\n",
                    );
                    for e in &page.elements {
                        let role = match e.role.as_str() {
                            "button" => "按钮",
                            "link" => "链接",
                            _ => "可点",
                        };
                        out.push_str(&format!("[{}] {role}「{}」\n", e.ref_no, e.text));
                    }
                } else if !page.clickables.is_empty() {
                    // 旧形状兜底(壳层还没升级时)
                    out.push_str("\n\n【可点元素】(把文字传给 click_text 再调一次)\n");
                    for c in &page.clickables {
                        out.push_str(&format!("- {c}\n"));
                    }
                }
                if !page.links.is_empty() {
                    out.push_str("\n【页内链接】(直链交给 web_download)\n");
                    for l in &page.links {
                        out.push_str(&format!("- {} → {}\n", l.text, l.url));
                    }
                }
                if page.click_ref_stale {
                    out.push_str("\n(那个编号已经失效——页面变过了,按上面新快照的编号再点)");
                }
                match (&wanted_click, page.clicked, &outcome.download) {
                    (Some(t), false, _) if !page.click_ref_stale => out.push_str(&format!(
                        "\n(没找到{t}对应的可点元素——从上面的清单里换一个再试)"
                    )),
                    (Some(t), true, None) => {
                        let hit = if page.clicked_desc.is_empty() {
                            String::new()
                        } else {
                            format!(",命中 {}", page.clicked_desc)
                        };
                        match &outcome.post_click_url {
                            Some(u) => out.push_str(&format!(
                                "\n(点了{t}{hit},页面跳到了 {u} 但没直接下载——上面就是新页的快照;\
                                 像文件直链也可以交给 web_download 试)"
                            )),
                            None => out.push_str(&format!(
                                "\n(点了{t}{hit},没触发下载——上面是点击后的页面状态,自己判断下一步)"
                            )),
                        }
                    }
                    _ => {}
                }
            }
            None if outcome.download.is_some() => {} // 有下载没页面快照:结果已经够用
            // 快照空 + 点击后跳了页 = 多半点进了文件本身(PDF/附件),当前窗成了文件查看器、
            // 注入脚本跑不了 → **绝不 bail 把这条线丢掉**:把去向交模型接 web_download。
            None if outcome.post_click_url.is_some() => {
                let u = outcome.post_click_url.as_deref().unwrap();
                out.push_str(&format!(
                    "点{}后页面跳到了 {u}——这多半就是文件本身(比如 PDF),用 web_download 下它;\
                     下不动(要登录/一次性链接)就如实告诉用户。",
                    wanted_click.as_deref().unwrap_or("")
                ));
            }
            None => anyhow::bail!(
                "页面渲染超时没回内容(站点太慢或反爬拦截)——退回 web_fetch,或让用户手动下载后给我文件"
            ),
        }
        if let Some(sid) = &outcome.session {
            out.push_str(&format!(
                "\n\n会话 {sid}(3 分钟内可继续:带 session 再调,click_ref 点编号 / back 返回 / url 跳新页)"
            ));
        }
        Ok(out.trim_end().to_string())
    }
}

/// 渲染页正文预算(与 web_fetch 的 FETCH_MAX_CHARS 同数量级)。
const PAGE_MAX_CHARS: usize = 6000;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::MediaRuntime;
    use crate::store::Store;
    use crate::webrender::{RenderOutcome, RenderedPage, WebRenderer};
    use std::sync::Arc;

    fn base_ctx(tag: &str) -> ToolCtx {
        let dir = std::env::temp_dir().join(format!("lw-webrender-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("t.db"));
        let store = Store::open(&dir.join("t.db")).unwrap();
        store.users.ensure_default_user().unwrap();
        ToolCtx {
            user_id: 1,
            conv_id: 1,
            media: MediaRuntime::detached(store.clone()),
            store,
            web: None,
        }
    }

    /// 假渲染器:回固定结局 + 记住最后一次请求(断言参数透传)。
    struct FakeRender(RenderOutcome, Mutex<Option<RenderRequest>>);
    impl FakeRender {
        fn new(o: RenderOutcome) -> Self {
            FakeRender(o, Mutex::new(None))
        }
    }
    use std::sync::Mutex;
    #[async_trait]
    impl WebRenderer for FakeRender {
        async fn render(&self, req: RenderRequest) -> anyhow::Result<RenderOutcome> {
            *self.1.lock().unwrap() = Some(req);
            Ok(RenderOutcome {
                page: self.0.page.clone(),
                download: self.0.download.clone(),
                post_click_url: self.0.post_click_url.clone(),
                session: self.0.session.clone(),
            })
        }
    }

    #[tokio::test]
    async fn no_renderer_is_honest_error() {
        let ctx = base_ctx("none");
        let err = WebRender::new()
            .run(serde_json::json!({"url": "https://x.example.com"}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("渲染组件"), "{err:#}");
        // 既无 url 也无 session 也要拦(带渲染器之前就该退回)
        let err = WebRender::new().run(serde_json::json!({}), &ctx).await.unwrap_err();
        assert!(err.to_string().contains("缺参数"), "{err:#}");
    }

    #[tokio::test]
    async fn renders_numbered_elements_and_session_line() {
        let mut ctx = base_ctx("page");
        let fake = Arc::new(FakeRender::new(RenderOutcome {
            page: Some(RenderedPage {
                title: "单据平台".into(),
                text: "这里是渲染后的正文".into(),
                links: vec![crate::web::PageLink {
                    text: "查看".into(),
                    url: "https://x.example.com/v".into(),
                }],
                elements: vec![
                    crate::webrender::PageElement {
                        ref_no: 1,
                        role: "button".into(),
                        text: "下载电子票".into(),
                        href: None,
                    },
                    crate::webrender::PageElement {
                        ref_no: 2,
                        role: "link".into(),
                        text: "查看清单".into(),
                        href: Some("https://x.example.com/list".into()),
                    },
                ],
                ..Default::default()
            }),
            session: Some("lw-render-1".into()),
            ..Default::default()
        }));
        ctx.web = Some(fake.clone());
        let out = WebRender::new()
            .run(serde_json::json!({"url": "https://x.example.com", "click_text": "导出"}), &ctx)
            .await
            .unwrap();
        assert!(out.contains("渲染后的正文"), "{out}");
        assert!(out.contains("[1] 按钮「下载电子票」") && out.contains("[2] 链接「查看清单」"), "{out}");
        assert!(out.contains("没找到「导出」对应的可点元素"), "{out}");
        assert!(out.contains("会话 lw-render-1"), "{out}");
    }

    #[tokio::test]
    async fn session_click_ref_and_back_pass_through() {
        let mut ctx = base_ctx("ref");
        let fake = Arc::new(FakeRender::new(RenderOutcome {
            page: Some(RenderedPage {
                title: "页".into(),
                text: "正文".into(),
                clicked: true,
                clicked_desc: "BUTTON「下载电子票据」".into(),
                ..Default::default()
            }),
            session: Some("lw-render-9".into()),
            ..Default::default()
        }));
        ctx.web = Some(fake.clone());
        // 无 url、带 session + click_ref:合法,且参数原样透传给壳层
        let out = WebRender::new()
            .run(serde_json::json!({"session": "lw-render-9", "click_ref": 3}), &ctx)
            .await
            .unwrap();
        assert!(out.contains("点了[3]") && out.contains("命中 BUTTON「下载电子票据」"), "{out}");
        let req = fake.1.lock().unwrap().clone().unwrap();
        assert_eq!(req.session.as_deref(), Some("lw-render-9"));
        assert_eq!(req.click_ref, Some(3));
        assert!(req.url.is_empty(), "没给 url = 停在当前页");

        // back 透传(字符串 "true" 也认——arg_bool 宽容)
        let _ = WebRender::new()
            .run(serde_json::json!({"session": "lw-render-9", "back": "true"}), &ctx)
            .await
            .unwrap();
        assert!(fake.1.lock().unwrap().clone().unwrap().back);
    }

    #[tokio::test]
    async fn stale_ref_is_reported() {
        let mut ctx = base_ctx("stale");
        ctx.web = Some(Arc::new(FakeRender::new(RenderOutcome {
            page: Some(RenderedPage {
                title: "页".into(),
                text: "正文".into(),
                click_ref_stale: true,
                ..Default::default()
            }),
            session: Some("s".into()),
            ..Default::default()
        })));
        let out = WebRender::new()
            .run(serde_json::json!({"session": "s", "click_ref": 7}), &ctx)
            .await
            .unwrap();
        assert!(out.contains("编号已经失效"), "{out}");
    }

    #[tokio::test]
    async fn post_click_navigation_is_reported_for_follow_up() {
        let mut ctx = base_ctx("nav");
        ctx.web = Some(Arc::new(FakeRender::new(RenderOutcome {
            page: Some(RenderedPage {
                title: "页".into(),
                text: "正文".into(),
                clicked: true,
                clicked_desc: "BUTTON「下载」".into(),
                ..Default::default()
            }),
            post_click_url: Some("https://x.example.com/f/abc.pdf".into()),
            ..Default::default()
        })));
        let out = WebRender::new()
            .run(serde_json::json!({"url": "https://x.example.com", "click_text": "下载"}), &ctx)
            .await
            .unwrap();
        assert!(out.contains("页面跳到了 https://x.example.com/f/abc.pdf"), "{out}");
        assert!(out.contains("命中 BUTTON「下载」"), "{out}");
    }

    #[tokio::test]
    async fn download_outcome_reports_path() {
        let mut ctx = base_ctx("dl");
        ctx.web = Some(Arc::new(FakeRender::new(RenderOutcome {
            page: Some(RenderedPage { clicked: true, ..Default::default() }),
            download: Some(PathBuf::from("/tmp/单据.pdf")),
            ..Default::default()
        })));
        let out = WebRender::new()
            .run(
                serde_json::json!({"url": "https://x.example.com", "click_text": "下载"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.contains("触发了下载") && out.contains("单据.pdf"), "{out}");
    }
}
