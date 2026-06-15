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

/// settings key(app 级):和风 key(填了即切和风源)、可选专属 host、记住的城市。
pub const KEY_QWEATHER: &str = "weather.qweather.key";
pub const KEY_QWEATHER_HOST: &str = "weather.qweather.host";
pub const KEY_CITY: &str = "weather.city";

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
#[async_trait]
pub trait WeatherSource: Send + Sync {
    async fn lookup(&self, city: &str, when: When) -> Result<Weather>;
}

/// app 级无归属资产(HTTP 连接池),住工具单例字段、不进 ToolCtx(同 web 的 WebClient)。
pub struct WeatherClient {
    http: reqwest::Client,
}

impl Default for WeatherClient {
    fn default() -> Self {
        Self::new()
    }
}

impl WeatherClient {
    pub fn new() -> WeatherClient {
        let http = reqwest::Client::builder()
            .user_agent(UA)
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(15))
            .build()
            .expect("reqwest client");
        WeatherClient { http }
    }

    /// 选源(按 key 有无)+ 查询。store 编排(读 key/host、定位记忆)在工具层走
    /// spawn_blocking,这里纯网络、无状态依赖 —— 好测、职责清(同 web 的 WebClient 不碰 store)。
    pub async fn report_for(
        &self,
        city: &str,
        key: Option<&str>,
        host: Option<&str>,
        when: When,
    ) -> Result<Weather> {
        match key.map(str::trim).filter(|k| !k.is_empty()) {
            Some(key) => {
                let host = host
                    .map(|h| h.trim().trim_end_matches('/').to_string())
                    .filter(|h| !h.is_empty());
                qweather::QWeatherSource::new(self.http.clone(), key.to_string(), host)
                    .lookup(city, when)
                    .await
            }
            None => open_meteo::OpenMeteoSource::new(self.http.clone()).lookup(city, when).await,
        }
    }

    /// 搜狐 cityjson:免 key、自动识别请求方公网 IP → 城市。
    /// 响应形如 `var returnCitySN = {"cip":"..","cid":"..","cname":"杭州市"};`。
    pub async fn locate_city(&self) -> Result<String> {
        let bytes = self
            .http
            .get("http://pv.sohu.com/cityjson?ie=utf-8")
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        let body = String::from_utf8_lossy(&bytes);
        parse_cityjson(&body).context("定位响应里没有城市名")
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
    fn normalize_strips_admin_suffix() {
        assert_eq!(normalize_city("杭州市"), "杭州");
        assert_eq!(normalize_city("西湖区"), "西湖");
        assert_eq!(normalize_city("北京"), "北京");
        // 先长后短:自治州不被"州"误伤(列表无"州",整体去"自治州")
        assert_eq!(normalize_city("楚雄彝族自治州"), "楚雄彝族");
    }
}
