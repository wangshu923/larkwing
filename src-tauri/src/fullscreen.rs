//! Windows 专属:检测「别的程序是否正在全屏」(游戏 / 全屏视频 / 演示),
//! 好让常驻悬浮窗(always_on_top)及时让位 —— 否则会浮在全屏画面上打扰人。
//!
//! 为什么只 Windows:Mac 原生 space 行为已让 always_on_top 窗口天然不覆盖别 app 的
//! 原生全屏(用户实测正确),无需此逻辑;故本模块整块 `#[cfg(windows)]`,Mac 不编译、
//! 不发事件,前端 foreignFs 恒 false 维持原行为(见 App.vue 显隐规则)。
//!
//! 判据 = 前台窗口铺满它所在那块显示器(含任务栏区 rcMonitor)。最大化窗口只覆盖工作区
//! rcWork、到不了 rcMonitor 底边,故能与真全屏区分(任务栏自动隐藏时二者重合 = 已知的
//! 良性误判,那种场景悬浮窗本就会被盖住,让位也合理)。壳层按固定间隔轮询调它(见 lib.rs)。

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::{
  GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONULL,
};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::{
  GetClassNameW, GetDesktopWindow, GetForegroundWindow, GetShellWindow, GetWindowRect,
  GetWindowThreadProcessId,
};

/// 前台是否有「别的程序」在全屏。无前台 / 桌面外壳 / 我们自己的窗口一律返回 false。
pub fn foreground_fullscreen() -> bool {
  unsafe {
    let hwnd = GetForegroundWindow();
    if hwnd.0.is_null() {
      return false;
    }
    // 桌面本身 / 外壳(任务栏)不算「别的程序全屏」
    if hwnd == GetDesktopWindow() || hwnd == GetShellWindow() {
      return false;
    }
    // 我们自己进程的窗口:主窗看视频全屏由前端 win.isFullscreen() 处理,float 自身不该自我误判
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid as *mut u32));
    if pid == GetCurrentProcessId() {
      return false;
    }
    // 桌面壁纸宿主:点到桌面时前台会是 Progman / WorkerW,其矩形铺满屏幕但不算全屏 app
    let mut buf = [0u16; 256];
    let n = GetClassNameW(hwnd, &mut buf);
    let class = String::from_utf16_lossy(&buf[..n.max(0) as usize]);
    if class == "Progman" || class == "WorkerW" {
      return false;
    }
    // 前台窗口矩形 vs 其所在显示器矩形:窗口完全盖住整块显示器 = 全屏
    let mut wr = RECT::default();
    if GetWindowRect(hwnd, &mut wr).is_err() {
      return false;
    }
    let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONULL);
    if mon.0.is_null() {
      return false;
    }
    let mut mi = MONITORINFO {
      cbSize: std::mem::size_of::<MONITORINFO>() as u32,
      ..Default::default()
    };
    if !GetMonitorInfoW(mon, &mut mi).as_bool() {
      return false;
    }
    let m = mi.rcMonitor;
    wr.left <= m.left && wr.top <= m.top && wr.right >= m.right && wr.bottom >= m.bottom
  }
}
