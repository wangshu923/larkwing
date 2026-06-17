//! 能力轴:天气(读)。模型没有"看天"的感官,这是它感知现实世界的原语。
//! **源 = 数据/可切换**(宪法多供应商同构):填了和风 key → 走和风(国内稳、气象局数据、
//! 生活指数);没填 → 走 Open-Meteo(免 key、境外)。**key 有无即选源信号,用户面零开关**
//! (每次按 settings 现读 → 填了 key 立即生效,不用重启)。
//! 定位免 key:搜狐 cityjson 自动识别请求方公网 IP → 城市,定一次存 settings 记住
//! (解决"老问在哪个城市");模型显式传 city 则临时查那座城,不覆盖记忆。
//! 真机 watch:① 两源国内可达性(Open-Meteo 境外、cityjson 编码);② 和风专属 API Host。

mod open_meteo;
mod qweather;

use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Serialize;

/// 像真浏览器的 UA(同 web 工具,裸 reqwest 易被某些端点拒)。
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// settings key(app 级):和风 JWT 接入三件套(专属 host + 项目 ID + 凭据 ID)、记住的城市。
/// 私钥不在此 —— 它是**全局**的(见 [`crate::crypto`]),所有走 Ed25519-JWT 的服务共用一把。
pub const KEY_QWEATHER_HOST: &str = "weather.qweather.host";
pub const KEY_QWEATHER_PROJECT: &str = "weather.qweather.project_id";
pub const KEY_QWEATHER_CREDENTIAL: &str = "weather.qweather.credential_id";
pub const KEY_CITY: &str = "weather.city";

/// 和风 JWT 接入配置:host(专属 API Host)+ project_id(JWT 的 `sub`)+ credential_id
/// (JWT 的 `kid`)+ private_pem(全局私钥)。四者齐备才成立 —— 缺任一回落 Open-Meteo。
#[derive(Debug, Clone)]
pub struct QWeatherCfg {
    pub host: String,
    pub project_id: String,
    pub credential_id: String,
    pub private_pem: String,
}

/// 从 settings 读和风 JWT 配置(weather 工具 / scheduler 条件提醒两处共用):host/项目ID/凭据ID
/// 任一为空,或全局私钥还没生成 → None(选源回落免 key 的 Open-Meteo)。同步 Repo,在 blocking 上下文调。
pub fn qweather_cfg(settings: &crate::store::SettingsRepo) -> anyhow::Result<Option<QWeatherCfg>> {
    let read = |k: &str| -> anyhow::Result<String> {
        Ok(settings.get(None, k)?.map(|s| s.trim().to_string()).unwrap_or_default())
    };
    let host = read(KEY_QWEATHER_HOST)?;
    let project_id = read(KEY_QWEATHER_PROJECT)?;
    let credential_id = read(KEY_QWEATHER_CREDENTIAL)?;
    // 私钥是秘密:走 keyring(回落 settings),不从 SQLite 明文读(§6.3)
    let private_pem = crate::secrets::get(settings, crate::crypto::KEY_ED25519_PRIVATE)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if host.is_empty() || project_id.is_empty() || credential_id.is_empty() || private_pem.is_empty() {
        return Ok(None);
    }
    Ok(Some(QWeatherCfg {
        host: host.trim_end_matches('/').to_string(),
        project_id,
        credential_id,
        private_pem,
    }))
}

/// 查哪一段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    Now,
    Today,
    ThreeDay,
}

impl When {
    pub fn parse(s: Option<&str>) -> When {
        match s.map(str::trim) {
            Some("3d") | Some("forecast") | Some("3天") | Some("三天") => When::ThreeDay,
            Some("today") | Some("今天") => When::Today,
            _ => When::Now,
        }
    }

    /// 要不要预报(源据此决定拉不拉 daily)。
    pub fn wants_forecast(self) -> bool {
        !matches!(self, When::Now)
    }
}

/// 一天预报(精简)。
#[derive(Debug, Clone, Serialize)]
pub struct DayForecast {
    pub date: String,
    pub high_c: i32,
    pub low_c: i32,
    pub text: String,
}

/// 天气报告(实况 + 可选预报 + 生活提示)。各源填能填的,缺的留空/空集。
#[derive(Debug, Clone, Serialize)]
pub struct Weather {
    pub city: String,
    pub temp_c: Option<i32>,
    pub feels_c: Option<i32>,
    pub text: String,
    pub humidity: Option<i32>,
    pub wind: Option<String>,
    pub tips: Vec<String>,
    pub days: Vec<DayForecast>,
    pub source: &'static str,
}

