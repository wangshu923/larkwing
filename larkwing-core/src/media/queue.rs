//! 成队规则(纯函数,不碰运行时):同文件夹怎么算一部剧、怎么自然排序、随机怎么走。
//!
//! 视频要过**数字骨架**分组(防平铺电影库误触),音频刻意不套 —— 同文件夹全部音频
//! 就是一个歌单。`series_key` 一律单向哈希,绝不落绝对路径。

use super::*;

/// 从一个本地文件推断出它所属的「剧集」队列:同文件夹、**同类**(视频/音频)、**数字骨架相同**
/// 的兄弟文件,自然排序。返回 `(series_key, 有序集列表)`;**不成系列**(平铺电影库 / 一文件夹一部 /
/// 未知类型)→ None,退回单集播放(现行为)。
///
/// 数字骨架分组防误判:把文件名里的数字段抹成 `#` 当骨架 —— `小猪佩奇E01/E02` 同骨架 `小猪佩奇E#`
/// = 一季;`肖申克的救赎 / 阿甘正传` 骨架各异 = 各自单独不续播。`series_key` = `local:FNV(小写父目录+骨架)`
/// **单向哈希,绝不落绝对路径**(§6.2);整棵目录搬走仍对得上(同相对结构 → 同 key)。
pub(super) fn local_episodes(current: &std::path::Path) -> Option<Series> {
    // 目录入参(音频文件夹,`play()` 已归一):整夹音频即队列,起点 = 排序后第一首
    //(build_queue 按 url 匹配不到目录 → requested=0 = 自然起点,续播规则照常适用)。
    if current.is_dir() {
        return audio_folder_queue(current);
    }
    let parent = current.parent()?;
    let cur_name = current.file_name()?.to_str()?;
    // 当前文件的桶:队列只收同桶文件(放视频不混进音频,反之亦然)。
    let want_video = probe::is_video_ext(current);
    if !want_video && !probe::is_audio_ext(current) {
        return None; // 未知类型,不组队列
    }
    // 音频:同文件夹全部音频 = 一个队列(歌单/专辑/故事集心智,连播是常识)。下面的数字骨架
    // 闸只适合视频(防平铺电影库误触)——歌名天然各异,套用它 = 永远单曲放完就停(2026-07-21 修)。
    if !want_video {
        return audio_folder_queue(parent);
    }

    let cur_skel = digit_skeleton(file_stem_str(cur_name));
    let mut group: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(parent).ok()? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if !path.is_file() || !probe::is_video_ext(&path) {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
            if digit_skeleton(file_stem_str(name)) == cur_skel {
                group.push(name.to_string());
            }
        }
    }
    if group.len() < 2 {
        return None; // 不成系列 → 单集
    }
    group.sort_by(|a, b| natural_cmp(a, b));
    let key_material = format!("{}\u{1f}{}", parent.to_string_lossy().to_lowercase(), cur_skel);
    Some(Series {
        key: format!("local:{}", fnv1a_hex(&key_material)),
        title: folder_title(parent),
        entries: folder_entries(parent, &group),
    })
}

/// 本地剧集的剧名 = 所在文件夹名(家里的剧集夹通常就叫剧名;盘根 / 拿不到 → None)。
fn folder_title(dir: &std::path::Path) -> Option<String> {
    dir.file_name().map(|s| s.to_string_lossy().into_owned()).filter(|s| !s.is_empty())
}

/// 单部内容(电影 / 单个网页视频)的续播 key:本地按小写全路径哈希(**不落绝对路径**,§6.2)、
/// 网络按页面地址哈希。与剧集 key 前缀不同,不串。
pub(super) fn single_key(page_url: &str) -> String {
    if is_local_path(page_url) {
        format!("local:file:{}", fnv1a_hex(&page_url.to_lowercase()))
    } else {
        format!("web:{}", fnv1a_hex(page_url))
    }
}

/// 单部内容在进度表里的「集身份」:本地 = 文件名(相对名,不带目录),网络 = 页面地址。
pub(super) fn single_episode_id(page_url: &str) -> String {
    if is_local_path(page_url) {
        std::path::Path::new(page_url)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| page_url.to_string())
    } else {
        page_url.to_string()
    }
}

/// 当前队列的 series_key(track_progress 用;锁由调用方持有)。
pub(super) fn pl_key(guard: &std::sync::MutexGuard<'_, Option<Playlist>>) -> String {
    guard.as_ref().map(|pl| pl.series_key.clone()).unwrap_or_default()
}

/// 目录判定:本地路径且真是目录(非本地/不存在都算否;fs 探一次,亚毫秒)。
pub(super) fn is_dir_path(s: &str) -> bool {
    is_local_path(s) && std::path::Path::new(s).is_dir()
}

