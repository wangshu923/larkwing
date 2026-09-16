//! relay · 进度条预览与封面:/thumb/(本地抽帧 + B 站雪碧图裁格,两级有界缓存 + 串行闸)、/cover/(内嵌 / 侧车 / 远端三源归一 JPEG)。

use super::*;

/// 把 hover 秒数落到 `THUMB_GRID` 格(向下取整),负数/非有限值一律归 0。
/// 缓存键与 ffmpeg 的 `-ss` 都用它的结果 —— 前端也量化过,这里再落一次是为了
/// **键规范化**:不管客户端送来什么,同一格永远命中同一张。
fn quantize_thumb_t(t: f64) -> u64 {
    if !t.is_finite() || t <= 0.0 {
        return 0;
    }
    ((t / THUMB_GRID).floor() as u64).saturating_mul(THUMB_GRID as u64)
}

#[derive(serde::Deserialize, Default)]
pub(super) struct ThumbQuery {
    /// 要看第几秒(hover 位置换算出来的;服务端还会落格,见 `quantize_thumb_t`)。
    /// `Option` = 没带 `?t=` 时自己回 400;带了但不是数字由 axum 的 Query 拒收(也是 400)。
    #[serde(default)]
    t: Option<f64>,
}

/// 抽一帧当缩略图:输入 seek(`-ss` 在 `-i` 前 = 直落最近关键帧,不解码前面的内容)+ 一帧 JPEG。
/// **`-map 0:V:0?` 的大写 V 是要紧的**:小写 v 会把 mkv/mp4 里的**封面图轨**(attached pic)算成
/// 视频流,ffmpeg 的默认挑轨也一样 —— 挑中封面的话整条进度条 hover 出来全是同一张海报。
/// 尾巴上的 `?` = 这条流不存在就别报错(纯音频视频文件、怪容器)。
fn build_thumb_cmd(ffmpeg: &Path, path: &Path, t: u64) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.arg("-hide_banner").arg("-loglevel").arg("error").arg("-nostdin");
    if t > 0 {
        cmd.arg("-ss").arg(t.to_string());
    }
    cmd.arg("-i")
        .arg(path)
        .arg("-map")
        .arg("0:V:0?")
        .arg("-an")
        .arg("-sn")
        .arg("-dn")
        .arg("-frames:v")
        .arg("1")
        // -2 让高度对齐偶数(编码器友好,JPEG 无所谓但一致);宽度定死 → 前端图框尺寸恒定。
        .arg("-vf")
        .arg(format!("scale={THUMB_WIDTH}:-2"))
        .arg("-f")
        .arg("mjpeg") // 单帧 mjpeg = 一张普普通通的 JPEG
        .arg("pipe:1");
    cmd
}

/// 进度条 hover 预览:`/thumb/{token}?t=秒`。定格 → 查缓存 → (串行闸)→ 出图 → 收进缓存。
/// 两种预览源走同一条流水线,只差「定格」与「出图」两步:本地文件 = 量化到 THUMB_GRID + ffmpeg
/// 抽一帧;雪碧图 = 在采样表里定帧 + 下载大图裁一格。出不来一律 404,让前端一次性降级成
/// "只有时间气泡"(§3.5:不给半张破图,也不装作有图)。
pub(super) async fn thumb(
    State(state): State<Arc<Inner>>,
    AxPath(token): AxPath<String>,
    Query(q): Query<ThumbQuery>,
) -> Response {
    // `?t=` 缺失/负数/非有限 → 400(是调用方写错了,不是"这片没有缩略图")
    let Some(t) = q.t.filter(|t| t.is_finite() && *t >= 0.0) else {
        return bad(StatusCode::BAD_REQUEST);
    };
    let Some(entry) = lookup(&state, &token) else { return bad(StatusCode::NOT_FOUND) };
    // 缓存键第二维:本地路 = 落格后的秒;雪碧图路 = 帧号(同帧永远同一张图)。
    let at = match entry.as_ref() {
        Entry::Thumb { .. } => quantize_thumb_t(t),
        Entry::Sprites { sheet } => match sprite_frame(&sheet.index, t) {
            Some(frame) => frame as u64,
            None => return bad(StatusCode::NOT_FOUND), // 空采样表 = 这片没预览图
        },
        _ => return bad(StatusCode::NOT_FOUND), // 播放条目蹭不了这个端点
    };
    let key = (token, at);

    if let Some(bytes) = state.thumbs.lk().get(&key) {
        return thumb_response(bytes);
    }
    // 串行:排队期间前面那位可能正好抽的就是这一格 → 拿到许可先再查一次缓存
    let Ok(_permit) = state.thumb_gate.acquire().await else {
        return bad(StatusCode::SERVICE_UNAVAILABLE); // 闸被关(进程收尾),不该发生
    };
    if let Some(bytes) = state.thumbs.lk().get(&key) {
        return thumb_response(bytes);
    }

    let out = match entry.as_ref() {
        Entry::Thumb { path, ffmpeg } => ffmpeg_thumb(ffmpeg, path, at).await,
        Entry::Sprites { sheet } => sprite_thumb(&state, sheet, at as usize).await,
        _ => None,
    };
    // 抽不出(片尾之后的格 / 只有音轨 / 参数不合 / 大图下不来)与超时都走这:一律 404 降级
    let Some(out) = out.filter(|b| !b.is_empty()) else { return bad(StatusCode::NOT_FOUND) };
    let bytes = Arc::new(out);
    state.thumbs.lk().put(key, bytes.clone());
    thumb_response(bytes)
}

