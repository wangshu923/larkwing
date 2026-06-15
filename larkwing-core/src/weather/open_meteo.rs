//! Open-Meteo 天气源:免 key、开源、全球覆盖。**默认源**(用户没填和风 key 时走它)。
//! 代价 = 服务器境外,国内可达性看网络(真机 watch-item)。
//! 链路:城市名 → geocoding 取经纬度 → forecast 取实况/预报;WMO 天气码 → 中文词。
//! 生活提示 Open-Meteo 不提供 → 从天气码 + 温度推导几条家用提示。

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde_json::Value;

use super::{normalize_city, DayForecast, Weather, WeatherSource, When};

pub struct OpenMeteoSource {
    http: reqwest::Client,
}

impl OpenMeteoSource {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }

    /// 城市名 → (纬度, 经度, 规范名)。先试原名,搜不到去行政尾缀重试(杭州市 → 杭州)。
    async fn geocode(&self, city: &str) -> Result<(f64, f64, String)> {
        let norm = normalize_city(city);
        let candidates: Vec<&str> = if norm == city { vec![city] } else { vec![city, norm] };
        for name in candidates {
            if let Some(hit) = self.geocode_one(name).await? {
                return Ok(hit);
            }
        }
        bail!("找不到城市「{city}」的位置")
    }

    async fn geocode_one(&self, name: &str) -> Result<Option<(f64, f64, String)>> {
        let text = self
            .http
            .get("https://geocoding-api.open-meteo.com/v1/search")
            .query(&[("name", name), ("count", "1"), ("language", "zh"), ("format", "json")])
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let json: Value = serde_json::from_str(&text).context("geocoding 解析失败")?;
        let Some(r) = json.get("results").and_then(Value::as_array).and_then(|a| a.first()) else {
            return Ok(None);
        };
        let lat = r.get("latitude").and_then(Value::as_f64).context("geocoding 缺纬度")?;
        let lon = r.get("longitude").and_then(Value::as_f64).context("geocoding 缺经度")?;
        let resolved = r.get("name").and_then(Value::as_str).unwrap_or(name).to_string();
        Ok(Some((lat, lon, resolved)))
    }
}

#[async_trait]
impl WeatherSource for OpenMeteoSource {
    async fn lookup(&self, city: &str, when: When) -> Result<Weather> {
        let (lat, lon, name) = self.geocode(city).await?;

        let mut q: Vec<(&str, String)> = vec![
            ("latitude", lat.to_string()),
            ("longitude", lon.to_string()),
            (
                "current",
                "temperature_2m,relative_humidity_2m,apparent_temperature,weather_code,wind_speed_10m"
                    .into(),
            ),
            ("timezone", "auto".into()),
        ];
        if when.wants_forecast() {
            q.push(("daily", "weather_code,temperature_2m_max,temperature_2m_min".into()));
            q.push(("forecast_days", "3".into()));
        }

        let text = self
            .http
            .get("https://api.open-meteo.com/v1/forecast")
            .query(&q)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let json: Value = serde_json::from_str(&text).context("天气解析失败")?;

        let cur = json.get("current").context("天气响应缺 current")?;
        let code = cur.get("weather_code").and_then(Value::as_i64).unwrap_or(-1);
        let temp = cur.get("temperature_2m").and_then(Value::as_f64).map(round_i32);
        let feels = cur.get("apparent_temperature").and_then(Value::as_f64).map(round_i32);
        let humidity = cur.get("relative_humidity_2m").and_then(Value::as_f64).map(round_i32);
        let wind = cur
            .get("wind_speed_10m")
            .and_then(Value::as_f64)
            .map(|w| format!("风速 {w:.0} km/h"));

        let days = parse_days(json.get("daily"), when);
        let tips = derive_tips(code, &days, temp);

        Ok(Weather {
            city: name,
            temp_c: temp,
            feels_c: feels,
            text: wmo_text(code).to_string(),
            humidity,
            wind,
            tips,
            days,
            source: "Open-Meteo",
        })
    }
}