/// 随机播放的下一首:从「这轮没放过的」里挑,命中即记进履历。都放过时,
/// wrap = true(列表循环 / 用户点名「下一首」)→ 重开一轮(排除当前,免紧挨着重复;
/// 队列只剩一首时退化为重放),否则 None(这一轮放完了)。seed 注入可测(§4.11 免不了随机,
/// 但挑选逻辑本身是纯函数)。
pub(super) fn shuffle_advance(pl: &mut Playlist, wrap: bool, seed: u64) -> Option<usize> {
    let total = pl.entries.len();
    let mut candidates: Vec<usize> = (0..total).filter(|i| !pl.played.contains(i)).collect();
    if candidates.is_empty() {
        if !wrap {
            return None;
        }
        pl.played = vec![pl.index]; // 重开一轮:当前这首不马上重复
        candidates = (0..total).filter(|&i| i != pl.index).collect();
        if candidates.is_empty() {
            candidates.push(pl.index); // 队列只有一首:只能重放它
        }
    }
    let pick = candidates[(xorshift64(seed) as usize) % candidates.len()];
    pl.played.push(pick);
    Some(pick)
}

/// 随机播放的「上一首」:弹掉当前、回到履历上一首。没有更早的 = None(如实报,不瞎跳)。
pub(super) fn shuffle_back(pl: &mut Playlist) -> Option<usize> {
    if pl.played.len() < 2 {
        return None;
    }
    pl.played.pop();
    pl.played.last().copied()
}

/// 挑歌用的种子(时间戳;无需密码学质量)。
pub(super) fn shuffle_seed() -> u64 {
    crate::store::now_ms() as u64
}

/// 一步 xorshift 伪随机(0 种子防呆)。
fn xorshift64(seed: u64) -> u64 {
    let mut s = seed | 1;
    s ^= s << 13;
    s ^= s >> 7;
    s ^= s << 17;
    s
}

/// 文件夹里全部音频文件名(natural sort;子目录/非音频跳过)。读不了 = 空。
pub(super) fn audio_folder_files(dir: &std::path::Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut group: Vec<String> = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_file() || !probe::is_audio_ext(&path) {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
            group.push(name.to_string());
        }
    }
    group.sort_by(|a, b| natural_cmp(a, b));
    group
}

/// 音频整夹队列:<2 首不成队列(单曲不出集数 UI)。series_key 只认「哪个文件夹」
/// (目录 + audio 桶标记)——从任一首进、或直接给文件夹,都是同一个 key → 续播记录共享。
/// (原音频骨架 key 的老续播记录〔有声书章节类〕一次性失联,之后照常;拍板可接受。)
fn audio_folder_queue(dir: &std::path::Path) -> Option<Series> {
    let group = audio_folder_files(dir);
    if group.len() < 2 {
        return None;
    }
    let key_material = format!("{}\u{1f}audio", dir.to_string_lossy().to_lowercase());
    Some(Series {
        key: format!("local:{}", fnv1a_hex(&key_material)),
        title: folder_title(dir),
        entries: folder_entries(dir, &group),
    })
}

/// 文件名列表 → 队列条目(id = 相对文件名〔续播记忆存它,绝不落绝对路径 §6.2〕,url = 绝对路径)。
fn folder_entries(dir: &std::path::Path, names: &[String]) -> Vec<EpisodeRef> {
    names
        .iter()
        .map(|name| EpisodeRef {
            id: name.clone(),
            url: dir.join(name).to_string_lossy().into_owned(),
            title: file_stem_str(name).to_string(),
        })
        .collect()
}

/// 取文件名(无目录)的主名部分(去最后一段扩展名);无扩展名则原样。
fn file_stem_str(name: &str) -> &str {
    std::path::Path::new(name).file_stem().and_then(|s| s.to_str()).unwrap_or(name)
}

/// 把文件名里每一段连续 ASCII 数字抹成一个 `#`,得到「骨架」(分组用)。
fn digit_skeleton(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_digit = false;
    for c in s.chars() {
        if c.is_ascii_digit() {
            if !prev_digit {
                out.push('#');
            }
            prev_digit = true;
        } else {
            out.push(c);
            prev_digit = false;
        }
    }
    out
}

