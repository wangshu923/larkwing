//! 壳层「网页渲染器」接缝(web_render 工具的机器件):要跑 JS 才出内容的页面(SPA)
//! 交给一个隐藏的系统 WebView 窗真渲染。开窗是壳层能力(core 不依赖 tauri,§6.1)——
//! core 只定义数据形 + trait,壳层 boot 时经 `Engine::set_web_renderer` 注入实现
//! (trait 接缝哲学,§5;没注入 = 工具如实说没有渲染组件,core 单测/eval 不受影响)。
//! 页面内容经 relay `/collect/{token}`(loopback、一次性 token)POST 回来 ——
//! **不给远程页任何 IPC 桥**(与 B 站扫码登录窗同风险等级;§7.4 网页内容 = 不可信输入)。

use std::path::PathBuf;
use std::time::Duration;

use crate::web::PageLink;

/// 渲染后页面(注入脚本抽取、`/collect` 回传的 JSON 形)。字段全宽松缺省 ——
/// 载荷来自页面世界,坏形状只当空,绝不让一张恶意页把回合炸掉。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct RenderedPage {
    pub title: String,
    pub text: String,
    pub links: Vec<PageLink>,
    /// 渲染后 DOM 里「可点」元素的文字(button / [onclick] / role=button 这类无 href 的):
    /// 模型据此决定第二跳的 click_text。
    pub clickables: Vec<String>,
    /// click_text 命中并点了(不代表点出了下载 —— 下载由原生 on_download 判)。
    pub clicked: bool,
    /// 实际点中的元素描述(如 `BUTTON「下载」`):多个候选时模型能核对点没点对。
    pub clicked_desc: String,
    /// **编号交互元素**(文本版 Set-of-Marks,L2 会话式浏览的 grounding):快照时给页面
    /// 元素打 `data-lw-ref` 编号,下一步 `click_ref` 按编号点——同文字多按钮不再靠猜。
    /// 编号只在同一页面内有效(跳转即作废,快照会重新编)。
    pub elements: Vec<PageElement>,
    /// 按编号点击但编号已失效(页面变了/元素没了):模型按新快照的编号重试。
    pub click_ref_stale: bool,
}

/// 一个编号交互元素。role:button / link / input / click(泛 onclick 类)。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct PageElement {
    #[serde(rename = "ref")]
    pub ref_no: u32,
    pub role: String,
    pub text: String,
    /// 链接类元素的目标(有 = 也可以直接交给 web_download)。
    pub href: Option<String>,
}

/// 一次渲染/浏览请求(工具 → 壳层)。L2 会话式浏览(2026-07-10 用户拍板解锁):
/// 窗口跨调用存活,看 → 点(编号)→ 返回 → 再看连续走;**动作空间只有这三样,
/// 输入/填表仍不做**(等 Tool::risk 确认闸门,§7.8)。
#[derive(Debug, Clone)]
pub struct RenderRequest {
    /// 打开的地址。带 session 续用时可空(空 = 不导航,只在当前页上动作/观察)。
    pub url: String,
    /// 继续已有会话窗(上次结局里的 session id);None(配 url)= 开新窗。
    /// 窗已收摊(TTL/被挤)= 明白话错误,模型带 url 重开。
    pub session: Option<String>,
    /// 点上次快照里的编号元素(同页有效;优先于 click_text)。
    pub click_ref: Option<u32>,
    /// 按文字点第一个包含该文字的可点元素(没有编号时的退路;命中排序见壳层)。
    pub click_text: Option<String>,
    /// 返回上一页(优先于点击)。
    pub back: bool,
    /// 点击若触发下载,成品落这个目录(壳层负责唯一名,复用 `files::dedupe_path` 口径)。
    pub download_dir: PathBuf,
    /// 单步预算(壳层超时即收手回快照;会话窗本身由 TTL 管生死)。
    pub timeout: Duration,
}

/// 渲染结局:页面快照(None = 一直没回传)+ 下载产物(点击触发才有)+ 点击后跳转
/// + 会话号(窗还活着,可继续)。
#[derive(Debug, Default)]
pub struct RenderOutcome {
    pub page: Option<RenderedPage>,
    pub download: Option<PathBuf>,
    /// 点击后页面跳去的地址(有跳转、没下载时才有意义):往往就是文件直链——
    /// 模型可接 web_download 试(带会话的平台可能下不动,下不动如实说)。
    pub post_click_url: Option<String>,
    /// 会话窗 id(TTL 内可继续:带 session + click_ref/back 再调)。
    pub session: Option<String>,
}

#[async_trait::async_trait]
pub trait WebRenderer: Send + Sync {
    async fn render(&self, req: RenderRequest) -> anyhow::Result<RenderOutcome>;
}
