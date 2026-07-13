//! 能力轴:会话收尾(控制原语)。免唤醒连续对话(跟进窗)的显式关闭入口 —— 模型判断
//! 这轮语音交流已结束时调它,回合收尾后前端不再开跟进窗、安静回到待唤醒(§7.5)。
//!
//! 性质同 enter_mode:不干世界的活,只发一个"本轮结束、别再接着听"的带外信号。引擎在
//! turn loop 里记下"本回合调过它",随收尾的 `TurnEvent::Done { end_session }` 递给前端;
//! 前端据此走 wakeResume(收窗)而非 wakeFollowUp(开窗)。**信号不进回复文本** —— 回复
//! 在念/显/落库三处都保持干净(区别于 __IGNORE__ 那种整轮蒸发的全文哨兵)。
//! 打字回合调它无害(前端对非唤醒回合的 wakeResume 是 no-op)。

use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolSpec};

pub(super) struct EndConversation {
    spec: ToolSpec,
}

impl EndConversation {
    pub(super) fn new() -> EndConversation {
        EndConversation {
            spec: ToolSpec {
                name: "end_conversation",
                description: "结束当前这轮语音对话。当你判断交流已经收尾、用户多半不会再接话时调用:\
                              比如 TA 说了再见 / 没事了 / 先这样,或者事情已经办完、话到此为止。\
                              调用后正常把话说完即可(该道别就自然道个别),之后会安静回到待唤醒、\
                              不再免唤醒接话。默认是继续留着听 —— 只要还拿不准用户想不想接着说,就别调用。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                timeout: std::time::Duration::from_secs(5),
                ui_key: "tool.end_conversation",
            },
        }
    }
}

#[async_trait]
impl Tool for EndConversation {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, _args: serde_json::Value, _ctx: &ToolCtx) -> anyhow::Result<String> {
        // 纯信号:真正的收尾动作(不开跟进窗)由引擎据"本轮调过它"在 Done 上标记、前端执行。
        // 结果是喂回模型的观察 —— 让它安心把话说完、别再画蛇添足调别的工具。
        Ok("已标记本轮语音对话结束,回复完成后会安静回到待唤醒。简短把话说完即可,不用再做别的。".into())
    }
}
