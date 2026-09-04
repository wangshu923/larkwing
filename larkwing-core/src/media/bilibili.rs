//! B 站源:公开搜索 API(robot bilibili/api.py 移植)。搜索不走 yt-dlp ——
//! 直调 API 快,且返回结构化的标题/UP主/时长,正好喂播放卡片;流解析才归 yt-dlp。
//! 已知风险(robot 注释原样继承):B 站可能收紧 WBI 签名,届时此处拿到 -412/-403,
//! 错误按 RiskControl 上抛(带登录态时概率显著降低),签名实现参考 yt-dlp。

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use super::{EpisodeRef, MediaHit, MediaSource, SearchError, SpriteSheet};

const SEARCH_URL: &str = "https://api.bilibili.com/x/web-interface/search/type";
/// 视频详情(分P `pages` + 合集 `ugc_season`):非 WBI 端点,UA+Referer 即可,多集发现走它。
const VIEW_URL: &str = "https://api.bilibili.com/x/web-interface/view";
/// 番剧(PGC)整季详情:`ep_id=` / `season_id=` 二选一,一次回整季 `episodes`。番剧与 UGC 稿件
/// 是**两套内容体系**——番剧集的 bvid 拿去问 VIEW_URL 是 -404,多集发现只能走这个端点。
/// 免 WBI 签名(与 VIEW_URL 同)。
const PGC_SEASON_URL: &str = "https://api.bilibili.com/pgc/view/web/season";
/// 进度条预览雪碧图(`bvid=` 必填、`cid=` 选 P、`index=1` 才带采样时刻表)。免 WBI(2026-09-04
/// 实测:不带 UA/Referer 也 200);`code=-404` = 稿件不存在、`-400` = 缺 bvid。**分P 不带 cid 恒回
/// P1 的图**(实测 P1/P2 各自一张),所以 `?p=N` 的 cid 必须先查到,查不到宁可没图。
const VIDEOSHOT_URL: &str = "https://api.bilibili.com/x/player/videoshot";
/// 分P 清单(每 P 的 cid):比 VIEW_URL 轻、且 view 被 412 风控时它仍 200(2026-09-04 实测),
/// 「第 N P 的 cid」只从这里拿。
const PAGELIST_URL: &str = "https://api.bilibili.com/x/player/pagelist";
/// 裸 UA 常被 412,挂一个像真浏览器的(robot 同款手法,版本号更新)。
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";
const REFERER: &str = "https://www.bilibili.com/";

pub struct Bilibili {
    net: crate::net::Client,
}

impl Bilibili {
    pub fn new() -> Bilibili {
        let net = crate::net::Client::new(|b| {
            b.connect_timeout(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(15))
        });
        Bilibili { net }
    }

    /// 「尽力件」端点的共用请求:UA + Referer(+ 登录 cookie)GET 一个 JSON。非 200(含 412/403
    /// 风控)→ `Ok(None)` 静默退化;传输 / 非 JSON → Err(调用方记日志后同样退化)。`code` 由调用方看。
    async fn fetch_json(
        &self,
        url: &str,
        query: &[(&str, &str)],
        cookie_header: Option<&str>,
    ) -> Result<Option<serde_json::Value>> {
        let resp = self
            .net
            .send(url, |c| {
                let req = c.get(url).query(query).header("User-Agent", UA).header("Referer", REFERER);
                match cookie_header {
                    Some(cookie) => req.header("Cookie", cookie),
                    None => req,
                }
            })
            .await
            .map_err(|e| anyhow!("{url} 请求失败: {e}"))?;
        if resp.status().as_u16() != 200 {
            return Ok(None);
        }
        let payload: serde_json::Value =
            resp.json().await.map_err(|e| anyhow!("{url} 响应不是 JSON: {e}"))?;
        Ok(Some(payload))
    }

    /// 番剧整季详情的 `result` 载荷(注意番剧走 `result`、UGC 走 `data`,别抄错)。
    /// 「尽力件」:任何一步不顺一律 `Ok(None)`。多集发现与雪碧图(找这一集的 bvid/cid)共用。
    async fn pgc_season(
        &self,
        pgc: &PgcRef,
        cookie_header: Option<&str>,
    ) -> Result<Option<serde_json::Value>> {
        let (param, value) = pgc.query();
        let Some(payload) = self.fetch_json(PGC_SEASON_URL, &[(param, value)], cookie_header).await?
        else {
            return Ok(None); // 含 412/403 风控:静默退化(播放路径会引导扫码登录)
        };
        if payload["code"].as_i64().unwrap_or(-1) != 0 {
            return Ok(None);
        }
        Ok(Some(payload["result"].clone()))
    }

    /// 番剧整季发现。与 UGC 那条同款「尽力件」纪律:任何一步不顺一律 `Ok(None)` 退化成单集。
    async fn pgc_episodes(
        &self,
        pgc: &PgcRef,
        cookie_header: Option<&str>,
    ) -> Result<Option<(String, Vec<EpisodeRef>)>> {
        Ok(self.pgc_season(pgc, cookie_header).await?.as_ref().and_then(parse_season))
    }

    /// 进度条预览雪碧图:`bvid` 必填,`cid` 选分P(None = P1 / 单 P)。**尽力件**:非 200 / `code≠0`
    /// / 形状不对 一律 `Ok(None)`(= 没有预览图,不是失败)。解析归 `parse_videoshot`(纯函数)。
    pub(crate) async fn videoshot(
        &self,
        bvid: &str,
        cid: Option<u64>,
        cookie_header: Option<&str>,
    ) -> Result<Option<SpriteSheet>> {
        let cid_s = cid.map(|c| c.to_string());
        let mut query = vec![("bvid", bvid), ("index", "1")];
        if let Some(c) = cid_s.as_deref() {
            query.push(("cid", c));
        }
        let Some(payload) = self.fetch_json(VIDEOSHOT_URL, &query, cookie_header).await? else {
            return Ok(None);
        };
        Ok(parse_videoshot(&payload))
    }