/// daily 三个平行数组 → DayForecast 列表(Today 只取首日)。
fn parse_days(daily: Option<&Value>, when: When) -> Vec<DayForecast> {
    let Some(daily) = daily else { return Vec::new() };
    let dates = daily.get("time").and_then(Value::as_array);
    let codes = daily.get("weather_code").and_then(Value::as_array);
    let highs = daily.get("temperature_2m_max").and_then(Value::as_array);
    let lows = daily.get("temperature_2m_min").and_then(Value::as_array);
    let (Some(dates), Some(highs), Some(lows)) = (dates, highs, lows) else {
        return Vec::new();
    };
    let take = if when == When::Today { 1 } else { dates.len() };
    (0..dates.len().min(take))
        .map(|i| {
            let dcode = codes.and_then(|c| c.get(i)).and_then(Value::as_i64).unwrap_or(-1);
            DayForecast {
                date: dates[i].as_str().unwrap_or("").to_string(),
                high_c: highs.get(i).and_then(Value::as_f64).map(round_i32).unwrap_or(0),
                low_c: lows.get(i).and_then(Value::as_f64).map(round_i32).unwrap_or(0),
                text: wmo_text(dcode).to_string(),
            }
        })
        .collect()
}

fn round_i32(f: f64) -> i32 {
    f.round() as i32
}

/// WMO weather code → 中文天气词(表来自 Open-Meteo 文档)。
pub(crate) fn wmo_text(code: i64) -> &'static str {
    match code {
        0 => "晴",
        1 => "晴间多云",
        2 => "多云",
        3 => "阴",
        45 | 48 => "雾",
        51 | 53 | 55 => "毛毛雨",
        56 | 57 => "冻雨",
        61 => "小雨",
        63 => "中雨",
        65 => "大雨",
        66 | 67 => "冻雨",
        71 => "小雪",
        73 => "中雪",
        75 => "大雪",
        77 => "雪粒",
        80 => "阵雨",
        81 => "强阵雨",
        82 => "暴雨",
        85 | 86 => "阵雪",
        95 => "雷阵雨",
        96 | 99 => "雷阵雨伴冰雹",
        _ => "未知",
    }
}

/// Open-Meteo 无官方生活指数 → 从天气码 + 温度推几条家用提示。
/// 有今日预报时用其高/低温(更准),否则退化到实况温度。
fn derive_tips(code: i64, days: &[DayForecast], temp: Option<i32>) -> Vec<String> {
    let mut tips = Vec::new();
    let rainy = matches!(code, 51..=67 | 80..=82 | 95..=99);
    let snowy = matches!(code, 71..=77 | 85 | 86);
    let high = days.first().map(|d| d.high_c).or(temp);
    let low = days.first().map(|d| d.low_c).or(temp);

    if rainy {
        tips.push("有雨,出门记得带伞".to_string());
    }
    if snowy {
        tips.push("有雪,路面湿滑注意保暖防摔".to_string());
    }
    if let Some(h) = high {
        if h >= 35 {
            tips.push("高温,注意防暑、多补水".to_string());
        } else if h >= 30 && matches!(code, 0 | 1) {
            tips.push("天晴日头足,注意防晒".to_string());
        }
    }
    if let Some(l) = low {
        if l <= 0 {
            tips.push("严寒,注意保暖防冻".to_string());
        } else if l <= 8 {
            tips.push("较冷,记得加件衣裳".to_string());
        }
    }
    tips
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wmo_maps_common_codes() {
        assert_eq!(wmo_text(0), "晴");
        assert_eq!(wmo_text(61), "小雨");
        assert_eq!(wmo_text(75), "大雪");
        assert_eq!(wmo_text(95), "雷阵雨");
        assert_eq!(wmo_text(12345), "未知");
    }

    #[test]
    fn tips_from_code_and_temp() {
        assert!(derive_tips(0, &[], Some(36)).iter().any(|t| t.contains("防暑")));
        assert!(derive_tips(63, &[], Some(20)).iter().any(|t| t.contains("带伞")));
        assert!(derive_tips(3, &[], Some(-2)).iter().any(|t| t.contains("保暖")));
        assert!(derive_tips(0, &[], Some(22)).is_empty(), "舒适天无提示");
    }

    #[test]
    fn parse_days_respects_today_window() {
        let daily = serde_json::json!({
            "time": ["2026-06-15", "2026-06-16", "2026-06-17"],
            "weather_code": [0, 61, 3],
            "temperature_2m_max": [31.4, 28.0, 27.6],
            "temperature_2m_min": [21.0, 22.5, 20.7]
        });
        let today = parse_days(Some(&daily), When::Today);
        assert_eq!(today.len(), 1);
        assert_eq!(today[0].high_c, 31);
        assert_eq!(today[0].text, "晴");
        let three = parse_days(Some(&daily), When::ThreeDay);
        assert_eq!(three.len(), 3);
        assert_eq!(three[1].text, "小雨");
        assert!(parse_days(None, When::ThreeDay).is_empty());
    }
}
