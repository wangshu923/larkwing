//! 和风天气源(QWeather)—— **JWT(Ed25519)认证**。和风 2024 起按开发者分配专属 API Host
//! (`xxx.qweatherapi.com`),老的公共域名(`devapi`/`geoapi`/`api.qweather.com`)2026 上半年
//! 已陆续停服,故无"免 host 兜底"可言:用和风必须有专属 host。
//!
//! 认证:全局私钥([`crate::crypto`])签一个 JWT —— header `kid` = 凭据 ID、payload `sub` = 项目 ID,
//! 随请求走 `Authorization: Bearer`。host / 项目 ID / 凭据 ID 都是**数据**(settings),变了/换了
//! 改设置不改代码;私钥是全局的、所有 Ed25519-JWT 服务共用。
//!
//! 路径要点:专属 host 上 **GeoAPI 是 `/geo/v2/city/lookup`**(比 `/v7` 业务接口多一层 `/geo` 前缀,
//! 这是从老 `geoapi.qweather.com` 迁过来的人最容易踩的坑);实况/预报/指数仍是 `/v7/...`。

use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde_json::Value;

use super::{DayForecast, QWeatherCfg, Weather, WeatherSource, When};

/// JWT 有效期:和风上限 24h;我们每次 lookup 现签短时(15min),够一组请求(城市+实况+预报+指数)用。
const JWT_TTL: Duration = Duration::from_secs(15 * 60);

pub struct QWeatherSource {
    cfg: QWeatherCfg,
}

impl QWeatherSource {
    pub fn new(cfg: QWeatherCfg) -> Self {
        Self { cfg }
    }

    /// 用全局私钥签一个和风格式 JWT(kid=凭据ID,sub=项目ID)。
    fn sign(&self) -> Result<String> {
        crate::crypto::sign_jwt(
            &self.cfg.private_pem,
            &self.cfg.credential_id,
            &self.cfg.project_id,
            JWT_TTL,
        )
        .context("和风 JWT 签发失败(检查全局密钥/凭据 ID/项目 ID)")
    }

    /// 城市名 → (和风 LocationID, 规范城市名)。GeoAPI 在专属 host 上走 `/geo/v2/city/lookup`。
    async fn location_id(
        &self,
        net: &crate::net::Client,
        jwt: &str,
        city: &str,
    ) -> Result<(String, String)> {
        let url = geo_url(&self.cfg.host);
        let text = net
            .send(&url, |c| {
                c.get(&url).query(&[("location", city), ("lang", "zh")]).bearer_auth(jwt)
            })
            .await?
            .error_for_status()?
            .text()
            .await?;
        let json: Value = serde_json::from_str(&text).context("和风 GeoAPI 解析失败")?;
        check_code(&json, "GeoAPI")?;
        let loc = json
            .get("location")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .context("和风 GeoAPI 没返回城市")?;
        let id = loc.get("id").and_then(Value::as_str).context("缺 LocationID")?.to_string();
        let name = loc.get("name").and_then(Value::as_str).unwrap_or(city).to_string();
        Ok((id, name))
    }

    /// 生活指数:穿衣(3)/紫外线(5)/舒适度(8) —— 和风独有,best-effort(失败不拖垮主结果)。
    async fn indices(
        &self,
        net: &crate::net::Client,
        jwt: &str,
        id: &str,
    ) -> Result<Vec<String>> {
        let url = v7_url(&self.cfg.host, "indices/1d");
        let text = net
            .send(&url, |c| {
                c.get(&url).query(&[("type", "3,5,8"), ("location", id)]).bearer_auth(jwt)
            })
            .await?
            .error_for_status()?
            .text()
            .await?;
        let json: Value = serde_json::from_str(&text)?;
        check_code(&json, "生活指数")?;
        Ok(json
            .get("daily")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|d| {
                        let name = d.get("name").and_then(Value::as_str)?;
                        let cat = d.get("category").and_then(Value::as_str)?;
                        Some(format!("{name}:{cat}"))
                    })
                    .collect()
            })
            .unwrap_or_default())
    }
}

#[async_trait]
impl WeatherSource for QWeatherSource {
    async fn lookup(&self, net: &crate::net::Client, city: &str, when: When) -> Result<Weather> {
        // 一次 lookup 签一个 JWT,城市/实况/预报/指数四个请求共用。
        let jwt = self.sign()?;
        let host = &self.cfg.host;
        let (id, name) = self.location_id(net, &jwt, city).await?;

        // 实况
        let now_url = v7_url(host, "weather/now");
        let now_text = net
            .send(&now_url, |c| c.get(&now_url).query(&[("location", id.as_str())]).bearer_auth(&jwt))
            .await?
            .error_for_status()?
            .text()
            .await?;
        let now_json: Value = serde_json::from_str(&now_text).context("和风实况解析失败")?;
        check_code(&now_json, "实况")?;
        let now = now_json.get("now").context("和风实况缺 now")?;
        let text = now.get("text").and_then(Value::as_str).unwrap_or("").to_string();
        let temp = str_i32(now.get("temp"));
        let feels = str_i32(now.get("feelsLike"));
        let humidity = str_i32(now.get("humidity"));
        let wind = match (
            now.get("windDir").and_then(Value::as_str),
            now.get("windScale").and_then(Value::as_str),
        ) {
            (Some(dir), Some(scale)) if !dir.is_empty() => Some(format!("{dir} {scale}级")),
            _ => None,
        };

        // 预报(仅 Today/ThreeDay 拉)
        let days = if when.wants_forecast() {
            let fc_url = v7_url(host, "weather/3d");
            let fc_text = net
                .send(&fc_url, |c| {
                    c.get(&fc_url).query(&[("location", id.as_str())]).bearer_auth(&jwt)
                })
                .await?
                .error_for_status()?
                .text()
                .await?;
            let fc: Value = serde_json::from_str(&fc_text).context("和风预报解析失败")?;
            check_code(&fc, "预报")?;
            parse_days(&fc, when)
        } else {
            Vec::new()
        };

        let tips = self.indices(net, &jwt, &id).await.unwrap_or_default();

        Ok(Weather {
            city: name,
            temp_c: temp,
            feels_c: feels,
            text,
            humidity,
            wind,
            tips,
            days,
            source: "和风天气",
        })
    }
}

