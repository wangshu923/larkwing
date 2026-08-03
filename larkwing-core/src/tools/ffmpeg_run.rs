//! 能力轴:影音(加工)。把 ffmpeg 直通给模型:剪/转/抽轨/拼接/滤镜全靠它自己的
//! ffmpeg 知识组参数,程序不做操作矩阵(§5;结构化参数集只会做减法 = 「话术承诺了、
//! 参数没给」的老坑)。**§6.3「参数无后门」的有闸例外**:args 就是工具本体,闸全在
//! 边界上 —— 输入 = `-i` 后那个 token + 滤镜内嵌文件(subtitles=/ass=/movie=),逐个过
//! 授权圈「读」;输出**永不进 args**(独立参数,过「存入」闸 + 永不覆盖);网址/管道/
//! 覆盖旗/旁路写文件的 flag 一律拒收,拒绝话术自解释。执行在 media/edit.rs(回合内
//! 30s 窗,超了自动转 bgtasks 后台)。

use std::path::PathBuf;

use anyhow::Context;
use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolRisk, ToolSpec};
use crate::media::{EditOutcome, EditRequest};

pub(super) struct FfmpegRun {
    spec: ToolSpec,
}

impl FfmpegRun {
    pub(super) fn new() -> FfmpegRun {
        FfmpegRun {
            spec: ToolSpec {
                name: "ffmpeg_run",
                description: "跑一条 ffmpeg 命令加工本机音视频:剪一段/转格式/抽音轨/多段\
                              拼接/调音量/变速/压缩尺寸/截帧/烧字幕等,参数靠你的 ffmpeg \
                              知识自由组合。args 是参数数组(一项一个 token,别做引号转义),\
                              输入文件写在 args 的 -i 后面(绝对路径,支持 ~ 开头);**输出\
                              文件不写进 args**——用 output(带扩展名的文件名,格式由扩展名\
                              决定)和 dir(缺省 = 第一个输入旁边)指定,程序落盘:绝不覆盖\
                              已有文件(重名自动加序号),也绝不改动输入原件。要重编码视频\
                              就写 -c:v h264,程序会换成这台机器最快的编码器(有显卡走显卡);\
                              想精确控画质才写具体编码器名(那样不替换)。只吃本机文件:\
                              网址、-y、-f concat/lavfi 都不收;多段拼接 = 多个 -i 加 \
                              filter_complex 的 concat。快活当场返回;超过半分钟自动转后台\
                              (任务条可见、可叫停,跑完自动回来汇报)。长片重编码前先剪\
                              十几秒试参数。失败会带 ffmpeg 报错回来,照着改参数重试。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "args": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "ffmpeg 参数,按顺序一项一个(含 -i 与输入的绝对路径;不含输出文件)"
                        },
                        "output": {
                            "type": "string",
                            "description": "输出文件名,带扩展名(输出格式由它决定);只是名字,不带目录"
                        },
                        "dir": {
                            "type": "string",
                            "description": "输出目录绝对路径(可选;缺省放在第一个输入文件旁边)"
                        }
                    },
                    "required": ["args", "output"]
                }),
                // 回合内窗 30s + 首次 ffmpeg 组件下载的余量(lyrics_fetch 同口径)
                timeout: std::time::Duration::from_secs(180),
                ui_key: "tool.ffmpeg_run",
            },
        }
    }
}

#[async_trait]
impl Tool for FfmpegRun {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn risk(&self) -> ToolRisk {
        ToolRisk::Mutating
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let raw = args
            .get("args")
            .and_then(serde_json::Value::as_array)
            .filter(|a| !a.is_empty())
            .ok_or_else(|| anyhow::anyhow!("缺少 args 参数(ffmpeg 参数数组)"))?;
        let mut tokens = Vec::with_capacity(raw.len());
        for v in raw {
            // 宽容收数字/布尔(模型偶尔把 `-t 10` 的 10 发成 JSON number;arg_bool 同族 §4.4)
            match v {
                serde_json::Value::String(s) => tokens.push(s.clone()),
                serde_json::Value::Number(n) => tokens.push(n.to_string()),
                serde_json::Value::Bool(b) => tokens.push(b.to_string()),
                other => anyhow::bail!("args 每一项要是字符串,收到:{other}"),
            }
        }
        let scan = scan_args(&tokens)?;