    /// 第 `page` P(1 起)的 cid。找不到那一 P / 端点不顺 → `Ok(None)`。
    async fn page_cid(
        &self,
        bvid: &str,
        page: u32,
        cookie_header: Option<&str>,
    ) -> Result<Option<u64>> {
        let Some(payload) = self.fetch_json(PAGELIST_URL, &[("bvid", bvid)], cookie_header).await?
        else {
            return Ok(None);
        };
        if payload["code"].as_i64().unwrap_or(-1) != 0 {
            return Ok(None);
        }
        Ok(page_cid_from(&payload["data"], page))
    }
}

#[async_trait]
impl MediaSource for Bilibili {
    fn id(&self) -> &'static str {
        "bilibili"
    }

    fn login_url(&self) -> &'static str {
        "https://passport.bilibili.com/login"
    }

    fn cookie_url(&self) -> &'static str {
        "https://www.bilibili.com"
    }

    fn login_cookie(&self) -> &'static str {
        "SESSDATA"
    }

    async fn search(
        &self,
        keyword: &str,
        limit: usize,
        cookie_header: Option<&str>,
    ) -> Result<Vec<MediaHit>, SearchError> {
        let keyword = keyword.trim();
        if keyword.is_empty() {
            return Ok(Vec::new());
        }
        let resp = self
            .net
            .send(SEARCH_URL, |c| {
                let req = c
                    .get(SEARCH_URL)
                    .query(&[
                        ("search_type", "video"),
                        ("keyword", keyword),
                        ("page", "1"),
                        ("order", "totalrank"),
                    ])
                    .header("User-Agent", UA)
                    .header("Referer", "https://www.bilibili.com/");
                match cookie_header {
                    Some(cookie) => req.header("Cookie", cookie),
                    None => req,
                }
            })
            .await
            .map_err(|e| SearchError::Other(anyhow!("搜索请求失败: {e}")))?;
        let status = resp.status().as_u16();
        if status == 412 || status == 403 {
            return Err(SearchError::RiskControl);
        }
        if status != 200 {
            return Err(SearchError::Other(anyhow!("搜索 HTTP {status}")));
        }
        let payload: serde_json::Value =
            resp.json().await.map_err(|e| SearchError::Other(anyhow!("搜索响应不是 JSON: {e}")))?;
        let code = payload["code"].as_i64().unwrap_or(-1);
        if code == -412 || code == -403 || code == -101 {
            return Err(SearchError::RiskControl);
        }
        if code != 0 {
            let msg = payload["message"].as_str().unwrap_or("?");
            return Err(SearchError::Other(anyhow!("搜索 code={code} message={msg}")));
        }
        Ok(parse_results(&payload, limit))
    }

    /// 多集发现,**两条端点各管一套内容体系**:番剧(ep/ss)走 PGC season;UGC 稿件(BV)走 view
    /// API 的 `pages`(分P)与 `ugc_season`(合集)。**尽力件**——拿不到(短链 / 风控 / 非视频)
    /// 一律 `Ok(None)` 退化成单集,绝不挡播放(风控后续由 resolve 的 AuthRequired 引导登录,
    /// 登录重放时带 cookie 再发现一次)。
    async fn episodes(
        &self,
        page_url: &str,
        cookie_header: Option<&str>,
    ) -> Result<Option<(String, Vec<EpisodeRef>)>> {
        // 番剧优先判:它的 URL 里没有 BV 号,落到下面的 extract_bvid 只会一路 None。
        if let Some(pgc) = extract_pgc(page_url) {
            return self.pgc_episodes(&pgc, cookie_header).await;
        }
        let Some(bvid) = extract_bvid(page_url) else {
            return Ok(None); // b23.tv 短链 / av 号 → 不在分P/合集发现范围
        };
        let resp = self
            .net
            .send(VIEW_URL, |c| {
                let req = c
                    .get(VIEW_URL)
                    .query(&[("bvid", bvid.as_str())])
                    .header("User-Agent", UA)
                    .header("Referer", "https://www.bilibili.com/");
                match cookie_header {
                    Some(cookie) => req.header("Cookie", cookie),
                    None => req,
                }
            })
            .await
            .map_err(|e| anyhow!("view 请求失败: {e}"))?;
        if resp.status().as_u16() != 200 {
            return Ok(None); // 含 412/403 风控:静默退化单集(resolve 路径会处理登录)
        }
        let payload: serde_json::Value =
            resp.json().await.map_err(|e| anyhow!("view 响应不是 JSON: {e}"))?;
        if payload["code"].as_i64().unwrap_or(-1) != 0 {
            return Ok(None);
        }
        Ok(parse_view(&payload["data"], &bvid))
    }

    /// 进度条预览雪碧图(`player/videoshot`)。三种页面形态各自定位到「哪一 P」:
    ///   · 番剧 ep 形 → season 端点找这一集的 bvid + cid(番剧集自带 bvid,videoshot 认;ss 形
    ///     到不了这里 —— build_queue 早把它换成首集的 ep 形,真到了就 None 别硬凑);
    ///   · UGC `?p=N`(N≥2)→ pagelist 拿第 N P 的 cid;**拿不到就 None**(不带 cid 回的是 P1 的图,
    ///     拿 P1 的图冒充第 N P 比没图更糟,§3.5);
    ///   · UGC 单 P / P1 → bvid 即可。
    /// 尽力件:短链 / av 号 / 任何一步不顺 → `Ok(None)`。调用方(play_entry)另套超时。
    async fn sprites(
        &self,
        page_url: &str,
        cookie_header: Option<&str>,
    ) -> Result<Option<SpriteSheet>> {
        if let Some(pgc) = extract_pgc(page_url) {
            let PgcRef::Ep(ep_id) = &pgc else { return Ok(None) };
            let Some(result) = self.pgc_season(&pgc, cookie_header).await? else {
                return Ok(None);
            };
            let Some((bvid, cid)) = pgc_episode_ids(&result, ep_id) else { return Ok(None) };
            return self.videoshot(&bvid, Some(cid), cookie_header).await;
        }
        let Some(bvid) = extract_bvid(page_url) else { return Ok(None) };
        let cid = match extract_page(page_url) {
            Some(p) if p >= 2 => match self.page_cid(&bvid, p, cookie_header).await? {
                Some(cid) => Some(cid),
                None => return Ok(None),
            },
            _ => None,
        };
        self.videoshot(&bvid, cid, cookie_header).await
    }
}