/// 本地路出图:ffmpeg 抽 `at` 秒那一帧(见 `build_thumb_cmd`),`THUMB_TIMEOUT` 兜住弱机解不动。
async fn ffmpeg_thumb(ffmpeg: &Path, path: &Path, at: u64) -> Option<Vec<u8>> {
    let cmd = build_thumb_cmd(ffmpeg, path, at);
    match tokio::time::timeout(THUMB_TIMEOUT, run_ffmpeg_collect(cmd, THUMB_MAX_BYTES)).await {
        Ok(out) => out,
        Err(_) => {
            tracing::warn!(path = %path.display(), at, "抽缩略图超时,放弃这一格");
            None
        }
    }
}

/// 雪碧图定帧:`index[k]` = 第 k 帧的采样秒(升序),取**采样时刻 ≤ t 的最后一帧**;t 在首帧之前
/// (或负数 / 非有限)取第 0 帧,超过末帧取末帧(片尾 hover 也有图,不像本地路那样 404);
/// 空表 → None(定不了格)。纯函数、可测。
fn sprite_frame(index: &[f64], t: f64) -> Option<usize> {
    if index.is_empty() {
        return None;
    }
    let t = if t.is_finite() { t.max(0.0) } else { 0.0 };
    // 升序表:`<= t` 的元素个数 - 1 = 最后一个 ≤ t 的下标;一个都不 ≤ → 0
    let n = index.partition_point(|&x| x <= t);
    Some(n.saturating_sub(1).min(index.len() - 1))
}

/// 帧号 → (第几张大图, 列, 行):大图按行优先装 `x_len × y_len` 格,帧号跨图连续。
/// 帧号超出大图数(采样表比图多)→ None。纯函数、可测。
fn sprite_cell(sheet: &SpriteSheet, frame: usize) -> Option<(usize, u32, u32)> {
    let per = (sheet.x_len as usize).checked_mul(sheet.y_len as usize)?;
    if per == 0 {
        return None;
    }
    let img = frame / per;
    if img >= sheet.images.len() {
        return None;
    }
    let pos = (frame % per) as u32;
    Some((img, pos % sheet.x_len, pos / sheet.x_len))
}

/// 雪碧图路出图:定位到大图与格 → 大图(缓存 / 下载)→ 裁格 + 缩放 + JPEG(CPU 活挪到阻塞线程,
/// HD 大图解码是百毫秒级、别占 runtime 线程)。整段套 `THUMB_TIMEOUT`;任何一步不顺 → None。
async fn sprite_thumb(state: &Inner, sheet: &Arc<SpriteSheet>, frame: usize) -> Option<Vec<u8>> {
    let (img, col, row) = sprite_cell(sheet, frame)?;
    let url = sheet.images.get(img)?.clone();
    let work = async {
        let bytes = fetch_sprite_image(state, sheet, &url).await?;
        let sheet = sheet.clone();
        tokio::task::spawn_blocking(move || crop_sprite_cell(&bytes, &sheet, col, row))
            .await
            .ok()
            .flatten()
    };
    match tokio::time::timeout(THUMB_TIMEOUT, work).await {
        Ok(out) => out,
        Err(_) => {
            tracing::warn!(url = %url, frame, "雪碧图取图超时,放弃这一格");
            None
        }
    }
}

/// 取一张雪碧图大图的编码字节:先查整图缓存,没有则带防盗链头下载(`fetch_image_bytes`),成功即入缓存。
async fn fetch_sprite_image(state: &Inner, sheet: &SpriteSheet, url: &str) -> Option<Arc<Vec<u8>>> {
    if let Some(bytes) = state.sprites.lk().get(&url.to_string()) {
        return Some(bytes);
    }
    let buf = fetch_image_bytes(state, url, &sheet.headers, SPRITE_IMG_MAX_BYTES).await?;
    let bytes = Arc::new(buf);
    state.sprites.lk().put(url.to_string(), bytes.clone());
    Some(bytes)
}

/// 带防盗链头下载一张图的字节(走 net::Client,§4.6),边读边封顶 `cap`(超了就弃,绝不把不明大小的
/// 东西拉进内存)。雪碧图大图与远端封面共用;非 2xx / 空体 / 断流一律 None。
async fn fetch_image_bytes(
    state: &Inner,
    url: &str,
    headers: &[(String, String)],
    cap: usize,
) -> Option<Vec<u8>> {
    let mut resp = state
        .net
        .send(url, |c| {
            let mut req = c.get(url);
            for (k, v) in headers {
                req = req.header(k, v);
            }
            req
        })
        .await
        .ok()?;
    if !resp.status().is_success() {
        tracing::info!(url = %url, status = resp.status().as_u16(), "图片取不到");
        return None;
    }
    if resp.content_length().is_some_and(|n| n > cap as u64) {
        return None;
    }
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                if buf.len() + chunk.len() > cap {
                    return None;
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(_) => return None,
        }
    }
    if buf.is_empty() {
        return None;
    }
    Some(buf)
}

