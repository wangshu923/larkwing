//! 能力轴:影音(加工)。`ffmpeg_run` 工具的机器件:替模型跑一条 ffmpeg 命令,做剪一段/
//! 转格式/抽音轨/拼接/滤镜这类本机加工。**知识归模型,边界归机器**(§5 通用回合循环):
//! 参数怎么组是模型自己的 ffmpeg 知识,程序不做操作矩阵;这里只管边界——输出永不进模型
//! 参数(独立落点,dedupe 永不覆盖、临时件写完改名,§7.2 三规①同源;输入原件一个字节
//! 不碰),输入在工具层已过授权圈。**不做成本预测**:裸参数估不准耗时,一律先跑,
//! `IN_TURN_WAIT` 内没跑完自动转 bgtasks 后台(〔此刻〕/进度/取消/收尾唤回合汇报/卡死
//! 看门狗全白拿)。`-c:v h264` 占位 = 换成这台机器最快的编码器(探测缓存在 relay,
//! §4.8 单源;copy 路不带占位 = 不探测)。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::AsyncBufReadExt;

use crate::bus::Text;
use crate::components::Component;

use super::MediaRuntime;

/// 回合内等待窗:起跑后等这么久,没跑完就转后台接着跑(§4.11 常量单源;方案已确认)。
const IN_TURN_WAIT: Duration = Duration::from_secs(30);
/// 报错尾巴留存(行数/字符):stderr 可能很长,喂回模型自纠只要末尾这点(量约束 §7.2)。
const STDERR_TAIL_LINES: usize = 40;
const STDERR_TAIL_CHARS: usize = 2000;
/// 探测形(只探不产)的超时与输出上限:横幅通常 1–3KB,截断保**头部**(信息在前)。
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
const PROBE_OUT_MAX: usize = 4000;

/// 工具层校验/过闸后的一次加工请求。
pub struct EditRequest {
    /// 已过闸的 ffmpeg 参数(含 `-i` 输入,绝对路径;**不含输出文件**)。
    pub args: Vec<String>,
    /// `-i` 输入清单(探时长/打进度用第一项)。
    pub inputs: Vec<PathBuf>,
    /// 意向输出(工具层已过授权圈 create;落盘前再 dedupe,永不覆盖)。
    pub dest: PathBuf,
    /// args 里带 `-c:v h264` 占位 → 换成这台机器最快的编码器。
    pub wants_h264: bool,
}