/// GeoAPI 城市搜索 URL:专属 host 上是 `/geo/v2/city/lookup`(多一层 `/geo`)。
fn geo_url(host: &str) -> String {
    format!("{host}/geo/v2/city/lookup")
}

/// `/v7` 业务接口 URL(实况 `weather/now`、预报 `weather/3d`、指数 `indices/1d`)。
fn v7_url(host: &str, leaf: &str) -> String {
    format!("{host}/v7/{leaf}")
}

/// 和风 code:"200" 成功,其余是错误码(401 鉴权、402 超额、403 无权限…)。
fn check_code(json: &Value, what: &str) -> Result<()> {
    match json.get("code").and_then(Value::as_str) {
        Some("200") => Ok(()),
        Some(other) => bail!("和风{what}返回错误码 {other}(检查密钥/项目/凭据/额度)"),
        None => bail!("和风{what}响应无 code 字段"),
    }
}

/// 和风温度/湿度等是字符串数字("26") → i32。
fn str_i32(v: Option<&Value>) -> Option<i32> {
    v.and_then(Value::as_str)?.trim().parse().ok()
}

fn parse_days(fc: &Value, when: When) -> Vec<DayForecast> {
    let Some(arr) = fc.get("daily").and_then(Value::as_array) else {
        return Vec::new();
    };
    let take = if when == When::Today { 1 } else { arr.len() };
    arr.iter()
        .take(take)
        .map(|d| DayForecast {
            date: d.get("fxDate").and_then(Value::as_str).unwrap_or("").to_string(),
            high_c: str_i32(d.get("tempMax")).unwrap_or(0),
            low_c: str_i32(d.get("tempMin")).unwrap_or(0),
            text: d.get("textDay").and_then(Value::as_str).unwrap_or("").to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geo_url_has_geo_prefix() {
        // 专属 host 上 GeoAPI 必须是 /geo/v2/...,漏了 /geo 会 404(从老 geoapi 域名迁移的经典坑)。
        assert_eq!(
            geo_url("https://abc.qweatherapi.com"),
            "https://abc.qweatherapi.com/geo/v2/city/lookup"
        );
    }

    #[test]
    fn v7_url_builds_business_endpoints() {
        let h = "https://abc.qweatherapi.com";
        assert_eq!(v7_url(h, "weather/now"), "https://abc.qweatherapi.com/v7/weather/now");
        assert_eq!(v7_url(h, "weather/3d"), "https://abc.qweatherapi.com/v7/weather/3d");
        assert_eq!(v7_url(h, "indices/1d"), "https://abc.qweatherapi.com/v7/indices/1d");
    }

    #[test]
    fn check_code_distinguishes_success() {
        assert!(check_code(&serde_json::json!({"code": "200"}), "x").is_ok());
        assert!(check_code(&serde_json::json!({"code": "401"}), "x").is_err());
        assert!(check_code(&serde_json::json!({}), "x").is_err());
    }

    #[test]
    fn str_i32_parses_qweather_strings() {
        assert_eq!(str_i32(Some(&serde_json::json!("26"))), Some(26));
        assert_eq!(str_i32(Some(&serde_json::json!("-3"))), Some(-3));
        assert_eq!(str_i32(Some(&serde_json::json!(""))), None);
        assert_eq!(str_i32(Some(&serde_json::json!(26))), None, "和风是字符串,数字型不认");
    }

    #[test]
    fn parse_days_maps_qweather_forecast() {
        let fc = serde_json::json!({
            "code": "200",
            "daily": [
                {"fxDate": "2026-06-15", "tempMax": "31", "tempMin": "21", "textDay": "晴"},
                {"fxDate": "2026-06-16", "tempMax": "28", "tempMin": "22", "textDay": "小雨"}
            ]
        });
        let today = parse_days(&fc, When::Today);
        assert_eq!(today.len(), 1);
        assert_eq!(today[0].high_c, 31);
        assert_eq!(today[0].text, "晴");
        let three = parse_days(&fc, When::ThreeDay);
        assert_eq!(three.len(), 2);
        assert_eq!(three[1].low_c, 22);
    }
}
