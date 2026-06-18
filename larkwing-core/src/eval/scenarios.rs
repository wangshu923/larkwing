//! 第一批评估场景:每条都对应 AGENT.md 里一条踩坑铁律 / 用户准则 —— 把「文档里的教训」
//! 变成「可执行的回归守卫」。加场景 = 在 `suite()` 里再 push 一个(Rust + 组合子,非 DSL)。
//!
//! 这些场景**只在真模型下有意义**(`examples/eval.rs`);判官逻辑本身的自测在 `super::tests`。

use super::*;

/// 第一批场景。覆盖:工具选择 / 记忆该记不该记 + 分类 / few-shot 泄漏回归 / recall 触发 /
/// 提炼宁缺毋滥 + 正例。
pub fn suite() -> Vec<Scenario> {
    vec![
        // 闲聊不该动工具(§6.5 反例纪律:不该调工具的对话就别调)。
        Scenario::turn("chitchat-no-tool")
            .note("闲聊不调任何工具(§6.5 反例)")
            .say("今天有点累,随便陪我聊两句吧")
            .check(no_tool_calls()),
        // 安全/身份事实:要记,且归 identity(§13.4 遗忘非对称 —— 过敏绝不能被当普通 fact 下沉)。
        Scenario::turn("capture-allergy-identity")
            .note("过敏要记、且归 identity 不被下沉(§13.4)")
            .say("记一下,我女儿对花生过敏")
            .check(tool_called("remember"))
            .check(memory_written(None, "花生"))
            .check(memory_written(Some("identity"), "花生")),
        // 一次性琐事不该记(§13.3 三道筛子:以后用不上的不写)。
        Scenario::turn("no-capture-oneoff")
            .note("一次性琐事不该 remember(§13.3 三道筛子)")
            .say("我中午吃了碗牛肉拉面")
            .check(tool_not_called("remember")),
        // few-shot 泄漏回归(§6.5 实锤):一次性「求推荐」请求不该 remember 任何东西 ——
        // 用户没陈述任何自身事实,模型若把自己给的/示范里的建议内容记成用户真事就是泄漏。
        // (原先盯死「湿地公园/科技馆/采摘园」三个名字 = few-shot 去事实化后已删 → 死覆盖永远过;
        //  改盯「别 remember」这个本质,与具体名字脱钩、抓得住任何建议内容被误记。)
        Scenario::turn("fewshot-no-leak")
            .note("一次性求推荐不该 remember 任何东西(§6.5:建议内容不得被当用户真事记下)")
            .say("周末有点无聊,有什么推荐的吗?")
            .check(tool_not_called("remember")),
        // 放视频先查本地、别被示范带着直奔网络(§6.5 few-shot 泄漏实锤:模型把示范里
        // 「某片本地没有」当成既知事实、跳过 fs_find)。登记了电影目录就该先 fs_find 它,
        // 本地没有再 media_search。
        Scenario::turn("media-local-first")
            .note("放视频先查本地目录再上网,别被示范带偏(§6.5 + 先本地后网络)")
            .seed(|s, _u| {
                let _ = s.briefings.upsert("home", "media", "电影在 D:\\Movies", true);
            })
            .say("放一下流浪地球2")
            .check(tool_called("fs_find"))
            .check(custom("fs_find 先于 media_search", |o| {
                let find = o.trace.iter().position(|s| s.name == "fs_find");
                let search = o.trace.iter().position(|s| s.name == "media_search");
                match (find, search) {
                    (Some(f), Some(se)) => f < se,
                    (Some(_), None) => true, // 只查本地、没上网也算守规矩
                    _ => false,              // 没查本地就直奔网络 = 回归
                }
            })),
        // 旧事重提该用 recall 去取,而不是装不记得 / 瞎答(§13.7)。
        Scenario::turn("recall-triggers")
            .note("旧事重提、信息在按需层(episodic 非常驻)→ 该调 recall(§13.7)")
            .seed(|s, u| {
                // episodic = 非常驻(default_resident = !episodic)→ 不进前缀 → 必须 recall 才拿得到。
                // (上一版的坑:seed 成 experience 默认进前缀,模型直接照答、本就不必 recall。)
                let _ = s.memory.add(
                    u,
                    "episodic",
                    "用户提到过自己放松时爱听的一个歌单叫『雨天纯音乐』",
                    "explicit",
                );
            })
            .say("上次我说的那个放松的歌单叫什么来着?")
            .check(tool_called("recall")),
        // 提炼宁缺毋滥:没值得记的就不记,空结果是常态(2026-06-18 用户准则)。
        Scenario::consolidate("consolidate-restraint")
            .note("没值得记的就别提炼,空是常态(2026-06-18 宁缺毋滥)")
            .line("user", "今天天气不错啊")
            .line("assistant", "是呀,阳光挺好的~")
            .line("user", "中午随便吃了点")
            .line("assistant", "吃饱就好,下午也加油呀")
            .check(distilled_empty()),
        // 提炼正例:被反复纠正的偏好应被蒸馏成一条经验(Phase 3 该学到的)。
        // 隐式偏好捕获:用户在纠正中**清楚显露**了「按歌手整理」的习惯与缘由,但**从不说「记住」**
        // —— 显式「记住」本该对话内 remember 当场抓;consolidation 的本职是悟出这类隐式模式
        // (§13.2 公理 3「懂你来自程序性记忆,不来自被命令记」)。
        Scenario::consolidate("consolidate-learns-correction")
            .note("隐式偏好(纠正中显露、从不说『记住』)应被提炼成经验(Phase 3 正例)")
            .line("user", "帮我整理一下音乐文件夹,太乱了")
            .line("assistant", "好的,我先按专辑把歌归到一起了")
            .line("user", "别按专辑,按歌手分——我找歌一直都是按歌手找的")
            .line("assistant", "明白,按歌手重新分好了")
            .line("user", "对,这样我一眼就能看到某个歌手的全部歌")
            .line("assistant", "嗯,都归到各自歌手名下了")
            .check(distilled_at_least(1))
            // 内容判定放宽到同义词(模型可能写「按艺人/演唱者分类」),用逃生口写任意 Rust。
            .check(custom("提炼出按歌手/艺人分类的经验", |o| {
                o.distilled > 0
                    && o.memories.iter().any(|m| {
                        let c = &m.content;
                        c.contains("歌手") || c.contains("艺人") || c.contains("演唱者")
                    })
            })),
    ]
}