/// 天气数据源(trait 接缝,源=数据)。按城市名查;城市→坐标/LocationID 各源内部解决。
/// net 由调用方传入(直连优先、失败兜底走代理):Open-Meteo 境外靠它过墙,和风/cityjson 墙内直连。
#[async_trait]
pub trait WeatherSource: Send + Sync {
    async fn lookup(&self, net: &crate::net::Client, city: &str, when: When) -> Result<Weather>;
}

/// app 级无归属资产(HTTP 连接池),住工具单例字段、不进 ToolCtx(同 web 的 WebClient)。
pub struct WeatherClient {
    net: crate::net::Client,
}

impl Default for WeatherClient {
    fn default() -> Self {
        Self::new()
    }
}

impl WeatherClient {
    pub fn new() -> WeatherClient {
        let net = crate::net::Client::new(|b| {
            // 和风 API 一律 gzip 压缩(文档示例都带 --compressed):不开自动解压,.text() 拿到的是压缩
            // 字节,serde 当场炸「expected value at line 1 column 1」。开了 reqwest 据 Content-Encoding 透明解压。
            b.user_agent(UA)
                .gzip(true)
                .connect_timeout(Duration::from_secs(8))
                .timeout(Duration::from_secs(15))
        });
        WeatherClient { net }
    }

    /// 选源 + 查询:给了和风 JWT 配置 → 走和风(国内稳、生活指数);否则 Open-Meteo(免 key)。
    /// store 编排(读配置、定位记忆)在工具层走 spawn_blocking,这里纯网络、无状态依赖 ——
    /// 好测、职责清(同 web 的 WebClient 不碰 store)。
    pub async fn report_for(
        &self,
        city: &str,
        qw: Option<QWeatherCfg>,
        when: When,
    ) -> Result<Weather> {
        match qw {
            Some(cfg) => qweather::QWeatherSource::new(cfg).lookup(&self.net, city, when).await,
            None => open_meteo::OpenMeteoSource::new().lookup(&self.net, city, when).await,
        }
    }

    /// 公网 IP → 城市(免 key)。**多源兜底**:依次试,首个拿到城市即用 —— 一个挂了/无果就换下一个
    /// (单源挂掉就"老问在哪个城市")。不挑国家:出口在哪报哪城(海外出口报海外城也对,用户可显式
    /// 说城市覆盖)。顺序 = 国内优先(搜狐,中文、快)→ ip-api(干净 JSON,境内外都通)→ ipip(国内兜底)。
    pub async fn locate_city(&self) -> Result<String> {
        type Parser = fn(&str) -> Option<String>;
        const PROBES: &[(&str, Parser)] = &[
            ("http://pv.sohu.com/cityjson?ie=utf-8", parse_cityjson),
            ("http://ip-api.com/json/?lang=zh-CN&fields=status,city", parse_ipapi),
            ("http://myip.ipip.net/", parse_ipip),
        ];
        let mut last_err: Option<anyhow::Error> = None;
        for (url, parse) in PROBES {
            match self.fetch_city(url, *parse).await {
                Ok(city) => return Ok(city),
                Err(e) => {
                    tracing::debug!(url, error = %e, "定位源没拿到城市,换下一个");
                    last_err = Some(e);
                }
            }
        }
        Err(last_err
            .unwrap_or_else(|| anyhow::anyhow!("没有可用的定位源"))
            .context("所有定位源都没拿到城市"))
    }

    /// 取一个定位源:GET → 文本 → 该源解析器抽城市。任一环节失败 = 这个源没拿到(换下一个)。
    async fn fetch_city(&self, url: &str, parse: fn(&str) -> Option<String>) -> Result<String> {
        let bytes = self.net.send(url, |c| c.get(url)).await?.error_for_status()?.bytes().await?;
        let body = String::from_utf8_lossy(&bytes);
        parse(&body).context("定位响应里没有城市名")
    }
}

/// 从 `var returnCitySN = {...};` 抽出城市名(cname)。
fn parse_cityjson(body: &str) -> Option<String> {
    let start = body.find('{')?;
    let end = body.rfind('}')? + 1;
    let json: serde_json::Value = serde_json::from_str(body.get(start..end)?).ok()?;
    let cname = json.get("cname")?.as_str()?.trim();
    if cname.is_empty() || cname == "未知" {
        return None;
    }
    Some(cname.to_string())
}

