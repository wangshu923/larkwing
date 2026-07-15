//! Larkwing 引擎(纯 Rust,不依赖 tauri —— 硬边界)。
//! 模块边界 = 未来的 crate 切割线:llm 不依赖 store,engine 是唯一合流点。

pub mod attach;
pub mod bus;
pub mod channels;
pub mod components;
pub mod confirm;
pub mod crypto;
pub mod datadir;
pub mod engine;
pub mod eval;
pub mod files;
pub mod llm;
pub mod media;
pub mod net;
pub mod scenes;
pub mod secrets;
pub mod scheduler;
pub mod store;
pub mod tasks;
pub mod tools;
pub mod voice;
pub mod weather;
pub mod web;
pub mod webrender;