pub enum EditOutcome {
    /// 回合内跑完:成品路径 + 大小 + 实际用的视频编码器(占位替换了才有值)。
    Done { path: PathBuf, bytes: u64, encoder: Option<&'static str> },
    /// 转后台了(bgtasks 登记处可见可停;收尾自动唤回合汇报)。
    Background { title: String },
}

impl MediaRuntime {
    /// 跑一条 ffmpeg(ffmpeg_run 的机器件)。`origin` = (user_id, conv_id):转后台时
    /// 收尾汇报靠它唤回合(§7.1 批量收尾回报同一套机器)。
    pub async fn ffmpeg_edit(&self, req: EditRequest, origin: (i64, i64)) -> Result<EditOutcome> {
        anyhow::ensure!(!req.inputs.is_empty(), "没有输入文件");
        let ffmpeg = self.ensure_component(Component::Ffmpeg).await?;
        // 占位替换:真要重编码才探编码器(copy 路零探测,§7.1 硬件加速判据同款);
        // 探测结果整进程缓存在 relay(与播放转码链共享同一次探测)。
        let (args, encoder) = if req.wants_h264 {
            let relay = self
                .inner
                .relay
                .get_or_try_init(super::relay::Relay::start)
                .await
                .context("转发服务起不来")?;
            let enc_args = super::relay::video_encode_args(relay.video_encoder(&ffmpeg).await);
            (substitute_h264(&req.args, enc_args), Some(enc_args[1]))
        } else {
            (req.args.clone(), None)
        };
        // 源时长给进度当分母;拿不到(或剪辑目标远短于源)就报「已处理到几分几秒」。
        let duration =
            self.probe_with_ffmpeg(&ffmpeg, &req.inputs[0]).await.duration_seconds;

        let dest_dir = req
            .dest
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| anyhow::anyhow!("输出路径没有上级目录"))?;
        let ext = req.dest.extension().and_then(|e| e.to_str()).unwrap_or("bin").to_string();
        static SEQ: AtomicU64 = AtomicU64::new(0);
        // 临时件与成品同目录同扩展(muxer 按扩展名认格式;同卷 → 收尾 rename 原子)
        let tmp = dest_dir.join(format!(
            ".lw-edit-{}-{}.{ext}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));

        let mut cmd = tokio::process::Command::new(&ffmpeg);
        cmd.args(["-hide_banner", "-nostdin", "-loglevel", "error"]);
        cmd.args(["-progress", "pipe:1", "-nostats"]);
        cmd.args(&args);
        cmd.arg(&tmp);
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        // 回合被取消 / 看门狗 abort → future 被 drop:子进程跟着杀,临时件由 TempGuard 收
        // (Windows 上进程尸体未落地时删临时件可能失败 —— 隐藏点前缀文件,残留无害)。
        cmd.kill_on_drop(true);
        super::no_console(&mut cmd);
        let mut child = cmd.spawn().context("ffmpeg 起不来")?;
        let mut guard = TempGuard(Some(tmp));

        // -progress 键值流 → 「已处理到第几秒」;stderr → 报错尾巴
        let out_secs = Arc::new(Mutex::new(0.0f64));
        if let Some(stdout) = child.stdout.take() {
            let out_secs = out_secs.clone();
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Some(s) = parse_progress_line(&line) {
                        *out_secs.lock().expect("progress lock") = s;
                    }
                }
            });
        }
        let tail = Arc::new(Mutex::new(VecDeque::<String>::new()));
        if let Some(stderr) = child.stderr.take() {
            let tail = tail.clone();
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut t = tail.lock().expect("tail lock");
                    if t.len() >= STDERR_TAIL_LINES {
                        t.pop_front();
                    }
                    t.push_back(line);
                }
            });
        }

        // —— 回合内窗:跑完当场回;没跑完转后台 ——
        tokio::select! {
            status = child.wait() => {
                let status = status.context("等 ffmpeg 退出失败")?;
                return if status.success() {
                    let (path, bytes) = finalize(&mut guard, &req.dest)?;
                    Ok(EditOutcome::Done { path, bytes, encoder })
                } else {
                    // stderr 读取器可能比 wait() 晚一拍,给一口气排空缓冲再取尾巴
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    Err(anyhow::anyhow!(
                        "ffmpeg 退出码 {}:\n{}\n(照报错改参数再试;长片先剪十几秒试参数)",
                        status.code().unwrap_or(-1),
                        tail_text(&tail)
                    ))
                };
            }
            _ = tokio::time::sleep(IN_TURN_WAIT) => {}
        }

        // —— 转后台(cap 满 = 如实退回,不排队;半成品清理交 TempGuard) ——
        let name = req
            .dest
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "输出".into());
        let ticket = match self.inner.bg.submit(format!("音视频加工({name})"), origin, 100) {
            Ok(t) => t,
            Err(e) => {
                let _ = child.kill().await;
                return Err(anyhow::anyhow!(
                    "{e:#};这活半分钟没跑完、想转后台接着跑但后台满了,这次先停了\
                     (半成品已清理),等几件后台事跑完再来"
                ));
            }
        };
        let ticket_id = ticket.id();
        let task = self.inner.tasks.start("ffmpeg", Text::new("task.ffmpeg"));
        let dest = req.dest.clone();
        let bg_name = name.clone();
        let join = tokio::spawn(async move {
            let mut guard = guard;
            let mut last = -1.0f64;
            loop {
                tokio::select! {
                    status = child.wait() => {
                        match status {
                            Ok(s) if s.success() => match finalize(&mut guard, &dest) {
                                Ok((path, bytes)) => {
                                    task.done();
                                    let enc_note = encoder
                                        .map(|e| format!(",视频编码器用了 {e}"))
                                        .unwrap_or_default();
                                    ticket.finish(true, format!(
                                        "音视频加工好了:{}({}{enc_note})。输入原件没动;\
                                         把成品放哪了简短告诉用户。",
                                        path.display(),
                                        crate::files::human_size(bytes)
                                    ));
                                }
                                Err(e) => {
                                    task.fail("task.err.ffmpeg", serde_json::Value::Null);
                                    ticket.finish(false, format!(
                                        "音视频加工({bg_name})最后落盘失败:{e:#}。如实告诉用户。"
                                    ));
                                }
                            },
                            _ => {
                                // 同回合内路:等 stderr 读取器排空再取尾巴
                                tokio::time::sleep(Duration::from_millis(200)).await;
                                task.fail("task.err.ffmpeg", serde_json::Value::Null);
                                ticket.finish(false, format!(
                                    "音视频加工({bg_name})没成,ffmpeg 报错:{}。如实告诉用户;\
                                     要重试就照报错改参数。",
                                    tail_text(&tail)
                                ));
                            }
                        }
                        break;
                    }
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {
                        if ticket.is_cancelled() {
                            let _ = child.kill().await;
                            task.fail("task.err.cancelled", serde_json::Value::Null);
                            ticket.finish(false, format!(
                                "音视频加工({bg_name})按要求停下了,半成品已清理。"
                            ));
                            break;
                        }
                        // 只在真有推进时打点:僵住的 ffmpeg 不喂看门狗,10min 判卡照常触发
                        let cur = *out_secs.lock().expect("progress lock");
                        if cur > last {
                            last = cur;
                            let (text, frac, done) = progress_bits(cur, duration);
                            ticket.beat(done, text.clone());
                            task.step_progress(
                                "step.ffmpeg",
                                serde_json::json!({ "t": bg_name, "p": text }),
                                frac,
                            );
                        }
                    }
                }
            }
        });
        self.inner.bg.attach_abort(ticket_id, join.abort_handle());
        Ok(EditOutcome::Background { title: name })
    }
}

