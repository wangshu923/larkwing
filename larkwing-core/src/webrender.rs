//! 壳层「网页渲染器」接缝(web_render 工具的机器件):要跑 JS 才出内容的页面(SPA)
//! 交给一个隐藏的系统 WebView 窗真渲染。开窗是壳层能力(core 不依赖 tauri,§6.1)——
//! core 只定义数据形 + trait,壳层 boot 时经 `Engine::set_web_renderer` 注入实现
//! (trait 接缝哲学,§5;没注入 = 工具如实说没有渲染组件,core 单测/eval 不受影响)。
//! 页面内容经 relay `/collect/{token}`(loopback、一次性 token)POST 回来 ——
//! **不给远程页任何 IPC 桥**(与 B 站扫码登录窗同风险等级;§7.4 网页内容 = 不可信输入)。

use std::path::PathBuf;
use std::time::Duration;

use crate::web::PageLink;

/// 单次上传的总字节上限(工具层验元数据、壳层读字节时再验)。取值沿用 web_download 的
/// 50MB 闸口径(`tools/web.rs::DOWNLOAD_MAX_BYTES`)——网页世界一次进出的文件同一尺度,
/// 不另造新默认;再大的 base64 分片注入也会让页面内存吃紧。
pub const UPLOAD_MAX_BYTES: u64 = 50 * 1024 * 1024;

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
    /// 动作的补充说明(编号对上了但动作有折扣/没做成时的人话:「这个框只收一个文件,
    /// 先传了第一个」「[N] 不是文件上传框」)。空 = 没什么要补充的。
    pub click_note: String,
    /// **编号交互元素**(文本版 Set-of-Marks,L2 会话式浏览的 grounding):快照时给页面
    /// 元素打 `data-lw-ref` 编号——可点的(按钮/链接)按编号 `click_ref` 点,可填的
    /// (输入框/文本域/可编辑区)按编号 `type_ref` 填、可选的(下拉)按编号 `select_ref` 选、
    /// 勾选框按编号点。同文字多元素不再靠猜。编号只在同一页面内有效(跳转即作废、重新编)。
    pub elements: Vec<PageElement>,
    /// 按编号操作但编号已失效(页面变了/元素没了):模型按新快照的编号重试。
    pub click_ref_stale: bool,
    /// 滚动位置提示(如「上面还有约 1 屏 / 下面还有约 3 屏」):模型据此决定要不要 scroll
    /// 去够屏外内容。空 = 一屏装得下 / 探不到。
    pub scroll_hint: String,
}

/// 一个编号交互元素。role:button / link / click(泛 onclick)/ input(文本框)/
/// textarea / select(下拉)/ checkbox / radio / editable(contenteditable)/
/// file(文件上传框——常见实现是 `display:none` 的隐藏 input,快照对它豁免可见性过滤)。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct PageElement {
    #[serde(rename = "ref")]
    pub ref_no: u32,
    pub role: String,
    /// 展示名:按钮/链接的文字,或输入类的 label(label/placeholder/aria-label/name 择一)。
    pub text: String,
    /// 链接类元素的目标(有 = 也可以直接交给 web_download)。
    pub href: Option<String>,
    /// 输入类当前值(input/textarea/select 选中项/contenteditable 文本);填后下张快照回读 =
    /// 天然的「填对没」校验。password 类不回读(见 `secret`)。
    pub value: String,
    /// checkbox / radio 的勾选态(其余类型为 None)。
    pub checked: Option<bool>,
    /// select 的可选项文本(供模型给 `select_option` 对上)。
    pub options: Vec<String>,
    /// 敏感字段(type=password):标出让模型知道它在,但**不回读 value、也不该往里填**
    /// (凭证交用户在可见小窗自己输;第一批不做闸,靠工具描述引导)。
    pub secret: bool,
    /// 文件上传框的 accept 属性(站点声明收什么,如 `.pdf,image/*`;空 = 没声明)。
    pub accept: String,
    /// 文件上传框是否收多个文件(`multiple` 属性)。
    pub multiple: bool,
}