        let output = args
            .get("output")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("缺少 output 参数(输出文件名,带扩展名)"))?;
        anyhow::ensure!(
            !output.contains('/') && !output.contains('\\'),
            "output 只要文件名;目录用 dir 参数"
        );
        let clean = crate::files::sanitize_filename(output);
        let cp = std::path::Path::new(&clean);
        anyhow::ensure!(
            cp.extension().and_then(|e| e.to_str()).is_some_and(|e| !e.is_empty())
                && cp.file_stem().and_then(|s| s.to_str()).is_some_and(|s| !s.is_empty()),
            "output 要「名字.扩展名」形(输出格式由扩展名决定),收到:{output}"
        );

        let dir = args
            .get("dir")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(super::expand_home);
        let dest_dir = match &dir {
            Some(d) => {
                let p = PathBuf::from(d);
                anyhow::ensure!(p.is_absolute(), "dir 要绝对路径,收到:{d}");
                p
            }
            None => scan.inputs[0]
                .parent()
                .map(std::path::Path::to_path_buf)
                .ok_or_else(|| anyhow::anyhow!("第一个输入没有上级目录,显式给 dir"))?,
        };
        let dest = dest_dir.join(&clean);

        // 授权圈(§7.2):输入 + 滤镜要读的文件 = 读;输出落点 = 存入。全部前置在动手前。
        let mut reads: Vec<String> =
            scan.inputs.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        reads.extend(scan.filter_reads.iter().cloned());
        super::guard::ensure(ctx, super::guard::Access::Read, &reads).await?;
        super::guard::ensure(
            ctx,
            super::guard::Access::Create,
            &[dest.to_string_lossy().into_owned()],
        )
        .await?;
        if dir.is_some() {
            // 授权后才建目录(§7.8 pdf 同款次序)
            std::fs::create_dir_all(&dest_dir)
                .with_context(|| format!("建不出输出目录 {}", dest_dir.display()))?;
        }

        let req = EditRequest {
            args: scan.args,
            inputs: scan.inputs,
            dest,
            wants_h264: scan.wants_h264,
        };
        match ctx.media.ffmpeg_edit(req, (ctx.user_id, ctx.conv_id)).await? {
            EditOutcome::Done { path, bytes, encoder } => {
                let enc_note =
                    encoder.map(|e| format!(",视频编码器用了 {e}")).unwrap_or_default();
                Ok(format!(
                    "加工完成:{}({}{enc_note})。输入原件没动;重名时已自动加序号。",
                    path.display(),
                    crate::files::human_size(bytes)
                ))
            }
            EditOutcome::Background { title } => Ok(format!(
                "这活半分钟内没跑完,已转后台接着跑({title}),任务条上有进度、可以叫停;\
                 **跑完会自动回来一条结果汇报**(成没成、放哪了),到时再转述。现在告诉用户\
                 已经开工、跑完会说一声就好。"
            )),
        }
    }
}

/// 扫描/收窄模型给的参数(纯函数,单测钉着):抽输入、抽滤镜内嵌文件、认 h264 占位,
/// 拒掉网址/管道/覆盖旗/旁路写文件的 flag/孤零零的裸值(= 想把输出塞进 args)。
#[derive(Debug)]
struct Scan {
    args: Vec<String>,
    inputs: Vec<PathBuf>,
    filter_reads: Vec<String>,
    wants_h264: bool,
}

/// 旁路写文件 / 与程序职责冲突的 flag(带 `:spec` 后缀的形也按前缀拦)。
const BANNED: &[(&str, &str)] = &[
    ("-y", "覆盖策略由程序管(输出永不覆盖),别传 -y"),
    ("-n", "覆盖策略由程序管,别传 -n"),
    ("-progress", "进度上报程序自带,别传 -progress"),
    ("-passlogfile", "-passlogfile 会旁路写文件,不开放(两遍编码请改单遍质量模式)"),
    ("-report", "-report 会旁路写日志文件,不开放"),
    ("-vstats", "-vstats 会旁路写文件,不开放"),
    ("-vstats_file", "-vstats_file 会旁路写文件,不开放"),
    ("-dump_attachment", "-dump_attachment 会旁路写文件,不开放"),
    ("-segment_list", "-segment_list 会旁路写文件,不开放"),
    ("-filter_script", "-filter_script 从文件读滤镜,直接写进 -vf 即可"),
    ("-filter_complex_script", "-filter_complex_script 从文件读滤镜,直接写进 -filter_complex 即可"),
    ("-fpre", "-fpre 从文件读预设,不开放"),
];

