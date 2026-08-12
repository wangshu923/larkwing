//! 压缩包引擎(fs_unzip / fs_zip 的机器件,自包含、不依赖 engine):
//! - **按文件内容认格式**(zip / rar / 7z 魔数;0.2.27「名字是 mp4 内容不是」同教训),
//!   不看后缀——资源站的假后缀照认。
//! - 先盘点(`preflight`:条数 / 解开总量 / 要不要密码)再动手:总量超闸、缺密码都在
//!   动手前如实退回,不做一半才发现。
//! - 解压走「新文件夹」语义(调用方备好**全新**目标目录)→ 永不覆盖天然成立;包内重名
//!   条目落盘时 `dedupe_path` 加序号。条目路径逐组件重组(拒 `..` 与绝对路径 = zip-slip;
//!   组件过 `sanitize_filename` 清洗,Windows 非法字符不炸)。
//! - zip 老编码文件名:非 UTF-8 按 GB18030 回退解码(国内 zip 常态,乱码 = 白做)。
//! - 全程同步(调用方 `spawn_blocking`);进度 / 取消经 `Progress` 原子量,取消粒度 =
//!   条目之间(正在写的这个写完就停,bgtasks「不撕一半」同语义)。
//! - 7z 的 bzip2 压缩形没开(少见;撞上如实报「不支持的压缩方法」,别当 bug)。

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use anyhow::{bail, Context, Result};

/// 解开总量闸(与 torrent 同一个「影视量级」口径,§4.11 不另造第二个数)。
pub const ARCHIVE_MAX_BYTES: u64 = 50 * 1024 * 1024 * 1024;
/// 跳过条目的点名封顶(量约束 §7.2:汇总 + 点名,不随条数爆)。
const SKIPPED_NAME_CAP: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Zip,
    Rar,
    SevenZ,
}

impl Format {
    pub fn label(self) -> &'static str {
        match self {
            Format::Zip => "zip",
            Format::Rar => "rar",
            Format::SevenZ => "7z",
        }
    }
}

/// 按魔数认格式(不看后缀)。认不出 = 明白话(§3.5)。
pub fn detect_format(path: &Path) -> Result<Format> {
    let mut f =
        File::open(path).with_context(|| format!("打不开压缩包 {}", path.display()))?;
    let mut head = [0u8; 8];
    let n = f.read(&mut head).with_context(|| format!("读不了 {}", path.display()))?;
    let head = &head[..n];
    if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
        return Ok(Format::Zip);
    }
    if head.starts_with(b"Rar!\x1a\x07") {
        return Ok(Format::Rar);
    }
    if head.starts_with(&[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C]) {
        return Ok(Format::SevenZ);
    }
    bail!(
        "认不出压缩格式(按文件内容认,支持 zip/rar/7z):{}——可能不是压缩包,\
         或是分卷的中间卷(分卷 rar 要给第一卷)",
        path.display()
    )
}

/// 盘点结果:动手前的三件事实。
#[derive(Debug)]
pub struct Overview {
    /// 文件条目数(不含目录)。
    pub entries: usize,
    /// 解开后的总字节。
    pub total_bytes: u64,
    /// 有加密条目且没给密码(调用方据此当场问用户,别做一半才发现)。
    pub needs_password: bool,
}

/// 进度 / 取消旗(调用方 Arc 共享;done = 已完成的文件条目数)。
#[derive(Default)]
pub struct Progress {
    pub done: AtomicUsize,
    pub cancel: AtomicBool,
}

impl Progress {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
    fn tick(&self) {
        self.done.fetch_add(1, Ordering::Relaxed);
    }
}

/// 解压结果(量约束:数字汇总 + 封顶点名)。
#[derive(Debug)]
pub struct ExtractReport {
    pub files: usize,
    pub bytes: u64,
    /// 「名字(原因)」,封顶 `SKIPPED_NAME_CAP` 条。
    pub skipped: Vec<String>,
    pub skipped_total: usize,
    pub cancelled: bool,
}