/// 自然排序:把数字段当数字比(`E2 < E10`、`第2集 < 第10集`),其余按字符比。
/// 同骨架文件的数字/非数字段天然对齐,逐段比即可;大小写仅作末位 tiebreak(稳定)。
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (mut x, mut y) = (a, b);
    loop {
        match (x.is_empty(), y.is_empty()) {
            (true, true) => return Ordering::Equal,
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            _ => {}
        }
        let x_digit = x.as_bytes()[0].is_ascii_digit();
        let y_digit = y.as_bytes()[0].is_ascii_digit();
        if x_digit && y_digit {
            let xe = x.find(|c: char| !c.is_ascii_digit()).unwrap_or(x.len());
            let ye = y.find(|c: char| !c.is_ascii_digit()).unwrap_or(y.len());
            let (xn, xr) = x.split_at(xe);
            let (yn, yr) = y.split_at(ye);
            // 比数值:去前导零后按长度、再按字节(任意长度都对,不靠 parse 免溢出)。
            let (xt, yt) = (xn.trim_start_matches('0'), yn.trim_start_matches('0'));
            match xt.len().cmp(&yt.len()).then_with(|| xt.cmp(yt)) {
                Ordering::Equal => {
                    x = xr;
                    y = yr;
                }
                ord => return ord,
            }
        } else if !x_digit && !y_digit {
            let xe = x.find(|c: char| c.is_ascii_digit()).unwrap_or(x.len());
            let ye = y.find(|c: char| c.is_ascii_digit()).unwrap_or(y.len());
            let (xs, xr) = x.split_at(xe);
            let (ys, yr) = y.split_at(ye);
            match xs.to_lowercase().cmp(&ys.to_lowercase()).then_with(|| xs.cmp(ys)) {
                Ordering::Equal => {
                    x = xr;
                    y = yr;
                }
                ord => return ord,
            }
        } else {
            // 一边数字一边非数字(同骨架不会到此;泛用兜底):按首字节定序,确定即可。
            return x.as_bytes()[0].cmp(&y.as_bytes()[0]);
        }
    }
}