/// 从页面 URL 抽 BV 号(`BV` 后接的字母数字)。没有 → None(短链 / av / 番剧 ep)。
fn extract_bvid(url: &str) -> Option<String> {
    let i = url.find("BV")?;
    let rest = &url[i..];
    let end = rest[2..]
        .find(|c: char| !c.is_ascii_alphanumeric())
        .map(|e| e + 2)
        .unwrap_or(rest.len());
    let bvid = &rest[..end];
    (bvid.len() >= 5).then(|| bvid.to_string())
}

/// 从页面 URL 抽分P 号(`?p=3` / `&p=3`,1 起)。没有 / 不是正整数 → None(按 P1 处理)。
fn extract_page(url: &str) -> Option<u32> {
    let query = url.split_once('?')?.1;
    let query = query.split('#').next().unwrap_or(query);
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix("p="))
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|p| *p >= 1)
}

/// pagelist 的 `data`(数组,每项 `{cid, page, …}`)里找第 `page` P 的 cid。纯函数、可测。
fn page_cid_from(data: &serde_json::Value, page: u32) -> Option<u64> {
    data.as_array()?
        .iter()
        .find(|p| p["page"].as_u64() == Some(page as u64))
        .and_then(|p| p["cid"].as_u64())
}

/// 番剧 season `result` 里找 ep 号为 `ep_id` 的那一集的 `(bvid, cid)`。videoshot 要 bvid(必填)
/// + cid(定位到这一集);缺任一 → None(别拿别集的图冒充)。纯函数、可测。
fn pgc_episode_ids(result: &serde_json::Value, ep_id: &str) -> Option<(String, u64)> {
    let want = ep_id.parse::<i64>().ok()?;
    result["episodes"].as_array()?.iter().find_map(|ep| {
        let id = ep["ep_id"].as_i64().or_else(|| ep["id"].as_i64())?;
        if id != want {
            return None;
        }
        let bvid = ep["bvid"].as_str().filter(|s| s.starts_with("BV"))?;
        let cid = ep["cid"].as_u64().filter(|c| *c > 0)?;
        Some((bvid.to_string(), cid))
    })
}

/// 雪碧图大图地址补协议:B 站给的是 `//i0.hdslb.com/...` 无协议形,补成 https;已带协议的原样。
fn absolutize(url: &str) -> String {
    if url.starts_with("//") {
        format!("https:{url}")
    } else {
        url.to_string()
    }
}

/// 雪碧图单边像素上限:HD 雪碧图是 4800×2700,8192 已留足余量;再大 = 不是雪碧图(或数据坏了),
/// 别照着它的声明去解码一张巨图。
const SPRITE_SIDE_MAX: u64 = 8192;

