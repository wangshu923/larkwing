//! 假 provider:定时吐词,开发(LARKWING_FAKE_LLM=1)与测试用。
//! 它两个方向的翻译都没有 —— "provider = 把 ChatRequest 变成 ChatEvent 流的东西",
//! 会不会说某家方言只是实现细节。
//! 脚本模式:预置 FakeTurn 队列,依次弹出 —— engine 工具循环的确定性测试靠它,不碰网络。

use std::collections::VecDeque;
use std::sync::Mutex;

use tokio::sync::mpsc;

use super::{ChatEvent, ChatMessage, ChatRequest, LlmError, LlmProvider, ToolCall, ToolChoice, Usage};

/// 一轮剧本:先流 text,再以 tool_calls 决定 stop_reason(空 = end_turn,非空 = tool_use)。
#[derive(Debug, Clone, Default)]
pub struct FakeTurn {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    /// 回传的假 usage(记账链路测试用);默认全 0 = engine 不记账不点灯。
    pub usage: Usage,
}

pub struct FakeLlm {
    /// 每个字符之间的间隔;测试用小值,开发演示用 ~40ms。
    pub delay_ms: u64,
    /// 剧本队列;空 = 回声模式(默认行为)。
    script: Mutex<VecDeque<FakeTurn>>,
}

impl Default for FakeLlm {
    fn default() -> Self {
        Self::with_delay(40)
    }
}

impl FakeLlm {
    pub fn with_delay(delay_ms: u64) -> Self {
        Self { delay_ms, script: Mutex::new(VecDeque::new()) }
    }

    pub fn scripted(turns: Vec<FakeTurn>) -> Self {
        Self { delay_ms: 1, script: Mutex::new(turns.into()) }
    }
}

#[async_trait::async_trait]
impl LlmProvider for FakeLlm {
    async fn chat_stream(&self, req: ChatRequest) -> Result<mpsc::Receiver<ChatEvent>, LlmError> {
        let scripted = self.script.lock().expect("fake script lock").pop_front();
        let (reply, mut tool_calls, usage) = match scripted {
            Some(turn) => (turn.text, turn.tool_calls, turn.usage),
            None => {
                let last_user = req
                    .messages
                    .iter()
                    .rev()
                    .find_map(|m| match m {
                        ChatMessage::User { content } => Some(content.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                let reply = format!(
                    "滴——收到「{last_user}」!我现在是替身 7274,等配好真钥匙就换本体上场!"
                );
                (reply, Vec::new(), Usage::default())
            }
        };
        // 像真 provider 一样尊重 tool_choice:None = 本次禁用工具(轮数到顶的强制收尾路径)
        if req.tool_choice == ToolChoice::None {
            tool_calls.clear();
        }
        let stop_reason = if tool_calls.is_empty() { "end_turn" } else { "tool_use" };

        let delay = std::time::Duration::from_millis(self.delay_ms);
        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(async move {
            for ch in reply.chars() {
                if tx.send(ChatEvent::Delta(ch.to_string())).await.is_err() {
                    return; // 取消 = 接收端 drop
                }
                tokio::time::sleep(delay).await;
            }
            let _ = tx
                .send(ChatEvent::Done {
                    usage,
                    stop_reason: Some(stop_reason.into()),
                    tool_calls,
                })
                .await;
        });
        Ok(rx)
    }
}
