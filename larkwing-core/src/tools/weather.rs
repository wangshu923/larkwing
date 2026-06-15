//! 能力轴:天气(读)。模型没有"看天"的感官 —— 查实况/今天/未来三天。
//! 城市免问:模型显式传 city → 用那座城;否则读 settings 记住的城市;再没有 →
//! 搜狐 cityjson 自动定位(并存 settings 记住,解决"老问在哪个城市")。
//! 源 = 数据:填了和风 key 走和风(国内稳、生活指数),没填走 Open-Meteo(免 key)。
//! store 访问(读 key/host/城市、回写定位)在此层走 spawn_blocking(Repo 全同步纪律);
//! WeatherClient 纯网络。真机 watch:两源国内可达性、cityjson 编码、和风专属 host。

use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;

use crate::weather::{Weather, WeatherClient, When, KEY_CITY, KEY_QWEATHER, KEY_QWEATHER_HOST};

use super::{Tool, ToolCtx, ToolSpec};

pub(super) struct WeatherTool {
    spec: ToolSpec,
    weather: Arc<WeatherClient>,
}

impl WeatherTool {
    pub(super) fn new(weather: Arc<WeatherClient>) -> WeatherTool {
        WeatherTool {
            spec: ToolSpec {
                name: "weather",
                description: "查天气。用户问冷不冷热不热、要不要带伞、穿什么、今天/明后天天气\
                              这类时使用。默认查所在城市 —— 系统会自动定位并记住,**别反问在\
                              哪个城市**;用户明确说要查别的城市时,才把城市名填进 city。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "city": {
                            "type": "string",
                            "description": "城市名(中文,如「杭州」);不填 = 所在城市(自动定位/已记住的)"
                        },
                        "when": {
                            "type": "string",
                            "enum": ["now", "today", "3d"],
                            "description": "now=此刻实况(默认);today=今天;3d=未来三天预报"
                        }
                    }
                }),
                timeout: std::time::Duration::from_secs(30),
                ui_key: "tool.weather",
            },
            weather,
        }
    }
}

#[async_trait]
impl Tool for WeatherTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let when = When::parse(args.get("when").and_then(serde_json::Value::as_str));
        let city_arg =
            args.get("city").and_then(serde_json::Value::as_str).map(str::trim).filter(|s| !s.is_empty());

        let city = resolve_city(ctx, &self.weather, city_arg).await?;

        // 选源用的 key/host(同步 Repo → spawn_blocking)
        let store = ctx.store.clone();
        let (key, host) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            Ok((store.settings.get(None, KEY_QWEATHER)?, store.settings.get(None, KEY_QWEATHER_HOST)?))
        })
        .await
        .context("读天气设置任务挂了")??;

        let report = self.weather.report_for(&city, key.as_deref(), host.as_deref(), when).await?;
        Ok(render(&report, when))
    }
}

/// 城市解析链(weather + watch 工具共用):显式 city → settings 记住的 → cityjson 自动定位
/// (并存 settings 记住,解决"老问在哪个城市")。定位机器集中在工具层,scheduler 只用结果。
pub(super) async fn resolve_city(
    ctx: &ToolCtx,
    weather: &WeatherClient,
    city_arg: Option<&str>,
) -> anyhow::Result<String> {
    if let Some(c) = city_arg.map(str::trim).filter(|s| !s.is_empty()) {
        return Ok(c.to_string());
    }
    let store = ctx.store.clone();
    let saved = tokio::task::spawn_blocking(move || store.settings.get(None, KEY_CITY))
        .await
        .context("读城市设置任务挂了")??;
    if let Some(c) = saved.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        return Ok(c);
    }
    let located = weather.locate_city().await?;
    let store = ctx.store.clone();
    let to_save = located.clone();
    tokio::task::spawn_blocking(move || store.settings.set(None, KEY_CITY, &to_save))
        .await
        .context("记住城市任务挂了")??;
    Ok(located)
}

/// Weather → 给模型的观察文本(不是 UI 文案;用模型当前人格的语言组织最顺,同 now/web)。
fn render(w: &Weather, when: When) -> String {
    let mut out = String::new();

    // 实况行
    let mut now_line = format!("{} 当前{}", w.city, w.text);
    if let Some(t) = w.temp_c {
        now_line.push_str(&format!(" {t}℃"));
        if let Some(f) = w.feels_c {
            if f != t {
                now_line.push_str(&format!("(体感 {f}℃)"));
            }
        }
    }
    if let Some(h) = w.humidity {
        now_line.push_str(&format!(",湿度 {h}%"));
    }
    if let Some(wind) = &w.wind {
        now_line.push_str(&format!(",{wind}"));
    }
    out.push_str(&now_line);

    // 预报
    if when.wants_forecast() && !w.days.is_empty() {
        out.push_str("\n预报:");
        for d in &w.days {
            out.push_str(&format!("\n{} {}~{}℃ {}", d.date, d.low_c, d.high_c, d.text));
        }
    }

    // 生活提示
    if !w.tips.is_empty() {
        out.push_str(&format!("\n生活提示:{}", w.tips.join(";")));
    }

    out.push_str(&format!("\n(来源:{})", w.source));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::weather::DayForecast;

    fn sample() -> Weather {
        Weather {
            city: "杭州".into(),
            temp_c: Some(26),
            feels_c: Some(28),
            text: "多云".into(),
            humidity: Some(70),
            wind: Some("东南风 3级".into()),
            tips: vec!["穿衣:舒适".into(), "紫外线:中等".into()],
            days: vec![
                DayForecast { date: "2026-06-15".into(), high_c: 31, low_c: 21, text: "晴".into() },
                DayForecast {
                    date: "2026-06-16".into(),
                    high_c: 28,
                    low_c: 22,
                    text: "小雨".into(),
                },
            ],
            source: "和风天气",
        }
    }

    #[test]
    fn render_now_shows_current_no_forecast() {
        let out = render(&sample(), When::Now);
        assert!(out.contains("杭州 当前多云 26℃(体感 28℃)"));
        assert!(out.contains("湿度 70%"));
        assert!(out.contains("东南风 3级"));
        assert!(out.contains("生活提示:穿衣:舒适;紫外线:中等"));
        assert!(out.contains("(来源:和风天气)"));
        assert!(!out.contains("预报:"), "now 不出预报");
    }

    #[test]
    fn render_3d_shows_forecast_lines() {
        let out = render(&sample(), When::ThreeDay);
        assert!(out.contains("预报:"));
        assert!(out.contains("2026-06-15 21~31℃ 晴"));
        assert!(out.contains("2026-06-16 22~28℃ 小雨"));
    }

    #[test]
    fn render_tolerates_missing_fields() {
        let bare = Weather {
            city: "北京".into(),
            temp_c: None,
            feels_c: None,
            text: "晴".into(),
            humidity: None,
            wind: None,
            tips: vec![],
            days: vec![],
            source: "Open-Meteo",
        };
        let out = render(&bare, When::Now);
        assert!(out.contains("北京 当前晴"));
        assert!(out.contains("(来源:Open-Meteo)"));
        assert!(!out.contains("生活提示"));
    }
}
