//! 第一批评估场景:每条都对应 AGENT.md 里一条踩坑铁律 / 用户准则 —— 把「文档里的教训」
//! 变成「可执行的回归守卫」。加场景 = 在 `suite()` 里再 push 一个(Rust + 组合子,非 DSL)。
//!
//! 这些场景**只在真模型下有意义**(`examples/eval.rs`);判官逻辑本身的自测在 `super::tests`。

use super::*;

/// 第一批场景。覆盖:工具选择 / 记忆该记不该记 + 分类 / few-shot 泄漏回归 / recall 触发 /
/// 提炼宁缺毋滥 + 正例 / fs 真实整理 / 超长写入退回后自愈。
pub fn suite() -> Vec<Scenario> {
    // fs-organize 的真实临时目录(pid 隔离防并行冲突;seed 每 run 重置 = run 间零串扰)
    let fs_dir = std::env::temp_dir().join(format!("lw-eval-fs-{}", std::process::id()));
    let fs_dir_s = fs_dir.to_string_lossy().to_string();
    let (seed_dir, check_dir) = (fs_dir.clone(), fs_dir);

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
        // 说话人归属遵循度(§一家人法条 + 说话人显性化 watch-item):家人说的「我…」偏好该记到
        // TA 名下、不记到主人头上(声纹 / 渠道归人共用 speaker_user;say_as 模拟入站带标记)。
        // 真模型才验得出「〔某某说〕→ 我=TA」这条法条听不听话。
        Scenario::turn("speaker-attribution-family")
            .note("家人说的偏好归 TA、不归主人(§一家人法条遵循度)")
            .seed(|s, _u| {
                let _ = s.users.create("小明");
            })
            .say_as("小明", "记一下,我不吃香菜")
            .check(tool_called("remember"))
            .check(memory_written(None, "香菜"))
            .check(custom("香菜归小明、不归主人", |o| {
                // 全家记忆里:有一条「香菜」归非主人(小明)、且没有「香菜」错记到主人名下。
                let has = |is_owner: bool| {
                    o.all_memories
                        .iter()
                        .any(|m| m.content.contains("香菜") && (m.user_id == o.owner_id) == is_owner)
                };
                has(false) && !has(true)
            })),
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
        // ── 工具选择守卫(§16.3 扩面:把 §7.4 各能力域的「该用哪个工具」变回归)──
        // 定时提醒该走 reminder_set(说人话、模型用 now 推时间;cron 概念不暴露 §7.4)。
        Scenario::turn("reminder-sets-job")
            .note("「明早提醒我吃药」→ reminder_set(§7.4 提醒三件套)")
            .say("明天早上八点提醒我吃药")
            .check(tool_called("reminder_set")),
        // 时效性信息该上网查证,不硬编瞎答(§7.4 web 搜索即抓取)。
        Scenario::turn("web-for-fresh-info")
            .note("时效性信息 → web_search,不硬答(§7.4)")
            .say("帮我搜一下今天有什么国内大新闻")
            .check(tool_called("web_search")),
        // 问天气走专用 weather 工具,不该退化成网页搜索(各司其职;§7.4)。
        Scenario::turn("weather-uses-weather-tool")
            .note("问天气 → weather 工具(不是 web_search)")
            .say("上海明天天气怎么样,要不要带伞?")
            .check(tool_called("weather")),
        // 环境/资源事实(资源放哪)→ 需知 briefing_write(§7.3 三路路由)。换「工作文档」资源
        // (few-shot 演示的是「电影」)验路由泛化,不照抄示范。
        // ⚠️ 只断言「调了 briefing_write」,不断言「没调 remember」:资源位置同时也算一条关于用户的事实、
        // 记进小本本不算错(初版加了 tool_not_called(remember) → 实测假阳性:模型很合理地 briefing 记位置
        // + experience 记「以后怎么做」的程序性偏好,§13.2)。守住「资源事实进了 briefing 渠道」这个本质即可。
        Scenario::turn("briefing-routes-resource")
            .note("「我的工作文档放在 E:\\Docs」→ briefing_write(环境/资源进需知,§7.3 三路)")
            .say("我的工作文档都放在 E:\\Docs 这个文件夹")
            .check(tool_called("briefing_write")),
        // ── Phase 3 激进维护:LLM 纠错替换的行为守卫(2026-06-23)──
        // 用户明确纠正了已记得的旧偏好 → 提炼器应发 replaces 指令走 supersede,产出一条
        // source=correction 的新记忆(覆盖旧的)。删除侧由 supersede 单测保;这里验**模型认不认纠正、
        // 走没走纠错路**(source=correction 只可能由 supersede 产生 → 与「plain add 没认出纠正」区分开)。
        // 断言走 all_memories(全量),不走 memories(id 差集)—— supersede 删+重插复用 rowid 会让
        // 新条撞回 seed 的 id、被差集漏看(2026-06-23:此场景一度 0/5 假阴,verbose 实锤 supersede
        // 其实触发了、判官看不见;改 memory_with_source 看全量快照即真。见 grader 同源自测)。
        Scenario::consolidate("correction-supersedes")
            .note("明确纠正旧记忆 → 提炼出 source=correction 的替换(LLM 纠错行为,Phase 3)")
            .seed(|s, u| {
                let _ = s.memory.add(u, "fact", "用户喜欢喝美式咖啡", "explicit");
            })
            .line("user", "我现在不喝美式了,改喝拿铁,以后别给我推荐美式")
            .line("assistant", "好的,记住啦,以后都按拿铁来~")
            .check(distilled_at_least(1))
            .check(memory_with_source("correction", None)),
        // ── v0.2.4 扩面 ──
        // fs 整理 = 直交原语自行组合(§7.2/§5:没有 organize_media 任务工具,模型自己
        // mkdir+move);断言看**真实磁盘终态**,比只看轨迹硬 —— 挪没挪、挪对没挪对一目了然。
        Scenario::turn("fs-organize")
            .note("「把 mp3 挪进 music 子文件夹」→ fs 原语组合真把文件搬对(§7.2)")
            .seed(move |_s, _u| {
                let _ = std::fs::remove_dir_all(&seed_dir);
                let _ = std::fs::create_dir_all(&seed_dir);
                for f in ["a.mp3", "b.mp3", "c.txt"] {
                    let _ = std::fs::write(seed_dir.join(f), b"x");
                }
            })
            .say(&format!("把 {fs_dir_s} 这个文件夹里的 mp3 文件都挪到它下面的 music 子文件夹里"))
            .check(tool_called("fs_move"))
            .check(custom("mp3 真进了 music/,txt 原地不动", move |_o| {
                check_dir.join("music").join("a.mp3").is_file()
                    && check_dir.join("music").join("b.mp3").is_file()
                    && !check_dir.join("a.mp3").exists()
                    && !check_dir.join("b.mp3").exists()
                    && check_dir.join("c.txt").is_file()
            })),
        // 超长写入退回后自愈(§6.5:超上限一律 bail 退回、绝不静默截断;FACT_MAX_CHARS=200)。
        // 模型两条合法出路都算过:①先塞整段 → 被退回 → 精简/拆条重写;②自己先拆几条分别记。
        // 守的是「长信息最终落成可用记忆、回合正常收尾」,而不是死磕某一条执行路径。
        Scenario::turn("overlong-note-recovers")
            .note("超长「记住这段」→ 退回不截断,模型精简/拆条后仍记下要点(§6.5)")
            .say("帮我原原本本记一下这段话,一点都别丢:我爸妈下个月要从老家过来住一阵子,\
                他们俩习惯早睡早起,我爸每天早上六点必须去公园打太极拳,风雨无阻,我妈对海鲜过敏,\
                虾蟹贝类都碰不得,但她特别爱吃素三鲜饺子,尤其是荠菜馅的;给他们住的客房那台空调的\
                遥控器一直放在电视柜从左数第二个抽屉里,床头柜里有备用的老花镜;他们来的那天我得去\
                高铁站接,车是上午十点半到站,到时候提醒我提前一个小时出门,顺便把后备箱里的杂物\
                清一清好放行李,老人家拎着大包小包不方便,别让他们在站里等太久")
            .check(tool_called("remember"))
            .check(custom("要点真落进了记忆(过敏/太极/接站任一)", |o| {
                o.memories.iter().any(|m| {
                    m.content.contains("过敏")
                        || m.content.contains("太极")
                        || m.content.contains("十点半")
                })
            })),
    ]
}
