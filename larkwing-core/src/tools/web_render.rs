//! 能力轴:JS 才出内容的网页(SPA)→ 可读内容 / 可点下载 / 会话式浏览。
//! web_fetch 的「真浏览器」档:静态抓取回来是空壳时交给壳层 WebView 窗真渲染
//! (webrender 接缝,壳层注入;没注入 = 如实说没有渲染组件)。
//! L2 会话式浏览(2026-07-10 用户拍板,DOM/文本编号快照路线):每步回**编号元素**
//! 快照(文本版 Set-of-Marks),窗口跨调用存活(session,TTL 3 分钟)——看 → 点编号 →
//! 返回 → 再看连续走。完全操作第一批(2026-07-14)开了填字/批量填表/选下拉/按键/提交/
//! 滚动 + 截图可选第二只眼;**文件上传(2026-07-15)**:upload_ref+upload_paths 把本机
//! 文件传给页面的上传框(壳层 DataTransfer 注入)。**凭证代填 / CDP 可信输入仍不做**,
//! 敏感字段标出交用户在可见小窗自己输(§7.8;确认闸门另案)。

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;

use crate::webrender::{FillField, PageElement, RenderRequest};

use super::{Tool, ToolCtx, ToolOutput, ToolRisk, ToolSpec};

/// 从工具入参取可选字符串(trim + 空即无)。
fn str_arg(args: &serde_json::Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// 从工具入参取可选编号(宽容:真数字 / 数字形字符串都认,同 `arg_u64` quirk)。
fn opt_ref(args: &serde_json::Value, key: &str) -> Option<u32> {
    match args.get(key) {
        Some(serde_json::Value::Number(n)) => n.as_u64().map(|v| v as u32),
        Some(serde_json::Value::String(s)) => s.trim().parse::<u32>().ok(),
        _ => None,
    }
}

/// 这次「想干的动作」的人话描述(用于「没找到/已操作」措辞;实际命中以 clicked_desc 为准)。
fn wanted_action(
    click_ref: Option<u32>,
    click_text: Option<&str>,
    type_ref: Option<u32>,
    fill: &[FillField],
    select_ref: Option<u32>,
    press_key: Option<&str>,
    upload_ref: Option<u32>,
) -> Option<String> {
    if let Some(n) = upload_ref {
        return Some(format!("往[{n}]传文件"));
    }
    if let Some(n) = click_ref {
        return Some(format!("[{n}]"));
    }
    if let Some(t) = click_text {
        return Some(format!("「{t}」"));
    }
    if let Some(n) = type_ref {
        return Some(format!("填入[{n}]"));
    }
    if !fill.is_empty() {
        return Some("批量填表".into());
    }
    if let Some(n) = select_ref {
        return Some(format!("选[{n}]"));
    }
    if let Some(k) = press_key {
        return Some(format!("按键 {k}"));
    }
    None
}

/// 一个交互元素渲染成给模型看的一行(按类别带值/选项/勾选态)。
fn render_element(e: &PageElement) -> String {
    let n = e.ref_no;
    let val = |v: &str| if v.is_empty() { "空".to_string() } else { format!("「{v}」") };
    match e.role.as_str() {
        "button" => format!("[{n}] 按钮「{}」\n", e.text),
        "link" => format!("[{n}] 链接「{}」\n", e.text),
        "input" if e.secret => {
            format!("[{n}] 密码框「{}」(敏感,别代填——请用户在小窗里自己输)\n", e.text)
        }
        "input" => format!("[{n}] 输入框「{}」= {}\n", e.text, val(&e.value)),
        "textarea" => format!("[{n}] 文本域「{}」= {}\n", e.text, val(&e.value)),
        "select" => {
            let opts = if e.options.is_empty() {
                String::new()
            } else {
                format!(";可选:{}", e.options.join(" / "))
            };
            format!("[{n}] 下拉「{}」= {}{}\n", e.text, val(&e.value), opts)
        }
        "checkbox" | "radio" => {
            let mark = if e.checked == Some(true) { "☑" } else { "☐" };
            format!("[{n}] {mark} 勾选「{}」(click_ref 点它切换)\n", e.text)
        }
        "file" => {
            let mut extra = String::new();
            if !e.accept.is_empty() {
                extra.push_str(&format!(";收:{}", e.accept));
            }
            if e.multiple {
                extra.push_str(";可传多个");
            }
            format!(
                "[{n}] 文件上传框「{}」= 已选 {}{extra}(upload_ref={n} + upload_paths 传本机文件)\n",
                e.text,
                val(&e.value)
            )
        }
        "editable" => format!("[{n}] 可编辑区「{}」= {}\n", e.text, val(&e.value)),
        _ => format!("[{n}] 可点「{}」\n", e.text),
    }
}

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
                              抓回来是空壳、说「动态加载」时用这个)。每次返回渲染后的正文 + 带\
                              编号的交互元素(可点 [3] 按钮「下载」/ 可填 [5] 输入框「邮箱」/ \
                              可选 [7] 下拉「城市」/ 勾选框),并给一个 session 号——窗口保持 3 \
                              分钟,可连续操作:带 session 加 click_ref 点编号、type_ref+text 往\
                              编号输入框填字、fill 批量填表、select_ref+option 选下拉、勾选框用 \
                              click_ref 点、press_key 按键(Enter/Escape…)、submit 提交表单、\
                              upload_ref+upload_paths 往文件上传框传本机文件(只传用户点名/这次\
                              差事里的文件)、scroll 上/下翻页、back 返回、再带 url 跳新地址。填完\
                              下张快照会回读各框当前值,自己核对填对没。点出的下载自动存到本机并\
                              返回路径。\
                              浏览窗在屏幕右下角对用户可见:遇到要登录/扫码/验证码,或要填密码/\
                              银行卡这类敏感信息,别自己填——请用户在那个小窗里操作,完成后你带 \
                              session 继续。比 web_fetch 慢得多,先试 web_fetch 不行再用它。",
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
                            "description": "返回上一页(优先于其它动作)"
                        },
                        "type_ref": {
                            "type": "integer",
                            "description": "往这个编号的输入框/文本域/可编辑区填字(配 text);编号只在同一页有效"
                        },
                        "text": {
                            "type": "string",
                            "description": "配 type_ref:要填进去的文字(会替换原有内容)"
                        },
                        "fill": {
                            "type": "array",
                            "description": "批量填表:一次填多个字段,比逐个 type 省事",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "ref": { "type": "integer", "description": "字段编号" },
                                    "value": { "type": "string", "description": "填入的值" }
                                }
                            }
                        },
                        "select_ref": {
                            "type": "integer",
                            "description": "选这个编号的原生下拉(配 option)"
                        },
                        "option": {
                            "type": "string",
                            "description": "配 select_ref:要选的选项(选项文字或值)"
                        },
                        "upload_ref": {
                            "type": "integer",
                            "description": "往这个编号的文件上传框传本机文件(配 upload_paths);快照里标「文件上传框」的编号"
                        },
                        "upload_paths": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "配 upload_ref:要上传的本机文件绝对路径;多个 = 传给支持多选的框(单个也用数组)"
                        },
                        "submit": {
                            "type": "boolean",
                            "description": "填完(type/fill)后提交所在表单;按回车提交请用它,合成回车不触发提交"
                        },
                        "press_key": {
                            "type": "string",
                            "description": "按一个键:Enter / Escape / Tab / ArrowDown 等(喂搜索框/下拉的按键监听)"
                        },
                        "scroll": {
                            "type": "string",
                            "description": "翻页看屏外内容:up / down"
                        },
                        "wait_text": {
                            "type": "string",
                            "description": "动作后等这段文字出现再读页(动态内容慢慢加载时用);等不到也照常返回"
                        },
                        "screenshot": {
                            "type": "boolean",
                            "description": "顺便截当前页一张图(想看画面长啥样时用;文字快照说不清版式/图形时才需要,多数情况不用)。得先有打开的页面(配 url 或 session)"
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

    // run 只取文本(无图降级路,给不看图的场景);turn loop 实际走 run_output,把截图当图片
    // part 带回 —— web_render 是「工具结果多媒体」(ToolResult.parts)的第一个真消费者。
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        Ok(self.browse(args, ctx).await?.0)
    }

    async fn run_output(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx,
    ) -> anyhow::Result<ToolOutput> {
        let (text, shot) = self.browse(args, ctx).await?;
        Ok(ToolOutput { text, images: shot.into_iter().collect() })
    }
}