fn scan_args(raw: &[String]) -> anyhow::Result<Scan> {
    anyhow::ensure!(!raw.is_empty(), "args 不能为空");
    let mut args = Vec::with_capacity(raw.len() + 2);
    let mut inputs = Vec::new();
    let mut filter_reads = Vec::new();
    let mut wants_h264 = false;
    let mut prev_was_flag = false;
    let mut i = 0;
    while i < raw.len() {
        let t = raw[i].trim();
        anyhow::ensure!(!t.is_empty(), "args 里有空项");
        anyhow::ensure!(!t.contains("://"), "只加工本机文件,网址不收:{t}");
        anyhow::ensure!(!t.starts_with("pipe:") && t != "-", "管道/标准流不收:{t}");
        if t == "-i" {
            let v = raw
                .get(i + 1)
                .map(|s| s.trim())
                .ok_or_else(|| anyhow::anyhow!("-i 后面缺输入路径"))?;
            anyhow::ensure!(!v.contains("://"), "只加工本机文件,网址不收:{v}");
            let p = super::expand_home(v);
            let pb = PathBuf::from(&p);
            anyhow::ensure!(pb.is_absolute(), "输入要绝对路径(支持 ~ 开头),收到:{v}");
            anyhow::ensure!(pb.is_file(), "输入文件不存在:{p}");
            inputs.push(pb);
            args.push("-i".into());
            args.push(p);
            i += 2;
            prev_was_flag = false;
            continue;
        }
        if t.starts_with('-') && t.len() > 1 {
            let base = t.split(':').next().unwrap_or(t);
            if let Some((_, why)) = BANNED.iter().find(|(b, _)| *b == base) {
                anyhow::bail!("{why}");
            }
            if t == "-f" {
                match raw.get(i + 1).map(|s| s.trim()) {
                    Some("concat") => anyhow::bail!(
                        "-f concat 不开放(它从列表文件里读路径,绕开文件授权);\
                         多段拼接用多个 -i 加 filter_complex 的 concat"
                    ),
                    Some("lavfi") => anyhow::bail!("-f lavfi 不开放(只加工本机文件)"),
                    _ => {}
                }
            }
            if matches!(t, "-c:v" | "-vcodec" | "-codec:v")
                && raw.get(i + 1).map(|s| s.trim()) == Some("h264")
            {
                wants_h264 = true;
            }
            args.push(t.to_string());
            i += 1;
            prev_was_flag = true;
            continue;
        }
        // 不带 - 的裸值必须紧跟 flag(ffmpeg 的值都是单 token;孤零零的裸值 = 多半想把
        // 输出写进 args —— 那是没过闸的写出口,拒收,§7.2 圈不能有缝)
        anyhow::ensure!(
            prev_was_flag,
            "参数「{t}」孤零零的:输出文件别写进 args(用 output 参数),值要紧跟它的 flag"
        );
        for path in find_filter_paths(t)? {
            anyhow::ensure!(
                !path.starts_with('~'),
                "滤镜里的文件路径写绝对路径(~ 在滤镜里不展开):{path}"
            );
            anyhow::ensure!(!path.contains("://"), "滤镜只读本机文件:{path}");
            let pb = PathBuf::from(&path);
            anyhow::ensure!(pb.is_absolute(), "滤镜里的文件路径要绝对路径:{path}");
            anyhow::ensure!(pb.is_file(), "滤镜要读的文件不存在:{path}");
            filter_reads.push(path);
        }
        args.push(t.to_string());
        i += 1;
        prev_was_flag = false;
    }
    anyhow::ensure!(!inputs.is_empty(), "至少要有一个 -i 输入");
    Ok(Scan { args, inputs, filter_reads, wants_h264 })
}

/// 会读文件的滤镜(名=值形)。amovie 在 movie 前(子串关系,先长后短防错切)。
const FILE_FILTERS: &[&str] = &["subtitles", "amovie", "movie", "ass"];