impl ExtractReport {
    fn new() -> ExtractReport {
        ExtractReport { files: 0, bytes: 0, skipped: Vec::new(), skipped_total: 0, cancelled: false }
    }

    fn skip(&mut self, name: &str, why: &str) {
        self.skipped_total += 1;
        if self.skipped.len() < SKIPPED_NAME_CAP {
            self.skipped.push(format!("{name}({why})"));
        }
    }

    /// 跳过说明(工具结果 / 收尾汇报共用;没跳过 = 空串)。
    pub fn skipped_note(&self) -> String {
        if self.skipped_total == 0 {
            return String::new();
        }
        let mut names = self.skipped.join("、");
        if self.skipped_total > self.skipped.len() {
            names.push_str(" 等");
        }
        format!("跳过 {} 个条目:{names}。", self.skipped_total)
    }
}

/// 盘点:条数 / 总量 / 要不要密码。zip 逐条读原始头(不解压);rar/7z 加密到头部时
/// 连清单都列不出 → 直接判「要密码」。
pub fn preflight(path: &Path, format: Format, password: Option<&str>) -> Result<Overview> {
    match format {
        Format::Zip => {
            let mut za = zip::ZipArchive::new(BufReader::new(
                File::open(path).with_context(|| format!("打不开 {}", path.display()))?,
            ))
            .map_err(zip_err)?;
            let (mut entries, mut total, mut enc) = (0usize, 0u64, false);
            for i in 0..za.len() {
                let f = za.by_index_raw(i).map_err(zip_err)?;
                if f.is_dir() {
                    continue;
                }
                entries += 1;
                total = total.saturating_add(f.size());
                enc |= f.encrypted();
            }
            Ok(Overview { entries, total_bytes: total, needs_password: enc && password.is_none() })
        }
        Format::SevenZ => {
            let reader = sevenz_rust2::SevenZReader::open(path, pw7(password));
            let reader = match reader {
                Err(sevenz_rust2::Error::PasswordRequired) if password.is_none() => {
                    return Ok(Overview { entries: 0, total_bytes: 0, needs_password: true });
                }
                other => other.map_err(sevenz_err)?,
            };
            let (mut entries, mut total) = (0usize, 0u64);
            for e in &reader.archive().files {
                if e.is_directory() {
                    continue;
                }
                entries += 1;
                total = total.saturating_add(e.size());
            }
            Ok(Overview { entries, total_bytes: total, needs_password: false })
        }
        Format::Rar => {
            let list = match password {
                Some(p) => unrar::Archive::with_password(path, p).open_for_listing(),
                None => unrar::Archive::new(path).open_for_listing(),
            };
            let list = match list {
                Err(e)
                    if e.code == unrar::error::Code::MissingPassword && password.is_none() =>
                {
                    return Ok(Overview { entries: 0, total_bytes: 0, needs_password: true });
                }
                other => other.map_err(rar_err)?,
            };
            let (mut entries, mut total, mut enc) = (0usize, 0u64, false);
            for item in list {
                let h = item.map_err(rar_err)?;
                if !h.is_file() {
                    continue;
                }
                entries += 1;
                total = total.saturating_add(h.unpacked_size);
                enc |= h.is_encrypted();
            }
            Ok(Overview { entries, total_bytes: total, needs_password: enc && password.is_none() })
        }
    }
}

/// 解压到 `dest`(调用方备好的**全新**目录)。半成品不在这清(调用方按 取消/失败 收拾
/// 整个目录——目标恒为全新目录,整删安全)。
pub fn extract(
    path: &Path,
    format: Format,
    dest: &Path,
    password: Option<&str>,
    prog: &Progress,
) -> Result<ExtractReport> {
    match format {
        Format::Zip => extract_zip(path, dest, password, prog),
        Format::SevenZ => extract_7z(path, dest, password, prog),
        Format::Rar => extract_rar(path, dest, password, prog),
    }
}