/// 封面:`/cover/{token}`。查缓存 →(与缩略图同一把串行闸,别并发起 ffmpeg)→ 按来源取原图 →
/// 归一成 ≤ COVER_MAX_EDGE 的 JPEG → 收进缓存。取不出一律 404,前端回落 ♪ 占位(§3.5 不给破图)。
pub(super) async fn cover(State(state): State<Arc<Inner>>, AxPath(token): AxPath<String>) -> Response {
    let Some(entry) = lookup(&state, &token) else { return bad(StatusCode::NOT_FOUND) };
    let Entry::Cover(src) = entry.as_ref() else { return bad(StatusCode::NOT_FOUND) };
    let key = cover_key(src);
    if let Some(bytes) = state.covers.lk().get(&key) {
        return thumb_response(bytes);
    }
    let Ok(_permit) = state.thumb_gate.acquire().await else {
        return bad(StatusCode::SERVICE_UNAVAILABLE);
    };
    if let Some(bytes) = state.covers.lk().get(&key) {
        return thumb_response(bytes);
    }
    let raw: Option<Vec<u8>> = match src {
        CoverSrc::Embedded { path, ffmpeg } => {
            let cmd = build_cover_cmd(ffmpeg, path);
            match tokio::time::timeout(THUMB_TIMEOUT, run_ffmpeg_collect(cmd, COVER_SRC_MAX_BYTES)).await {
                Ok(out) => out,
                Err(_) => {
                    tracing::warn!(path = %path.display(), "抽内嵌封面超时,放弃");
                    None
                }
            }
        }
        CoverSrc::Sidecar(path) => {
            let p = path.clone();
            tokio::task::spawn_blocking(move || read_capped(&p, COVER_SRC_MAX_BYTES)).await.ok().flatten()
        }
        CoverSrc::Remote { url, headers } => {
            // 与内嵌 / 雪碧图两路同一把超时:relay 的 net::Client 只设了建连超时(它同时服务整片的
            // /s/ 流,不能加总超时),CDN 建连后不吐字节就会在这儿永挂 —— 而此刻正握着 `thumb_gate`
            // 那把串行闸,后面所有缩略图 / 封面都得排队等它(2026-09-16 体检修)。
            match tokio::time::timeout(
                THUMB_TIMEOUT,
                fetch_image_bytes(&state, url, headers, COVER_SRC_MAX_BYTES),
            )
            .await
            {
                Ok(v) => v,
                Err(_) => {
                    tracing::warn!(url = %url, "取远端封面超时,放弃");
                    None
                }
            }
        }
    };
    let out = match raw {
        Some(bytes) => tokio::task::spawn_blocking(move || {
            cover_jpeg(&bytes, COVER_MAX_EDGE, COVER_JPEG_QUALITY)
        })
        .await
        .ok()
        .flatten(),
        None => None,
    };
    let Some(out) = out.filter(|b| !b.is_empty()) else { return bad(StatusCode::NOT_FOUND) };
    let bytes = Arc::new(out);
    state.covers.lk().put(key, bytes.clone());
    thumb_response(bytes)
}

/// 封面缓存键 = 来源身份:同一个文件 / 同一张远端图,换一次播放(新 token)照样命中。
fn cover_key(src: &CoverSrc) -> String {
    match src {
        CoverSrc::Embedded { path, .. } => format!("embedded:{}", path.display()),
        CoverSrc::Sidecar(path) => format!("sidecar:{}", path.display()),
        CoverSrc::Remote { url, .. } => format!("remote:{url}"),
    }
}

/// 读一个小文件到内存,超过 `cap` 的不读(侧车图理应几百 KB;巨图不是封面)。
fn read_capped(path: &Path, cap: usize) -> Option<Vec<u8>> {
    let len = std::fs::metadata(path).ok()?.len();
    if len == 0 || len > cap as u64 {
        return None;
    }
    std::fs::read(path).ok()
}

/// 抽音频文件的内嵌封面:attached_pic 是一条单帧 Video 流(mjpeg / png),`-c copy` 原字节吐到 stdout。
/// **这里用小写 `0:v:0`**(与缩略图的大写 `V` 相反):大写 V 会把封面轨排除掉,而这次要的正是它。
/// 文件没封面流 → ffmpeg 报错无输出 → 调用方按 404 处理。
fn build_cover_cmd(ffmpeg: &Path, path: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-nostdin")
        .arg("-i")
        .arg(path)
        .arg("-map")
        .arg("0:v:0")
        .arg("-an")
        .arg("-sn")
        .arg("-dn")
        .arg("-frames:v")
        .arg("1")
        .arg("-c:v")
        .arg("copy")
        .arg("-f")
        .arg("image2pipe")
        .arg("pipe:1");
    cmd
}

/// 任意图(jpg / png / webp / bmp)→ JPEG:最长边超 `max_edge` 才等比缩小(不放大、不裁),透明合到黑底
/// (to_rgb8;封面几乎没有透明,不为它多一路 PNG)。解码限额 `COVER_SRC_MAX_EDGE`。纯 CPU、同步、
/// 可测;调用方放阻塞线程。relay 的封面端点(512)与下载嵌封面(2000)共用。
pub(crate) fn cover_jpeg(bytes: &[u8], max_edge: u32, quality: u8) -> Option<Vec<u8>> {
    let mut reader =
        image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(COVER_SRC_MAX_EDGE);
    limits.max_image_height = Some(COVER_SRC_MAX_EDGE);
    reader.limits(limits);
    let img = reader.decode().ok()?;
    if img.width() == 0 || img.height() == 0 {
        return None;
    }
    let img = if img.width().max(img.height()) > max_edge {
        img.thumbnail(max_edge, max_edge)
    } else {
        img
    };
    let rgb = image::DynamicImage::ImageRgb8(img.to_rgb8());
    let mut buf = std::io::Cursor::new(Vec::new());
    let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
    rgb.write_with_encoder(enc).ok()?;
    Some(buf.into_inner())
}