/// 在参数 token 里找「会读文件的滤镜」内嵌路径,抽出来给授权圈过闸。**不改写 token**
/// (滤镜有自己的转义规则,重写容易写坏 —— 路径必须已是绝对路径)。认不出形状 = Err
/// (宁可拒收,圈不能有缝)。词边界防 `bypass=`/`x264opts=…` 误配。
fn find_filter_paths(token: &str) -> anyhow::Result<Vec<String>> {
    let chars: Vec<char> = token.chars().collect();
    let mut found = Vec::new();
    for name in FILE_FILTERS {
        let pat: Vec<char> = format!("{name}=").chars().collect();
        let mut i = 0;
        while i + pat.len() <= chars.len() {
            if chars[i..i + pat.len()] != pat[..] {
                i += 1;
                continue;
            }
            let boundary =
                i == 0 || matches!(chars[i - 1], ',' | ';' | '[' | ']' | '\'' | '"');
            if !boundary {
                i += pat.len();
                continue;
            }
            let (value, consumed) = extract_filter_value(&chars[i + pat.len()..])
                .with_context(|| format!("滤镜 {name}= 的文件路径没解析出来"))?;
            // `subtitles=filename=xxx` / `f=xxx` 选项名形
            let path = value
                .strip_prefix("filename=")
                .or_else(|| value.strip_prefix("f="))
                .unwrap_or(&value)
                .to_string();
            anyhow::ensure!(!path.is_empty(), "滤镜 {name}= 里的文件路径是空的");
            found.push(path);
            i += pat.len() + consumed;
        }
    }
    Ok(found)
}

