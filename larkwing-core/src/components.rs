//! 第三方组件(yt-dlp / ffmpeg)**用时下载**:不进安装包(宪法 §10「不打包 Python」
//! 由此消解 —— 包里没有任何东西,运行时下载到数据目录,性质同浏览器下载文件)。
//! robot 同款思路(它的 installer.py),收敛成:固定直链 + 镜像列表数据化(国内家庭
//! 网络拉不动 GitHub,镜像优先、官方兜底)+ 哈希校验(发布方提供 SUMS 才校,见 spec)。
//! 进度全程走 Tasks(HUD 可见,含"到哪一步了")。

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use sha2::Digest;
use tokio::io::AsyncWriteExt;

use crate::tasks::{TaskHandle, Tasks};
use crate::bus::Text;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Component {
    YtDlp,
    Ffmpeg,
}

/// 压缩形态:None = 裸二进制;Zip = 取出 entry 名以 suffix 结尾的那一个文件。
enum Archive {
    None,
    Zip { entry_suffix: &'static str },
}

struct Spec {
    /// 落盘文件名(含平台后缀)。
    bin_name: &'static str,
    /// 主下载直链(github 的走镜像前缀;其余直连)。
    url: &'static str,
    /// 同目录的 SHA2-256SUMS 清单(发布方提供才有;None = 只靠 TLS,记录在案)。
    sums_url: Option<&'static str>,
    /// SUMS 清单里对应的资产名。
    sums_asset: &'static str,
    archive: Archive,
    /// HUD 标题 key。
    label_key: &'static str,
}

impl Component {
    fn spec(self) -> Result<Spec> {
        // 固定 latest 直链:不需要 GitHub API(国内连不上),镜像可整体前缀。
        match (self, std::env::consts::OS) {
            (Component::YtDlp, "windows") => Ok(Spec {
                bin_name: "yt-dlp.exe",
                url: "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe",
                sums_url: Some(
                    "https://github.com/yt-dlp/yt-dlp/releases/latest/download/SHA2-256SUMS",
                ),
                sums_asset: "yt-dlp.exe",
                archive: Archive::None,
                label_key: "task.download.ytdlp",
            }),
            (Component::YtDlp, "macos") => Ok(Spec {
                bin_name: "yt-dlp",
                url: "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_macos",
                sums_url: Some(
                    "https://github.com/yt-dlp/yt-dlp/releases/latest/download/SHA2-256SUMS",
                ),
                sums_asset: "yt-dlp_macos",
                archive: Archive::None,
                label_key: "task.download.ytdlp",
            }),
            (Component::Ffmpeg, "windows") => Ok(Spec {
                bin_name: "ffmpeg.exe",
                // yt-dlp 官方维护的 ffmpeg 构建,latest 命名稳定
                url: "https://github.com/yt-dlp/FFmpeg-Builds/releases/latest/download/ffmpeg-master-latest-win64-gpl.zip",
                sums_url: None, // 该仓库不发 SUMS:只靠 TLS,记录在案(PLAN §9)
                sums_asset: "",
                archive: Archive::Zip { entry_suffix: "bin/ffmpeg.exe" },
                label_key: "task.download.ffmpeg",
            }),
            (Component::Ffmpeg, "macos") => Ok(Spec {
                bin_name: "ffmpeg",
                // 开发机兜底(发布目标是 Windows);evermeet 非 github,不走镜像
                url: "https://evermeet.cx/ffmpeg/getrelease/zip",
                sums_url: None,
                sums_asset: "",
                archive: Archive::Zip { entry_suffix: "ffmpeg" },
                label_key: "task.download.ffmpeg",
            }),
            (c, os) => bail!("组件 {c:?} 不支持当前平台 {os}"),
        }
    }

    /// PATH 兜底时找的命令名(开发机 brew 装过就直接用,不下载)。
    fn path_name(self) -> &'static str {
        match self {
            Component::YtDlp => "yt-dlp",
            Component::Ffmpeg => "ffmpeg",
        }
    }
}

/// 镜像前缀列表(数据,settings `media.gh_mirrors` 可覆盖):对 github.com 直链做
/// 整体前缀(ghproxy 约定);空串 = 直连官方。顺序即尝试顺序。
pub const DEFAULT_GH_MIRRORS: &[&str] =
    &["https://ghproxy.net/", "https://ghfast.top/", ""];

/// 按镜像列表展开候选 URL。非 github 直链不做前缀。(voice 模型下载复用。)
pub(crate) fn candidates(url: &str, mirrors: &[String]) -> Vec<String> {
    if !url.starts_with("https://github.com/") {
        return vec![url.to_string()];
    }
    mirrors.iter().map(|m| format!("{m}{url}")).collect()
}

pub struct Components {
    dir: PathBuf,
    tasks: Tasks,
    http: reqwest::Client,
    /// 并发去重:同一组件同时只有一个下载在跑(后到的等同一份结果)。
    locks: [tokio::sync::Mutex<()>; 2],
}

