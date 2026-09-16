//! 拖进主窗的文件怎么处理(2026-09-16 ★ 用户拍板):**纯函数**只看路径、扩展名与目录内容,不碰播放。
//!
//! 规则:视频 / 音频文件 → 直接播第一个;音频文件夹 → 当歌单(与 `media_play` 目录入参同口径);
//! 视频文件夹 → 按自然排序第一集起播(剧集队列由 `local_episodes` 自然成立);图片 / 文档 → 当聊天附件;
//! **有媒体也有别的 = 全当附件,不猜**;文件夹当不了附件,混拖时剔掉。
//! 这是程序放 / 读用户**亲手拖进来**的文件,不是模型的手脚 → 不经工具层、不过授权圈、不入表(§7.2);
//! 拖入不进聊天流(像按了个钮),〔此刻〕自然带「在播 X」。

use std::path::Path;

use super::probe::{is_audio_ext, is_video_ext};
use super::queue::{audio_folder_files, natural_cmp};

/// 分流结果(过桥给壳层命令;`Play` 由壳层真去 `play()`,成了回 `DropOutcome::Played`)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropPlan {
    /// 播这个:文件绝对路径,或音频文件夹路径;`audio_only` = 只出声。
    Play { url: String, audio_only: bool },
    /// 全部当聊天附件(只含文件)。
    Attach { paths: Vec<String> },
    /// 什么都没拖进来 / 只拖了空文件夹。
    Nothing,
}

/// 壳层命令 `drop_paths` 的返回(前端据此决定:什么都不用做 / 把路径挂进附件小票)。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DropOutcome {
    Played { title: String },
    Attach { paths: Vec<String> },
    Nothing,
}

enum MediaHit {
    AudioFile,
    VideoFile,
    AudioDir,
    /// 视频文件夹:带上自然排序后的第一集(队列由播放链自己发现)。
    VideoDir(String),
}

/// 一条路径是不是媒体(文件按扩展名;文件夹看里面:有音频当歌单,没音频有视频当剧集)。
fn media_kind(p: &Path) -> Option<MediaHit> {
    if p.is_dir() {
        if !audio_folder_files(p).is_empty() {
            return Some(MediaHit::AudioDir);
        }
        let mut vids: Vec<String> = std::fs::read_dir(p)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .filter(|f| f.is_file() && is_video_ext(f))
            .map(|f| f.to_string_lossy().into_owned())
            .collect();
        if vids.is_empty() {
            return None;
        }
        vids.sort_by(|a, b| natural_cmp(a, b));
        return Some(MediaHit::VideoDir(vids.swap_remove(0)));
    }
    if !p.is_file() {
        return None;
    }
    if is_audio_ext(p) {
        Some(MediaHit::AudioFile)
    } else if is_video_ext(p) {
        Some(MediaHit::VideoFile)
    } else {
        None
    }
}

pub fn plan_drop(paths: &[String]) -> DropPlan {
    let paths: Vec<&String> = paths.iter().filter(|p| !p.trim().is_empty()).collect();
    if paths.is_empty() {
        return DropPlan::Nothing;
    }
    let hits: Vec<Option<MediaHit>> = paths.iter().map(|p| media_kind(Path::new(p))).collect();
    if hits.iter().any(Option::is_none) {
        // 混着别的东西(图 / 文档 / 空文件夹):全当附件,不猜;文件夹当不了附件,剔掉
        let files: Vec<String> =
            paths.iter().filter(|p| Path::new(p).is_file()).map(|p| (*p).clone()).collect();
        return if files.is_empty() { DropPlan::Nothing } else { DropPlan::Attach { paths: files } };
    }
    match hits.into_iter().next().flatten() {
        Some(MediaHit::AudioFile) | Some(MediaHit::AudioDir) => {
            DropPlan::Play { url: paths[0].clone(), audio_only: true }
        }
        Some(MediaHit::VideoFile) => DropPlan::Play { url: paths[0].clone(), audio_only: false },
        Some(MediaHit::VideoDir(first)) => DropPlan::Play { url: first, audio_only: false },
        None => DropPlan::Nothing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("lw-drop-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    fn touch(dir: &Path, name: &str) -> String {
        let p = dir.join(name);
        std::fs::write(&p, b"x").unwrap();
        p.to_string_lossy().into_owned()
    }

    /// 视频 / 音频文件直接播;图片 / 文档当附件;混拖全当附件;空 = Nothing。
    #[test]
    fn files_route_by_extension() {
        let d = scratch("files");
        let mp4 = touch(&d, "片.mp4");
        let mp3 = touch(&d, "歌.mp3");
        let jpg = touch(&d, "图.jpg");
        let pdf = touch(&d, "单.pdf");
        assert_eq!(plan_drop(std::slice::from_ref(&mp4)), DropPlan::Play { url: mp4.clone(), audio_only: false });
        assert_eq!(plan_drop(&[mp3.clone(), mp4.clone()]), DropPlan::Play { url: mp3.clone(), audio_only: true }, "多个媒体播第一个");
        assert_eq!(plan_drop(&[jpg.clone(), pdf.clone()]), DropPlan::Attach { paths: vec![jpg.clone(), pdf.clone()] });
        assert_eq!(plan_drop(&[mp4.clone(), jpg.clone()]), DropPlan::Attach { paths: vec![mp4, jpg] }, "混拖不猜,全当附件");
        assert_eq!(plan_drop(&[]), DropPlan::Nothing);
        assert_eq!(plan_drop(&["  ".into()]), DropPlan::Nothing);
    }

    /// 文件夹:有音频当歌单(整夹路径交 play,与 media_play 目录入参同口径);只有视频按自然排序第一集起;
    /// 空文件夹不是媒体 → 混拖时剔掉、单拖 = Nothing。
    #[test]
    fn folders_become_playlist_or_first_episode() {
        let songs = scratch("songs");
        touch(&songs, "b.mp3");
        touch(&songs, "a.mp3");
        let songs_s = songs.to_string_lossy().into_owned();
        assert_eq!(plan_drop(std::slice::from_ref(&songs_s)), DropPlan::Play { url: songs_s, audio_only: true });

        let show = scratch("show");
        let e10 = touch(&show, "剧 第10集.mkv");
        let e2 = touch(&show, "剧 第2集.mkv");
        touch(&show, "海报.jpg");
        let _ = e10;
        assert_eq!(
            plan_drop(&[show.to_string_lossy().into_owned()]),
            DropPlan::Play { url: e2, audio_only: false },
            "自然排序:第 2 集在第 10 集前;夹里的图不算"
        );

        let empty = scratch("empty");
        assert_eq!(plan_drop(&[empty.to_string_lossy().into_owned()]), DropPlan::Nothing);
        let jpg = touch(&show, "另一张.jpg");
        assert_eq!(
            plan_drop(&[empty.to_string_lossy().into_owned(), jpg.clone()]),
            DropPlan::Attach { paths: vec![jpg] },
            "空文件夹当不了附件,剔掉"
        );
    }
}