/// 滤镜值提取:引号形(`'...'` 内全字面)或裸形(到未转义的 `:` / `,` 为止,`\x` 解转义)。
/// 返回 (值, 消费的字符数)。
fn extract_filter_value(rest: &[char]) -> anyhow::Result<(String, usize)> {
    if rest.first() == Some(&'\'') {
        let mut out = String::new();
        let mut i = 1;
        while i < rest.len() {
            if rest[i] == '\'' {
                return Ok((out, i + 1));
            }
            out.push(rest[i]);
            i += 1;
        }
        anyhow::bail!("引号没闭合");
    }
    let mut out = String::new();
    let mut i = 0;
    while i < rest.len() {
        match rest[i] {
            '\\' if i + 1 < rest.len() => {
                out.push(rest[i + 1]);
                i += 2;
            }
            // `:` = 下一个选项;`,`/`;` = 链上下一滤镜;`[` = 输出标签起点
            ':' | ',' | ';' | '[' | ']' => break,
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    // Windows 盘符没转义的坑:`subtitles=C:\x.srt` 会在 C 后被冒号截断 —— 给明白话
    if out.len() == 1
        && out.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && rest.get(i) == Some(&':')
    {
        anyhow::bail!("Windows 路径的盘符冒号要转义(C\\:)或整段用单引号包住");
    }
    Ok((out, i))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    /// 建一个真实存在的临时输入文件(scan 会查 is_file)。
    fn temp_input(name: &str) -> (PathBuf, tempfile_dir::Dir) {
        let dir = tempfile_dir::Dir::new("lw-ffrun");
        let p = dir.path().join(name);
        std::fs::write(&p, b"x").unwrap();
        (p, dir)
    }

    /// 极简临时目录守卫(项目无 tempfile 依赖;进程号 + 序号防并发互删)。
    mod tempfile_dir {
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicU64, Ordering};
        pub struct Dir(PathBuf);
        impl Dir {
            pub fn new(tag: &str) -> Dir {
                static SEQ: AtomicU64 = AtomicU64::new(0);
                let p = std::env::temp_dir().join(format!(
                    "{tag}-{}-{}",
                    std::process::id(),
                    SEQ.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::create_dir_all(&p).unwrap();
                Dir(p)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn scan_happy_path_extracts_inputs_and_placeholder() {
        let (input, _d) = temp_input("a.mp4");
        let ip = input.to_string_lossy().into_owned();
        let scan = scan_args(&s(&[
            "-ss", "30", "-to", "00:01:00", "-i", &ip, "-c:v", "h264", "-c:a", "copy",
        ]))
        .unwrap();
        assert_eq!(scan.inputs, vec![input]);
        assert!(scan.wants_h264);
        assert!(scan.filter_reads.is_empty());
        assert_eq!(scan.args.len(), 10, "token 原样保序: {:?}", scan.args);

        // 显式编码器不算占位
        let scan2 = scan_args(&s(&["-i", &ip, "-c:v", "libx264"])).unwrap();
        assert!(!scan2.wants_h264);
    }

    #[test]
    fn scan_rejects_dangerous_shapes() {
        let (input, _d) = temp_input("a.mp4");
        let ip = input.to_string_lossy().into_owned();
        let cases: &[(&[&str], &str)] = &[
            (&["-i", "https://example.com/a.mp4"], "网址"),
            (&["-i", &ip, "-y"], "-y"),
            (&["-i", &ip, "-f", "concat"], "concat"),
            (&["-i", &ip, "-f", "lavfi"], "lavfi"),
            (&["-i", &ip, "-progress", "p.txt"], "-progress"),
            (&["-i", &ip, "-dump_attachment:t:0", "x.ttf"], "dump_attachment"),
            (&["-i", &ip, "-c", "copy", "out.mp4"], "孤零零"),
            (&["-i"], "-i 后面缺"),
            (&["-c", "copy"], "至少要有一个 -i"),
            (&["-i", "/不存在/xx.mp4"], "不存在"),
            (&["-i", "相对路径.mp4"], "绝对路径"),
            (&["-i", &ip, "-map", "pipe:1"], "管道"),
        ];
        for (args, expect) in cases {
            let err = scan_args(&s(args)).unwrap_err().to_string();
            assert!(err.contains(expect), "args={args:?} 应含「{expect}」,得到: {err}");
        }
        // 负号形的 -map 排除值是合法 flag 形,不误杀
        scan_args(&s(&["-i", &ip, "-map", "0", "-map", "-0:s", "-c", "copy"])).unwrap();
    }

    #[test]
    fn scan_gates_filter_embedded_files() {
        let (input, d) = temp_input("a.mp4");
        let ip = input.to_string_lossy().into_owned();
        let srt = d.path().join("字 幕.srt");
        std::fs::write(&srt, b"1").unwrap();
        let sp = srt.to_string_lossy().into_owned();

        // 裸形
        let scan = scan_args(&s(&["-i", &ip, "-vf", &format!("subtitles={sp}")])).unwrap();
        assert_eq!(scan.filter_reads, vec![sp.clone()]);
        // 引号形(带空格路径)
        let scan =
            scan_args(&s(&["-i", &ip, "-vf", &format!("subtitles='{sp}':force_style=x")]))
                .unwrap();
        assert_eq!(scan.filter_reads, vec![sp.clone()]);
        // filename= 选项形 + 链上多滤镜
        let scan = scan_args(&s(&[
            "-i", &ip, "-vf", &format!("scale=640:-2,subtitles=filename={sp}"),
        ]))
        .unwrap();
        assert_eq!(scan.filter_reads, vec![sp.clone()]);
        // 滤镜文件不存在 = 拒
        let err = scan_args(&s(&["-i", &ip, "-vf", "subtitles=/不存在/x.srt"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("不存在"), "{err}");
    }

    #[test]
    fn filter_path_finder_boundaries_and_escapes() {
        // 词边界:bypass=1 / x264opts 里的 ass= 不误配
        assert!(find_filter_paths("x264opts=bypass=1").unwrap().is_empty());
        assert!(find_filter_paths("volume=2").unwrap().is_empty());
        // 转义冒号(Windows 盘符转义形)
        assert_eq!(
            find_filter_paths(r"subtitles=C\:/sub/x.srt").unwrap(),
            vec!["C:/sub/x.srt"]
        );
        // 未转义盘符 = 给明白话
        let err = find_filter_paths(r"subtitles=C:\sub\x.srt").unwrap_err();
        assert!(format!("{err:#}").contains("盘符"), "{err:#}");
        // amovie 与 movie 不串味;标签前缀边界认得出
        assert_eq!(find_filter_paths("amovie=/m/a.mp3,volume=1").unwrap(), vec!["/m/a.mp3"]);
        assert_eq!(find_filter_paths("[0:v]movie=/m/b.mp4[ov]").unwrap(), vec!["/m/b.mp4"]);
        // 引号没闭合 = 拒
        assert!(find_filter_paths("subtitles='/a/b.srt").is_err());
    }
}