impl MediaRuntime {
    /// 探测形(工具层 `output` 缺省时走这):照人在终端里的习惯跑 `ffmpeg -i …` 只看不产,
    /// **原样返回 ffmpeg 的信息输出**(stderr 横幅:时长/编码/音轨/标签)——模型本来就读得懂,
    /// 不做解析层。`-i` 无输出必然非零退出,不看退出码(probe_with_ffmpeg 同款);没有输出
    /// 落盘动作(孤裸值/旁路 flag 已在工具层拒掉),天然只读且秒回,不进后台机器。
    pub async fn ffmpeg_probe(&self, args: Vec<String>) -> anyhow::Result<String> {
        let ffmpeg = self.ensure_component(Component::Ffmpeg).await?;
        let mut cmd = tokio::process::Command::new(&ffmpeg);
        cmd.args(["-hide_banner", "-nostdin"]);
        cmd.args(&args);
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
        cmd.kill_on_drop(true);
        super::no_console(&mut cmd);
        let out = tokio::time::timeout(PROBE_TIMEOUT, cmd.output())
            .await
            .map_err(|_| anyhow::anyhow!("ffmpeg 探测超时"))?
            .context("ffmpeg 起不来")?;
        let text = String::from_utf8_lossy(&out.stderr);
        let text = text.trim();
        if text.is_empty() {
            return Ok("(ffmpeg 没有输出任何信息)".into());
        }
        Ok(cap_head(text, PROBE_OUT_MAX))
    }
}

/// 截断保头部(探测横幅信息在前,与报错尾巴的 tail 方向相反),超长如实说明(§3.5)。
fn cap_head(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    let head: String = chars[..max].iter().collect();
    format!("{head}\n…(输出太长,后面截了)")
}

/// 半路死(回合取消 / 看门狗 abort / panic)时收走临时件;成功改名后解除。
struct TempGuard(Option<PathBuf>);

impl Drop for TempGuard {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// 成品落位:dedupe(永不覆盖)→ 临时件改名 → 解除清理守卫。
fn finalize(guard: &mut TempGuard, dest: &Path) -> Result<(PathBuf, u64)> {
    let tmp = guard.0.clone().ok_or_else(|| anyhow::anyhow!("临时件已丢失"))?;
    let out = crate::files::dedupe_path(dest);
    std::fs::rename(&tmp, &out)
        .with_context(|| format!("成品改名失败 {} -> {}", tmp.display(), out.display()))?;
    guard.0 = None;
    let bytes = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    Ok((out, bytes))
}

/// 把「-c:v h264」占位(含 -vcodec/-codec:v 形)换成探测出的编码器参数序列(全部出现处)。
fn substitute_h264(args: &[String], enc_args: &'static [&'static str]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len() + enc_args.len());
    let mut i = 0;
    while i < args.len() {
        if matches!(args[i].as_str(), "-c:v" | "-vcodec" | "-codec:v")
            && args.get(i + 1).map(String::as_str) == Some("h264")
        {
            out.extend(enc_args.iter().map(|s| (*s).to_string()));
            i += 2;
            continue;
        }
        out.push(args[i].clone());
        i += 1;
    }
    out
}

/// ffmpeg `-progress` 键值流里抽「已处理到第几秒」。注意 `out_time_ms` 历史上就是**微秒**
/// (ffmpeg 将错就错的老字段,与 out_time_us 同值);`out_time=` 是 HH:MM:SS.micro 钟面形。
pub(crate) fn parse_progress_line(line: &str) -> Option<f64> {
    let line = line.trim();
    let us = |v: &str| {
        v.trim().parse::<i64>().ok().filter(|x| *x >= 0).map(|x| x as f64 / 1e6)
    };
    if let Some(v) = line.strip_prefix("out_time_us=") {
        return us(v);
    }
    if let Some(v) = line.strip_prefix("out_time_ms=") {
        return us(v);
    }
    if let Some(v) = line.strip_prefix("out_time=") {
        return parse_clock(v.trim());
    }
    None
}