fn extract_zip(
    path: &Path,
    dest: &Path,
    password: Option<&str>,
    prog: &Progress,
) -> Result<ExtractReport> {
    let mut za = zip::ZipArchive::new(BufReader::new(
        File::open(path).with_context(|| format!("打不开 {}", path.display()))?,
    ))
    .map_err(zip_err)?;
    let mut report = ExtractReport::new();
    for i in 0..za.len() {
        if prog.cancelled() {
            report.cancelled = true;
            return Ok(report);
        }
        let mut f = match password {
            Some(p) => za.by_index_decrypt(i, p.as_bytes()).map_err(zip_err)?,
            None => za.by_index(i).map_err(zip_err)?,
        };
        let name = decode_zip_name(f.name_raw());
        let Some(rel) = safe_rel_path(&name) else {
            if !f.is_dir() {
                report.skip(&name, "路径不安全");
            }
            continue;
        };
        let out = dest.join(rel);
        if f.is_dir() {
            std::fs::create_dir_all(&out)
                .with_context(|| format!("建不出目录 {}", out.display()))?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("建不出目录 {}", parent.display()))?;
        }
        // 目标目录是全新的,但包内可能有重名条目 → 加序号,永不覆盖
        let out = crate::files::dedupe_path(&out);
        let mut w = File::create(&out).with_context(|| format!("建不出 {}", out.display()))?;
        std::io::copy(&mut f, &mut w).with_context(|| {
            format!("解出「{name}」失败(带密码的包多半是密码不对,不然就是包坏了)")
        })?;
        report.files += 1;
        report.bytes = report.bytes.saturating_add(f.size());
        prog.tick();
    }
    Ok(report)
}

fn extract_7z(
    path: &Path,
    dest: &Path,
    password: Option<&str>,
    prog: &Progress,
) -> Result<ExtractReport> {
    let mut reader = sevenz_rust2::SevenZReader::open(path, pw7(password)).map_err(sevenz_err)?;
    let mut report = ExtractReport::new();
    let mut inner_err: Option<anyhow::Error> = None;
    let dest = dest.to_path_buf();
    reader
        .for_each_entries(|entry, r| {
            if prog.cancelled() {
                report.cancelled = true;
                return Ok(false);
            }
            let name = entry.name().to_string();
            let Some(rel) = safe_rel_path(&name) else {
                if !entry.is_directory() {
                    report.skip(&name, "路径不安全");
                }
                // folder 流是顺序解压的,跳过的条目也要把字节排空,否则后面的全错位
                std::io::copy(r, &mut std::io::sink())?;
                return Ok(true);
            };
            let out = dest.join(rel);
            let step = (|| -> Result<()> {
                if entry.is_directory() {
                    std::fs::create_dir_all(&out)
                        .with_context(|| format!("建不出目录 {}", out.display()))?;
                    return Ok(());
                }
                if let Some(parent) = out.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("建不出目录 {}", parent.display()))?;
                }
                let out = crate::files::dedupe_path(&out);
                let mut w =
                    File::create(&out).with_context(|| format!("建不出 {}", out.display()))?;
                std::io::copy(r, &mut w).with_context(|| {
                    format!("解出「{name}」失败(带密码的包多半是密码不对,不然就是包坏了)")
                })?;
                report.files += 1;
                report.bytes = report.bytes.saturating_add(entry.size());
                prog.tick();
                Ok(())
            })();
            match step {
                Ok(()) => Ok(true),
                Err(e) => {
                    inner_err = Some(e);
                    Ok(false)
                }
            }
        })
        .map_err(sevenz_err)?;
    if let Some(e) = inner_err {
        return Err(e);
    }
    Ok(report)
}