impl WebRender {
    /// 浏览一步 + 渲染结果文本 +(可选)截图 data-URL。run/run_output 共享此核心。
    async fn browse(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx,
    ) -> anyhow::Result<(String, Option<String>)> {
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
        let click_ref = opt_ref(&args, "click_ref");
        let click_text = str_arg(&args, "click_text");
        let back = super::arg_bool(&args, "back", false);
        // 输入类动作(完全操作第一批)
        let type_ref = opt_ref(&args, "type_ref");
        let type_text = str_arg(&args, "text");
        let fill: Vec<FillField> = args
            .get("fill")
            .and_then(serde_json::Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|it| {
                        Some(FillField {
                            ref_no: opt_ref(it, "ref")?,
                            value: str_arg(it, "value").unwrap_or_default(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let select_ref = opt_ref(&args, "select_ref");
        let select_option = str_arg(&args, "option");
        // 上传:路径在这里就验(缺文件/超闸别开窗白跑一步);字节由壳层读、注入页面。
        let upload_ref = opt_ref(&args, "upload_ref");
        let upload_paths: Vec<PathBuf> = match args.get("upload_paths") {
            Some(serde_json::Value::String(s)) if !s.trim().is_empty() => {
                vec![PathBuf::from(s.trim())]
            }
            Some(serde_json::Value::Array(a)) => a
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .collect(),
            _ => Vec::new(),
        };
        if upload_ref.is_some() || !upload_paths.is_empty() {
            anyhow::ensure!(
                upload_ref.is_some(),
                "传文件要配 upload_ref(上张快照里「文件上传框」的编号)"
            );
            anyhow::ensure!(
                !upload_paths.is_empty(),
                "upload_paths 是空的——给要上传的本机文件绝对路径"
            );
            let mut total: u64 = 0;
            for p in &upload_paths {
                anyhow::ensure!(p.is_absolute(), "上传路径要绝对路径,收到: {}", p.display());
                let meta = std::fs::metadata(p)
                    .with_context(|| format!("找不到要上传的文件: {}", p.display()))?;
                anyhow::ensure!(meta.is_file(), "{} 不是文件,传不了", p.display());
                total += meta.len();
            }
            anyhow::ensure!(
                total <= crate::webrender::UPLOAD_MAX_BYTES,
                "这批文件共 {},超过单次上传上限 {}——分开传或挑小的",
                super::fs::human_size(total),
                super::fs::human_size(crate::webrender::UPLOAD_MAX_BYTES)
            );
        }
        let submit = super::arg_bool(&args, "submit", false);
        let press_key = str_arg(&args, "press_key");
        let scroll = str_arg(&args, "scroll");
        let wait_text = str_arg(&args, "wait_text");
        let want_shot = super::arg_bool(&args, "screenshot", false);
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

        let wanted = wanted_action(
            click_ref,
            click_text.as_deref(),
            type_ref,
            &fill,
            select_ref,
            press_key.as_deref(),
            upload_ref,
        );
        let outcome = renderer
            .render(RenderRequest {
                url: url.clone().unwrap_or_default(),
                session,
                click_ref,
                click_text,
                back,
                type_ref,
                type_text,
                fill,
                submit,
                upload_ref,
                upload_paths,
                select_ref,
                select_option,
                press_key,
                scroll,
                wait_text,
                screenshot: want_shot,
                download_dir,
                timeout: RENDER_TIMEOUT,
            })
            .await?;

        let mut out = String::new();
        if let Some(path) = &outcome.download {
            out.push_str(&format!("触发了下载,已存到 {}\n", path.display()));
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
                        "\n\n【交互元素】(带 session 用编号:click_ref 点 / type_ref+text 填 / \
                         select_ref+option 选 / upload_ref+upload_paths 传文件;编号只在本页有效)\n",
                    );
                    for e in &page.elements {
                        out.push_str(&render_element(e));
                    }
                    if !page.scroll_hint.is_empty() {
                        out.push_str(&format!(
                            "(滚动位置:{};要看屏外内容用 scroll=up/down)\n",
                            page.scroll_hint
                        ));
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
                    out.push_str("\n(那个编号已经失效——页面变过了,按上面新快照的编号再操作)");
                }
                if !page.click_note.is_empty() {
                    out.push_str(&format!("\n({})", page.click_note));
                }
                match (&wanted, page.clicked, &outcome.download) {
                    (Some(t), false, _) if !page.click_ref_stale => out.push_str(&format!(
                        "\n(没找到{t}对应的元素——从上面的清单里换一个再试)"
                    )),
                    (_, true, None) => {
                        let did = if page.clicked_desc.is_empty() {
                            "已操作".to_string()
                        } else {
                            page.clicked_desc.clone()
                        };
                        match &outcome.post_click_url {
                            Some(u) => out.push_str(&format!(
                                "\n({did},页面跳到了 {u} 但没直接下载——上面是新页快照;\
                                 像文件直链也可以交给 web_download 试)"
                            )),
                            None => out.push_str(&format!(
                                "\n({did}——上面是操作后的页面状态;填过的框看回读值核对,自己判断下一步)"
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
                    "操作后页面跳到了 {u}——这多半就是文件本身(比如 PDF),用 web_download 下它;\
                     下不动(要登录/一次性链接)就如实告诉用户。"
                ));
            }
            None => anyhow::bail!(
                "页面渲染超时没回内容(站点太慢或反爬拦截)——退回 web_fetch,或让用户手动下载后给我文件"
            ),
        }
        // 截图(工具结果多媒体第一个消费者):截到就随 ToolOutput 图片 part 回给模型,文本注一句;
        // 想截没截到如实说(没打开窗 / 平台组件不支持——不塞空图,§3.5 不静默)。
        if outcome.screenshot.is_some() {
            out.push_str("\n(已附上当前页面截图)");
        } else if want_shot {
            out.push_str("\n(想截图但没截到——得先有打开的页面,或这台机器不支持截图)");
        }
        if let Some(sid) = &outcome.session {
            out.push_str(&format!(
                "\n\n会话 {sid}(3 分钟内可继续:带 session 再调,click_ref 点编号 / back 返回 / url 跳新页)"
            ));
        }
        Ok((out.trim_end().to_string(), outcome.screenshot))
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
                screenshot: self.0.screenshot.clone(),
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
                        ..Default::default()
                    },
                    crate::webrender::PageElement {
                        ref_no: 2,
                        role: "link".into(),
                        text: "查看清单".into(),
                        href: Some("https://x.example.com/list".into()),
                        ..Default::default()
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
        assert!(out.contains("没找到「导出」对应的元素"), "{out}");
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
        assert!(out.contains("BUTTON「下载电子票据」"), "{out}");
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
        assert!(out.contains("BUTTON「下载」"), "{out}");
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

    #[tokio::test]
    async fn input_actions_pass_through() {
        let mut ctx = base_ctx("input");
        let fake = Arc::new(FakeRender::new(RenderOutcome {
            page: Some(RenderedPage {
                title: "表单".into(),
                text: "正文".into(),
                clicked: true,
                clicked_desc: "填入[2]".into(),
                ..Default::default()
            }),
            session: Some("s".into()),
            ..Default::default()
        }));
        ctx.web = Some(fake.clone());
        // type_ref + text + submit
        let _ = WebRender::new()
            .run(
                serde_json::json!({"session": "s", "type_ref": 2, "text": "a@b.com", "submit": true}),
                &ctx,
            )
            .await
            .unwrap();
        let req = fake.1.lock().unwrap().clone().unwrap();
        assert_eq!(req.type_ref, Some(2));
        assert_eq!(req.type_text.as_deref(), Some("a@b.com"));
        assert!(req.submit);

        // fill 批量(含字符串编号,宽容解析)
        let _ = WebRender::new()
            .run(
                serde_json::json!({"session": "s", "fill": [{"ref": 1, "value": "张三"}, {"ref": "3", "value": "李四"}]}),
                &ctx,
            )
            .await
            .unwrap();
        let req = fake.1.lock().unwrap().clone().unwrap();
        assert_eq!(req.fill.len(), 2);
        assert_eq!((req.fill[0].ref_no, req.fill[0].value.as_str()), (1, "张三"));
        assert_eq!((req.fill[1].ref_no, req.fill[1].value.as_str()), (3, "李四"));

        // select + option / press_key + scroll + wait_text
        let _ = WebRender::new()
            .run(serde_json::json!({"session": "s", "select_ref": 5, "option": "北京"}), &ctx)
            .await
            .unwrap();
        let req = fake.1.lock().unwrap().clone().unwrap();
        assert_eq!(req.select_ref, Some(5));
        assert_eq!(req.select_option.as_deref(), Some("北京"));
        let _ = WebRender::new()
            .run(
                serde_json::json!({"session": "s", "press_key": "Enter", "scroll": "down", "wait_text": "结果"}),
                &ctx,
            )
            .await
            .unwrap();
        let req = fake.1.lock().unwrap().clone().unwrap();
        assert_eq!(req.press_key.as_deref(), Some("Enter"));
        assert_eq!(req.scroll.as_deref(), Some("down"));
        assert_eq!(req.wait_text.as_deref(), Some("结果"));
    }

    #[tokio::test]
    async fn upload_args_validate_and_pass_through() {
        let mut ctx = base_ctx("upload");
        let fake = Arc::new(FakeRender::new(RenderOutcome {
            page: Some(RenderedPage {
                title: "表单".into(),
                text: "正文".into(),
                clicked: true,
                clicked_desc: "传文件[4]:单据.pdf".into(),
                ..Default::default()
            }),
            session: Some("s".into()),
            ..Default::default()
        }));
        ctx.web = Some(fake.clone());

        // 配对校验:只给 ref 缺 paths / 只给 paths 缺 ref,都要明白话退回
        let err = WebRender::new()
            .run(serde_json::json!({"session": "s", "upload_ref": 4}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("upload_paths"), "{err:#}");
        let err = WebRender::new()
            .run(serde_json::json!({"session": "s", "upload_paths": ["/tmp/x.pdf"]}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("upload_ref"), "{err:#}");

        // 相对路径 / 不存在的文件:开窗前就拦
        let err = WebRender::new()
            .run(
                serde_json::json!({"session": "s", "upload_ref": 4, "upload_paths": ["单据.pdf"]}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("绝对路径"), "{err:#}");
        let gone = std::env::temp_dir().join("lw-upload-不存在-xyz.pdf");
        let err = WebRender::new()
            .run(
                serde_json::json!({"session": "s", "upload_ref": 4, "upload_paths": [gone.to_string_lossy()]}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("找不到"), "{err:#}");

        // 真文件透传(单个字符串也认——quirk 宽容,同 arg_bool 哲学)
        let f = std::env::temp_dir().join(format!("lw-upload-{}-单据.pdf", std::process::id()));
        std::fs::write(&f, b"%PDF-1.4 fake").unwrap();
        let out = WebRender::new()
            .run(
                serde_json::json!({"session": "s", "upload_ref": 4, "upload_paths": f.to_string_lossy()}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.contains("传文件[4]:单据.pdf"), "{out}");
        let req = fake.1.lock().unwrap().clone().unwrap();
        assert_eq!(req.upload_ref, Some(4));
        assert_eq!(req.upload_paths, vec![f.clone()]);
        let _ = std::fs::remove_file(f);
    }

    #[tokio::test]
    async fn renders_file_element_and_click_note() {
        let mut ctx = base_ctx("file-el");
        ctx.web = Some(Arc::new(FakeRender::new(RenderOutcome {
            page: Some(RenderedPage {
                title: "表单".into(),
                text: "正文".into(),
                elements: vec![crate::webrender::PageElement {
                    ref_no: 4,
                    role: "file".into(),
                    text: "附件".into(),
                    value: "老单据.pdf".into(),
                    accept: ".pdf,image/*".into(),
                    multiple: true,
                    ..Default::default()
                }],
                click_note: "这个框只收一个文件,先传了第一个:单据.pdf".into(),
                ..Default::default()
            }),
            session: Some("s".into()),
            ..Default::default()
        })));
        let out = WebRender::new()
            .run(serde_json::json!({"url": "https://x.example.com"}), &ctx)
            .await
            .unwrap();
        assert!(
            out.contains("[4] 文件上传框「附件」= 已选 「老单据.pdf」;收:.pdf,image/*;可传多个"),
            "{out}"
        );
        assert!(out.contains("upload_ref=4"), "{out}");
        assert!(out.contains("(这个框只收一个文件,先传了第一个:单据.pdf)"), "{out}");
    }

    #[tokio::test]
    async fn renders_form_elements_with_value_options_checked() {
        let mut ctx = base_ctx("form");
        let fake = Arc::new(FakeRender::new(RenderOutcome {
            page: Some(RenderedPage {
                title: "表单".into(),
                text: "正文".into(),
                elements: vec![
                    crate::webrender::PageElement {
                        ref_no: 1,
                        role: "input".into(),
                        text: "邮箱".into(),
                        value: "a@b.com".into(),
                        ..Default::default()
                    },
                    crate::webrender::PageElement {
                        ref_no: 2,
                        role: "input".into(),
                        text: "密码".into(),
                        secret: true,
                        ..Default::default()
                    },
                    crate::webrender::PageElement {
                        ref_no: 3,
                        role: "select".into(),
                        text: "城市".into(),
                        value: "北京".into(),
                        options: vec!["北京".into(), "上海".into()],
                        ..Default::default()
                    },
                    crate::webrender::PageElement {
                        ref_no: 4,
                        role: "checkbox".into(),
                        text: "同意".into(),
                        checked: Some(true),
                        ..Default::default()
                    },
                ],
                scroll_hint: "下面约 2 屏".into(),
                ..Default::default()
            }),
            session: Some("s".into()),
            ..Default::default()
        }));
        ctx.web = Some(fake.clone());
        let out = WebRender::new()
            .run(serde_json::json!({"url": "https://x.example.com"}), &ctx)
            .await
            .unwrap();
        assert!(out.contains("[1] 输入框「邮箱」= 「a@b.com」"), "{out}");
        assert!(out.contains("[2] 密码框「密码」") && out.contains("别代填"), "{out}");
        assert!(out.contains("[3] 下拉「城市」= 「北京」") && out.contains("可选:北京 / 上海"), "{out}");
        assert!(out.contains("[4] ☑ 勾选「同意」"), "{out}");
        assert!(out.contains("下面约 2 屏"), "{out}");
    }

    #[tokio::test]
    async fn screenshot_flows_as_image_part_via_run_output() {
        let mut ctx = base_ctx("shot");
        let shot = "data:image/png;base64,SHOTBYTES";
        let fake = Arc::new(FakeRender::new(RenderOutcome {
            page: Some(RenderedPage { title: "页".into(), text: "正文".into(), ..Default::default() }),
            session: Some("s".into()),
            screenshot: Some(shot.into()),
            ..Default::default()
        }));
        ctx.web = Some(fake.clone());
        // run_output:截图当图片 part 带回,文本注一句;screenshot 请求透传给壳层
        let out = WebRender::new()
            .run_output(serde_json::json!({"session": "s", "screenshot": true}), &ctx)
            .await
            .unwrap();
        assert_eq!(out.images, vec![shot.to_string()]);
        assert!(out.text.contains("已附上当前页面截图"), "{}", out.text);
        assert!(fake.1.lock().unwrap().clone().unwrap().screenshot);
        // run(纯文本降级路):不带图,文本仍在
        let text = WebRender::new()
            .run(serde_json::json!({"session": "s", "screenshot": true}), &ctx)
            .await
            .unwrap();
        assert!(text.contains("已附上当前页面截图"), "{text}");

        // 想截没截到(outcome.screenshot=None)→ 如实说、不塞空图
        let fake2 = Arc::new(FakeRender::new(RenderOutcome {
            page: Some(RenderedPage { title: "页".into(), text: "正文".into(), ..Default::default() }),
            session: Some("s".into()),
            ..Default::default()
        }));
        ctx.web = Some(fake2);
        let out2 = WebRender::new()
            .run_output(serde_json::json!({"session": "s", "screenshot": true}), &ctx)
            .await
            .unwrap();
        assert!(out2.images.is_empty());
        assert!(out2.text.contains("没截到"), "{}", out2.text);
    }
}
