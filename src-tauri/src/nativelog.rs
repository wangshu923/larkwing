//! 原生 stderr 落盘(`logs/native.log`):sherpa-onnx / onnxruntime / espeak-ng 这类 C/C++
//! 依赖把**真实报错** `fprintf(stderr)` —— Windows 正式版是 GUI 子系统、没有控制台,这些线索
//! 原本直接蒸发(实锤 2026-07-03:克隆模型「文件齐全却加载失败」,binding 只回 `None`,
//! 真因全在 stderr 里没人看见)。boot 把 **fd 2** 重定向到追加文件;Rust 侧 tracing 有自己的
//! writer(logs/larkwing.log),互不影响。**仅正式版启用**(dev 在终端跑,stderr 本来就看得见)。
//! 重定向失败只 warn —— 少一份线索,不挡功能(§3.5 兜底而非门槛)。

use std::io::Write;
use std::path::{Path, PathBuf};

/// 超过此值就在 boot 时轮转一代(native 输出量很低,防极端刷屏把盘写大)。
/// 只留一代 ⇒ 磁盘上限 ≈ 2×这个值。`logkeep` 只认 `larkwing.log.YYYY-MM-DD` 形,管不到这份,
/// 故自己管。**这个数字待用户拍板**(§4.11 写死的产品默认值),单源在此。
const ROTATE_AT: u64 = 5 * 1024 * 1024;

/// boot 期对现有 `native.log` 的处置(**纯判定,可单测**;真动手在 `rotate_if_big`)。
#[derive(Debug, PartialEq, Eq)]
enum Boot {
    /// 继续往后追加(文件不存在 / 读不到大小 / 还没到阈值)。
    Append,
    /// 轮转成 `native.log.1`(覆盖上一代)再从空文件起。
    Rotate,
}

/// `None` = 文件不存在或 metadata 读不到 → 照旧追加(读不到就别乱动别人的文件)。
fn plan_boot(size: Option<u64>) -> Boot {
    match size {
        Some(n) if n > ROTATE_AT => Boot::Rotate,
        _ => Boot::Append,
    }
}

/// 在完整文件名后追加后缀(`with_extension` 会把 `.log` 换掉,不能用)。
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// 现有 native.log 超阈值 → 改名成 `native.log.1`(上一代直接被顶掉,只留一代)。
/// **必须在 fd 2 重定向之前做**:此刻本进程还没打开它,改名零竞态、零后台线程。
/// 原先是 `write(&path, b"")` 一把清空 —— 最后那 5MB 线索(常是崩前的原生报错)全丢了。
fn rotate_if_big(path: &Path) {
    if plan_boot(std::fs::metadata(path).ok().map(|m| m.len())) != Boot::Rotate {
        return;
    }
    let prev = sibling(path, ".1");
    match std::fs::rename(path, &prev) {
        Ok(()) => tracing::info!(prev = %prev.display(), "native.log 超上限,旧的轮转成 .1"),
        // 轮转失败不挡启动:大不了继续追加(§3.5 兜底而非门槛)
        Err(e) => tracing::warn!(err = %e, "native.log 轮转失败,继续追加"),
    }
}

/// 把本进程的原生 stderr(fd 2)重定向到 `logs_dir/native.log`,并写一行 boot 分隔标记。
pub fn redirect_stderr(logs_dir: &Path, version: &str) {
    let path = logs_dir.join("native.log");
    rotate_if_big(&path);
    // 标记行先用普通文件句柄写(重定向前后都可靠),定位「这一次启动」的起点
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "==== larkwing {version} boot (epoch {ts}) ====");
    }
    match redirect_impl(&path) {
        Ok(()) => tracing::info!(path = %path.display(), "原生库 stderr 已落盘(sherpa/ORT/espeak 的真实报错看这里)"),
        Err(e) => tracing::warn!(err = %e, "原生 stderr 重定向失败(不影响功能,只是少了原生库报错线索)"),
    }
}

