//! 歌词(.lrc):两级来源 —— ① 平台人工 CC 字幕(下载路同源白拿,时间轴与视频对齐);
//! ② LRCLIB(lrclib.net,开放免鉴权的社区歌词库,按「歌名 + 歌手 + 时长」匹配)。
//! 形态 = 音频旁**同名 .lrc**(中文播放器生态事实标准),**绝不改动音频原件**;已有
//! .lrc 一律跳过不覆盖。找不到 = 如实说;时长容差把关防配错版本(Live/remix)——
//! 「宁可不配,绝不配错」(声纹 §9「宁可不认」同构)。野 API(网易云/QQ 歌词接口)不接。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::bgtasks::cap_names;
use crate::bus::Text;
use crate::components::Component;

use super::resolver::SubtitleRef;
use super::MediaRuntime;

/// 存量补歌词一次封顶(失控 backstop):超了如实退回让模型分批。
pub const LYRICS_BATCH_MAX: usize = 200;
/// 超过这个数转后台 job(一首约半秒,20 首内回合里等得起;再多任务条见)。
const IN_TURN_MAX: usize = 20;
/// LRCLIB 候选与本地时长的容差(秒):差得多 = 多半是别的版本,不要。
const DURATION_TOL: f64 = 3.0;
/// LRCLIB 基地址(测试注入假服务;正式恒为官网)。
const LRCLIB_API: &str = "https://lrclib.net/api";

/// 一次配词的结论(下载路与存量路共用;工具话术据此如实说)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LyricsResult {
    /// 来自平台字幕(人工 CC,时间轴与视频对齐)。
    Cc,
    /// 来自歌词库(带时间轴)。
    Lib,
    /// 来自歌词库(纯文本,无时间轴 —— 还是比没有强)。
    LibPlain,
    /// 旁边已有同名 .lrc,跳过(绝不覆盖)。
    Existed,
    /// 两级来源都没找到(或纯音乐)。
    NotFound,
}

/// 存量批量的单文件结论。
pub enum LyricsFileResult {
    Got(LyricsResult),
    /// 标签里没歌名、也没给 title 参数 → 让模型从文件名判断后带参重试。
    MissingTitle,
    /// 文件不存在 / 不是音频 / 写不进去(带原因)。
    Unusable(String),
}

pub struct LyricsItem {
    pub path: PathBuf,
    pub title: Option<String>,
    pub artist: Option<String>,
}

pub enum LyricsBatchOutcome {
    /// 回合内跑完:逐文件结论(工具层组话,只点名失败)。
    Report(Vec<(PathBuf, LyricsFileResult)>),
    /// 大批量已转后台 job(任务条见进度;配不上的按 fail 收尾点名数目)。
    JobStarted { total: usize },
}