fn parse_clock(v: &str) -> Option<f64> {
    let mut it = v.split(':');
    let (h, m, s) = (it.next()?, it.next()?, it.next()?);
    if it.next().is_some() {
        return None;
    }
    let h = h.parse::<i64>().ok().filter(|x| *x >= 0)?;
    let m = m.parse::<i64>().ok().filter(|x| (0..60).contains(x))?;
    let s = s.parse::<f64>().ok().filter(|x| (0.0..60.0).contains(x))?;
    Some(h as f64 * 3600.0 + m as f64 * 60.0 + s)
}

/// 进度三件:给人看的文本(百分比或钟面)、HUD 分数、bgtasks 计数(0..100)。
/// 分母 = 源文件时长 —— 剪辑目标短于源时百分比会偏低,只影响观感不影响正确性。
fn progress_bits(cur: f64, duration: Option<f64>) -> (String, f32, usize) {
    match duration.filter(|d| *d > 0.5) {
        Some(d) => {
            let frac = (cur / d).clamp(0.0, 0.99);
            (format!("{:.0}%", frac * 100.0), frac as f32, (frac * 100.0) as usize)
        }
        None => (clock_text(cur), 0.0, 0),
    }
}

fn clock_text(s: f64) -> String {
    let t = s.max(0.0) as u64;
    format!("{:02}:{:02}", t / 60, t % 60)
}

fn tail_text(tail: &Arc<Mutex<VecDeque<String>>>) -> String {
    let t = tail.lock().expect("tail lock");
    let joined = t.iter().cloned().collect::<Vec<_>>().join("\n");
    let joined = joined.trim();
    if joined.is_empty() {
        return "(ffmpeg 没吐报错文本,多半是参数形状问题)".into();
    }
    let chars: Vec<char> = joined.chars().collect();
    if chars.len() > STDERR_TAIL_CHARS {
        chars[chars.len() - STDERR_TAIL_CHARS..].iter().collect()
    } else {
        joined.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_line_forms() {
        assert_eq!(parse_progress_line("out_time_us=1500000"), Some(1.5));
        assert_eq!(parse_progress_line("out_time_ms=1500000"), Some(1.5), "ms 字段其实是微秒");
        assert_eq!(parse_progress_line("out_time=00:01:02.500000"), Some(62.5));
        assert_eq!(parse_progress_line("out_time=N/A"), None, "没起来时 ffmpeg 会给 N/A");
        assert_eq!(parse_progress_line("out_time_us=N/A"), None);
        assert_eq!(parse_progress_line("frame=42"), None);
        assert_eq!(parse_progress_line("progress=end"), None);
    }

    #[test]
    fn h264_placeholder_substitution() {
        use crate::media::relay::{video_encode_args, VideoEncoder};
        let enc = video_encode_args(VideoEncoder::Software);
        let args: Vec<String> =
            ["-i", "/a.mp4", "-c:v", "h264", "-c:a", "aac"].iter().map(|s| s.to_string()).collect();
        let out = substitute_h264(&args, enc);
        assert!(out.iter().any(|t| t == enc[1]), "占位换成了具体编码器: {out:?}");
        assert!(!out.windows(2).any(|w| w[0] == "-c:v" && w[1] == "h264"), "占位不再残留");
        assert!(out.iter().any(|t| t == "-c:a"), "其余参数原样保留");

        // 显式编码器不替换(模型要精确控制时的出路)
        let explicit: Vec<String> =
            ["-c:v", "libx264", "-crf", "18"].iter().map(|s| s.to_string()).collect();
        assert_eq!(substitute_h264(&explicit, enc), explicit);
        // -vcodec 形也认
        let vcodec: Vec<String> = ["-vcodec", "h264"].iter().map(|s| s.to_string()).collect();
        assert!(substitute_h264(&vcodec, enc).iter().any(|t| t == enc[1]));
    }

    #[test]
    fn probe_output_caps_head_not_tail() {
        assert_eq!(cap_head("短的", 100), "短的");
        let long: String = "横幅在前".chars().cycle().take(200).collect();
        let capped = cap_head(&long, 50);
        assert!(capped.starts_with("横幅在前"), "保头部: {capped}");
        assert!(capped.ends_with("…(输出太长,后面截了)"), "如实说明: {capped}");
        assert_eq!(capped.chars().take_while(|c| *c != '\n').count(), 50);
    }

    #[test]
    fn progress_text_pct_and_clock() {
        let (text, frac, done) = progress_bits(30.0, Some(120.0));
        assert_eq!((text.as_str(), done), ("25%", 25));
        assert!((frac - 0.25).abs() < 1e-6);
        // 分母缺失 → 钟面;推进到头 clamp 99%
        assert_eq!(progress_bits(75.0, None).0, "01:15");
        assert_eq!(progress_bits(500.0, Some(120.0)).0, "99%");
    }
}