impl Components {
    pub fn new(dir: PathBuf, tasks: Tasks) -> Components {
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("reqwest client");
        Components { dir, tasks, http, locks: Default::default() }
    }

    /// 组件就绪:托管目录命中 → PATH 兜底(开发机)→ 用时下载(进度上 HUD)。
    /// 返回可执行文件路径。drop-safe:下载中途被取消只留 .part 残文件,下次覆盖重下。
    pub async fn ensure(&self, c: Component, mirrors: &[String]) -> Result<PathBuf> {
        let spec = c.spec()?;
        let managed = self.dir.join(spec.bin_name);
        if managed.is_file() {
            return Ok(managed);
        }
        if let Some(found) = which(c.path_name()) {
            tracing::info!(component = ?c, path = %found.display(), "PATH 命中,跳过下载");
            return Ok(found);
        }
        let _guard = self.locks[c as usize].lock().await;
        if managed.is_file() {
            return Ok(managed); // 排队期间别人下完了
        }
        let task = self.tasks.start("download", Text::new(spec.label_key));
        match self.download(&spec, mirrors, &task).await {
            Ok(()) => {
                task.done();
                Ok(managed)
            }
            Err(e) => {
                task.fail("task.err.download", serde_json::Value::Null);
                Err(e)
            }
        }
    }

    async fn download(&self, spec: &Spec, mirrors: &[String], task: &TaskHandle) -> Result<()> {
        tokio::fs::create_dir_all(&self.dir).await?;
        let part = self.dir.join(format!("{}.part", spec.bin_name));

        // 1. 拉文件:镜像依次尝试,哪个先通用哪个
        let mut fetched = None;
        let mut last_err: Option<anyhow::Error> = None;
        for url in candidates(spec.url, mirrors) {
            let host = url.split('/').nth(2).unwrap_or("?").to_string();
            task.step("step.connect", serde_json::json!({ "host": host }));
            match self.fetch_to(&url, &part, task).await {
                Ok(()) => {
                    fetched = Some(url);
                    break;
                }
                Err(e) => {
                    tracing::warn!(url, err = %format!("{e:#}"), "下载失败,换下一个源");
                    last_err = Some(e);
                }
            }
        }
        let used_url = match fetched {
            Some(u) => u,
            None => return Err(last_err.unwrap_or_else(|| anyhow::anyhow!("没有可用下载源"))),
        };

        // 2. 校验(发布方有 SUMS 才校;清单经同一镜像取,信任锚 = TLS + 镜像声誉)
        if let Some(sums_url) = spec.sums_url {
            task.step("step.verify", serde_json::Value::Null);
            let mirror = used_url.strip_suffix(spec.url).unwrap_or("");
            let sums_full = format!("{mirror}{sums_url}");
            let sums = self
                .http
                .get(&sums_full)
                .send()
                .await
                .context("取 SUMS 清单失败")?
                .error_for_status()?
                .text()
                .await?;
            let expect = parse_sums(&sums, spec.sums_asset)
                .with_context(|| format!("SUMS 清单里没有 {}", spec.sums_asset))?;
            let actual = sha256_file(part.clone()).await?;
            if !expect.eq_ignore_ascii_case(&actual) {
                tokio::fs::remove_file(&part).await.ok();
                bail!("SHA256 校验不过:期望 {expect},实际 {actual}");
            }
        }

        // 3. 解压(zip 包取出目标二进制;裸二进制跳过)
        if let Archive::Zip { entry_suffix } = spec.archive {
            task.step("step.extract", serde_json::Value::Null);
            let (zip_path, out_path) = (part.clone(), self.dir.join(format!("{}.bin", spec.bin_name)));
            let out = out_path.clone();
            tokio::task::spawn_blocking(move || unzip_entry(&zip_path, entry_suffix, &out))
                .await
                .context("解压任务挂了")??;
            tokio::fs::remove_file(&part).await.ok();
            tokio::fs::rename(&out_path, &part).await?;
        }

        // 4. 可执行位 + 原子就位
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o755)).await?;
        }
        tokio::fs::rename(&part, self.dir.join(spec.bin_name)).await?;
        Ok(())
    }

    /// 流式落盘 + 进度上报;见自由函数 `fetch_url_to`(voice 模型下载共用)。
    async fn fetch_to(&self, url: &str, dest: &Path, task: &TaskHandle) -> Result<()> {
        fetch_url_to(&self.http, url, dest, task).await
    }
}