impl MediaRuntime {
    /// 给本机已有音频批量配歌词(lyrics_fetch 工具的机器件)。ffmpeg 必备(读标签/时长,
    /// 首次用时下载);≤IN_TURN_MAX 回合内跑完出逐文件报告,更多转后台 job。
    /// `origin` = (user_id, conv_id):后台 job 收尾把结果插成一条 due=now 的一次性任务,
    /// 调度器捡起自启回合 → **模型拿到成败名单向用户转述**(不能只在任务条红一下,§3.5)。
    pub async fn lyrics_for_files(
        &self,
        items: Vec<LyricsItem>,
        origin: (i64, i64),
    ) -> Result<LyricsBatchOutcome> {
        anyhow::ensure!(!items.is_empty(), "没有收到文件");
        anyhow::ensure!(
            items.len() <= LYRICS_BATCH_MAX,
            "一次最多 {LYRICS_BATCH_MAX} 个文件,收到 {} 个——分批来",
            items.len()
        );
        let ffmpeg = self.ensure_component(Component::Ffmpeg).await?;
        if items.len() <= IN_TURN_MAX {
            let net = lyrics_client();
            let mut report = Vec::with_capacity(items.len());
            for it in &items {
                report.push((it.path.clone(), self.lyrics_one(&net, &ffmpeg, it).await));
            }
            return Ok(LyricsBatchOutcome::Report(report));
        }
        let total = items.len();
        // 登记进后台差事登记处(§bgtasks:此刻/status 可查、可取消、收尾/卡死必汇报);
        // cap 满 = 如实退回给模型。
        let ticket =
            self.inner.bg.submit(format!("批量配歌词({total} 个)"), origin, total)?;
        let ticket_id = ticket.id();
        let this = self.clone();
        let join = tokio::spawn(async move {
            let net = lyrics_client();
            let task = this.inner.tasks.start("lyrics", Text::new("task.lyrics"));
            let mut results: Vec<(PathBuf, LyricsFileResult)> = Vec::with_capacity(total);
            let mut cancelled = false;
            for (i, it) in items.iter().enumerate() {
                if ticket.is_cancelled() {
                    cancelled = true;
                    break;
                }
                let name = it
                    .path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                ticket.beat(i, format!("《{name}》"));
                task.step_progress(
                    "step.lyrics_batch",
                    serde_json::json!({ "t": name, "i": i + 1, "n": total }),
                    i as f32 / total as f32,
                );
                let r = this.lyrics_one(&net, &ffmpeg, it).await;
                match &r {
                    LyricsFileResult::Got(
                        LyricsResult::Lib | LyricsResult::LibPlain | LyricsResult::Existed,
                    ) => {}
                    LyricsFileResult::Unusable(why) => {
                        tracing::warn!(path = %it.path.display(), "配歌词没成: {why}");
                        ticket.miss(&name);
                    }
                    _ => ticket.miss(&name),
                }
                results.push((it.path.clone(), r));
            }
            let missed = results
                .iter()
                .filter(|(_, r)| {
                    !matches!(
                        r,
                        LyricsFileResult::Got(
                            LyricsResult::Lib | LyricsResult::LibPlain | LyricsResult::Existed
                        )
                    )
                })
                .count();
            if cancelled {
                task.fail("task.err.cancelled", serde_json::Value::Null);
            } else if missed == 0 {
                task.done();
            } else {
                task.fail(
                    "task.err.lyrics_batch",
                    serde_json::json!({ "fail": missed, "total": total }),
                );
            }
            // 收尾汇报经登记处唤回合(§7.4 wake_turn 同一套机器),模型向用户转述。
            let report = if cancelled {
                format!(
                    "批量配歌词按要求停下了(跑到 {}/{total}):{}。把结果简短告诉用户;\
                     剩下的要不要接着配,听用户的。",
                    results.len(),
                    compose_batch_summary(&results)
                )
            } else {
                format!(
                    "批量配歌词跑完了(共 {total} 个):{}。把结果简短告诉用户;\
                     没找到/缺歌名的要不要接着处理,听用户的。",
                    compose_batch_summary(&results)
                )
            };
            ticket.finish(!cancelled && missed == 0, report);
        });
        self.inner.bg.attach_abort(ticket_id, join.abort_handle());
        Ok(LyricsBatchOutcome::JobStarted { total })
    }

    /// 单文件:读标签/时长(override 优先)→ 歌词库 → 写旁挂 .lrc。
    async fn lyrics_one(
        &self,
        net: &crate::net::Client,
        ffmpeg: &Path,
        it: &LyricsItem,
    ) -> LyricsFileResult {
        if !it.path.is_file() {
            return LyricsFileResult::Unusable("文件不存在".into());
        }
        if !super::probe::is_audio_ext(&it.path) {
            return LyricsFileResult::Unusable("不是音频文件".into());
        }
        if it.path.with_extension("lrc").exists() {
            return LyricsFileResult::Got(LyricsResult::Existed);
        }
        let pr = self.probe_with_ffmpeg(ffmpeg, &it.path).await;
        let Some(title) = it.title.clone().or(pr.tag_title).filter(|s| !s.trim().is_empty())
        else {
            return LyricsFileResult::MissingTitle;
        };
        let artist = it.artist.clone().or(pr.tag_artist).filter(|s| !s.trim().is_empty());
        match lrclib_lookup(net, LRCLIB_API, &title, artist.as_deref(), pr.duration_seconds)
            .await
        {
            Ok(Some((lrc, synced))) => match write_lrc_beside(&it.path, &lrc) {
                Ok(true) => LyricsFileResult::Got(if synced {
                    LyricsResult::Lib
                } else {
                    LyricsResult::LibPlain
                }),
                Ok(false) => LyricsFileResult::Got(LyricsResult::Existed),
                Err(e) => LyricsFileResult::Unusable(format!("写歌词文件失败: {e:#}")),
            },
            Ok(None) => LyricsFileResult::Got(LyricsResult::NotFound),
            // 网络级失败与「没找到」分开报(§3.5 如实):失败是能重试的,没找到不是。
            Err(e) => LyricsFileResult::Unusable(format!("歌词库没连上: {e:#}")),
        }
    }
}

