//! 原生 stderr 落盘(`logs/native.log`):sherpa-onnx / onnxruntime / espeak-ng 这类 C/C++
//! 依赖把**真实报错** `fprintf(stderr)` —— Windows 正式版是 GUI 子系统、没有控制台,这些线索
//! 原本直接蒸发(实锤 2026-07-03:克隆模型「文件齐全却加载失败」,binding 只回 `None`,
//! 真因全在 stderr 里没人看见)。boot 把 **fd 2** 重定向到追加文件;Rust 侧 tracing 有自己的
//! writer(logs/larkwing.log),互不影响。**仅正式版启用**(dev 在终端跑,stderr 本来就看得见)。
//! 重定向失败只 warn —— 少一份线索,不挡功能(§3.5 兜底而非门槛)。

use std::io::Write;
use std::path::Path;

/// 追加超过此值就在 boot 时清一次(native 输出量很低,防极端刷屏把盘写大)。
const TRUNCATE_AT: u64 = 5 * 1024 * 1024;

/// 把本进程的原生 stderr(fd 2)重定向到 `logs_dir/native.log`,并写一行 boot 分隔标记。
pub fn redirect_stderr(logs_dir: &Path, version: &str) {
    let path = logs_dir.join("native.log");
    if std::fs::metadata(&path).map(|m| m.len() > TRUNCATE_AT).unwrap_or(false) {
        let _ = std::fs::write(&path, b"");
    }
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
