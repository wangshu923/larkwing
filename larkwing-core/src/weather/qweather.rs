//! 和风天气源(QWeather):要 key,但国内稳、数据来自中国气象局、自带生活指数。
//! 填了 `weather.qweather.key` 即走它(WeatherClient 按 key 有无选源)。
//! 真机 watch:和风 2024 起按开发者分配**专属 API Host**(xxx.qweatherapi.com);
//! 用户在设置填 `weather.qweather.host` 时 geo/weather 都走它,否则用历史免费版
//! 分离 host(geoapi/devapi)。host 是数据,变了/坏了改 settings 不改代码。

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde_json::Value;

use super::{DayForecast, Weather, WeatherSource, When};

pub struct QWeatherSource {
    http: reqwest::Client,
    key: String,
    host: Option<String>,
}

impl QWeatherSource {
    pub fn new(http: reqwest::Client, key: String, host: Option<String>) -> Self {
        Self { http, key, host }
    }

    /// (geo_host, api_host):专属 host 统一服务两者;否则历史免费版分离 host。
    fn hosts(&self) -> (String, String) {
        match &self.host {
            Some(h) => (h.clone(), h.clone()),
            None => (
                "https://geoapi.qweather.com".to_string(),
                "https://devapi.qweather.com".to_string(),
            ),
        }
    }

    /// 城市名 → (和风 LocationID, 规范城市名)。
    async fn location_id(&self, geo_host: &str, city: &str) -> Result<(String, String)> {
        let text = self
            .http
            .get(format!("{geo_host}/v2/city/lookup"))
            .query(&[("location", city), ("key", self.key.as_str()), ("lang", "zh")])
            .send()
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
    async fn indices(&self, api_host: &str, id: &str) -> Result<Vec<String>> {
        let text = self
            .http
            .get(format!("{api_host}/v7/indices/1d"))
            .query(&[("type", "3,5,8"), ("location", id), ("key", self.key.as_str())])
            .send()
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
    async fn lookup(&self, city: &str, when: When) -> Result<Weather> {
        let (geo_host, api_host) = self.hosts();
        let (id, name) = self.location_id(&geo_host, city).await?;

        // 实况
        let now_text = self
            .http
            .get(format!("{api_host}/v7/weather/now"))
            .query(&[("location", id.as_str()), ("key", self.key.as_str())])
            .send()
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
            let fc_text = self
                .http
                .get(format!("{api_host}/v7/weather/3d"))
                .query(&[("location", id.as_str()), ("key", self.key.as_str())])
                .send()
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

        let tips = self.indices(&api_host, &id).await.unwrap_or_default();

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

/// 和风 code:"200" 成功,其余是错误码(401 鉴权、402 超额、403 无权限…)。
fn check_code(json: &Value, what: &str) -> Result<()> {
    match json.get("code").and_then(Value::as_str) {
        Some("200") => Ok(()),
        Some(other) => bail!("和风{what}返回错误码 {other}(检查 key/host/额度)"),
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