/// 逐文件结论 → 一段人读得懂的汇总(工具结果与后台收尾汇报共用单源;量约束 §7.2:
/// 汇总数字 + 只点名要处理的,每类点名封顶 cap_names)。
pub(crate) fn compose_batch_summary(results: &[(PathBuf, LyricsFileResult)]) -> String {
    let mut done = 0usize;
    let mut plain = 0usize;
    let mut existed = 0usize;
    let (mut not_found, mut missing, mut unusable) = (Vec::new(), Vec::new(), Vec::new());
    let stem = |p: &Path| p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    for (path, r) in results {
        match r {
            LyricsFileResult::Got(LyricsResult::Lib | LyricsResult::Cc) => done += 1,
            LyricsFileResult::Got(LyricsResult::LibPlain) => {
                done += 1;
                plain += 1;
            }
            LyricsFileResult::Got(LyricsResult::Existed) => existed += 1,
            LyricsFileResult::Got(LyricsResult::NotFound) => not_found.push(stem(path)),
            LyricsFileResult::MissingTitle => missing.push(stem(path)),
            LyricsFileResult::Unusable(why) => unusable.push(format!("{}({why})", stem(path))),
        }
    }
    let mut out = format!("配好 {done} 个");
    if plain > 0 {
        out.push_str(&format!("(其中 {plain} 个是纯文本歌词、无逐句时间轴)"));
    }
    if existed > 0 {
        out.push_str(&format!(";{existed} 个旁边已有歌词文件,跳过没动"));
    }
    if !not_found.is_empty() {
        out.push_str(&format!(";没找到歌词 {} 个:{}", not_found.len(), cap_names(&not_found)));
    }
    if !missing.is_empty() {
        out.push_str(&format!(
            ";缺歌名 {} 个(从文件名判断出歌名/歌手后,带 title/artist 对它们重试):{}",
            missing.len(),
            cap_names(&missing)
        ));
    }
    if !unusable.is_empty() {
        out.push_str(&format!(";处理不了:{}", cap_names(&unusable)));
    }
    out
}

/// 下载完成后配歌词(download 路;错误只 warn,**绝不影响下载成败**):
/// 人工 CC 字幕优先(时间轴与视频对齐,B 站剪辑版也对得上)→ 歌词库。
pub(super) async fn lyrics_for_download(
    net: &crate::net::Client,
    subs: &[SubtitleRef],
    sub_headers: &[(String, String)],
    title: &str,
    artist: Option<&str>,
    duration: Option<f64>,
    audio_path: &Path,
) -> LyricsResult {
    if audio_path.with_extension("lrc").exists() {
        return LyricsResult::Existed;
    }
    if let Some(sub) = pick_subtitle(subs) {
        match fetch_json(net, &sub.url, sub_headers).await {
            Ok(v) => {
                if let Some(lrc) = bilibili_sub_to_lrc(&v) {
                    match write_lrc_beside(audio_path, &lrc) {
                        Ok(true) => return LyricsResult::Cc,
                        Ok(false) => return LyricsResult::Existed,
                        Err(e) => tracing::warn!("写字幕歌词失败,转歌词库: {e:#}"),
                    }
                }
            }
            Err(e) => tracing::info!("字幕拉取失败,转歌词库: {e:#}"),
        }
    }
    match lrclib_lookup(net, LRCLIB_API, title, artist, duration).await {
        Ok(Some((lrc, synced))) => match write_lrc_beside(audio_path, &lrc) {
            Ok(true) => {
                if synced {
                    LyricsResult::Lib
                } else {
                    LyricsResult::LibPlain
                }
            }
            Ok(false) => LyricsResult::Existed,
            Err(e) => {
                tracing::warn!("写歌词文件失败: {e:#}");
                LyricsResult::NotFound
            }
        },
        Ok(None) => LyricsResult::NotFound,
        Err(e) => {
            tracing::warn!("歌词库查询失败: {e:#}");
            LyricsResult::NotFound
        }
    }
}