/// 一次渲染/浏览请求(工具 → 壳层)。L2 会话式浏览(2026-07-10 解锁)+ **完全操作
/// 第一批(2026-07-14):看 / 点 / 返回 / 填字 / 批量填表 / 选下拉 / 按键 / 滚动** +
/// **文件上传(2026-07-15:本机文件 base64 分片注入 → 页面里组装 File + DataTransfer
/// 赋给 `input.files`,Playwright 同思路的纯 JS 版)**。窗口跨调用存活,连续走。一次调用
/// 做**一个主动作**(壳层按优先级择一:back > 输入类(upload/type/fill/select/press)>
/// click > scroll);`submit`(表单提交)是输入类的修饰,`wait_text`(动作后等文字出现再
/// 快照)对任何动作通用。**凭证代填 / CDP 可信输入仍不做**(§7.8)。
#[derive(Debug, Clone, Default)]
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
    /// 返回上一页(优先于一切动作)。
    pub back: bool,
    /// 往编号输入框/文本域/可编辑区填字(配 `type_text`)。
    pub type_ref: Option<u32>,
    /// 配 `type_ref` 的文本内容。
    pub type_text: Option<String>,
    /// 批量填表:一次填多个编号字段(比逐个 type 省轮次)。
    pub fill: Vec<FillField>,
    /// type / fill 之后提交所在表单(`form.requestSubmit()`;合成回车不触发提交,故走这条)。
    pub submit: bool,
    /// 往编号文件上传框传本机文件(配 `upload_paths`;优先级最高的输入动作)。
    pub upload_ref: Option<u32>,
    /// 配 `upload_ref`:要上传的本机文件绝对路径(工具层已验存在/大小;壳层读字节注入)。
    pub upload_paths: Vec<PathBuf>,
    /// 原生下拉按选项文本/值选(配 `select_option`)。
    pub select_ref: Option<u32>,
    /// 配 `select_ref` 的目标选项(文本或 value,壳层两边试匹配)。
    pub select_option: Option<String>,
    /// 按键:Enter / Escape / Tab / ArrowDown 等(喂 SPA 的按键监听 / 触发下拉)。
    pub press_key: Option<String>,
    /// 滚动翻页:"up" / "down"(够屏外内容)。
    pub scroll: Option<String>,
    /// 动作后等这段文字出现再快照(SPA 异步内容的显式等待;超时也照常快照,不算错)。
    pub wait_text: Option<String>,
    /// 模型自报「这一步有对外后果」(工具 `confirm` 参数,§7.8 确认闸的自报半边):
    /// 壳层解析动作目标后**必回 needs_confirm 不执行**(拿真实按钮文本/host 给确认卡),
    /// 与词表命中同路。只对 click/submit 有意义(工具层置位)。
    pub force_confirm: bool,
    /// **内部字段,绝不进工具 schema**:用户已点头,跳过高危词表/自报检查执行动作。
    /// 页面注入教模型传什么参数都够不到这里(工具入参解析不读它)。
    pub confirmed: bool,
    /// confirmed 重发时核对动作目标文本未变(变了 = 页面动了,按 stale 处理不执行——
    /// 顺手治「快照后按钮被 JS 换字」的正确性问题)。
    pub expect_text: Option<String>,
    /// 顺便截当前渲染窗一张图(模型自行决定要不要看画面;只对能看图的模型有用,非视觉出向降级)。
    /// **没有活跃渲染窗 = 截不了**(截图依附浏览窗,不凭空截);平台/组件不支持也如实回 None。
    pub screenshot: bool,
    /// 点击若触发下载,成品落这个目录(壳层负责唯一名,复用 `files::dedupe_path` 口径)。
    pub download_dir: PathBuf,
    /// 单步预算(壳层超时即收手回快照;会话窗本身由 TTL 管生死)。
    pub timeout: Duration,
}

/// 批量填表的一个字段(`fill` 数组元素)。
#[derive(Debug, Clone, Default)]
pub struct FillField {
    pub ref_no: u32,
    pub value: String,
}

/// 动作撞了确认闸(§7.8):壳层解析到目标、**没执行**,把现场信息交回工具层去问用户。
/// 用户允许 → 工具带 `confirmed + expect_text` 原样重发这一步。
#[derive(Debug, Clone, Default)]
pub struct PendingConfirm {
    /// 动作目标的现场文本(按钮文字 / 表单提交按钮文字;自报无按钮时为动作占位描述)——
    /// 确认卡原文 + 重发时的 `expect_text`。页面数据,非 core 文案。
    pub target_text: String,
    /// click | submit
    pub kind: String,
    /// 当前页 host(壳层从渲染窗现取,确认卡「在哪个站」)。
    pub host: String,
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
    /// 页面截图(data: URL,`data:image/png;base64,…`):仅 `screenshot=true` 且截到才有。
    /// 工具把它当图片 part 回给模型(工具结果多媒体第一个消费者;非视觉模型出向降级成占位)。
    pub screenshot: Option<String>,
    /// 动作撞确认闸没执行(高危词表命中 / 模型自报):工具层据此问用户、允许后重发。
    pub needs_confirm: Option<PendingConfirm>,
}

#[async_trait::async_trait]
pub trait WebRenderer: Send + Sync {
    async fn render(&self, req: RenderRequest) -> anyhow::Result<RenderOutcome>;
}