/// 流式落盘 + 进度上报(≥1% 才报一次,别刷屏);单 chunk 30s 无数据判失败。
pub(crate) async fn fetch_url_to(
    http: &reqwest::Client,
    url: &str,
    dest: &Path,
    task: &TaskHandle,
) -> Result<()> {
    let resp = http.get(url).send().await?.error_for_status()?;
    let total = resp.content_length().unwrap_or(0);
    let mut file = tokio::fs::File::create(dest).await?;
    let mut stream = resp.bytes_stream();
    let mut done: u64 = 0;
    let mut last_pct = -1i32;
    loop {
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(30), stream.next())
            .await
            .map_err(|_| anyhow::anyhow!("下载流 30s 无数据"))?;
        let Some(chunk) = chunk else { break };
        let bytes = chunk?;
        file.write_all(&bytes).await?;
        done += bytes.len() as u64;
        if total > 0 {
            let pct = ((done as f64 / total as f64) * 100.0) as i32;
            if pct > last_pct {
                last_pct = pct;
                task.step_progress(
                    "step.download",
                    serde_json::json!({
                        "done": (done as f64 / 1_048_576.0 * 10.0).round() / 10.0,
                        "total": (total as f64 / 1_048_576.0 * 10.0).round() / 10.0,
                    }),
                    (done as f64 / total as f64) as f32,
                );
            }
        }
    }
    file.flush().await?;
    if total > 0 && done < total {
        bail!("下载不完整: {done}/{total}");
    }
    if done < 1024 {
        bail!("下载内容异常地小({done} 字节),按失败处理");
    }
    Ok(())
}

/// PATH 查找(开发机 brew/winget 装过就直接用)。
fn which(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let exe = dir.join(format!("{name}.exe"));
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}

/// SUMS 清单格式:`<hex>  <asset>` 每行一条。
fn parse_sums(sums: &str, asset: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut it = line.split_whitespace();
        let (hash, name) = (it.next()?, it.next()?);
        (name == asset || name == format!("*{asset}")).then(|| hash.to_string())
    })
}

async fn sha256_file(path: PathBuf) -> Result<String> {
    tokio::task::spawn_blocking(move || -> Result<String> {
        let mut file = std::fs::File::open(&path)?;
        let mut hasher = sha2::Sha256::new();
        std::io::copy(&mut file, &mut hasher)?;
        Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
    })
    .await
    .context("哈希任务挂了")?
}

/// 从 zip 里取出第一个以 entry_suffix 结尾的文件写到 dest。
fn unzip_entry(zip_path: &Path, entry_suffix: &str, dest: &Path) -> Result<()> {
    let file = std::fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file).context("zip 打不开")?;
    let idx = (0..archive.len())
        .find(|&i| {
            archive
                .by_index(i)
                .map(|e| e.is_file() && e.name().ends_with(entry_suffix))
                .unwrap_or(false)
        })
        .with_context(|| format!("zip 里没有 *{entry_suffix}"))?;
    let mut entry = archive.by_index(idx)?;
    let mut out = std::fs::File::create(dest)?;
    std::io::copy(&mut entry, &mut out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_urls_expand_with_mirrors_others_stay_direct() {
        let mirrors: Vec<String> =
            DEFAULT_GH_MIRRORS.iter().map(|s| s.to_string()).collect();
        let gh = candidates("https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_macos", &mirrors);
        assert_eq!(gh.len(), 3);
        assert!(gh[0].starts_with("https://ghproxy.net/https://github.com/"));
        assert_eq!(gh[2], "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_macos");

        let direct = candidates("https://evermeet.cx/ffmpeg/getrelease/zip", &mirrors);
        assert_eq!(direct, vec!["https://evermeet.cx/ffmpeg/getrelease/zip".to_string()]);
    }

    #[test]
    fn sums_parsing_handles_plain_and_star_prefixed_names() {
        let sums = "abc123  yt-dlp.exe\ndef456  *yt-dlp_macos\n";
        assert_eq!(parse_sums(sums, "yt-dlp.exe").as_deref(), Some("abc123"));
        assert_eq!(parse_sums(sums, "yt-dlp_macos").as_deref(), Some("def456"));
        assert_eq!(parse_sums(sums, "nope"), None);
    }

    #[test]
    fn unzip_picks_entry_by_suffix() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("lw-zip-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let zip_path = dir.join("a.zip");
        {
            let f = std::fs::File::create(&zip_path).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default();
            w.start_file("pkg/doc/readme.txt", opts).unwrap();
            w.write_all(b"nope").unwrap();
            w.start_file("pkg/bin/ffmpeg.exe", opts).unwrap();
            w.write_all(b"BINARY").unwrap();
            w.finish().unwrap();
        }
        let dest = dir.join("out.bin");
        unzip_entry(&zip_path, "bin/ffmpeg.exe", &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"BINARY");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn specs_exist_for_supported_platforms() {
        // 当前平台(mac 开发 / win 发布)必须有 spec;其余平台报清晰错误
        if matches!(std::env::consts::OS, "macos" | "windows") {
            Component::YtDlp.spec().unwrap();
            Component::Ffmpeg.spec().unwrap();
        }
    }
}