/// 歌词/字幕客户端:LRCLIB 明说希望调用方带可识别 UA(免费公共服务,识别友好);
/// 国内直连可达性未验 —— net::Client 直连优先→失败落代理正好兜底(§4.6)。
fn lyrics_client() -> crate::net::Client {
    crate::net::Client::new(|b| {
        b.user_agent(concat!("larkwing/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(20))
    })
}

/// LRCLIB 查询梯子:① 精确 `/get`(歌手+时长,服务端 ±2s)→ ② `/search` 歌名+歌手
/// **含简繁双轨变体**(2026-07-27 真机实锤:港台老歌在库里普遍**繁体**收录——简体
/// 《电台情歌》查空、繁体《電台情歌》16 条带时间轴;大陆用户嘴里/标签里全是简体,
/// 查询侧双向出变体补齐)→ ③ 只按歌名(丢歌手,救「Karen Mok/莫文蔚」两套拼写),
/// 加「歌名归一相等」闸防同名不同歌。每级都过时长容差。
/// Ok(None) = 没找到/纯音乐;Err = 网络级失败。
async fn lrclib_lookup(
    net: &crate::net::Client,
    api_base: &str,
    title: &str,
    artist: Option<&str>,
    duration: Option<f64>,
) -> Result<Option<(String, bool)>> {
    // ① 精确 get(最快路;原文命中就不折腾变体)
    if let (Some(a), Some(d)) = (artist, duration) {
        let url = format!("{api_base}/get");
        let dur = format!("{}", d.round() as i64);
        let resp = net
            .send(&url, |c| {
                c.get(&url).query(&[
                    ("track_name", title),
                    ("artist_name", a),
                    ("duration", dur.as_str()),
                ])
            })
            .await
            .context("歌词库请求失败")?;
        if resp.status().is_success() {
            let v: serde_json::Value = resp.json().await.context("歌词库响应不是 JSON")?;
            // /get 的时长匹配服务端已做,这里不再卡容差
            if let Some(hit) = pick_lyrics(std::slice::from_ref(&v), None, None) {
                return Ok(Some(hit));
            }
        }
        // 404(没精确命中)或其它非 2xx:都落 search 再试,不在这儿定生死
    }
    // ② 歌名+歌手(原文 → 简繁变体逐个试)
    for (t, a) in title_variants(title, artist) {
        if let Some(hit) = search_pick(net, api_base, &t, a.as_deref(), duration, None).await? {
            return Ok(Some(hit));
        }
    }
    // ③ 只按歌名兜底(库里歌手拼写对不上时;归一相等闸 + 时长容差双保险)
    if artist.is_some() {
        for (t, _) in title_variants(title, None) {
            if let Some(hit) =
                search_pick(net, api_base, &t, None, duration, Some(title)).await?
            {
                return Ok(Some(hit));
            }
        }
    }
    Ok(None)
}

/// 查询用的简繁变体:原文优先,再补「简→繁」「繁→简」里真的变了字的(去重)。
/// 歌手同向转换(劉德華↔刘德华;ASCII 名转换是 no-op)。
/// **台/臺 歧义特判**(2026-07-27 实锤):转换器按正字给「電臺」,而唱片元数据/LRCLIB
/// 几乎都用「電台」(真查:電台情歌 16 条、電臺情歌 0 条)→ 繁体变体含「臺」时**再补一个
/// 全换「台」的形,且台形靠前**(命中率高省一次请求)。再撞同类歧义字照此加,别上大表。
fn title_variants(title: &str, artist: Option<&str>) -> Vec<(String, Option<String>)> {
    use character_converter::{simplified_to_traditional, traditional_to_simplified};
    let mut out: Vec<(String, Option<String>)> =
        vec![(title.to_string(), artist.map(str::to_string))];
    let push = |t: String, a: Option<String>, out: &mut Vec<(String, Option<String>)>| {
        if out.iter().all(|(seen, _)| *seen != t) {
            out.push((t, a));
        }
    };
    for conv in [simplified_to_traditional, traditional_to_simplified] {
        let t = conv(title).into_owned();
        let a = artist.map(|x| conv(x).into_owned());
        if t.contains('臺') {
            push(t.replace('臺', "台"), a.clone().map(|x| x.replace('臺', "台")), &mut out);
        }
        push(t, a, &mut out);
    }
    out
}

/// 跑一次 `/search` 并挑候选。`want_title` = 只按歌名搜时的归一相等闸(防同名不同歌)。
async fn search_pick(
    net: &crate::net::Client,
    api_base: &str,
    title: &str,
    artist: Option<&str>,
    duration: Option<f64>,
    want_title: Option<&str>,
) -> Result<Option<(String, bool)>> {
    let url = format!("{api_base}/search");
    let mut q: Vec<(&str, &str)> = vec![("track_name", title)];
    if let Some(a) = artist {
        q.push(("artist_name", a));
    }
    let resp = net.send(&url, |c| c.get(&url).query(&q)).await.context("歌词库请求失败")?;
    anyhow::ensure!(resp.status().is_success(), "歌词库查询失败 HTTP {}", resp.status());
    let v: serde_json::Value = resp.json().await.context("歌词库响应不是 JSON")?;
    let list = v.as_array().cloned().unwrap_or_default();
    Ok(pick_lyrics(&list, duration, want_title))
}

/// 候选里挑一条(纯函数可测):滤纯音乐;知道本地时长就按容差把关(候选没报时长
/// 也不敢认),多条同过时挑**时长最接近**的(时间轴对得最齐);`want_title` 给了则
/// 歌名归一相等才认(简繁/空白/大小写归一)。优先带时间轴,都没有才要纯文本。
fn pick_lyrics(
    cands: &[serde_json::Value],
    want_dur: Option<f64>,
    want_title: Option<&str>,
) -> Option<(String, bool)> {
    let dur_ok = |c: &serde_json::Value| match (want_dur, c["duration"].as_f64()) {
        (Some(w), Some(d)) => (w - d).abs() <= DURATION_TOL,
        (Some(_), None) => false,
        (None, _) => true,
    };
    let title_ok = |c: &serde_json::Value| match want_title {
        Some(w) => c["trackName"].as_str().is_some_and(|t| norm_title(t) == norm_title(w)),
        None => true,
    };
    // 时长差(不知道本地时长 = 不参与排序,保持原序)
    let dur_diff = |c: &serde_json::Value| match (want_dur, c["duration"].as_f64()) {
        (Some(w), Some(d)) => (w - d).abs(),
        _ => 0.0,
    };
    let mut usable: Vec<_> = cands
        .iter()
        .filter(|c| c["instrumental"] != serde_json::Value::Bool(true))
        .filter(|c| dur_ok(c) && title_ok(c))
        .collect();
    usable.sort_by(|a, b| dur_diff(a).total_cmp(&dur_diff(b)));
    for c in &usable {
        if let Some(s) = c["syncedLyrics"].as_str().filter(|s| !s.trim().is_empty()) {
            return Some((s.to_string(), true));
        }
    }
    for c in &usable {
        if let Some(s) = c["plainLyrics"].as_str().filter(|s| !s.trim().is_empty()) {
            return Some((s.to_string(), false));
        }
    }
    None
}

/// 歌名归一(比较用):繁→简 + 去空白 + 小写。
fn norm_title(s: &str) -> String {
    character_converter::traditional_to_simplified(s)
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

/// 平台字幕挑一条:优先中文(zh*),否则第一条。外语歌的 zh 字幕常是翻译 ——
/// 中文用户拿它当歌词也是常用形态,工具话术会标注「来自视频字幕」。
fn pick_subtitle(subs: &[SubtitleRef]) -> Option<&SubtitleRef> {
    subs.iter().find(|s| s.lang.starts_with("zh")).or_else(|| subs.first())
}

/// B 站字幕 JSON(`body:[{from,to,content}]`)→ LRC 文本。非该形状/空内容 → None(别硬转)。
fn bilibili_sub_to_lrc(json: &serde_json::Value) -> Option<String> {
    let body = json["body"].as_array()?;
    let mut out = String::new();
    for item in body {
        let (Some(from), Some(content)) = (item["from"].as_f64(), item["content"].as_str())
        else {
            continue;
        };
        let content = content.trim();
        if content.is_empty() {
            continue;
        }
        let cs = (from.max(0.0) * 100.0).round() as u64;
        out.push_str(&format!(
            "[{:02}:{:02}.{:02}]{}\n",
            cs / 6000,
            (cs / 100) % 60,
            cs % 100,
            content
        ));
    }
    (!out.is_empty()).then_some(out)
}

/// 音频旁写同名 .lrc:已存在 = 跳过(Ok(false),绝不覆盖);先临时件再改名。
fn write_lrc_beside(audio: &Path, content: &str) -> Result<bool> {
    let dest = audio.with_extension("lrc");
    if dest.exists() {
        return Ok(false);
    }
    let tmp = dest.with_file_name(format!(".lw-lrc-{}.tmp", std::process::id()));
    std::fs::write(&tmp, content)
        .with_context(|| format!("写不进歌词临时文件 {}", tmp.display()))?;
    std::fs::rename(&tmp, &dest).or_else(|_| {
        std::fs::copy(&tmp, &dest).map(|_| ()).and_then(|()| std::fs::remove_file(&tmp))
    })?;
    Ok(true)
}

async fn fetch_json(
    net: &crate::net::Client,
    url: &str,
    headers: &[(String, String)],
) -> Result<serde_json::Value> {
    let resp = net
        .send(url, |c| {
            let mut req = c.get(url);
            for (k, v) in headers {
                req = req.header(k, v);
            }
            req
        })
        .await
        .context("字幕请求失败")?;
    anyhow::ensure!(resp.status().is_success(), "字幕拉取失败 HTTP {}", resp.status());
    resp.json().await.context("字幕不是 JSON")
}

/// 旁挂 .lrc 上限:正常歌词几 KB,超大的当异常不带(防怪文件撑爆 IPC 事件)。
const SIDECAR_MAX_BYTES: u64 = 200 * 1024;

/// 本地音频旁边的同名 .lrc(lyrics_fetch / 下载链的产物,或用户自己攒的):有就整份
/// 原文带给前端滚当前句(歌词是数据不是我们产的文案,§6.6)。老 .lrc 常是 GBK →
/// 非 UTF-8 按 GB18030 回退解码(乱码 = 白做);解不出/超大/为空一律 None,绝不半截。
pub(super) fn sidecar_lyrics(path: &std::path::Path) -> Option<String> {
    let lrc = path.with_extension("lrc");
    let meta = std::fs::metadata(&lrc).ok()?;
    if !meta.is_file() || meta.len() > SIDECAR_MAX_BYTES {
        return None;
    }
    let bytes = std::fs::read(&lrc).ok()?;
    let text = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => {
            let (s, _, bad) = encoding_rs::GB18030.decode(e.as_bytes());
            if bad {
                return None;
            }
            s.into_owned()
        }
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_lyrics_reads_utf8_and_gbk_skips_odd() {
        let dir = std::env::temp_dir().join(format!("lw-lrc-side-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let song = dir.join("歌.m4a");
        std::fs::write(&song, b"x").unwrap();
        // 没有 .lrc → None
        assert!(sidecar_lyrics(&song).is_none());
        // UTF-8 原样
        std::fs::write(dir.join("歌.lrc"), "[00:01.00]第一句").unwrap();
        assert_eq!(sidecar_lyrics(&song).unwrap(), "[00:01.00]第一句");
        // GBK 老编码解得回中文
        let (gbk, _, _) = encoding_rs::GB18030.encode("[00:02.00]老编码歌词");
        std::fs::write(dir.join("歌.lrc"), &gbk[..]).unwrap();
        assert_eq!(sidecar_lyrics(&song).unwrap(), "[00:02.00]老编码歌词");
        // 空白 → None;超大 → None
        std::fs::write(dir.join("歌.lrc"), "  \n ").unwrap();
        assert!(sidecar_lyrics(&song).is_none());
        std::fs::write(dir.join("歌.lrc"), vec![b'x'; (SIDECAR_MAX_BYTES + 1) as usize]).unwrap();
        assert!(sidecar_lyrics(&song).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sub_to_lrc_formats_timestamps() {
        let json = serde_json::json!({
            "body": [
                { "from": 1.5, "to": 3.0, "content": "第一句" },
                { "from": 75.25, "to": 80.0, "content": "第二句" },
                { "from": 90.0, "to": 91.0, "content": "  " }
            ]
        });
        let lrc = bilibili_sub_to_lrc(&json).unwrap();
        assert_eq!(lrc, "[00:01.50]第一句\n[01:15.25]第二句\n", "空行剔除、分秒进位");
        assert!(bilibili_sub_to_lrc(&serde_json::json!({"x": 1})).is_none(), "非字幕形不硬转");
    }

    #[test]
    fn pick_prefers_synced_and_guards_duration() {
        let cands = vec![
            serde_json::json!({ "duration": 251.0, "instrumental": false,
                                "syncedLyrics": "", "plainLyrics": "纯文本" }),
            serde_json::json!({ "duration": 251.0, "instrumental": false,
                                "syncedLyrics": "[00:01.00]词", "plainLyrics": "词" }),
        ];
        let (l, synced) = pick_lyrics(&cands, Some(250.0), None).unwrap();
        assert!(synced && l.contains("[00:01.00]"), "优先带时间轴");

        assert!(
            pick_lyrics(&cands, Some(200.0), None).is_none(),
            "时长差 50s = 别的版本,宁可不配"
        );
        let inst = vec![serde_json::json!({ "duration": 250.0, "instrumental": true,
                                            "syncedLyrics": "[00:01.00]x" })];
        assert!(pick_lyrics(&inst, Some(250.0), None).is_none(), "纯音乐不配词");
        let no_dur = vec![serde_json::json!({ "instrumental": false, "plainLyrics": "词" })];
        assert!(pick_lyrics(&no_dur, Some(250.0), None).is_none(), "候选没报时长不敢认");
        let (_, synced) = pick_lyrics(&no_dur, None, None).unwrap();
        assert!(!synced, "本地时长未知则不卡容差,纯文本也收");
    }

    #[test]
    fn pick_takes_closest_duration_and_title_gate_normalizes() {
        // 多条同过容差 → 挑时长最接近的(时间轴对得最齐)
        let cands = vec![
            serde_json::json!({ "trackName": "電台情歌", "duration": 245.3,
                                "instrumental": false, "syncedLyrics": "[00:01.00]远版" }),
            serde_json::json!({ "trackName": "電台情歌", "duration": 246.8,
                                "instrumental": false, "syncedLyrics": "[00:01.00]近版" }),
        ];
        let (l, _) = pick_lyrics(&cands, Some(247.0), None).unwrap();
        assert!(l.contains("近版"), "挑时长最接近的: {l}");

        // 歌名归一闸:繁简/空白/大小写归一后相等才认;不等的(同名不同歌/改版名)拒
        let (l, _) = pick_lyrics(&cands, Some(247.0), Some("电台情歌")).unwrap();
        assert!(l.contains("近版"), "「電台情歌」归一后等于「电台情歌」");
        assert!(
            pick_lyrics(&cands, Some(247.0), Some("电台情歌2011")).is_none(),
            "歌名不等 → 只按歌名搜的兜底不敢认"
        );
    }

    #[test]
    fn variants_cover_traditional_script() {
        let v = title_variants("电台情歌", Some("莫文蔚"));
        assert_eq!(v[0].0, "电台情歌", "原文优先");
        // 真实案例钉死:库里的形是「電台情歌」(台),转换器正字给「電臺」——
        // 两个繁体形都必须在,且台形在臺形前(元数据惯用形先试,省请求)。
        let pos_tai = v.iter().position(|(t, _)| t == "電台情歌").expect("必须有台形变体");
        let pos_taii = v.iter().position(|(t, _)| t == "電臺情歌").expect("必须有臺形变体");
        assert!(pos_tai < pos_taii, "台形靠前: {v:?}");
        // 繁体输入反向也出简体变体
        let v2 = title_variants("電台情歌", None);
        assert!(v2.iter().any(|(t, _)| t.contains('电')), "{v2:?}");
    }

    /// 真网回归(要连 lrclib.net,开发机手动跑:`cargo test lrclib_real -- --ignored`):
    /// 真机实锤案例「电台情歌/莫文蔚/247s」——库里只有繁体「電台情歌」条目,
    /// 简繁梯子必须真命中带时间轴的歌词。
    #[tokio::test]
    #[ignore = "要真网(lrclib.net),开发机手动跑"]
    async fn lrclib_real_lookup_traditional_entry() {
        let net = lyrics_client();
        let hit = lrclib_lookup(&net, LRCLIB_API, "电台情歌", Some("莫文蔚"), Some(247.0))
            .await
            .expect("lrclib 网络可达")
            .expect("简繁梯子应命中繁体收录的条目");
        assert!(hit.1, "应拿到带时间轴的版本");
        assert!(hit.0.contains('['), "LRC 形: {}", &hit.0[..hit.0.len().min(80)]);
    }

    /// 端到端走梯子:歌名+歌手原文 search 空 → 简繁变体 search 命中(真机「电台情歌」案)。
    #[tokio::test]
    async fn lrclib_ladder_finds_traditional_entry() {
        use axum::{extract::Query, routing::get, Router};
        use std::collections::HashMap;
        async fn get404() -> (axum::http::StatusCode, &'static str) {
            (axum::http::StatusCode::NOT_FOUND, "{\"message\":\"not found\"}")
        }
        async fn search(Query(q): Query<HashMap<String, String>>) -> String {
            let track = q.get("track_name").map(String::as_str).unwrap_or("");
            // 模拟库里只有繁体条目(简体查空 = 真实 LRCLIB 行为)
            if track.contains('電') {
                serde_json::json!([{ "trackName": track, "duration": 248.3,
                                     "instrumental": false,
                                     "syncedLyrics": "[00:29.00]繁体库里的词" }])
                .to_string()
            } else {
                "[]".to_string()
            }
        }
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/api/get", get(get404)).route("/api/search", get(search)),
            )
            .await
            .ok();
        });
        let net = lyrics_client();
        let base = format!("http://127.0.0.1:{port}/api");
        let (lrc, synced) = lrclib_lookup(&net, &base, "电台情歌", Some("莫文蔚"), Some(247.0))
            .await
            .unwrap()
            .expect("简繁变体轨应命中");
        assert!(synced && lrc.contains("繁体库里的词"));
    }

    #[test]
    fn batch_summary_categorizes_and_caps_names() {
        use LyricsFileResult as R;
        let p = |n: &str| PathBuf::from(format!("/x/{n}"));
        let mut results = vec![
            (p("a.m4a"), R::Got(LyricsResult::Lib)),
            (p("b.flac"), R::Got(LyricsResult::LibPlain)),
            (p("c.mp3"), R::Got(LyricsResult::Existed)),
            (p("d.mp3"), R::MissingTitle),
            (p("e.mp3"), R::Unusable("文件不存在".into())),
        ];
        for i in 0..15 {
            results.push((p(&format!("没词{i}.mp3")), R::Got(LyricsResult::NotFound)));
        }
        let s = compose_batch_summary(&results);
        assert!(s.contains("配好 2 个") && s.contains("其中 1 个是纯文本"), "{s}");
        assert!(s.contains("1 个旁边已有歌词文件"), "{s}");
        assert!(s.contains("没找到歌词 15 个") && s.contains("等 15 个"), "点名封顶: {s}");
        assert!(s.contains("缺歌名 1 个") && s.contains("d.mp3"), "{s}");
        assert!(s.contains("处理不了:e.mp3(文件不存在)"), "{s}");
    }

    #[test]
    fn subtitle_pick_prefers_chinese() {
        let subs = vec![
            SubtitleRef { lang: "en".into(), url: "e".into() },
            SubtitleRef { lang: "zh-Hans".into(), url: "z".into() },
        ];
        assert_eq!(pick_subtitle(&subs).unwrap().url, "z");
    }

    #[test]
    fn lrc_beside_never_overwrites() {
        let dir = std::env::temp_dir().join(format!("lw-lrc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let audio = dir.join("某曲目.m4a");
        std::fs::write(&audio, b"x").unwrap();
        assert!(write_lrc_beside(&audio, "[00:01.00]词\n").unwrap());
        assert!(!write_lrc_beside(&audio, "别的词").unwrap(), "已有 = 跳过");
        assert_eq!(std::fs::read_to_string(dir.join("某曲目.lrc")).unwrap(), "[00:01.00]词\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 假 LRCLIB(axum):/get 404 → /search 命中,时长容差在客户端把关。
    #[tokio::test]
    async fn lrclib_lookup_falls_back_to_search() {
        use axum::{extract::Query, routing::get, Router};
        use std::collections::HashMap;
        async fn get404() -> (axum::http::StatusCode, &'static str) {
            (axum::http::StatusCode::NOT_FOUND, "{\"message\":\"not found\"}")
        }
        async fn search(Query(q): Query<HashMap<String, String>>) -> String {
            assert_eq!(q.get("track_name").map(String::as_str), Some("示例曲目"));
            serde_json::json!([
                { "duration": 100.0, "instrumental": false, "syncedLyrics": "[00:02.00]太长版" },
                { "duration": 250.0, "instrumental": false, "syncedLyrics": "[00:01.00]对的版" }
            ])
            .to_string()
        }
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/api/get", get(get404)).route("/api/search", get(search)),
            )
            .await
            .ok();
        });
        let net = lyrics_client();
        let base = format!("http://127.0.0.1:{port}/api");
        let (lrc, synced) =
            lrclib_lookup(&net, &base, "示例曲目", Some("某演唱者"), Some(251.0))
                .await
                .unwrap()
                .unwrap();
        assert!(synced && lrc.contains("对的版"), "时长 ±3s 挑对版本: {lrc}");
    }
}
