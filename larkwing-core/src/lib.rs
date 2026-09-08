//! Larkwing 引擎(纯 Rust,不依赖 tauri —— 硬边界)。
//! 模块边界 = 未来的 crate 切割线:llm 不依赖 store,engine 是唯一合流点。

pub mod archive;
pub mod attach;
pub mod autobackup;
pub mod bgtasks;
pub mod bus;
pub mod channels;
pub mod components;
pub mod confirm;
pub mod crypto;
pub mod datadir;
pub mod engine;
pub mod eval;
pub mod files;
pub mod ftp;
pub mod llm;
// 锁「中毒」的单一解毒口(`.lk()` / `.rd()` / `.wr()`);壳层也用,故 pub。
pub mod lockext;
pub mod media;
pub mod net;
pub mod scenes;
pub mod secrets;
pub mod scheduler;
pub mod skills_builtin;
pub mod store;
pub mod tasks;
// 文本小工具(按字符截断的单一真相源);crate 内部件,不过桥。
pub(crate) mod text;
pub mod tools;
pub mod usage;
pub mod voice;
pub mod weather;
pub mod web;
pub mod webrender;
