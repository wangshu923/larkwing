//! 存储:执行层(db)+ 自治域(每域一个文件 = 表 + 迁移 + Repo)。
//! "自治"落在代码组织层,不落在存储层:一个 SQLite 文件、一条迁移流、schema 不隔离。

mod db;

pub mod briefings;
pub mod channels;
pub mod chat;
pub mod cloned_voices;
pub mod diary;
pub mod fsops;
pub mod jobs;
pub mod media_progress;
pub mod memory;
pub mod settings;
pub mod todos;
pub mod usage;
pub mod users;
pub mod voiceprints;

pub use briefings::{Briefing, BriefingRepo};
pub use channels::{ChannelRepo, ChannelThread};
pub use chat::{ChatRepo, Conversation, Message, SearchHit};
pub use cloned_voices::{ClonedVoice, ClonedVoiceRepo};
pub use db::Db;
pub use diary::{DiaryEntry, DiaryRepo};
pub(crate) use db::now_ms; // 给 engine/consolidate 跑维护轮时取 now(db 模块本身私有)

/// 转义 LIKE 通配符(`% _ \`)让查询串当字面量匹配(配 SQL 里 `ESCAPE '\'`)。
/// 单源:chat 搜索、memory 纠错替换共用,改转义规则只动这里(§6.3 一处真相)。
pub(crate) fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}
pub use fsops::{FsOpRepo, FsOpRow};
pub use jobs::{Job, JobRepo};
pub use media_progress::{MediaProgressRepo, Progress};
pub use memory::{MaintenanceLog, Memory, MemoryRepo};
pub use settings::SettingsRepo;
pub use todos::{Todo, TodoRepo};
pub use usage::{UsageRepo, UsageRound, UsageTotals};
pub use users::{User, UserRepo};
pub use voiceprints::VoiceprintRepo;

/// 纯装配袋:没有方法,每域一个字段。加新域 = 新文件 + 这里一个字段 + open() 注册一行。
#[derive(Clone)]
pub struct Store {
    pub users: UserRepo,
    pub chat: ChatRepo,
    pub channels: ChannelRepo,
    pub cloned_voices: ClonedVoiceRepo,
    pub memory: MemoryRepo,
    pub settings: SettingsRepo,
    pub usage: UsageRepo,
    pub briefings: BriefingRepo,
    pub jobs: JobRepo,
    pub fsops: FsOpRepo,
    pub voiceprints: VoiceprintRepo,
    pub media_progress: MediaProgressRepo,
    pub todos: TodoRepo,
    pub diary: DiaryRepo,
}

/// 全部域迁移(Store::open 执行;恢复备份预检拿它判「备份是否来自更新版本」)。
fn all_migrations() -> Vec<db::Migration> {
    [
        users::MIGRATIONS,
        settings::MIGRATIONS,
        chat::MIGRATIONS,
        channels::MIGRATIONS,
        cloned_voices::MIGRATIONS,
        memory::MIGRATIONS,
        usage::MIGRATIONS,
        briefings::MIGRATIONS,
        jobs::MIGRATIONS,
        fsops::MIGRATIONS,
        voiceprints::MIGRATIONS,
        media_progress::MIGRATIONS,
        todos::MIGRATIONS,
        diary::MIGRATIONS,
    ]
    .concat()
}

/// 本版程序认识的全部迁移 id(供 datadir 恢复预检:备份库里出现不认识的 id = 来自更新版本)。
pub fn migration_ids() -> Vec<&'static str> {
    all_migrations().iter().map(|m| m.id).collect()
}