/// 从雪碧图大图字节里裁出第 (col, row) 格、超宽则缩到 THUMB_WIDTH(不放大)、编成 JPEG。
/// 解码上限钉在**声明的整图尺寸**(x_len×tile_w / y_len×tile_h):大图比声明大 = 不是这张表说的
/// 那张图,拒解(顺带免掉照着坏数据解一张巨图);格落在图外(末张大图不满一页却被点到)→ None。
/// 纯 CPU、同步;调用方放阻塞线程。
pub(crate) fn crop_sprite_cell(bytes: &[u8], sheet: &SpriteSheet, col: u32, row: u32) -> Option<Vec<u8>> {
    let sheet_w = sheet.x_len.checked_mul(sheet.tile_w)?;
    let sheet_h = sheet.y_len.checked_mul(sheet.tile_h)?;
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(sheet_w);
    limits.max_image_height = Some(sheet_h);
    reader.limits(limits);
    let img = reader.decode().ok()?;
    let x = col.checked_mul(sheet.tile_w)?;
    let y = row.checked_mul(sheet.tile_h)?;
    if x.checked_add(sheet.tile_w)? > img.width() || y.checked_add(sheet.tile_h)? > img.height() {
        return None;
    }
    let mut cell = img.crop_imm(x, y, sheet.tile_w, sheet.tile_h);
    if sheet.tile_w > THUMB_WIDTH {
        let h = (sheet.tile_h as u64 * THUMB_WIDTH as u64 / sheet.tile_w as u64).max(1) as u32;
        cell = cell.resize_exact(THUMB_WIDTH, h, image::imageops::FilterType::Triangle);
    }
    let rgb = image::DynamicImage::ImageRgb8(cell.to_rgb8());
    let mut buf = std::io::Cursor::new(Vec::new());
    let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, SPRITE_JPEG_QUALITY);
    rgb.write_with_encoder(enc).ok()?;
    Some(buf.into_inner())
}

/// 缩略图响应:URL 已按格量化 + token 每次播放都换 → 可以放心让 WebView 自己缓存,
/// 光标来回扫同一段时连回环请求都省了。
fn thumb_response(bytes: Arc<Vec<u8>>) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "image/jpeg")
        .header("cache-control", "private, max-age=3600")
        .body(Body::from(bytes.as_ref().clone()))
        .unwrap_or_else(|_| bad(StatusCode::INTERNAL_SERVER_ERROR))
}

#[cfg(test)]
mod tests {
    use super::*;

    /* ——— 进度条 hover 缩略图 ——— */

    #[test]
    fn thumb_time_quantizes_to_grid() {
        assert_eq!(quantize_thumb_t(0.0), 0);
        assert_eq!(quantize_thumb_t(4.9), 0);
        assert_eq!(quantize_thumb_t(10.0), 10);
        assert_eq!(quantize_thumb_t(19.999), 10);
        assert_eq!(quantize_thumb_t(3600.4), 3600);
        // 脏输入一律归 0(前端算错/URL 被手改都不该让服务端算出个负数去喂 -ss)
        assert_eq!(quantize_thumb_t(-5.0), 0);
        assert_eq!(quantize_thumb_t(f64::NAN), 0);
        assert_eq!(quantize_thumb_t(f64::INFINITY), 0);
    }

    /// 抽帧命令的两件要紧事:**大写 V 挑真视频轨**(小写 v / 默认挑轨会挑中 mkv/mp4 里的封面图轨
    /// → 整条进度条 hover 出来全是同一张海报),以及 `-ss` 在 `-i` 之前(输入 seek,不解码前面)。
    #[test]
    fn thumb_cmd_picks_real_video_stream_and_seeks_on_input() {
        let args_at = |t: u64| {
            let c = build_thumb_cmd(Path::new("ffmpeg"), Path::new("/tmp/movie.mkv"), t);
            c.as_std().get_args().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<String>>()
        };
        let a = args_at(120);
        let pos = |needle: &str| a.iter().position(|s| s == needle);
        assert!(a.windows(2).any(|w| w[0] == "-map" && w[1] == "0:V:0?"), "必须大写 V:{a:?}");
        assert!(a.windows(2).any(|w| w[0] == "-frames:v" && w[1] == "1"), "只要一帧:{a:?}");
        assert!(a.windows(2).any(|w| w[0] == "-ss" && w[1] == "120"), "按格取整的秒:{a:?}");
        assert!(pos("-ss") < pos("-i"), "-ss 要在 -i 之前(输入 seek):{a:?}");
        assert!(
            a.windows(2).any(|w| w[0] == "-vf" && w[1] == format!("scale={THUMB_WIDTH}:-2")),
            "宽度定死、高度按比例:{a:?}"
        );
        assert!(a.windows(2).any(|w| w[0] == "-f" && w[1] == "mjpeg"), "单帧 mjpeg = 一张 JPEG:{a:?}");
        assert_eq!(a.last().unwrap(), "pipe:1");
        // 第 0 秒不带 -ss(省掉一次无意义的 seek)
        assert!(!args_at(0).iter().any(|s| s == "-ss"));
    }