#[cfg(unix)]
fn redirect_impl(path: &Path) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    if unsafe { libc::dup2(f.as_raw_fd(), 2) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(()) // f 随作用域关闭没关系:dup2 后 fd 2 是同一打开文件的独立引用
}

#[cfg(windows)]
fn redirect_impl(path: &Path) -> std::io::Result<()> {
    use std::os::windows::io::IntoRawHandle;
    // UCRT 的 POSIX 风格层(libc 带正确 link_name:open_osfhandle→_open_osfhandle 等):
    // 把 Win32 句柄包成 CRT fd,再顶到 fd 2。GUI 子系统下 fd 0/1/2 预分配但无效,
    // dup2 到 2 是标准做法(fprintf(stderr) 即走新目标,C++ std::cerr 同底层 fd 一并覆盖)。
    const O_APPEND: libc::c_int = 0x0008;
    let f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    let handle = f.into_raw_handle(); // 句柄所有权交给 CRT fd(随 close/进程退出回收)
    let fd = unsafe { libc::open_osfhandle(handle as libc::intptr_t, O_APPEND) };
    if fd < 0 {
        // CRT 没接管:句柄还给 File 收尸,别泄漏
        use std::os::windows::io::FromRawHandle;
        drop(unsafe { std::fs::File::from_raw_handle(handle) });
        return Err(std::io::Error::other("open_osfhandle 失败"));
    }
    let ok = unsafe { libc::dup2(fd, 2) } == 0; // UCRT _dup2:0 成功、-1 失败(非 POSIX 返回值)
    unsafe { libc::close(fd) }; // fd 2 已是独立复制,源 fd 用完即还
    if !ok {
        return Err(std::io::Error::other("dup2 到 stderr 失败"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 判定表:读不到大小 / 没到阈值 → 追加;严格超过阈值 → 轮转(等于阈值不算超)。
    #[test]
    fn plan_boot_rotates_only_above_the_cap() {
        assert_eq!(plan_boot(None), Boot::Append, "文件不存在 = 照旧追加");
        assert_eq!(plan_boot(Some(0)), Boot::Append);
        assert_eq!(plan_boot(Some(ROTATE_AT)), Boot::Append, "正好到阈值不轮转");
        assert_eq!(plan_boot(Some(ROTATE_AT + 1)), Boot::Rotate);
        assert_eq!(plan_boot(Some(ROTATE_AT * 3)), Boot::Rotate);
    }

    /// 后缀追加在**完整文件名**之后(`with_extension` 会把 `.log` 吃掉)。
    #[test]
    fn sibling_appends_after_the_whole_name() {
        assert_eq!(sibling(Path::new("/l/native.log"), ".1"), PathBuf::from("/l/native.log.1"));
    }

    /// 端到端:超限的现有文件被改名成 .1(内容留着,不是清空);.1 已存在则被顶掉。
    #[test]
    fn rotate_moves_old_content_aside_and_keeps_one_generation() {
        let dir = std::env::temp_dir().join(format!("larkwing-nativelog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("native.log");
        let prev = dir.join("native.log.1");

        // 没到阈值:原地不动,也不产生 .1
        std::fs::write(&path, b"small").unwrap();
        rotate_if_big(&path);
        assert_eq!(std::fs::read(&path).unwrap(), b"small");
        assert!(!prev.exists());

        // 超阈值:改名成 .1,内容留着(原先是清空,线索全丢)
        std::fs::write(&prev, b"older generation").unwrap();
        std::fs::write(&path, vec![b'x'; (ROTATE_AT + 1) as usize]).unwrap();
        rotate_if_big(&path);
        assert!(!path.exists(), "轮转后旧文件已挪走,调用方随后 create 新的");
        assert_eq!(std::fs::metadata(&prev).unwrap().len(), ROTATE_AT + 1, ".1 = 刚才那份");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
