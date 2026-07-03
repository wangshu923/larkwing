// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
  // 诊断探针入口:克隆加载失败时,主进程拉起 `exe --probe-zipvoice <模型目录>` 抓 sherpa 的
  // stderr(Windows 预编译 sherpa 是 /MT 静态 CRT,报错进程内接不到,只有子进程管道能收;
  // 见 core voice/tts.rs::probe_zipvoice)。在 tauri 装配前早退:不碰单实例/窗口/数据目录。
  let args: Vec<String> = std::env::args().collect();
  if args.get(1).map(String::as_str) == Some("--probe-zipvoice") {
    let ok = match args.get(2) {
      Some(dir) => larkwing_core::voice::probe_zipvoice(std::path::Path::new(dir)),
      None => {
        eprintln!("[probe] 缺模型目录参数");
        false
      }
    };
    std::process::exit(if ok { 0 } else { 1 });
  }
  app_lib::run();
}
