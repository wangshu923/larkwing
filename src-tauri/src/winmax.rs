//! Windows 专属:无边框窗(`decorations:false`)最大化会盖住任务栏 + 任务栏点击失灵
//! (Tauri #7103 / #14025,blocked-by-upstream 至今未修)。给主窗挂 WndProc subclass,
//! 拦截 `WM_GETMINMAXINFO`,把最大化的目标尺寸/位置钉到工作区 `rcWork`(不含任务栏)。
//! VSCode / Spotify 等无边框 app 同款标准做法。整块 `#[cfg(windows)]`,Mac 不编译。
//!
//! ⚠️ 本文件在 Mac 上完全不编译(cfg gate)、`cargo check` 也跳过 → **只能 Windows 真机验**(§8.1)。

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
  GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{MINMAXINFO, WM_GETMINMAXINFO};

/// subclass id:本进程内唯一即可(只挂主窗一次)。
const SUBCLASS_ID: usize = 1;

/// 拦 `WM_GETMINMAXINFO`:把无边框窗最大化的目标尺寸/位置钉到工作区(排除任务栏);
/// 其余消息透传默认处理。取不到显示器信息就不改(退回系统默认,宁可盖任务栏也不崩)。
unsafe extern "system" fn subclass_proc(
  hwnd: HWND,
  msg: u32,
  wparam: WPARAM,
  lparam: LPARAM,
  _id: usize,
  _ref: usize,
) -> LRESULT {
  if msg == WM_GETMINMAXINFO {
    let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    let mut mi = MONITORINFO {
      cbSize: core::mem::size_of::<MONITORINFO>() as u32,
      ..Default::default()
    };
    if GetMonitorInfoW(mon, &mut mi).as_bool() {
      let work = mi.rcWork; // 工作区(不含任务栏)
      let full = mi.rcMonitor; // 整块显示器
      let info = &mut *(lparam.0 as *mut MINMAXINFO);
      // 位置 = 工作区左上,相对显示器原点(多显示器时 rcMonitor 非 0)
      info.ptMaxPosition.x = work.left - full.left;
      info.ptMaxPosition.y = work.top - full.top;
      // 尺寸 = 工作区宽高(到不了任务栏)
      info.ptMaxSize.x = work.right - work.left;
      info.ptMaxSize.y = work.bottom - work.top;
      info.ptMaxTrackSize.x = info.ptMaxSize.x;
      info.ptMaxTrackSize.y = info.ptMaxSize.y;
      return LRESULT(0);
    }
  }
  DefSubclassProc(hwnd, msg, wparam, lparam)
}

/// 给主窗挂 subclass。lib.rs setup 里 `win.hwnd()` 后调,传 `hwnd.0 as isize`
/// (跨 windows-crate 版本安全:只传裸指针值,内部用本 crate 的 HWND 重建)。
pub fn fix_maximize(hwnd_raw: isize) {
  let hwnd = HWND(hwnd_raw as *mut core::ffi::c_void);
  unsafe {
    let _ = SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, 0);
  }
}