/// 解析 `player/videoshot` 整个响应(含 `code`)。纯函数、可测。真实形状(2026-09-04 实测):
/// ```json
/// {"code":0,"data":{"img_x_len":10,"img_y_len":10,"img_x_size":160,"img_y_size":90,
///   "image":["//i0.hdslb.com/bfs/videoshot/137649199.jpg"],"index":[0,0,5,11,17,…,211],
///   "pvdata":"//i0.hdslb.com/bfs/videoshot/137649199.bin","video_shots":{},"indexs":{}}}
/// ```
/// **`index` 是「帧数 + 1」项、首项是哨兵不是采样时刻**:两张真图核对 —— 33 项 ↔ 32 格有图
/// (格 0 = 片头黑帧,与格 1 完全不同)、44 项 ↔ 43 格;`pvdata` 二进制也是同样的 33 个 u16、
/// 首字为 0(格式头字)。故这里**剥掉首项**,得到「第 k 帧采样在 index[k] 秒」的干净表交 relay;
/// 不剥的话按 `index[i] ≤ t` 定格会整体错后一帧、末尾还会指到一个空格。
/// `code≠0` / 没有大图 / 没带 index=1(空表或只剩哨兵)/ 格尺寸为 0 / 声明尺寸离谱 → None。
pub(crate) fn parse_videoshot(payload: &serde_json::Value) -> Option<SpriteSheet> {
    if payload["code"].as_i64().unwrap_or(-1) != 0 {
        return None;
    }
    let data = &payload["data"];
    let dim = |key: &str| data[key].as_u64().filter(|v| *v > 0).and_then(|v| u32::try_from(v).ok());
    let (x_len, y_len) = (dim("img_x_len")?, dim("img_y_len")?);
    let (tile_w, tile_h) = (dim("img_x_size")?, dim("img_y_size")?);
    if x_len as u64 * tile_w as u64 > SPRITE_SIDE_MAX || y_len as u64 * tile_h as u64 > SPRITE_SIDE_MAX {
        return None;
    }
    let images: Vec<String> = data["image"]
        .as_array()?
        .iter()
        .filter_map(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(absolutize)
        .collect();
    if images.is_empty() {
        return None;
    }
    let raw = data["index"].as_array()?;
    if raw.len() < 2 {
        return None; // 没带 index=1(空表)或只剩哨兵 → 没有时刻表,定不了格
    }
    let index: Vec<f64> = raw[1..].iter().filter_map(|v| v.as_f64()).collect();
    if index.len() != raw.len() - 1 {
        return None; // 混进非数字 = 形状变了,别猜
    }
    Some(SpriteSheet {
        x_len,
        y_len,
        tile_w,
        tile_h,
        images,
        index,
        headers: vec![("User-Agent".into(), UA.into()), ("Referer".into(), REFERER.into())],
    })
}

/// 番剧(PGC)链接的两种形态。UGC 稿件(BV 号)不在此列 —— 两套内容体系、两套端点。
#[derive(Debug, PartialEq)]
enum PgcRef {
    /// `/bangumi/play/ep742483` → 单集
    Ep(String),
    /// `/bangumi/play/ss44871` → 整季
    Season(String),
}

impl PgcRef {
    /// 两种形态查同一个 season 端点,只是参数名不同。
    fn query(&self) -> (&'static str, &str) {
        match self {
            PgcRef::Ep(id) => ("ep_id", id),
            PgcRef::Season(id) => ("season_id", id),
        }
    }
}

/// 从页面 URL 抽番剧的 ep / ss 号。**必须落在 `/bangumi/play/` 路径下**——否则别处
/// 碰巧出现的 "ep"/"ss" 字样会被误认(`extract_bvid` 按 `BV` 大写字样找不会撞,这里两个
/// 小写字母太常见,得靠路径约束)。
fn extract_pgc(url: &str) -> Option<PgcRef> {
    let rest = url.split("/bangumi/play/").nth(1)?;
    // 号后面常跟 ?spm_id_from=… 之类跟踪参数,取到第一个非数字为止。
    let digits = |s: &str| -> Option<String> {
        let n: String = s.chars().take_while(char::is_ascii_digit).collect();
        (!n.is_empty()).then_some(n)
    };
    if let Some(s) = rest.strip_prefix("ep") {
        return digits(s).map(PgcRef::Ep);
    }
    if let Some(s) = rest.strip_prefix("ss") {
        return digits(s).map(PgcRef::Season);
    }
    None
}

/// 解析 `pgc/view/web/season` 的 `result`(注意番剧走 `result`、UGC 走 `data`)。纯函数、可测。
/// 只收 `episodes`(正片);`section`(PV / 特别篇 / 预告)刻意不并进队列——「自动播下一集」
/// 要的是正片顺序,混进花絮会把连播打乱。<2 集 → None(不成系列,同 `parse_view`)。
fn parse_season(result: &serde_json::Value) -> Option<(String, Vec<EpisodeRef>)> {
    let mut eps = Vec::new();
    for (i, ep) in result["episodes"].as_array()?.iter().enumerate() {
        // ep 号既是集身份也是可播地址的唯一来源,缺了就拼不出地址 → 跳过(同 ugc_season 滤 bvid)。
        let Some(id) = ep["ep_id"].as_i64().or_else(|| ep["id"].as_i64()) else { continue };
        eps.push(EpisodeRef {
            id: format!("ep{id}"),
            // **必须是 bangumi/play/ep 形**:番剧集自带的 bvid 在 UGC view 端点是 -404、
            // yt-dlp 也放不了,存 bvid 等于存了个放不出来的地址。
            url: format!("https://www.bilibili.com/bangumi/play/ep{id}"),
            title: episode_title(ep, i),
        });
    }
    if eps.len() < 2 {
        return None;
    }
    let key = result["season_id"]
        .as_i64()
        .map(|i| format!("bili:pgc:{i}"))
        // 缺 season_id 也别丢掉队列:拿首集 ep 号当季身份(首集不会变)。
        .unwrap_or_else(|| format!("bili:pgc:{}", eps[0].id));
    Some((key, eps))
}

/// 集名:番剧的 `title` 是集号("1" / "OVA" / "特别篇"),`long_title` 才是副标题。
/// 数字集号补成「第N集」,非数字原样留(别把 "OVA" 硬套成「第OVA集」);都空 → 按序号兜底。
fn episode_title(ep: &serde_json::Value, i: usize) -> String {
    let num = ep["title"].as_str().unwrap_or("").trim();
    let sub = ep["long_title"].as_str().unwrap_or("").trim();
    let head = if num.is_empty() {
        format!("第{}集", i + 1)
    } else if num.chars().all(|c| c.is_ascii_digit()) {
        format!("第{num}集")
    } else {
        num.to_string()
    };
    if sub.is_empty() {
        head
    } else {
        format!("{head} {sub}")
    }
}

/// 解析 view API 的 `data`:**合集优先**(ugc_season,整季多个 BV),其次**分P**(单 BV 多 P)。
/// 单集(无合集 + ≤1 P)→ None。纯函数、可测。集身份 `id`:合集用 bvid、分P 用 `pN`;
/// 分P 的 P1 用**裸 bvid url**(对齐 build_queue 的 url 匹配),P2+ 带 `?p=N`。
fn parse_view(data: &serde_json::Value, bvid: &str) -> Option<(String, Vec<EpisodeRef>)> {
    // 合集(ugc_season):跨 sections 拍平 episodes,每集一个独立 BV。
    if let Some(season) = data.get("ugc_season").filter(|v| v.is_object()) {
        let mut eps = Vec::new();
        if let Some(sections) = season["sections"].as_array() {
            for sec in sections {
                let Some(arr) = sec["episodes"].as_array() else { continue };
                for ep in arr {
                    let Some(bv) = ep["bvid"].as_str().filter(|s| s.starts_with("BV")) else {
                        continue;
                    };
                    let title = ep["title"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("第{}集", eps.len() + 1));
                    eps.push(EpisodeRef {
                        id: bv.to_string(),
                        url: format!("https://www.bilibili.com/video/{bv}"),
                        title,
                    });
                }
            }
        }
        if eps.len() >= 2 {
            let key = season["id"]
                .as_i64()
                .map(|i| format!("bili:season:{i}"))
                .unwrap_or_else(|| format!("bili:bv:{bvid}"));
            return Some((key, eps));
        }
    }
    // 分P(单 BV 多 P)。
    if let Some(pages) = data["pages"].as_array().filter(|p| p.len() >= 2) {
        let eps = pages
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let page = p["page"].as_i64().unwrap_or((i + 1) as i64);
                let title = p["part"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("P{page}"));
                let url = if page <= 1 {
                    format!("https://www.bilibili.com/video/{bvid}")
                } else {
                    format!("https://www.bilibili.com/video/{bvid}?p={page}")
                };
                EpisodeRef { id: format!("p{page}"), url, title }
            })
            .collect();
        return Some((format!("bili:bv:{bvid}"), eps));
    }
    None
}