fn extract_rar(
    path: &Path,
    dest: &Path,
    password: Option<&str>,
    prog: &Progress,
) -> Result<ExtractReport> {
    let mut report = ExtractReport::new();
    let mut open = match password {
        Some(p) => unrar::Archive::with_password(path, p).open_for_processing(),
        None => unrar::Archive::new(path).open_for_processing(),
    }
    .map_err(rar_err)?;
    loop {
        if prog.cancelled() {
            report.cancelled = true;
            return Ok(report);
        }
        let Some(header) = open.read_header().map_err(rar_err)? else { break };
        let entry = header.entry();
        let name = entry.filename.to_string_lossy().into_owned();
        let size = entry.unpacked_size;
        let is_file = entry.is_file();
        // unrar 按包内相对路径落盘(库自建子目录);路径不安全的条目跳过不解
        let safe = safe_rel_path(&name).is_some();
        open = if is_file && safe {
            let next = header.extract_with_base(dest).map_err(|e| {
                rar_err(e).context(format!("解出「{name}」失败(带密码的包多半是密码不对)"))
            })?;
            report.files += 1;
            report.bytes = report.bytes.saturating_add(size);
            prog.tick();
            next
        } else {
            if is_file {
                report.skip(&name, "路径不安全");
            }
            header.skip().map_err(rar_err)?
        };
    }
    Ok(report)
}

// ═══════════════════════════════════════════════════════════════════════════
// 打包(fs_zip):只产 zip(生态最通用;打 rar 是 RAR 许可明确不允许的方向)
// ═══════════════════════════════════════════════════════════════════════════

/// 打包计划:先盘清单(总量闸/进度分母都靠它),再动手写。
#[derive(Debug)]
pub struct ZipPlan {
    /// (磁盘绝对路径, 包内相对名——正斜杠分隔)。
    pub files: Vec<(PathBuf, String)>,
    pub total_bytes: u64,
    /// 跳过的(符号链接等),「名字(原因)」封顶点名。
    pub skipped: Vec<String>,
    pub skipped_total: usize,
}

/// 盘点要打包的东西:文件 = 一条;文件夹 = 整棵收(相对名带文件夹名前缀)。
/// 符号链接跳过(防环 + 打包语义含糊);重名相对路径自动加序号。
pub fn plan_zip(inputs: &[PathBuf]) -> Result<ZipPlan> {
    let mut plan =
        ZipPlan { files: Vec::new(), total_bytes: 0, skipped: Vec::new(), skipped_total: 0 };
    let mut seen: HashSet<String> = HashSet::new();
    for root in inputs {
        let meta = std::fs::metadata(root)
            .with_context(|| format!("读不到 {}(不存在?)", root.display()))?;
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| anyhow::anyhow!("路径没有名字:{}", root.display()))?;
        if meta.is_file() {
            let rel = dedupe_rel(&mut seen, name);
            plan.total_bytes = plan.total_bytes.saturating_add(meta.len());
            plan.files.push((root.clone(), rel));
        } else if meta.is_dir() {
            walk_dir(root, &name, &mut plan, &mut seen)?;
        } else {
            bail!("既不是文件也不是文件夹:{}", root.display());
        }
    }
    Ok(plan)
}

fn walk_dir(
    dir: &Path,
    prefix: &str,
    plan: &mut ZipPlan,
    seen: &mut HashSet<String>,
) -> Result<()> {
    let mut items: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("读不了文件夹 {}", dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("读不了文件夹 {}", dir.display()))?;
    items.sort_by_key(std::fs::DirEntry::file_name);
    for item in items {
        let name = item.file_name().to_string_lossy().into_owned();
        let rel = format!("{prefix}/{name}");
        let ftype = item.file_type().with_context(|| format!("读不到类型 {rel}"))?;
        if ftype.is_symlink() {
            plan.skipped_total += 1;
            if plan.skipped.len() < SKIPPED_NAME_CAP {
                plan.skipped.push(format!("{rel}(符号链接)"));
            }
            continue;
        }
        if ftype.is_dir() {
            walk_dir(&item.path(), &rel, plan, seen)?;
        } else {
            let len = item.metadata().map(|m| m.len()).unwrap_or(0);
            let rel = dedupe_rel(seen, rel);
            plan.total_bytes = plan.total_bytes.saturating_add(len);
            plan.files.push((item.path(), rel));
        }
    }
    Ok(())
}