/// FNV-1a 64 位 → 16 位十六进制。**稳定**(跨版本不变,续播记忆 key 依赖它),
/// 且单向 —— 本地 series_key 用它把「父目录+骨架」哈希掉,不在 DB 落绝对路径(§6.2)。
fn fnv1a_hex(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::testkit::*;

    #[test]
    fn local_path_detection() {
        assert!(is_local_path("/Users/me/Movies/a.mp4"));
        assert!(is_local_path("C:\\Movies\\a.mp4"));
        assert!(is_local_path("d:/film/b.mkv"));
        assert!(is_local_path("\\\\nas\\film\\c.mp4"), "UNC 路径");
        assert!(!is_local_path("https://www.bilibili.com/video/BV1"));
        assert!(!is_local_path("//cdn.example/x"), "protocol-relative 是网络");
        assert!(!is_local_path("movies/a.mp4"), "相对路径拒收");
    }

    #[test]
    fn natural_sort_orders_episodes_numerically() {
        let mut v = vec!["第10集.mp4", "第2集.mp4", "第1集.mp4", "第21集.mp4"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, vec!["第1集.mp4", "第2集.mp4", "第10集.mp4", "第21集.mp4"]);
        // E2 < E10(字典序会把 E10 排前面,自然序不会)
        let mut e = vec!["S01E10.mkv", "S01E2.mkv", "S01E1.mkv"];
        e.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(e, vec!["S01E1.mkv", "S01E2.mkv", "S01E10.mkv"]);
        // 前导零等价:E01 == E1(数值相等),不影响相对顺序的稳定
        assert_eq!(natural_cmp("E01", "E1"), std::cmp::Ordering::Equal);
    }

    #[test]
    fn skeleton_groups_series_and_separates_movies() {
        assert_eq!(digit_skeleton("小猪佩奇E01"), "小猪佩奇E#");
        assert_eq!(digit_skeleton("小猪佩奇E02"), "小猪佩奇E#");
        assert_eq!(digit_skeleton("S01E02"), "S#E#");
        // 不同剧 / 电影:骨架不同 → 不会被归到一组
        assert_ne!(digit_skeleton("肖申克的救赎"), digit_skeleton("阿甘正传"));
    }

    #[test]
    fn local_episodes_builds_queue_for_a_series() {
        let dir = std::env::temp_dir().join(format!("lw-ep-series-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 一季动画 + 一个字幕(应被过滤)+ 一个无关电影(骨架不同,应被排除)
        touch(&dir, "小猪佩奇 第1集.mp4");
        let e2 = touch(&dir, "小猪佩奇 第2集.mp4");
        touch(&dir, "小猪佩奇 第10集.mp4");
        touch(&dir, "小猪佩奇 第1集.srt"); // 非媒体,过滤
        touch(&dir, "无关电影.mp4"); // 骨架不同,排除

        let Series { key, entries: eps, .. } = local_episodes(&e2).expect("应识别为剧集");
        assert!(key.starts_with("local:"));
        assert_eq!(eps.len(), 3, "三集,排除字幕与无关电影");
        // 自然排序:1 < 2 < 10
        assert_eq!(eps[0].title, "小猪佩奇 第1集");
        assert_eq!(eps[1].title, "小猪佩奇 第2集");
        assert_eq!(eps[2].title, "小猪佩奇 第10集");
        // 集身份 = 相对文件名(不含绝对路径)
        assert_eq!(eps[0].id, "小猪佩奇 第1集.mp4");
        assert!(!eps[0].id.contains('/') && !eps[0].id.contains('\\'), "id 是相对名");
        // url 是可播的绝对路径
        assert!(is_local_path(&eps[1].url));
    }

    #[test]
    fn local_episodes_none_for_flat_movie_folder() {
        let dir = std::env::temp_dir().join(format!("lw-ep-movies-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let m = touch(&dir, "肖申克的救赎.mkv");
        touch(&dir, "阿甘正传.mkv");
        touch(&dir, "教父.mkv");
        // 平铺电影库:骨架各异 → 当前文件所在组只有 1 个 → 不续播
        assert!(local_episodes(&m).is_none(), "平铺电影不该误判成剧集");

        // 一文件夹一部电影也不续播
        let solo = std::env::temp_dir().join(format!("lw-ep-solo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&solo);
        std::fs::create_dir_all(&solo).unwrap();
        let only = touch(&solo, "某电影 (2024) 1080p.mp4");
        assert!(local_episodes(&only).is_none(), "单文件 → 单集");
    }

    #[test]
    fn audio_folder_groups_whole_folder_and_dir_input() {
        let dir = std::env::temp_dir().join(format!("lw-ep-audio-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 歌名各异(骨架各不同)也要整夹组队 —— 这正是音频不吃视频骨架闸的原因
        let a = touch(&dir, "小星星.mp3");
        let b = touch(&dir, "两只老虎.flac");
        touch(&dir, "说明.txt"); // 非媒体,过滤
        touch(&dir, "短片.mp4"); // 视频不混进音频桶

        let Series { key: key_a, entries: eps, .. } = local_episodes(&a).expect("整夹音频应组队");
        assert_eq!(eps.len(), 2, "只收音频");
        assert!(eps.iter().all(|e| e.id.ends_with(".mp3") || e.id.ends_with(".flac")));
        assert!(!eps[0].id.contains('/') && !eps[0].id.contains('\\'), "id 是相对名");
        // 从另一首进、或直接给文件夹:同一个 key + 同一份队列(续播记录共享)
        let key_b = local_episodes(&b).unwrap().key;
        let Series { key: key_dir, entries: eps_dir, .. } = local_episodes(&dir).unwrap();
        assert_eq!(key_a, key_b);
        assert_eq!(key_a, key_dir);
        assert_eq!(eps.len(), eps_dir.len());

        // 只有一首:不成队列(文件与目录入口一致)
        let solo = std::env::temp_dir().join(format!("lw-ep-audio-solo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&solo);
        std::fs::create_dir_all(&solo).unwrap();
        let only = touch(&solo, "独一首.mp3");
        assert!(local_episodes(&only).is_none(), "单曲 → 不出队列");
        assert!(local_episodes(&solo).is_none());
    }

    #[test]
    fn shuffle_advance_picks_unplayed_then_round_ends() {
        let mut pl = mk_shuffle_playlist(4);
        for seed in 1..=3u64 {
            let pick = shuffle_advance(&mut pl, false, seed).expect("这轮还有没放过的");
            assert_eq!(pl.played.iter().filter(|&&i| i == pick).count(), 1, "一轮内不重复");
        }
        assert_eq!(pl.played.len(), 4, "一轮放完");
        // 都放过:不循环 → 收尾;列表循环 → 重开一轮且不紧挨着重复当前
        assert!(shuffle_advance(&mut pl, false, 7).is_none());
        pl.index = 2;
        let again = shuffle_advance(&mut pl, true, 7).expect("循环重开一轮");
        assert_ne!(again, 2, "重开一轮不紧挨着重复当前那首");
        // 队列只有一首:重开只能重放它
        let mut one = mk_shuffle_playlist(1);
        assert_eq!(shuffle_advance(&mut one, true, 3), Some(0));
    }

    #[test]
    fn shuffle_back_walks_history() {
        let mut pl = mk_shuffle_playlist(3);
        pl.played = vec![2, 0, 1]; // 放过 2→0→1,当前 1
        assert_eq!(shuffle_back(&mut pl), Some(0));
        assert_eq!(shuffle_back(&mut pl), Some(2));
        assert_eq!(shuffle_back(&mut pl), None, "没有更早的了");
    }
}
