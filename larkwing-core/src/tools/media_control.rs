//! 能力轴:影音(控)。给"用户用嘴说暂停"用 —— 播放卡片上的按钮直连前端 VM,
//! 不经过模型,也不经过这里。

use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolSpec};

pub(super) struct MediaControl {
    spec: ToolSpec,
}

impl MediaControl {
    pub(super) fn new() -> MediaControl {
        MediaControl {
            spec: ToolSpec {
                name: "media_control",
                description: "控制正在播放的内容:pause 暂停 / resume 继续 / stop 停止 / \
                              louder 大点声 / softer 小点声(各约一档)/ volume 音量调到指定值\
                              (value=0–100,「调到一半」=50、「静音」=0)/ speed 倍速(value=0.25–3,\
                              如 1.5)/ seek 定位播放(value=秒,「跳到一分半」=90、\
                              「快进到十分钟」=600)/ next 下一集(下一首)/ prev 上一集(上一首)/ \
                              episode 跳到第几集(value=集数或第几首,「看第五集」=5;多集/多首列表可用)/ \
                              loop_one 单曲循环(「就循环这一首」)/ loop_all 列表循环(「循环放/一直放」,\
                              整个列表放完从头再来)/ loop_off 取消循环 / shuffle_on 随机播放\
                              (「随便放/打乱放」)/ shuffle_off 恢复顺序播放 / audio_track 切音轨\
                              (value=第几条,从 1 数;〔此刻〕背景列着可选音轨和语言,用户说\
                              「换英文原声/换国语」就挑对应语言那条的轨号;单音轨内容没得切,会如实说)。\
                              当前音量/播放进度/倍速/第几集/循环随机/音轨在〔此刻〕背景注记里,\
                              相对要求(「再大一点点」「快进五分钟」)按它算出绝对值后用 volume/seek。\
                              用户说「暂停/接着放/别放了/大点声/音量调到 30/1.5 倍速/跳到第 90 秒/\
                              下一首/看第五集/单曲循环/随机放/换英文原声」时用。没有在放东西就别调。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["pause", "resume", "stop", "louder", "softer", "volume", "speed", "seek", "next", "prev", "episode",
                                     "loop_one", "loop_all", "loop_off", "shuffle_on", "shuffle_off", "audio_track"]
                        },
                        "value": {
                            "type": "number",
                            "description": "volume=音量(0–100);speed=倍速(0.25–3);seek=定位到第几秒;episode=第几集(从 1 数);其它动作不传"
                        }
                    },
                    "required": ["action"]
                }),
                timeout: std::time::Duration::from_secs(5),
                ui_key: "tool.media_control",
            },
        }
    }
}

#[async_trait]
impl Tool for MediaControl {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let action = args
            .get("action")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("缺少 action 参数"))?;
        let value = args.get("value").and_then(serde_json::Value::as_f64);
        // next/prev/episode = 队列切集(现取现播,异步):走 core 队列,不是发给前端的嘴控指令。
        // 队列在 core 手里(B 站合集/分P、本地同一套)→ 「第几集」直接定位,模型不碰链接。
        // audio_track = 切音轨(mac 直传就地启停,其余重建管线原位续播),观察文本由 core 组。
        match action {
            "audio_track" => {
                let v = value
                    .ok_or_else(|| anyhow::anyhow!("audio_track 需要 value(第几条音轨,从 1 数)"))?;
                anyhow::ensure!(v.fract() == 0.0 && v >= 1.0, "音轨号要是从 1 起的整数,收到 {v}");
                ctx.media.set_audio_track(v as usize).await
            }
            "next" | "prev" | "episode" => {
                let outcome = if action == "episode" {
                    let v = value.ok_or_else(|| anyhow::anyhow!("episode 需要 value(第几集)"))?;
                    anyhow::ensure!(
                        v.fract() == 0.0 && v >= 1.0,
                        "第几集要是从 1 起的整数,收到 {v}"
                    );
                    ctx.media.jump_to_episode(ctx.user_id, v as usize).await?
                } else {
                    let delta = if action == "next" { 1 } else { -1 };
                    ctx.media.advance(ctx.user_id, delta).await?
                };
                match outcome {
                    crate::media::PlayOutcome::Playing(np) => Ok({
                        let unit = if matches!(np.kind, crate::media::MediaKind::Audio) {
                            "首"
                        } else {
                            "集"
                        };
                        match &np.playlist {
                            Some(p) => format!(
                                "已切到第{}{unit}《{}》(共{}{unit})",
                                p.index + 1,
                                np.title,
                                p.total
                            ),
                            None => format!("已切到《{}》", np.title),
                        }
                    }),
                    crate::media::PlayOutcome::AwaitingLogin { detail } => Ok(format!(
                        "这一集需要登录才能播放,请提示用户扫码登录;登录后会自动接着放。(原因:{detail})"
                    )),
                }
            }
            _ => {
                ctx.media.control(action, value)?;
                Ok("ok".into())
            }
        }
    }
}