fn parse_results(payload: &serde_json::Value, limit: usize) -> Vec<MediaHit> {
    let empty = Vec::new();
    let items = payload["data"]["result"].as_array().unwrap_or(&empty);
    items
        .iter()
        .filter_map(|item| {
            let bvid = item["bvid"].as_str()?;
            if !bvid.starts_with("BV") {
                return None;
            }
            Some(MediaHit {
                url: format!("https://www.bilibili.com/video/{bvid}"),
                title: clean_title(item["title"].as_str().unwrap_or("")),
                author: item["author"].as_str().unwrap_or("").to_string(),
                duration_seconds: parse_duration(item["duration"].as_str().unwrap_or("")),
                source: "bilibili".into(),
            })
        })
        .take(limit)
        .collect()
}

/// 去掉搜索结果标题里的高亮标签(<em class="keyword">…</em>)+ HTML 实体解码。
fn clean_title(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find('<') {
        let (head, tail) = rest.split_at(start);
        out.push_str(head);
        match tail.find('>') {
            // 只剥 em / /em 标签,别的尖括号当正文保留(标题里真可能有 <3 这种)
            Some(end) if tail[1..end].trim_start_matches('/').starts_with("em") => {
                rest = &tail[end + 1..];
            }
            _ => {
                out.push('<');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    unescape(&out).trim().to_string()
}

fn unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

/// B 站 duration 形如 "3:45" / "1:23:45";解析不出 = 0。
fn parse_duration(s: &str) -> i64 {
    let mut total = 0i64;
    for part in s.trim().split(':') {
        match part.parse::<i64>() {
            Ok(n) => total = total * 60 + n,
            Err(_) => return 0,
        }
    }
    if s.trim().is_empty() {
        0
    } else {
        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_search_payload_and_cleans_titles() {
        let payload = serde_json::json!({
            "code": 0,
            "data": { "result": [
                {
                    "bvid": "BV1xx411c7mD",
                    "title": "<em class=\"keyword\">恭喜发财</em> 刘德华 &amp; 高清",
                    "author": "某音乐区UP",
                    "duration": "3:45"
                },
                { "bvid": "av123", "title": "不是BV的过滤掉", "author": "x", "duration": "1:00" },
                {
                    "bvid": "BV1yy411c7mE",
                    "title": "时长带小时 &lt;3",
                    "author": "y",
                    "duration": "1:02:03"
                }
            ]}
        });
        let hits = parse_results(&payload, 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].url, "https://www.bilibili.com/video/BV1xx411c7mD");
        assert_eq!(hits[0].title, "恭喜发财 刘德华 & 高清");
        assert_eq!(hits[0].duration_seconds, 225);
        assert_eq!(hits[1].title, "时长带小时 <3");
        assert_eq!(hits[1].duration_seconds, 3723);
    }

    #[test]
    fn limit_caps_results() {
        let payload = serde_json::json!({
            "code": 0,
            "data": { "result": [
                { "bvid": "BV1", "title": "a", "author": "", "duration": "0:10" },
                { "bvid": "BV2", "title": "b", "author": "", "duration": "0:10" }
            ]}
        });
        assert_eq!(parse_results(&payload, 1).len(), 1);
    }

    #[test]
    fn duration_edge_cases() {
        assert_eq!(parse_duration(""), 0);
        assert_eq!(parse_duration("abc"), 0);
        assert_eq!(parse_duration("45"), 45);
    }

    #[test]
    fn extract_bvid_from_urls() {
        assert_eq!(
            extract_bvid("https://www.bilibili.com/video/BV1xx411c7mD").as_deref(),
            Some("BV1xx411c7mD")
        );
        // 带 ?p / 其它 query 也能抽出
        assert_eq!(
            extract_bvid("https://www.bilibili.com/video/BV1xx411c7mD?p=3&t=10").as_deref(),
            Some("BV1xx411c7mD")
        );
        // 短链 / av 号 / 番剧 ep → 无 BV
        assert_eq!(extract_bvid("https://b23.tv/abcdef"), None);
        assert_eq!(extract_bvid("https://www.bilibili.com/bangumi/play/ep123"), None);
    }

    #[test]
    fn parse_view_prefers_ugc_season() {
        let data = serde_json::json!({
            "pages": [ {"page":1,"part":"正片"} ], // 只有 1 P,但属于合集 → 合集赢
            "ugc_season": {
                "id": 778899,
                "sections": [
                    {"episodes": [
                        {"bvid":"BV1aa","title":"第一集 出发"},
                        {"bvid":"BV1bb","title":"第二集 抵达"},
                        {"bvid":"BV1cc","title":""} // 空标题 → 兜底"第3集"
                    ]}
                ]
            }
        });
        let (key, eps) = parse_view(&data, "BV1aa").unwrap();
        assert_eq!(key, "bili:season:778899");
        assert_eq!(eps.len(), 3);
        assert_eq!(eps[0].id, "BV1aa");
        assert_eq!(eps[0].url, "https://www.bilibili.com/video/BV1aa");
        assert_eq!(eps[1].title, "第二集 抵达");
        assert_eq!(eps[2].title, "第3集", "空标题兜底");
    }

    #[test]
    fn parse_view_multipart_when_no_season() {
        let data = serde_json::json!({
            "pages": [
                {"cid":1,"page":1,"part":"第1集"},
                {"cid":2,"page":2,"part":"第2集"},
                {"cid":3,"page":3,"part":""} // 空 → "P3"
            ]
        });
        let (key, eps) = parse_view(&data, "BV1zz").unwrap();
        assert_eq!(key, "bili:bv:BV1zz");
        assert_eq!(eps.len(), 3);
        // P1 用裸 url(对齐 build_queue 的 url 匹配),P2+ 带 ?p=
        assert_eq!(eps[0].url, "https://www.bilibili.com/video/BV1zz");
        assert_eq!(eps[0].id, "p1");
        assert_eq!(eps[1].url, "https://www.bilibili.com/video/BV1zz?p=2");
        assert_eq!(eps[2].title, "P3", "空 part 兜底");
    }

    #[test]
    fn parse_view_single_video_is_none() {
        // 单 P、无合集 → 不成系列
        let data = serde_json::json!({ "pages": [ {"page":1,"part":"正片"} ] });
        assert!(parse_view(&data, "BV1solo").is_none());
        // 啥都没有也 None
        assert!(parse_view(&serde_json::json!({}), "BV1x").is_none());
    }

    #[test]
    fn extract_pgc_from_bangumi_urls() {
        assert_eq!(
            extract_pgc("https://www.bilibili.com/bangumi/play/ep742483"),
            Some(PgcRef::Ep("742483".into()))
        );
        assert_eq!(
            extract_pgc("https://www.bilibili.com/bangumi/play/ss44871"),
            Some(PgcRef::Season("44871".into()))
        );
        // 分享链常带跟踪参数,照样认得出号
        assert_eq!(
            extract_pgc("https://www.bilibili.com/bangumi/play/ep742483?spm_id_from=333.1007"),
            Some(PgcRef::Ep("742483".into()))
        );
        // UGC 稿件 / 短链 / bangumi 路径但没号 → 不是番剧
        assert_eq!(extract_pgc("https://www.bilibili.com/video/BV1xx411c7mD"), None);
        assert_eq!(extract_pgc("https://b23.tv/abcdef"), None);
        assert_eq!(extract_pgc("https://www.bilibili.com/bangumi/play/"), None);
        // 「ep」「ss」这两个字母组合出现在别处不该误认(必须在 bangumi 路径下)
        assert_eq!(extract_pgc("https://example.com/deep/ep123"), None);
    }

    /// 夹具形状照真实 `pgc/view/web/season` 返回(季 44871「安全警长啦咘啦哆」,52 集)。
    #[test]
    fn parse_season_builds_queue_from_pgc_episodes() {
        let result = serde_json::json!({
            "season_id": 44871,
            "season_title": "安全警长啦咘啦哆",
            "episodes": [
                {"id": 742483, "ep_id": 742483, "bvid": "BV1aM4y1B7L9",
                 "title": "1", "long_title": "小鸡弟弟失踪案",
                 "link": "https://www.bilibili.com/bangumi/play/ep742483"},
                {"id": 742484, "ep_id": 742484, "title": "2", "long_title": "犀牛弟弟被抓走了"},
                {"id": 742485, "ep_id": 742485, "title": "3", "long_title": ""}
            ]
        });
        let (key, eps) = parse_season(&result).unwrap();
        assert_eq!(key, "bili:pgc:44871", "季 key 用 pgc 前缀,绝不与 ugc_season 的 bili:season: 撞车");
        assert_eq!(eps.len(), 3);
        // 集身份 = ep 号;url 必须用 bangumi/play/ep 形 —— 番剧集自带的 bvid 在 UGC view
        // 端点是 -404、yt-dlp 也放不了,存 bvid 形等于存了个放不出来的地址。
        assert_eq!(eps[0].id, "ep742483");
        assert_eq!(eps[0].url, "https://www.bilibili.com/bangumi/play/ep742483");
        assert_eq!(eps[0].title, "第1集 小鸡弟弟失踪案");
        assert_eq!(eps[1].url, "https://www.bilibili.com/bangumi/play/ep742484");
        assert_eq!(eps[2].title, "第3集", "没副标题就只报集号");
    }

    #[test]
    fn parse_season_keeps_non_numeric_episode_labels() {
        let (_, eps) = parse_season(&serde_json::json!({
            "season_id": 3,
            "episodes": [
                {"ep_id": 1, "title": "OVA", "long_title": "特别篇"},
                {"ep_id": 2, "title": "", "long_title": ""}
            ]
        }))
        .unwrap();
        assert_eq!(eps[0].title, "OVA 特别篇", "非数字集号原样保留,别硬套「第X集」");
        assert_eq!(eps[1].title, "第2集", "两边都空 → 按序号兜底");
    }

    #[test]
    fn parse_season_edge_cases() {
        // 只有 1 集 / 没有 episodes → 不成系列
        assert!(parse_season(&serde_json::json!({
            "season_id": 1, "episodes": [{"ep_id": 9, "title": "1"}]
        }))
        .is_none());
        assert!(parse_season(&serde_json::json!({ "season_id": 1 })).is_none());
        // 拼不出可播地址的条目跳过(缺 ep 号)
        let (_, eps) = parse_season(&serde_json::json!({
            "season_id": 2,
            "episodes": [{"ep_id": 11, "title": "1"}, {"title": "坏条目"}, {"ep_id": 13, "title": "3"}]
        }))
        .unwrap();
        assert_eq!(eps.len(), 2, "缺 ep 号的条目跳过,不生成放不了的地址");
        // 缺 season_id 也别丢掉队列 —— 拿首集 ep 号当季身份(首集不会变)
        let (key, _) = parse_season(&serde_json::json!({
            "episodes": [{"ep_id": 5, "title": "1"}, {"ep_id": 6, "title": "2"}]
        }))
        .unwrap();
        assert_eq!(key, "bili:pgc:ep5");
    }

    /// 夹具 = 2026-09-04 真实响应(BV1GJ411x7h7,index 截短)。断言格数 / 尺寸 / URL 补协议 /
    /// **哨兵剥离**(33 项 → 32 帧的规则,见 parse_videoshot 注释)/ 防盗链头随表走。
    #[test]
    fn parse_videoshot_real_shape() {
        let payload = serde_json::json!({
            "code": 0, "message": "OK", "ttl": 1,
            "data": {
                "pvdata": "//i0.hdslb.com/bfs/videoshot/137649199.bin",
                "img_x_len": 10, "img_y_len": 10, "img_x_size": 160, "img_y_size": 90,
                "image": ["//i0.hdslb.com/bfs/videoshot/137649199.jpg"],
                "index": [0, 0, 5, 11, 17, 23, 29],
                "video_shots": {}, "indexs": {}
            }
        });
        let s = parse_videoshot(&payload).expect("真实形状应解析出来");
        assert_eq!((s.x_len, s.y_len, s.tile_w, s.tile_h), (10, 10, 160, 90));
        assert_eq!(s.images, vec!["https://i0.hdslb.com/bfs/videoshot/137649199.jpg"], "补 https:");
        assert_eq!(s.index, vec![0.0, 5.0, 11.0, 17.0, 23.0, 29.0], "首项哨兵剥掉,第 k 帧 = index[k]");
        assert!(s.headers.iter().any(|(k, v)| k == "Referer" && v == REFERER), "防盗链头随表走");
        assert!(s.headers.iter().any(|(k, _)| k == "User-Agent"));
        // 多张大图(长片)+ 已带协议的地址原样保留
        let payload = serde_json::json!({
            "code": 0,
            "data": {
                "img_x_len": 10, "img_y_len": 10, "img_x_size": 480, "img_y_size": 270,
                "image": ["https://bimp.hdslb.com/a-0001.jpg", "http://bimp.hdslb.com/a-0002.jpg"],
                "index": [0, 0, 6]
            }
        });
        let s = parse_videoshot(&payload).unwrap();
        assert_eq!(s.images, vec!["https://bimp.hdslb.com/a-0001.jpg", "http://bimp.hdslb.com/a-0002.jpg"]);
        assert_eq!(s.index, vec![0.0, 6.0]);
    }

    #[test]
    fn parse_videoshot_rejects_unusable_payloads() {
        let ok_data = serde_json::json!({
            "img_x_len": 10, "img_y_len": 10, "img_x_size": 160, "img_y_size": 90,
            "image": ["//i0.hdslb.com/x.jpg"], "index": [0, 0, 5]
        });
        let with = |patch: serde_json::Value| {
            let mut d = ok_data.clone();
            for (k, v) in patch.as_object().unwrap() {
                d[k] = v.clone();
            }
            serde_json::json!({ "code": 0, "data": d })
        };
        assert!(parse_videoshot(&with(serde_json::json!({}))).is_some(), "基线可解析");
        // code≠0(-404「啥都木有」/ -400「请求错误」)→ None
        assert!(parse_videoshot(&serde_json::json!({"code": -404, "message": "啥都木有", "data": null}))
            .is_none());
        assert!(parse_videoshot(&serde_json::json!({"code": -400, "message": "请求错误", "ttl": 1}))
            .is_none());
        // 没带 index=1 → `index: []`;只剩哨兵 `[0]`;都定不了格
        assert!(parse_videoshot(&with(serde_json::json!({"index": []}))).is_none());
        assert!(parse_videoshot(&with(serde_json::json!({"index": [0]}))).is_none());
        // 没有大图 / 格尺寸为 0 / 声明尺寸离谱(别照声明解码巨图)
        assert!(parse_videoshot(&with(serde_json::json!({"image": []}))).is_none());
        assert!(parse_videoshot(&with(serde_json::json!({"img_x_size": 0}))).is_none());
        assert!(parse_videoshot(&with(serde_json::json!({"img_x_len": 100, "img_x_size": 1000}))).is_none());
        // index 混进非数字 → 形状变了,不猜
        assert!(parse_videoshot(&with(serde_json::json!({"index": [0, 0, "x"]}))).is_none());
    }

    #[test]
    fn extract_page_and_cid_lookup() {
        assert_eq!(extract_page("https://www.bilibili.com/video/BV1ys411472E?p=3"), Some(3));
        assert_eq!(extract_page("https://www.bilibili.com/video/BV1ys411472E?t=10&p=2#x"), Some(2));
        assert_eq!(extract_page("https://www.bilibili.com/video/BV1ys411472E"), None);
        assert_eq!(extract_page("https://www.bilibili.com/video/BV1ys411472E?p=0"), None);
        assert_eq!(extract_page("https://www.bilibili.com/video/BV1ys411472E?p=abc"), None);
        // pagelist 真实形状(BV1ys411472E,16 P)截两项
        let data = serde_json::json!([
            {"cid": 10959711, "page": 1, "from": "vupload", "part": "01", "duration": 300},
            {"cid": 10959712, "page": 2, "from": "vupload", "part": "02", "duration": 600}
        ]);
        assert_eq!(page_cid_from(&data, 2), Some(10959712));
        assert_eq!(page_cid_from(&data, 3), None, "没有第 3 P → None(宁可没图)");
        assert_eq!(page_cid_from(&serde_json::json!(null), 1), None);
    }

    #[test]
    fn pgc_episode_ids_finds_this_episode_only() {
        let result = serde_json::json!({
            "season_id": 44871,
            "episodes": [
                {"id": 742483, "ep_id": 742483, "bvid": "BV1aM4y1B7L9", "cid": 1066829657, "title": "1"},
                {"id": 742484, "ep_id": 742484, "bvid": "BV1bb", "cid": 2, "title": "2"},
                {"id": 742485, "ep_id": 742485, "title": "3"} // 缺 bvid/cid → 这一集没图
            ]
        });
        assert_eq!(pgc_episode_ids(&result, "742484"), Some(("BV1bb".to_string(), 2)));
        assert_eq!(pgc_episode_ids(&result, "742483"), Some(("BV1aM4y1B7L9".to_string(), 1066829657)));
        assert_eq!(pgc_episode_ids(&result, "742485"), None, "缺 bvid/cid 别拿别集的凑");
        assert_eq!(pgc_episode_ids(&result, "1"), None);
        assert_eq!(pgc_episode_ids(&result, "abc"), None);
    }

    /// 真网整链:videoshot → 解析出雪碧图 → 注册进 relay → `/thumb/{token}?t=` 真下载大图、
    /// 真裁一格出 JPEG(与前端契约完全一致的路)。「B 站换了 videoshot 形状 / 雪碧图 CDN 拒了」的绊线。
    /// `cargo test -p larkwing-core --lib real_bili_videoshot -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "打真网(B 站 videoshot + 雪碧图 CDN),开发机手动跑"]
    async fn real_bili_videoshot() {
        let bili = Bilibili::new();
        // Never Gonna Give You Up 官方 MV(213s,单 P):实测 33 项 index ↔ 32 格。
        let sheet = bili
            .videoshot("BV1GJ411x7h7", None, None)
            .await
            .expect("请求不该报错")
            .expect("公开稿件应有雪碧图");
        println!(
            "grid {}x{} tile {}x{} images={} frames={} first={:?} last={:?}",
            sheet.x_len,
            sheet.y_len,
            sheet.tile_w,
            sheet.tile_h,
            sheet.images.len(),
            sheet.index.len(),
            sheet.index.first(),
            sheet.index.last()
        );
        assert!(!sheet.images.is_empty() && sheet.images[0].starts_with("https://"));
        assert!(sheet.index.len() >= 10, "3 分半的片子该有几十帧");
        // 分P 路:同一稿件带 cid 与不带应给同一张图(P1);番剧 ep 形也走通
        let same = bili.videoshot("BV1GJ411x7h7", Some(137649199), None).await.unwrap().unwrap();
        assert_eq!(same.images, sheet.images, "cid=P1 与不带 cid 是同一张图");
        let pgc = bili
            .sprites("https://www.bilibili.com/bangumi/play/ep742483", None)
            .await
            .expect("请求不该报错");
        println!("pgc ep742483 sprites: {:?}", pgc.as_ref().map(|s| (&s.images, s.index.len())));

        // 端到端:relay 真下载大图 + 真裁格
        let relay = super::super::relay::Relay::start().await.unwrap();
        let base = relay.register_sprites(std::sync::Arc::new(sheet.clone()));
        let http = reqwest::Client::new();
        for t in [0.0, 30.0, 9999.0] {
            let r = http.get(format!("{base}?t={t}")).send().await.unwrap();
            assert_eq!(r.status().as_u16(), 200, "t={t} 该有图");
            assert_eq!(r.headers()["content-type"], "image/jpeg");
            let b = r.bytes().await.unwrap();
            assert!(b.starts_with(&[0xFF, 0xD8, 0xFF]), "JPEG 魔数");
            let img = image::load_from_memory(&b).unwrap();
            println!("t={t} → {} 字节 {}x{}", b.len(), img.width(), img.height());
            assert_eq!(
                img.width(),
                sheet.tile_w.min(super::super::relay::THUMB_WIDTH),
                "原格不放大、超宽才缩到 THUMB_WIDTH"
            );
        }
    }

    /// 单测只管纯函数,而接线(端点 / 参数名 / 番剧载荷在 `result` 不在 `data`)才是最容易错的
    /// 一层 —— 这条打真网把 `episodes()` 整条链验穿。「番剧放一集就停」的回归守卫。
    /// `cargo test -p larkwing-core --lib media::bilibili::tests::real_bangumi_season -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "打真网(B 站 PGC 端点),开发机手动跑"]
    async fn real_bangumi_season() {
        let bili = Bilibili::new();
        // 安全警长啦咘啦哆(旧名「拉布拉多警长」)第 1 集,52 集的季。
        let ep_url = "https://www.bilibili.com/bangumi/play/ep742483";
        let (key, eps) = bili
            .episodes(ep_url, None)
            .await
            .expect("请求不该报错")
            .expect("番剧应发现出整季,不该退化成单集");
        println!("key={key} 共 {} 集", eps.len());
        for e in eps.iter().take(3) {
            println!("   {} | {} | {}", e.id, e.title, e.url);
        }
        assert_eq!(key, "bili:pgc:44871");
        assert!(eps.len() >= 50, "这季有 52 集,拿到 {} 集", eps.len());
        // 用户贴进来的那一集必须能在队列里精确匹配上 —— 否则 build_queue 认不出「点的是第几集」。
        assert!(eps.iter().any(|e| e.url == ep_url), "首集 url 应与用户贴的形态逐字一致");
        // ss 形(整季链接)走同一个端点、应得同一个季。
        let (key2, eps2) = bili
            .episodes("https://www.bilibili.com/bangumi/play/ss44871", None)
            .await
            .expect("请求不该报错")
            .expect("ss 形也该发现出整季");
        assert_eq!((key2, eps2.len()), (key, eps.len()), "ep 形与 ss 形应指向同一季");
    }
}