/// ip-api.com:`{"status":"success","city":"杭州"}`。status 非 success / city 空 → None。
fn parse_ipapi(body: &str) -> Option<String> {
    let json: serde_json::Value = serde_json::from_str(body).ok()?;
    if json.get("status").and_then(serde_json::Value::as_str) != Some("success") {
        return None;
    }
    let city = json.get("city")?.as_str()?.trim();
    (!city.is_empty()).then(|| city.to_string())
}

/// myip.ipip.net:纯文本 `当前 IP:1.2.3.4  来自于:中国 浙江 杭州 电信`。取"来自于"之后那串,
/// 去掉末尾运营商,其后末个地名作城市(直辖市/海外同样适用)。拿不到 → None。
fn parse_ipip(body: &str) -> Option<String> {
    let geo = body
        .split("来自于")
        .nth(1)?
        .trim_start_matches(|c: char| c == ':' || c == '：' || c == ' ');
    let mut parts: Vec<&str> = geo.split_whitespace().collect();
    parts.pop()?; // 末尾是运营商(电信/联通/starhub.com…)
    let city = parts.pop()?.trim(); // 其后末个地名 = 城市
    (!city.is_empty() && city != "未知").then(|| city.to_string())
}

/// 城市名归一:去行政尾缀(市/区/县/自治州…)给 geocoding 用 —— Open-Meteo 对
/// "杭州市"可能搜不到,"杭州"才中。和风 GeoAPI 原生中文不需要,去了也无害。
pub(crate) fn normalize_city(city: &str) -> &str {
    for suffix in ["特别行政区", "自治州", "自治县", "地区", "市", "区", "县", "盟"] {
        if let Some(stripped) = city.strip_suffix(suffix) {
            if !stripped.is_empty() {
                return stripped;
            }
        }
    }
    city
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn when_parses_aliases() {
        assert_eq!(When::parse(Some("3d")), When::ThreeDay);
        assert_eq!(When::parse(Some("今天")), When::Today);
        assert_eq!(When::parse(Some("now")), When::Now);
        assert_eq!(When::parse(None), When::Now);
        assert!(When::ThreeDay.wants_forecast());
        assert!(!When::Now.wants_forecast());
    }

    #[test]
    fn cityjson_extracts_cname_and_rejects_unknown() {
        let ok = r#"var returnCitySN = {"cip": "1.2.3.4", "cid": "101210101", "cname": "杭州市"};"#;
        assert_eq!(parse_cityjson(ok).as_deref(), Some("杭州市"));
        let unknown = r#"var returnCitySN = {"cip": "127.0.0.1", "cid": "00", "cname": "未知"};"#;
        assert_eq!(parse_cityjson(unknown), None);
        assert_eq!(parse_cityjson("garbage"), None);
    }

    #[test]
    fn ipapi_extracts_city_and_rejects_failure() {
        assert_eq!(parse_ipapi(r#"{"status":"success","city":"杭州"}"#).as_deref(), Some("杭州"));
        assert_eq!(parse_ipapi(r#"{"status":"fail","message":"private range"}"#), None);
        assert_eq!(parse_ipapi(r#"{"status":"success","city":""}"#), None, "city 空算没拿到");
        assert_eq!(parse_ipapi("not json"), None);
    }

    #[test]
    fn ipip_parses_text_and_drops_isp() {
        // 国 省 市 运营商
        assert_eq!(
            parse_ipip("当前 IP：1.2.3.4  来自于：中国 浙江 杭州 电信").as_deref(),
            Some("杭州")
        );
        // 直辖市:国 市 市 运营商
        assert_eq!(
            parse_ipip("当前 IP：1.2.3.4  来自于：中国 北京 北京 联通").as_deref(),
            Some("北京")
        );
        // 海外:国 城市 运营商(实测本机出口)
        assert_eq!(
            parse_ipip("当前 IP：203.116.182.37  来自于：新加坡 新加坡   starhub.com").as_deref(),
            Some("新加坡")
        );
        assert_eq!(parse_ipip("乱码没有来自于"), None);
    }

    #[test]
    fn normalize_strips_admin_suffix() {
        assert_eq!(normalize_city("杭州市"), "杭州");
        assert_eq!(normalize_city("西湖区"), "西湖");
        assert_eq!(normalize_city("北京"), "北京");
        // 先长后短:自治州不被"州"误伤(列表无"州",整体去"自治州")
        assert_eq!(normalize_city("楚雄彝族自治州"), "楚雄彝族");
    }
}