/// 包内相对名去重(两个输入根撞名时加序号;点号在目录名里的不当扩展名切)。
fn dedupe_rel(seen: &mut HashSet<String>, rel: String) -> String {
    if seen.insert(rel.clone()) {
        return rel;
    }
    let (stem, ext) = match rel.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() && !e.contains('/') => {
            (s.to_string(), Some(e.to_string()))
        }
        _ => (rel.clone(), None),
    };
    for n in 2..100_000 {
        let cand = match &ext {
            Some(e) => format!("{stem} ({n}).{e}"),
            None => format!("{stem} ({n})"),
        };
        if seen.insert(cand.clone()) {
            return cand;
        }
    }
    rel
}

#[derive(Debug)]
pub struct ZipReport {
    pub files: usize,
    pub bytes_in: u64,
    pub cancelled: bool,
}

/// 按计划写 zip 到 `tmp`(调用方负责临时件→成品的改名与半路收拾)。
/// deflate 压缩;>4GB 单文件开 zip64;文件名 UTF-8(现代解压器通吃)。
pub fn create_zip(plan: &ZipPlan, tmp: &Path, prog: &Progress) -> Result<ZipReport> {
    let mut report = ZipReport { files: 0, bytes_in: 0, cancelled: false };
    let mut zw = zip::ZipWriter::new(
        File::create(tmp).with_context(|| format!("建不出 {}", tmp.display()))?,
    );
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .large_file(true);
    for (abs, rel) in &plan.files {
        if prog.cancelled() {
            report.cancelled = true;
            return Ok(report);
        }
        zw.start_file(rel.as_str(), opts).map_err(zip_err)?;
        let mut f =
            File::open(abs).with_context(|| format!("读不了 {}(半路被移走?)", abs.display()))?;
        let n = std::io::copy(&mut f, &mut zw)
            .with_context(|| format!("打包 {} 失败", abs.display()))?;
        report.bytes_in = report.bytes_in.saturating_add(n);
        report.files += 1;
        prog.tick();
    }
    zw.finish().map_err(zip_err)?;
    Ok(report)
}

// ═══════════════════════════════════════════════════════════════════════════
// 共用小件
// ═══════════════════════════════════════════════════════════════════════════

