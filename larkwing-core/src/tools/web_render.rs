//! 能力轴:JS 才出内容的网页(SPA)→ 可读内容 / 可点下载 / 会话式浏览。
//! web_fetch 的「真浏览器」档:静态抓取回来是空壳时交给壳层 WebView 窗真渲染
//! (webrender 接缝,壳层注入;没注入 = 如实说没有渲染组件)。
//! L2 会话式浏览(2026-07-10 用户拍板,DOM/文本编号快照路线):每步回**编号元素**
//! 快照(文本版 Set-of-Marks),窗口跨调用存活(session,TTL 3 分钟)——看 → 点编号 →
//! 返回 → 再看连续走。完全操作第一批(2026-07-14)开了填字/批量填表/选下拉/按键/提交/
//! 滚动 + 截图可选第二只眼;**文件上传(2026-07-15)**:upload_ref+upload_paths 把本机
//! 文件传给页面的上传框(壳层 DataTransfer 注入)。**凭证代填 / CDP 可信输入仍不做**,
//! 敏感字段标出交用户在可见小窗自己输(§7.8)。
//! **动作确认闸(2026-07-15,§7.8)**:点击/提交撞高危词表(壳层在动作执行点拿活 DOM
//! 文本核,单源 `crate::confirm`)或模型自报 `confirm=true` → 壳层不执行、回 needs_confirm
//! → 本工具在 run 内阻塞请用户点头(桌面卡/渠道回话/语音,先到先得)→ 允许则带
//! `confirmed + expect_text` 重发这一步(目标文本变了按 stale 收);拒/超时 = 观察不是错。

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
                              返回路径。read=true 通读当前页正文**全文**(一段段给,结果里带下一段\
                              offset;登录后的文章、动态加载的长文读全靠它);save_pdf=true 把当前\
                              页面带排版存成 PDF 落 dir(存档发票/订单/文章)。这两个是单独的一步,\
                              不与点击/填表同调。\
                              浏览窗在屏幕右下角对用户可见:遇到要登录/扫码/验证码,或要填密码/\
                              银行卡这类敏感信息,别自己填——请用户在那个小窗里操作,完成后你带 \
                              session 继续。点「付款/发布/删除」这类有实际后果的按钮会自动先请\
                              用户点头(结果里会告诉你同意了没);你自己判断某一步有对外后果时\
                              也主动带 confirm=true。用户没点头就别换路硬做,如实说需要确认。\
                              比 web_fetch 慢得多,先试 web_fetch 不行再用它。",
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
                        "confirm": {
                            "type": "boolean",
                            "description": "这次点击/提交会产生付款、发送、发布、删除等对外后果时带 true:先请用户点头再执行(等结果即可,同意与否会写在结果里)。高危按钮不带也会自动请示,带上是你替用户多把一道关"
                        },
                        "screenshot": {
                            "type": "boolean",
                            "description": "顺便截当前页一张图(想看画面长啥样时用;文字快照说不清版式/图形时才需要,多数情况不用)。得先有打开的页面(配 url 或 session)"
                        },
                        "read": {
                            "type": "boolean",
                            "description": "通读:抽当前页正文全文按段返回(读长文/登录后的文章用);单独一步,不与动作同调"
                        },
                        "offset": {
                            "type": "integer",
                            "description": "配 read:从第几个字接着读(0 起;上次通读结果里给了下一段的 offset)"
                        },
                        "save_pdf": {
                            "type": "boolean",
                            "description": "把当前页面带排版存成 PDF(存档单据/文章);落在 dir,文件名取页面标题、重名自动加序号。单独一步,不与动作同调"
                        },
                        "dir": {
                            "type": "string",
                            "description": "点出的下载 / save_pdf 的成品存到哪个文件夹(绝对路径);省略 = 系统「下载」文件夹"
                        }
                    }
                }),
                // 210s:两次单步渲染(40×2)+ 确认等待(桌面 60 / 渠道 120)+ 余量。
                // 确认等待吃工具预算(turn loop 的 timeout 包住整个 run_output),故须容纳。
                timeout: Duration::from_secs(210),
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
        Ok(ToolOutput { text, images: shot.into_iter().collect(), ..Default::default() })
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
                vec![PathBuf::from(super::expand_home(s.trim()))]
            }
            Some(serde_json::Value::Array(a)) => a
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| PathBuf::from(super::expand_home(s))) // 「~/xxx」宽容展开(§4.4)
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
            // 传给网页 = 读文件(§7.2 授权圈)
            let up: Vec<String> =
                upload_paths.iter().map(|p| p.to_string_lossy().into_owned()).collect();
            super::guard::ensure(ctx, super::guard::Access::Read, &up).await?;
        }
        let submit = super::arg_bool(&args, "submit", false);
        let press_key = str_arg(&args, "press_key");
        let scroll = str_arg(&args, "scroll");
        let wait_text = str_arg(&args, "wait_text");
        let want_shot = super::arg_bool(&args, "screenshot", false);
        // 观察形态(通读/存 PDF):单独一步,与动作互斥——混着调会让「做了没」变含糊
        let read = super::arg_bool(&args, "read", false);
        let read_offset = super::arg_u64(&args, "offset", 0);
        let want_pdf = super::arg_bool(&args, "save_pdf", false);
        if read || want_pdf {
            let acting = click_ref.is_some()
                || click_text.is_some()
                || back
                || type_ref.is_some()
                || !fill.is_empty()
                || select_ref.is_some()
                || press_key.is_some()
                || scroll.is_some()
                || upload_ref.is_some()
                || submit
                || want_shot;
            anyhow::ensure!(!acting, "read/save_pdf 是单独的一步——先做动作,下一步再读/存");
            anyhow::ensure!(!(read && want_pdf), "read 和 save_pdf 一次只做一件");
        }
        let download_dir = match args.get("dir").and_then(serde_json::Value::as_str).map(str::trim)
        {
            Some(d) if !d.is_empty() => {
                let p = PathBuf::from(super::expand_home(d)); // 「~/xxx」宽容展开(§4.4)
                anyhow::ensure!(p.is_absolute(), "dir 需要绝对路径,收到: {d}");
                p
            }
            _ => crate::files::default_download_dir(),
        };
        // 页面点出的下载落这里 = 存入(§7.2 授权圈;缺省「下载」夹在基线内,零打扰)
        super::guard::ensure(
            ctx,
            super::guard::Access::Create,
            &[download_dir.to_string_lossy().into_owned()],
        )
        .await?;
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
        // 模型自报(§7.8 确认闸的自报半边):只对真有「落地动作」的调用生效(点击/提交);
        // 纯看页/填字带 confirm 没有可确认的动作,忽略。单向阀:词表命中自报压不掉。
        let self_report = super::arg_bool(&args, "confirm", false);
        let force_confirm = self_report && (click_ref.is_some() || click_text.is_some() || submit);
        let mut req = RenderRequest {
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
            force_confirm,
            confirmed: false,
            expect_text: None,
            screenshot: want_shot,
            read,
            read_offset,
            save_pdf: want_pdf.then(|| download_dir.clone()),
            download_dir,
            timeout: RENDER_TIMEOUT,
        };
        let mut outcome = renderer.render(req.clone()).await?;

        // 动作撞确认闸(高危词表命中 / 模型自报):壳层没执行,在这儿阻塞请用户点头。
        // 拒/超时是**观察不是错**(Ok 文本喂回模型,让它如实收尾);允许 → 带 confirmed +
        // expect_text 重发这一步(`confirmed` 是内部字段不进 schema,页面注入教不动它)。
        if let Some(pc) = outcome.needs_confirm.take() {
            // 给模型看的动作描述(工具结果文本);用户可见的动词组装在前端字典(§6.6),
            // 卡片/审计只过桥 kind + 原文。
            let action_desc = match (pc.kind.as_str(), pc.target_text.is_empty()) {
                ("submit", true) => "提交表单".to_string(),
                ("submit", false) => format!("提交『{}』", pc.target_text),
                ("press", _) => format!("按 {} 键", pc.target_text),
                _ => format!("点『{}』", pc.target_text),
            };
            let session_note = outcome
                .session
                .as_deref()
                .map(|s| format!("\n(会话 {s} 还开着,带 session 可以继续看页面/做别的操作)"))
                .unwrap_or_default();
            let confirmer = ctx.confirm.as_ref().with_context(|| {
                format!(
                    "在 {} {action_desc}有实际后果,需要用户确认,但这里没有确认通道——这步没执行,如实告诉用户",
                    pc.host
                )
            })?;
            // 确认路由到回合来源:桌面会话(ui/system)= 卡片(+语音回合口头应答);
            // 渠道会话 = outbound 推回发起的那个 chat 等回话(人在手机上,超时放宽)。
            let origin = ctx
                .store
                .chat
                .get_conversation(ctx.conv_id)
                .ok()
                .flatten()
                .map(|c| c.channel)
                .unwrap_or_else(|| "ui".into());
            let timeout = if matches!(origin.as_str(), "ui" | "system") {
                crate::confirm::DESKTOP_TIMEOUT
            } else {
                crate::confirm::CHANNEL_TIMEOUT
            };
            let decision = confirmer
                .ask(
                    crate::confirm::ConfirmAsk {
                        user_id: ctx.user_id,
                        conv_id: ctx.conv_id,
                        origin,
                        host: pc.host.clone(),
                        action: pc.target_text.clone(),
                        kind: pc.kind.clone(),
                    },
                    timeout,
                )
                .await;
            use crate::confirm::ConfirmDecision::*;
            match decision {
                Allowed { .. } => {
                    req.confirmed = true;
                    req.expect_text = Some(pc.target_text.clone());
                    req.force_confirm = false;
                    // 续用同一个窗:首步可能是「url + click」一步式(带 url、无 session),
                    // 重发若原样带 url 会另开新窗重导航——改带回壳层给的 session、清 url。
                    if let Some(sid) = outcome.session.clone() {
                        req.session = Some(sid);
                        req.url = String::new();
                    }
                    outcome = renderer.render(req).await?;
                }
                // 渠道推送失败(手机够不着):不是用户拒了,如实区分(§3.5)
                Denied { via } if via == "unreachable" => {
                    return Ok((
                        format!(
                            "在 {} {action_desc}需要用户确认,但确认请求没能送到用户那\
                             (渠道断线/没有收件地址)——这步没执行,如实说明;\
                             用户可以在电脑上让我继续。{session_note}",
                            pc.host
                        ),
                        None,
                    ));
                }
                Denied { .. } => {
                    return Ok((
                        format!(
                            "用户看过了,选择先不执行——在 {} {action_desc}没有进行。\
                             别自己重试或换路硬做;问问用户想怎么调整。{session_note}",
                            pc.host
                        ),
                        None,
                    ));
                }
                TimedOut => {
                    return Ok((
                        format!(
                            "在 {} {action_desc}需要用户点头,等了一会儿没等到回应——这步没执行。\
                             如实告诉用户:这一步有实际后果,TA 方便时说一声再继续。{session_note}",
                            pc.host
                        ),
                        None,
                    ));
                }
                NoUi => {
                    return Ok((
                        format!(
                            "在 {} {action_desc}有实际后果,需要用户确认,但这台机器上没有可用的\
                             确认通道——这步没执行,如实告诉用户。{session_note}",
                            pc.host
                        ),
                        None,
                    ));
                }
            }
        }

        // ── 观察形态的结局(通读切片 / PDF 成品):不带编号快照,提前返回 ──
        let obs_session = outcome
            .session
            .as_deref()
            .map(|s| format!("\n(会话 {s} 还开着,带 session 可以继续)"))
            .unwrap_or_default();
        if let Some(r) = &outcome.read {
            let got = r.text.chars().count() as u64;
            if got == 0 {
                return Ok((
                    format!(
                        "《{}》通读:offset={} 已超出全文(共 {} 字)——读到头了。{obs_session}",
                        r.title, r.offset, r.total
                    ),
                    None,
                ));
            }
            let capped_note =
                if r.capped { "(页面比定格上限还长,只定格了开头一段)" } else { "" };
            let mut out = format!(
                "《{}》通读:第 {}–{} 字 / 共 {} 字{capped_note}\n\n{}",
                r.title,
                r.offset + 1,
                r.offset + got,
                r.total,
                r.text
            );
            let next = r.offset + got;
            if next < r.total {
                out.push_str(&format!(
                    "\n\n(还没完——继续读:带同一个 session、read=true、offset={next})"
                ));
            } else if r.capped {
                out.push_str("\n\n(定格内的内容到此为止,更后面的够不到了——如实说明即可)");
            } else {
                out.push_str("\n\n(全文完)");
            }
            out.push_str(&obs_session);
            return Ok((out, None));
        }
        if let Some(p) = &outcome.saved_pdf {
            return Ok((
                format!(
                    "已把当前页面存成 PDF:{}(重名自动加了序号)。要转图接 pdf_to_png,\
                     发手机接 send_file。{obs_session}",
                    p.display()
                ),
                None,
            ));
        }

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
            voice: None,
            confirm: None,
            grants: Default::default(),
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
            // 镜像真壳层契约:confirmed 重发不再回闸(用户已点头,动作执行)。
            let confirmed = req.confirmed;
            *self.1.lock().unwrap() = Some(req);
            Ok(RenderOutcome {
                page: self.0.page.clone(),
                download: self.0.download.clone(),
                post_click_url: self.0.post_click_url.clone(),
                session: self.0.session.clone(),
                screenshot: self.0.screenshot.clone(),
                needs_confirm: if confirmed { None } else { self.0.needs_confirm.clone() },
                read: self.0.read.clone(),
                saved_pdf: self.0.saved_pdf.clone(),
            })
        }
    }

    /// 观察形态(通读 / 存 PDF):互斥校验 + 结局文案(下一段 offset 指路 / 组合链指路)。
    #[tokio::test]
    async fn read_and_save_pdf_outcomes_compose() {
        // read + 动作混调 = 明白话退回(还没开窗就拦下,不白跑一步)
        let ctx0 = base_ctx("obs0");
        let tool = WebRender::new();
        let err = tool
            .run(serde_json::json!({"url": "http://x.test/", "read": true, "click_ref": 1}), &ctx0)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("单独"), "{err:#}");

        // 通读切片:标注区间/总长 + 指路下一段 offset
        let mut ctx = base_ctx("obs1");
        ctx.web = Some(Arc::new(FakeRender::new(RenderOutcome {
            read: Some(crate::webrender::ReadSlice {
                title: "长文".into(),
                text: "一二三".into(),
                offset: 0,
                total: 10,
                capped: false,
            }),
            session: Some("s1".into()),
            ..Default::default()
        })));
        let out = tool
            .run(serde_json::json!({"url": "http://x.test/", "read": true}), &ctx)
            .await
            .unwrap();
        assert!(out.contains("第 1–3 字 / 共 10 字"), "{out}");
        assert!(out.contains("offset=3"), "指路下一段:{out}");

        // 存 PDF:成品路径 + 组合链指路
        let mut ctx2 = base_ctx("obs2");
        ctx2.web = Some(Arc::new(FakeRender::new(RenderOutcome {
            saved_pdf: Some(std::path::PathBuf::from("/tmp/页面.pdf")),
            session: Some("s2".into()),
            ..Default::default()
        })));
        let out = tool
            .run(serde_json::json!({"url": "http://x.test/", "save_pdf": true}), &ctx2)
            .await
            .unwrap();
        assert!(out.contains("存成 PDF") && out.contains("页面.pdf"), "{out}");
        assert!(out.contains("pdf_to_png"), "组合链指路:{out}");
    }

    /// 确认闸测试件:真 Confirmer + 自动应答订阅者(pending 卡一到就点头/摇头)。
    fn confirmer_with_responder(
        store: &Store,
        allow: bool,
    ) -> std::sync::Arc<crate::confirm::Confirmer> {
        let bus = crate::bus::Bus::new();
        let confirmer = crate::confirm::Confirmer::new(bus.clone(), store.clone());
        let mut rx = bus.subscribe();
        let c2 = confirmer.clone();
        tokio::spawn(async move {
            while let Ok(ev) = rx.recv().await {
                if let crate::bus::AppEvent::Confirm(card) = ev {
                    if card.state == "pending" {
                        let reply = if allow {
                            crate::confirm::ConfirmReply::AllowOnce
                        } else {
                            crate::confirm::ConfirmReply::Deny
                        };
                        c2.resolve(card.id, reply, "desktop");
                    }
                }
            }
        });
        confirmer
    }

    fn risky_outcome() -> RenderOutcome {
        RenderOutcome {
            page: Some(RenderedPage {
                title: "订单页".into(),
                text: "操作后的正文".into(),
                clicked: true,
                clicked_desc: "BUTTON「确认支付 ¥128.00」".into(),
                ..Default::default()
            }),
            session: Some("lw-render-3".into()),
            needs_confirm: Some(crate::webrender::PendingConfirm {
                target_text: "确认支付 ¥128.00".into(),
                kind: "click".into(),
                host: "x.example.com".into(),
            }),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn risky_click_denied_is_honest_observation() {
        let mut ctx = base_ctx("deny");
        let fake = Arc::new(FakeRender::new(risky_outcome()));
        ctx.web = Some(fake.clone());
        ctx.confirm = Some(confirmer_with_responder(&ctx.store, false));
        let out = WebRender::new()
            .run(serde_json::json!({"session": "lw-render-3", "click_ref": 5}), &ctx)
            .await
            .unwrap();
        assert!(out.contains("选择先不执行") && out.contains("点『确认支付 ¥128.00』"), "{out}");
        assert!(out.contains("x.example.com"), "{out}");
        assert!(out.contains("会话 lw-render-3 还开着"), "{out}");
        // 只渲染了一次(没有 confirmed 重发)
        let req = fake.1.lock().unwrap().clone().unwrap();
        assert!(!req.confirmed);
        // 审计落了 denied 一行(action = 目标原文,动词由消费端组 §6.6)
        let log = ctx.store.confirms.list_recent(5).unwrap();
        assert_eq!(log[0].decision, "denied");
        assert_eq!((log[0].action.as_str(), log[0].kind.as_str()), ("确认支付 ¥128.00", "click"));
    }

    #[tokio::test]
    async fn risky_click_allowed_resends_confirmed_with_expect_text() {
        let mut ctx = base_ctx("allow");
        let fake = Arc::new(FakeRender::new(risky_outcome()));
        ctx.web = Some(fake.clone());
        ctx.confirm = Some(confirmer_with_responder(&ctx.store, true));
        // 「url + click_text」一步式:撞闸重发必须续用壳层给的 session、清 url(别另开新窗)
        let out = WebRender::new()
            .run(
                serde_json::json!({"url": "https://x.example.com/pay", "click_text": "确认支付"}),
                &ctx,
            )
            .await
            .unwrap();
        // 允许后 = 正常操作结果(第二次渲染的页面)
        assert!(out.contains("BUTTON「确认支付 ¥128.00」"), "{out}");
        assert!(!out.contains("选择先不执行"), "{out}");
        let req = fake.1.lock().unwrap().clone().unwrap();
        assert!(req.confirmed, "重发必须带 confirmed(内部字段)");
        assert_eq!(req.expect_text.as_deref(), Some("确认支付 ¥128.00"));
        assert!(!req.force_confirm);
        assert_eq!(req.session.as_deref(), Some("lw-render-3"), "重发续用同一个窗");
        assert!(req.url.is_empty(), "重发不再带 url(否则另开新窗重导航)");
        let log = ctx.store.confirms.list_recent(5).unwrap();
        assert_eq!((log[0].decision.as_str(), log[0].via.as_str()), ("allowed", "desktop"));
    }

    #[tokio::test]
    async fn self_report_sets_force_confirm_and_no_confirmer_is_honest() {
        let mut ctx = base_ctx("selfreport");
        let fake = Arc::new(FakeRender::new(risky_outcome()));
        ctx.web = Some(fake.clone());
        // 自报 + 没有确认通道(ctx.confirm=None):明白话退回、动作没执行
        let err = WebRender::new()
            .run(
                serde_json::json!({"session": "s", "click_text": "领取", "confirm": true}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("没有确认通道"), "{err:#}");
        let req = fake.1.lock().unwrap().clone().unwrap();
        assert!(req.force_confirm, "自报要置 force_confirm 透传壳层");
        // 自报但没有落地动作(纯看页):忽略,不置 force_confirm
        let fake2 = Arc::new(FakeRender::new(RenderOutcome {
            page: Some(RenderedPage { title: "页".into(), text: "正文".into(), ..Default::default() }),
            session: Some("s".into()),
            ..Default::default()
        }));
        ctx.web = Some(fake2.clone());
        let _ = WebRender::new()
            .run(serde_json::json!({"url": "https://x.example.com", "confirm": true}), &ctx)
            .await
            .unwrap();
        assert!(!fake2.1.lock().unwrap().clone().unwrap().force_confirm);
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