    #[test]
    fn cover_cmd_takes_attached_pic_stream_as_is() {
        let c = build_cover_cmd(Path::new("ffmpeg"), Path::new("/tmp/song.flac"));
        let a: Vec<String> = c.as_std().get_args().map(|s| s.to_string_lossy().into_owned()).collect();
        // 小写 v:封面轨就是 attached pic,这次要的正是它(缩略图那边用大写 V 排除它)
        assert!(a.windows(2).any(|w| w[0] == "-map" && w[1] == "0:v:0"), "{a:?}");
        assert!(a.windows(2).any(|w| w[0] == "-c:v" && w[1] == "copy"), "原字节不重编:{a:?}");
        assert!(a.windows(2).any(|w| w[0] == "-frames:v" && w[1] == "1"));
        assert!(a.windows(2).any(|w| w[0] == "-f" && w[1] == "image2pipe"));
        assert!(!a.iter().any(|s| s == "-ss"), "封面不按时间抽");
        assert_eq!(a.last().unwrap(), "pipe:1");
    }

    /// 造一张 PNG(纯色即可)的编码字节。
    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([200, 40, 40, 255]));
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img).write_to(&mut buf, image::ImageFormat::Png).unwrap();
        buf.into_inner()
    }

    #[test]
    fn cover_jpeg_fits_long_edge_and_never_upscales() {
        // 大图缩到最长边 512、比例不变;小图原尺寸;坏字节 None;输出是 JPEG(FF D8 开头)
        let big = cover_jpeg(&png_bytes(1200, 800), COVER_MAX_EDGE, COVER_JPEG_QUALITY).unwrap();
        assert!(big.starts_with(&[0xFF, 0xD8]));
        let dims = image::load_from_memory(&big).unwrap();
        assert_eq!((dims.width(), dims.height()), (512, 341));
        let small = cover_jpeg(&png_bytes(300, 300), COVER_MAX_EDGE, COVER_JPEG_QUALITY).unwrap();
        let dims = image::load_from_memory(&small).unwrap();
        assert_eq!((dims.width(), dims.height()), (300, 300), "不放大");
        assert!(cover_jpeg(b"not an image", COVER_MAX_EDGE, COVER_JPEG_QUALITY).is_none());
        // 缓存键按来源身份,不含 token
        let k1 = cover_key(&CoverSrc::Sidecar(PathBuf::from("/a/cover.jpg")));
        let k2 = cover_key(&CoverSrc::Sidecar(PathBuf::from("/a/cover.jpg")));
        let k3 = cover_key(&CoverSrc::Remote { url: "https://x/1.jpg".into(), headers: vec![] });
        assert_eq!(k1, k2);
        assert_ne!(k1, k3);
    }

    #[tokio::test]
    async fn cover_endpoint_serves_sidecar_and_degrades_to_404() {
        let relay = Relay::start().await.unwrap();
        let http = reqwest::Client::new();
        let dir = std::env::temp_dir().join(format!("lw-cover-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let side = dir.join("cover.png");
        std::fs::write(&side, png_bytes(900, 900)).unwrap();

        // 侧车图:200 + JPEG + 已缩到 512;同一来源再注册一个 token 照样命中(键按来源)
        let url = relay.register_cover(CoverSrc::Sidecar(side.clone()));
        let r = http.get(&url).send().await.unwrap();
        assert_eq!(r.status().as_u16(), 200);
        assert_eq!(r.headers().get("content-type").unwrap(), "image/jpeg");
        let bytes = r.bytes().await.unwrap();
        let img = image::load_from_memory(&bytes).unwrap();
        assert_eq!((img.width(), img.height()), (512, 512));
        let key = cover_key(&CoverSrc::Sidecar(side.clone()));
        assert!(relay.inner.covers.lk().get(&key).is_some(), "进了封面缓存");

        // 侧车图不存在 / 内嵌图 ffmpeg 起不来 / 远端拿不到 → 一律 404(前端回落 ♪)
        let gone = relay.register_cover(CoverSrc::Sidecar(dir.join("nope.jpg")));
        assert_eq!(http.get(&gone).send().await.unwrap().status().as_u16(), 404);
        let emb = relay.register_cover(CoverSrc::Embedded {
            path: dir.join("nope.mp3"),
            ffmpeg: PathBuf::from("/nonexistent/ffmpeg"),
        });
        assert_eq!(http.get(&emb).send().await.unwrap().status().as_u16(), 404);
        let remote = relay.register_cover(CoverSrc::Remote {
            url: "http://127.0.0.1:9/cover.jpg".into(),
            headers: vec![],
        });
        assert_eq!(http.get(&remote).send().await.unwrap().status().as_u16(), 404);
        // 播放条目蹭不了这个端点
        let file_url = relay.register_file(PathBuf::from("/tmp/whatever.mp3"));
        let token = file_url.rsplit('/').next().unwrap();
        let wrong = format!("http://127.0.0.1:{}/cover/{token}", relay.inner.port);
        assert_eq!(http.get(&wrong).send().await.unwrap().status().as_u16(), 404);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 雪碧图定帧:采样表 `index[k]` = 第 k 帧的秒(bilibili 解析时已剥掉哨兵),取 ≤ t 的最后一帧。
    #[test]
    fn sprite_frame_picks_last_sample_at_or_before_t() {
        let idx = [0.0, 5.0, 11.0, 17.0];
        assert_eq!(sprite_frame(&idx, 0.0), Some(0));
        assert_eq!(sprite_frame(&idx, 3.0), Some(0), "两格之间取前一格");
        assert_eq!(sprite_frame(&idx, 5.0), Some(1), "正好落在采样点取它自己");
        assert_eq!(sprite_frame(&idx, 12.9), Some(2));
        assert_eq!(sprite_frame(&idx, 17.0), Some(3));
        assert_eq!(sprite_frame(&idx, 9999.0), Some(3), "超末尾取最后一格(片尾 hover 也有图)");
        assert_eq!(sprite_frame(&idx, -4.0), Some(0), "负数归 0");
        assert_eq!(sprite_frame(&idx, f64::NAN), Some(0), "非有限归 0");
        // 首帧采样不在 0(理论形状):t 在首帧之前也取第 0 帧
        assert_eq!(sprite_frame(&[4.0, 9.0], 1.0), Some(0));
        assert_eq!(sprite_frame(&[], 1.0), None, "空表定不了格");
    }

    /// 帧号 → (大图, 列, 行):按行优先、跨图连续;超出大图数 → None。
    #[test]
    fn sprite_cell_maps_frame_to_image_and_grid() {
        let sheet = SpriteSheet {
            x_len: 3,
            y_len: 2,
            tile_w: 16,
            tile_h: 9,
            images: vec!["a".into(), "b".into()],
            index: Vec::new(),
            headers: Vec::new(),
        };
        assert_eq!(sprite_cell(&sheet, 0), Some((0, 0, 0)));
        assert_eq!(sprite_cell(&sheet, 2), Some((0, 2, 0)), "第一行末格");
        assert_eq!(sprite_cell(&sheet, 4), Some((0, 1, 1)), "第二行中格");
        assert_eq!(sprite_cell(&sheet, 6), Some((1, 0, 0)), "第 6 帧翻到第二张大图");
        assert_eq!(sprite_cell(&sheet, 11), Some((1, 2, 1)));
        assert_eq!(sprite_cell(&sheet, 12), None, "采样表比图多的尾巴 → 没图");
        let degenerate = SpriteSheet { x_len: 0, ..sheet.clone() };
        assert_eq!(sprite_cell(&degenerate, 0), None);
    }

    /// 造一张 2×2 格、每格 16×16 纯色的雪碧图 JPEG(16 对齐 = JPEG 宏块边界,颜色不串)。
    fn solid_sprite_jpeg(colors: &[[u8; 3]], x_len: u32, tile: u32) -> Vec<u8> {
        let y_len = (colors.len() as u32).div_ceil(x_len);
        let img = image::RgbImage::from_fn(x_len * tile, y_len * tile, |x, y| {
            let i = ((y / tile) * x_len + x / tile) as usize;
            image::Rgb(colors[i])
        });
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 95))
            .unwrap();
        buf.into_inner()
    }

    /// 回来的 JPEG 解码后取中心像素,断言是哪种纯色(JPEG 有量化误差,按通道主导判)。
    fn dominant_channel(jpeg: &[u8]) -> (u32, u32, char) {
        let img = image::load_from_memory(jpeg).unwrap().to_rgb8();
        let p = img.get_pixel(img.width() / 2, img.height() / 2).0;
        let ch = if p[0] > 180 && p[1] > 180 && p[2] < 90 {
            'Y'
        } else if p[0] > 180 && p[1] < 90 && p[2] < 90 {
            'R'
        } else if p[1] > 180 && p[0] < 90 && p[2] < 90 {
            'G'
        } else if p[2] > 180 && p[0] < 90 && p[1] < 90 {
            'B'
        } else {
            '?'
        };
        (img.width(), img.height(), ch)
    }

    /// 雪碧图端到端:假 CDN 出两张大图 → 注册 → `/thumb/{token}?t=` 按秒定帧、跨图定位、裁出正确
    /// 那一格(纯色验证)、大图只下一次(整图缓存)、同帧再来走裁格缓存、超宽格缩到 THUMB_WIDTH、
    /// 大图 404 / 采样表空 / 防盗链头缺失 都如实 404。
    #[tokio::test]
    async fn sprite_endpoint_crops_right_cell_and_caches_sheet() {
        use axum::routing::get as aget;
        use std::sync::atomic::AtomicUsize;

        const R: [u8; 3] = [255, 0, 0];
        const G: [u8; 3] = [0, 255, 0];
        const B: [u8; 3] = [0, 0, 255];
        const Y: [u8; 3] = [255, 255, 0];
        // 第一张:2×2 格 R G / B Y;第二张:2×2 格 Y B / G R;每格 16px。
        let sheet1 = solid_sprite_jpeg(&[R, G, B, Y], 2, 16);
        let sheet2 = solid_sprite_jpeg(&[Y, B, G, R], 2, 16);
        // 超宽格(每格 256px,2×1)→ 该缩到 192 宽
        let wide = solid_sprite_jpeg(&[R, G], 2, 256);
        let hits = Arc::new(AtomicUsize::new(0));

        // 「严格 CDN」:没带防盗链 Referer 就 403(同一个 handler 挂两个路径 —— 整图缓存按 URL 键,
        // 验「头没带 → 拒」得用一个还没被缓存过的地址)。
        let strict = {
            let hits = hits.clone();
            move |headers: HeaderMap| {
                let hits = hits.clone();
                let body = sheet1.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    if headers.get("referer").map(|v| v.as_bytes()) != Some(b"https://www.bilibili.com/") {
                        return bad(StatusCode::FORBIDDEN);
                    }
                    Response::builder()
                        .header("content-type", "image/jpeg")
                        .body(Body::from(body))
                        .unwrap()
                }
            }
        };
        let app = Router::new()
            .route("/s1.jpg", aget(strict.clone()))
            .route("/s1-uncached.jpg", aget(strict))
            .route("/s2.jpg", aget(move || async move { Body::from(sheet2.clone()) }))
            .route("/wide.jpg", aget(move || async move { Body::from(wide.clone()) }))
            .route("/gone.jpg", aget(|| async { bad(StatusCode::NOT_FOUND) }));
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let up_port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let up = |p: &str| format!("http://127.0.0.1:{up_port}{p}");

        let relay = Relay::start().await.unwrap();
        let http = reqwest::Client::new();
        let headers = vec![("Referer".to_string(), "https://www.bilibili.com/".to_string())];
        // 8 帧,采样 0/10/…/70s:前 4 帧在 s1、后 4 帧在 s2
        let sheet = SpriteSheet {
            x_len: 2,
            y_len: 2,
            tile_w: 16,
            tile_h: 16,
            images: vec![up("/s1.jpg"), up("/s2.jpg")],
            index: (0..8).map(|i| i as f64 * 10.0).collect(),
            headers: headers.clone(),
        };
        let base = relay.register_sprites(Arc::new(sheet));
        let fetch = |t: &str| {
            let http = http.clone();
            let url = format!("{base}?t={t}");
            async move {
                let r = http.get(url).send().await.unwrap();
                (r.status().as_u16(), r.bytes().await.unwrap().to_vec())
            }
        };

        let (code, jpeg) = fetch("0").await;
        assert_eq!(code, 200);
        assert!(jpeg.starts_with(&[0xFF, 0xD8, 0xFF]), "回的是 JPEG");
        assert_eq!(dominant_channel(&jpeg), (16, 16, 'R'), "t=0 → 帧 0 → s1 左上 = 红,原格不放大");
        assert_eq!(dominant_channel(&fetch("13").await.1).2, 'G', "t=13 → 帧 1(10s)→ s1 右上 = 绿");
        assert_eq!(dominant_channel(&fetch("25").await.1).2, 'B', "帧 2 → s1 左下 = 蓝");
        assert_eq!(dominant_channel(&fetch("39.9").await.1).2, 'Y', "帧 3 → s1 右下 = 黄");
        assert_eq!(hits.load(Ordering::SeqCst), 1, "四格同一张大图,只下载一次");
        assert_eq!(dominant_channel(&fetch("40").await.1).2, 'Y', "帧 4 翻到 s2 左上 = 黄");
        assert_eq!(dominant_channel(&fetch("9999").await.1).2, 'R', "超末尾 → 末帧 7 → s2 右下 = 红");
        // 同帧再来:裁格缓存命中,字节一致,大图仍只下过一次
        let (again, jpeg2) = fetch("3").await;
        assert_eq!((again, jpeg2), (200, jpeg));
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // 超宽格缩到 THUMB_WIDTH(高按比例)
        let wide_sheet = SpriteSheet {
            x_len: 2,
            y_len: 1,
            tile_w: 256,
            tile_h: 256,
            images: vec![up("/wide.jpg")],
            index: vec![0.0, 10.0],
            headers: Vec::new(),
        };
        let wb = relay.register_sprites(Arc::new(wide_sheet));
        let r = http.get(format!("{wb}?t=10")).send().await.unwrap();
        assert_eq!(r.status().as_u16(), 200);
        let (w, h, ch) = dominant_channel(&r.bytes().await.unwrap());
        assert_eq!((w, h, ch), (THUMB_WIDTH, THUMB_WIDTH, 'G'), "256 → 192,右格 = 绿");

        // 坏路径全 404:大图取不到 / 采样表比图多且点到图外 / 空采样表 / 防盗链头没带(CDN 拒)
        let gone = relay.register_sprites(Arc::new(SpriteSheet {
            x_len: 2,
            y_len: 2,
            tile_w: 16,
            tile_h: 16,
            images: vec![up("/gone.jpg")],
            index: vec![0.0, 10.0],
            headers: Vec::new(),
        }));
        assert_eq!(http.get(format!("{gone}?t=0")).send().await.unwrap().status().as_u16(), 404);
        let overflow = relay.register_sprites(Arc::new(SpriteSheet {
            x_len: 2,
            y_len: 2,
            tile_w: 16,
            tile_h: 16,
            images: vec![up("/s2.jpg")],
            index: (0..6).map(|i| i as f64 * 10.0).collect(), // 6 帧但只有 4 格
            headers: headers.clone(),
        }));
        assert_eq!(http.get(format!("{overflow}?t=55")).send().await.unwrap().status().as_u16(), 404);
        assert_eq!(http.get(format!("{overflow}?t=5")).send().await.unwrap().status().as_u16(), 200);
        let empty = relay.register_sprites(Arc::new(SpriteSheet {
            x_len: 2,
            y_len: 2,
            tile_w: 16,
            tile_h: 16,
            images: vec![up("/s1.jpg")],
            index: Vec::new(),
            headers: headers.clone(),
        }));
        assert_eq!(http.get(format!("{empty}?t=0")).send().await.unwrap().status().as_u16(), 404);
        let no_ref = relay.register_sprites(Arc::new(SpriteSheet {
            x_len: 2,
            y_len: 2,
            tile_w: 16,
            tile_h: 16,
            images: vec![up("/s1-uncached.jpg")],
            index: vec![0.0],
            headers: Vec::new(),
        }));
        assert_eq!(http.get(format!("{no_ref}?t=0")).send().await.unwrap().status().as_u16(), 404, "CDN 拒 → 404");
        // 同一地址带上头就成:整图缓存不会把上面那次 403 记成"有图"
        let with_ref = relay.register_sprites(Arc::new(SpriteSheet {
            x_len: 2,
            y_len: 2,
            tile_w: 16,
            tile_h: 16,
            images: vec![up("/s1-uncached.jpg")],
            index: vec![0.0],
            headers,
        }));
        assert_eq!(http.get(format!("{with_ref}?t=0")).send().await.unwrap().status().as_u16(), 200);
        // 参数错仍是 400(与本地路一致)
        assert_eq!(http.get(format!("{base}?t=-1")).send().await.unwrap().status().as_u16(), 400);
    }

    /// 端点的坏路径:`?t=` 不对 = 400(调用方写错了);拿不到帧/token 不对 = 404(前端据此降级
    /// 成"只有时间气泡")。这里的 Thumb 条目故意指向不存在的 ffmpeg —— 顺便验"抽不出就 404"。
    #[tokio::test]
    async fn thumb_endpoint_rejects_bad_requests_and_degrades() {
        let relay = Relay::start().await.unwrap();
        let base = relay
            .register_thumbs(PathBuf::from("/tmp/nope.mp4"), PathBuf::from("/nonexistent/ffmpeg"));
        let http = reqwest::Client::new();
        let code = |r: reqwest::Response| r.status().as_u16();

        assert_eq!(code(http.get(&base).send().await.unwrap()), 400, "没带 ?t=");
        assert_eq!(code(http.get(format!("{base}?t=abc")).send().await.unwrap()), 400, "t 不是数字");
        assert_eq!(code(http.get(format!("{base}?t=-3")).send().await.unwrap()), 400, "t 为负");
        // ffmpeg 起不来 → 抽不出 → 404(不是 500:对前端就是"这片没有预览图")
        assert_eq!(code(http.get(format!("{base}?t=30")).send().await.unwrap()), 404, "抽不出");

        // 播放条目(不是 Thumb)拿到 /thumb 下 = 404,不给别的 token 蹭这个端点
        let file_url = relay.register_file(PathBuf::from("/tmp/whatever.mp4"));
        let token = file_url.rsplit('/').next().unwrap();
        let port = relay.inner.port;
        let wrong = format!("http://127.0.0.1:{port}/thumb/{token}?t=0");
        assert_eq!(code(http.get(&wrong).send().await.unwrap()), 404, "条目类型不对");
        let unknown = format!("http://127.0.0.1:{port}/thumb/deadbeef?t=0");
        assert_eq!(code(http.get(&unknown).send().await.unwrap()), 404, "token 不存在");
    }

    /// 端到端(需 PATH 有 ffmpeg,平时 #[ignore],与上面 adaptive 那条同款):真片 → 真抽帧 →
    /// 断言回来的是一张 JPEG、量化落格、片尾之后如实 404。
    /// `cargo test -p larkwing-core --lib media::relay -- --ignored thumb` 手跑。
    #[tokio::test]
    #[ignore]
    async fn real_ffmpeg_thumb_endpoint() {
        use std::process::Command;
        let dir = std::env::temp_dir().join(format!("lw-thumb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.mp4");
        let ok = Command::new("ffmpeg")
            .args([
                "-y", "-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i",
                "testsrc=size=320x240:rate=25:duration=30", "-c:v", "libx264", "-preset",
                "ultrafast", "-pix_fmt", "yuv420p", "-g", "50",
            ])
            .arg(&src)
            .status()
            .expect("run ffmpeg")
            .success();
        assert!(ok, "生成测试源失败");

        let relay = Relay::start().await.unwrap();
        let base = relay.register_thumbs(src, PathBuf::from("ffmpeg"));
        let http = reqwest::Client::new();

        let r = http.get(format!("{base}?t=10")).send().await.unwrap();
        assert_eq!(r.status().as_u16(), 200);
        assert_eq!(r.headers()["content-type"], "image/jpeg");
        let b = r.bytes().await.unwrap();
        assert!(b.starts_with(&[0xFF, 0xD8, 0xFF]), "JPEG 魔数");
        assert!(b.len() > 1000, "不是个空壳:{} 字节", b.len());

        // 同一格再来一次:走缓存(字节完全一致)
        let again = http.get(format!("{base}?t=15")).send().await.unwrap(); // 15 与 10 同格
        assert_eq!(again.status().as_u16(), 200);
        assert_eq!(again.bytes().await.unwrap(), b, "10s 与 15s 落同一格 → 同一张");

        // 片尾之后没有帧 → 404(前端把这一格记下不再重试)
        let past = http.get(format!("{base}?t=600")).send().await.unwrap();
        assert_eq!(past.status().as_u16(), 404);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