/// 包内条目名 → 安全相对路径:反斜杠归一、拒 `..`/绝对形(zip-slip),组件过
/// `sanitize_filename` 清洗(外来名,替换式不退回)。None = 整条不安全,跳过。
fn safe_rel_path(name: &str) -> Option<PathBuf> {
    let norm = name.replace('\\', "/");
    let mut out = PathBuf::new();
    for comp in norm.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp == ".." {
            return None;
        }
        out.push(crate::files::sanitize_filename(comp));
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

/// zip 条目名解码:UTF-8 直用;不是 → GB18030(GBK 超集,国内 zip 常态);再不行 lossy。
fn decode_zip_name(raw: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(raw) {
        return s.to_string();
    }
    let (s, _, had_errors) = encoding_rs::GB18030.decode(raw);
    if !had_errors {
        return s.into_owned();
    }
    String::from_utf8_lossy(raw).into_owned()
}

fn pw7(password: Option<&str>) -> sevenz_rust2::Password {
    match password {
        Some(p) => sevenz_rust2::Password::from(p),
        None => sevenz_rust2::Password::empty(),
    }
}

fn zip_err(e: zip::result::ZipError) -> anyhow::Error {
    use zip::result::ZipError as E;
    match &e {
        E::UnsupportedArchive(msg) if msg.contains("Password") => {
            anyhow::anyhow!("这个 zip 带密码——问用户要密码,拿到后带 password 参数重试")
        }
        E::InvalidPassword => anyhow::anyhow!("密码不对(zip 拒绝解密)"),
        _ => anyhow::anyhow!("zip 读取失败:{e}"),
    }
}

fn sevenz_err(e: sevenz_rust2::Error) -> anyhow::Error {
    use sevenz_rust2::Error as E;
    match e {
        E::PasswordRequired => {
            anyhow::anyhow!("这个 7z 带密码——问用户要密码,拿到后带 password 参数重试")
        }
        E::MaybeBadPassword(_) => {
            anyhow::anyhow!("解不开——带密码的包多半是密码不对,不然就是包坏了")
        }
        other => anyhow::anyhow!("7z 读取失败:{other}"),
    }
}

fn rar_err(e: unrar::error::UnrarError) -> anyhow::Error {
    use unrar::error::Code;
    match e.code {
        Code::MissingPassword => {
            anyhow::anyhow!("这个 rar 带密码——问用户要密码,拿到后带 password 参数重试")
        }
        Code::BadPassword => anyhow::anyhow!("密码不对(rar 拒绝解密)"),
        Code::BadData => {
            anyhow::anyhow!("数据校验不过——包可能坏了;带密码的老 rar 密码不对也报这个")
        }
        _ => anyhow::anyhow!("rar 读取失败:{e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    /// 极简临时目录守卫(ffmpeg_run 测试同款;进程号 + 序号防并发互删)。
    struct Dir(PathBuf);
    impl Dir {
        fn new(tag: &str) -> Dir {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "lw-arch-{tag}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&p).unwrap();
            Dir(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 造一棵小源树:root/a.txt + root/子 目录/b.txt。
    fn source_tree(d: &Dir) -> PathBuf {
        let root = d.path().join("素材");
        std::fs::create_dir_all(root.join("子 目录")).unwrap();
        std::fs::write(root.join("a.txt"), b"AAA").unwrap();
        std::fs::write(root.join("子 目录").join("b.txt"), b"BBBB").unwrap();
        root
    }

    #[test]
    fn detects_by_magic_not_extension() {
        let d = Dir::new("magic");
        // 名字叫 .mp4,内容是 zip 头 → 认成 zip(0.2.27 同教训)
        let fake = d.path().join("假装是视频.mp4");
        std::fs::write(&fake, b"PK\x03\x04junk").unwrap();
        assert_eq!(detect_format(&fake).unwrap(), Format::Zip);
        let rar = d.path().join("r.bin");
        std::fs::write(&rar, b"Rar!\x1a\x07\x01\x00xx").unwrap();
        assert_eq!(detect_format(&rar).unwrap(), Format::Rar);
        let sz = d.path().join("s.dat");
        std::fs::write(&sz, [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C, 0, 0]).unwrap();
        assert_eq!(detect_format(&sz).unwrap(), Format::SevenZ);
        let junk = d.path().join("t.zip");
        std::fs::write(&junk, b"hello").unwrap();
        let err = detect_format(&junk).unwrap_err().to_string();
        assert!(err.contains("认不出"), "{err}");
    }

    #[test]
    fn zip_roundtrip_plan_create_preflight_extract() {
        let d = Dir::new("zip-rt");
        let root = source_tree(&d);
        let lone = d.path().join("单独.txt");
        std::fs::write(&lone, b"CC").unwrap();

        let plan = plan_zip(&[root, lone]).unwrap();
        assert_eq!(plan.files.len(), 3, "{plan:?}");
        assert_eq!(plan.total_bytes, 3 + 4 + 2);
        assert!(plan.files.iter().any(|(_, r)| r == "素材/子 目录/b.txt"), "{plan:?}");

        let archive = d.path().join("打包.zip");
        let prog = Progress::default();
        let rep = create_zip(&plan, &archive, &prog).unwrap();
        assert_eq!((rep.files, rep.bytes_in, rep.cancelled), (3, 9, false));

        assert_eq!(detect_format(&archive).unwrap(), Format::Zip);
        let ov = preflight(&archive, Format::Zip, None).unwrap();
        assert_eq!((ov.entries, ov.total_bytes, ov.needs_password), (3, 9, false));

        let dest = d.path().join("解出来");
        std::fs::create_dir_all(&dest).unwrap();
        let rep = extract(&archive, Format::Zip, &dest, None, &prog).unwrap();
        assert_eq!((rep.files, rep.bytes, rep.skipped_total, rep.cancelled), (3, 9, 0, false));
        assert_eq!(std::fs::read(dest.join("素材/子 目录/b.txt")).unwrap(), b"BBBB");
        assert_eq!(std::fs::read(dest.join("单独.txt")).unwrap(), b"CC");
    }

    /// 包内两个名字(`a?.txt`/`a*.txt`)经 Windows 非法字符清洗后撞成同名 → 落盘加序号
    /// 永不覆盖(zip crate 写入端拒绝真重名,借清洗碰撞走同一条 dedupe 路)。
    #[test]
    fn zip_colliding_sanitized_names_get_numbered() {
        let d = Dir::new("zip-dup");
        let archive = d.path().join("dup.zip");
        let mut zw = zip::ZipWriter::new(File::create(&archive).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        for (name, content) in [("a?.txt", b"one" as &[u8]), ("a*.txt", b"two2")] {
            zw.start_file(name, opts).unwrap();
            std::io::Write::write_all(&mut zw, content).unwrap();
        }
        zw.finish().unwrap();

        let dest = d.path().join("out");
        std::fs::create_dir_all(&dest).unwrap();
        let rep = extract(&archive, Format::Zip, &dest, None, &Progress::default()).unwrap();
        assert_eq!(rep.files, 2);
        assert_eq!(std::fs::read(dest.join("a_.txt")).unwrap(), b"one");
        assert_eq!(std::fs::read(dest.join("a_ (2).txt")).unwrap(), b"two2");
    }

    #[test]
    fn encrypted_zip_asks_then_opens_with_password() {
        let d = Dir::new("zip-pw");
        let archive = d.path().join("锁着.zip");
        let mut zw = zip::ZipWriter::new(File::create(&archive).unwrap());
        let opts = zip::write::SimpleFileOptions::default()
            .with_aes_encryption(zip::AesMode::Aes256, "open sesame");
        zw.start_file("秘密.txt", opts).unwrap();
        std::io::Write::write_all(&mut zw, b"SECRET").unwrap();
        zw.finish().unwrap();

        // 没给密码:盘点如实说「要密码」;硬解如实退回
        let ov = preflight(&archive, Format::Zip, None).unwrap();
        assert!(ov.needs_password);
        assert!(!preflight(&archive, Format::Zip, Some("open sesame")).unwrap().needs_password);
        let dest = d.path().join("no-pw");
        std::fs::create_dir_all(&dest).unwrap();
        let err = extract(&archive, Format::Zip, &dest, None, &Progress::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("密码"), "{err}");
        // 错密码 = 明白话;对密码 = 解出来
        let err = extract(&archive, Format::Zip, &dest, Some("wrong"), &Progress::default())
            .map(|r| format!("不该成功:{r:?}"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("密码"), "{err}");
        let rep =
            extract(&archive, Format::Zip, &dest, Some("open sesame"), &Progress::default())
                .unwrap();
        assert_eq!(rep.files, 1);
        assert_eq!(std::fs::read(dest.join("秘密.txt")).unwrap(), b"SECRET");
    }

    #[test]
    fn sevenz_roundtrip_and_password() {
        let d = Dir::new("7z");
        let root = source_tree(&d);
        let plain = d.path().join("素材.7z");
        sevenz_rust2::compress_to_path(&root, &plain).unwrap();
        assert_eq!(detect_format(&plain).unwrap(), Format::SevenZ);
        let ov = preflight(&plain, Format::SevenZ, None).unwrap();
        assert_eq!((ov.entries, ov.needs_password), (2, false));
        let dest = d.path().join("out");
        std::fs::create_dir_all(&dest).unwrap();
        let rep = extract(&plain, Format::SevenZ, &dest, None, &Progress::default()).unwrap();
        assert_eq!((rep.files, rep.cancelled), (2, false));
        assert_eq!(std::fs::read(dest.join("子 目录/b.txt")).unwrap(), b"BBBB");

        let locked = d.path().join("锁着.7z");
        sevenz_rust2::compress_to_path_encrypted(&root, &locked, "口令".into()).unwrap();
        let ov = preflight(&locked, Format::SevenZ, None).unwrap();
        assert!(ov.needs_password, "加密 7z 没密码 = 如实要密码");
        let dest2 = d.path().join("out2");
        std::fs::create_dir_all(&dest2).unwrap();
        let rep =
            extract(&locked, Format::SevenZ, &dest2, Some("口令"), &Progress::default()).unwrap();
        assert_eq!(rep.files, 2);
    }

    #[test]
    fn cancel_flag_stops_before_work() {
        let d = Dir::new("cancel");
        let root = source_tree(&d);
        let plan = plan_zip(std::slice::from_ref(&root)).unwrap();
        let archive = d.path().join("c.zip");
        let prog = Progress::default();
        create_zip(&plan, &archive, &prog).unwrap();

        prog.cancel.store(true, Ordering::Relaxed);
        let dest = d.path().join("out");
        std::fs::create_dir_all(&dest).unwrap();
        let rep = extract(&archive, Format::Zip, &dest, None, &prog).unwrap();
        assert!(rep.cancelled);
        assert_eq!(rep.files, 0);
        // 打包同款
        let rep2 = create_zip(&plan, &d.path().join("c2.zip"), &prog).unwrap();
        assert!(rep2.cancelled);
    }

    #[test]
    fn rel_path_safety_and_gbk_names() {
        assert_eq!(safe_rel_path("a/b.txt").unwrap(), PathBuf::from("a/b.txt"));
        assert_eq!(safe_rel_path(r"a\b.txt").unwrap(), PathBuf::from("a/b.txt"));
        assert_eq!(safe_rel_path("/etc/passwd").unwrap(), PathBuf::from("etc/passwd"));
        assert!(safe_rel_path("../逃逸.txt").is_none());
        assert!(safe_rel_path("a/../../b").is_none());
        assert!(safe_rel_path("").is_none());
        // Windows 盘符组件被清洗成普通名字,不再是绝对路径
        let p = safe_rel_path(r"C:\Windows\evil.dll").unwrap();
        assert!(!p.is_absolute(), "{p:?}");

        // GBK(GB18030)老编码文件名解得回中文
        let (gbk, _, _) = encoding_rs::GB18030.encode("中文歌单.txt");
        assert_eq!(decode_zip_name(&gbk), "中文歌单.txt");
        assert_eq!(decode_zip_name("已是utf8.txt".as_bytes()), "已是utf8.txt");
    }

    #[test]
    fn plan_zip_dedupes_colliding_roots() {
        let d = Dir::new("plan-dup");
        let a = d.path().join("甲");
        let b = d.path().join("乙");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("同名.txt"), b"1").unwrap();
        std::fs::write(b.join("同名.txt"), b"22").unwrap();
        let plan = plan_zip(&[a.join("同名.txt"), b.join("同名.txt")]).unwrap();
        let rels: Vec<&str> = plan.files.iter().map(|(_, r)| r.as_str()).collect();
        assert_eq!(rels, vec!["同名.txt", "同名 (2).txt"]);
    }
}