impl Store {
    pub fn open(path: &std::path::Path) -> anyhow::Result<Store> {
        let db = Db::open(path)?;
        let all = all_migrations();
        db.migrate(&all)?;
        Ok(Store {
            users: UserRepo::new(db.clone()),
            chat: ChatRepo::new(db.clone()),
            channels: ChannelRepo::new(db.clone()),
            cloned_voices: ClonedVoiceRepo::new(db.clone()),
            memory: MemoryRepo::new(db.clone()),
            settings: SettingsRepo::new(db.clone()),
            usage: UsageRepo::new(db.clone()),
            briefings: BriefingRepo::new(db.clone()),
            jobs: JobRepo::new(db.clone()),
            fsops: FsOpRepo::new(db.clone()),
            voiceprints: VoiceprintRepo::new(db.clone()),
            media_progress: MediaProgressRepo::new(db.clone()),
            todos: TodoRepo::new(db.clone()),
            diary: DiaryRepo::new(db),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_db(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "larkwing_test_{}_{}.db",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn open_twice_migrations_are_idempotent() {
        let path = temp_db("reopen");
        {
            let store = Store::open(&path).unwrap();
            store.users.ensure_default_user().unwrap();
        }
        let store = Store::open(&path).unwrap();
        let users = store.users.list().unwrap();
        assert_eq!(users.len(), 1);
    }

    #[test]
    fn ensure_default_user_is_idempotent() {
        let store = Store::open(&temp_db("default_user")).unwrap();
        let a = store.users.ensure_default_user().unwrap();
        let b = store.users.ensure_default_user().unwrap();
        assert_eq!(a.id, b.id);
        assert_eq!(store.users.list().unwrap().len(), 1);
    }

    #[test]
    fn settings_roundtrip_and_scopes() {
        let store = Store::open(&temp_db("settings")).unwrap();
        assert_eq!(store.settings.get(None, "llm.api_key").unwrap(), None);
        store.settings.set(None, "llm.api_key", "sk-1").unwrap();
        store.settings.set(None, "llm.api_key", "sk-2").unwrap(); // 覆盖
        store.settings.set(Some(7), "llm.api_key", "user-key").unwrap(); // 不串 scope
        assert_eq!(
            store.settings.get(None, "llm.api_key").unwrap().as_deref(),
            Some("sk-2")
        );
        assert_eq!(
            store.settings.get(Some(7), "llm.api_key").unwrap().as_deref(),
            Some("user-key")
        );
    }

    #[test]
    fn chat_flow_title_and_cascade_delete() {
        let store = Store::open(&temp_db("chat")).unwrap();
        let user = store.users.ensure_default_user().unwrap();
        let conv = store.chat.create_conversation(user.id, "companion").unwrap();

        store
            .chat
            .append_message(conv.id, "user", "今天天气真好,我们去散步吧!")
            .unwrap();
        store.chat.append_message(conv.id, "assistant", "汪!好呀好呀!").unwrap();

        // 标题 = 首条用户消息截断
        let got = store.chat.get_conversation(conv.id).unwrap().unwrap();
        assert!(got.title.starts_with("今天天气真好"));

        assert_eq!(store.chat.count_messages(conv.id).unwrap(), 2);
        let recent = store.chat.recent_messages(conv.id, 10).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].role, "user"); // 升序返回

        let page = store.chat.messages_page(conv.id, 1, 10).unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].role, "assistant");

        // 级联删除
        store.chat.delete_conversation(conv.id).unwrap();
        assert_eq!(store.chat.count_messages(conv.id).unwrap(), 0);
        assert!(store.chat.get_conversation(conv.id).unwrap().is_none());
    }

    #[test]
    fn search_messages_excludes_internal_rows_and_escapes_wildcards() {
        let store = Store::open(&temp_db("search")).unwrap();
        let user = store.users.ensure_default_user().unwrap();
        let conv = store.chat.create_conversation(user.id, "companion").unwrap();
        store.chat.append_message(conv.id, "user", "帮我整理下载文件夹里的电影").unwrap();
        store.chat.append_message(conv.id, "assistant", "好的,正在整理你的电影目录").unwrap();
        // tool / event 内部行也含「电影」,但不该被搜出来(UI 不渲染它们)。
        store.chat.append_message(conv.id, "tool", "fs_find: 电影 → 12 个文件").unwrap();
        store.chat.append_message(conv.id, "event", "提醒:该看电影了").unwrap();

        let hits = store.chat.search_messages(user.id, "电影", 50).unwrap();
        assert_eq!(hits.len(), 2, "只命中 user/assistant,排除 tool/event: {hits:?}");
        assert!(hits.iter().all(|h| h.role == "user" || h.role == "assistant"));
        assert!(hits.iter().all(|h| h.conversation_id == conv.id));

        // 空查询 = 空结果。
        assert!(store.chat.search_messages(user.id, "  ", 50).unwrap().is_empty());
        // 通配符当字面量:没有消息含字面 `%` → 0 命中(没转义会被当「匹配全部」)。
        assert!(store.chat.search_messages(user.id, "%", 50).unwrap().is_empty());
        // 归属隔离:别的用户搜不到。
        assert!(store.chat.search_messages(user.id + 999, "电影", 50).unwrap().is_empty());
    }

    #[test]
    fn memory_belongs_to_user() {
        let store = Store::open(&temp_db("memory")).unwrap();
        let user = store.users.ensure_default_user().unwrap();
        store.memory.add(user.id, "profile", "喜欢喝美式", "explicit").unwrap();
        store.memory.add(user.id, "fact", "养了一只猫叫团子", "explicit").unwrap();
        let memories = store.memory.list(user.id).unwrap();
        assert_eq!(memories.len(), 2);
        assert_eq!(memories[0].kind, "profile");
    }
}
