//! 锁「中毒」的单一解毒口。全项目拿 `std::sync::Mutex` / `RwLock` 只经这里。
//!
//! **病**:Rust 标准库的 `Mutex` 带毒锁机制 —— 持锁线程 panic,这把锁被永久标记成中毒,
//! 之后每次 `lock()` 都返回 `Err`。项目里 250+ 处写的是 `.lock().unwrap()` / `.expect(...)`,
//! 于是**第一个 panic 之后,每个访问点都跟着 panic**。要命的地方在于 tokio 任务 panic
//! 不会杀掉进程(而且 `panic = "abort"` 是根 `Cargo.toml` 里刻意不加的 —— `attach.rs` 的
//! `catch_unwind` 要靠 unwind),所以程序还活着、那块状态却彻底废掉,直到用户重启。
//! 实例:`media/mod.rs` 的 `playlist` 一中毒,队列 / 下一集 / 选集全失效。
//!
//! **解**:中毒不再传染 —— `PoisonError::into_inner()` 把数据拿回来接着用。代价是那份数据
//! **可能是半更新的**(panic 打断了某次修改)。对本项目的这些锁(注册表 / 缓存 / 播放态)
//! 「半更新的 HashMap」远好过「永久死掉的子系统」;唯一不能接受的是**悄悄**这么干。
//!
//! 所以这里有 `warn!`:§3.5「不静默失败」。它是那次 panic 在日志里唯一的痕迹
//! ——`.unwrap()` 的 panic 消息在正式版 Windows 会被 nativelog 捞走,而**恢复**这件事
//! 只有这儿会说。
//!
//! 用法:`use crate::lockext::LockExt;` 然后 `.lk()` 取代 `.lock().unwrap()`;
//! `RwLockExt` 的 `.rd()` / `.wr()` 取代 `.read()/.write().unwrap()`。
//! **禁止**再写裸 `.lock().unwrap()` / `.expect(...)`(规则 = AGENT.md §6.10;
//! 机器守卫 = `scripts/check-locks.sh`,已进 push/PR CI)。
//! 不影响 `tokio::sync::Mutex`(那是 `.lock().await`,压根没有毒锁概念)。

use std::panic::Location;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// 进程内累计恢复次数。只为给日志一个「第几次」,不参与任何判断。
static RECOVERED: AtomicU64 = AtomicU64::new(0);

/// 记一次中毒恢复。`#[cold]` = 这条路是异常路,别让它污染取锁的热路径代码布局。
///
/// **刻意不是每次都吼**:`larkwing.log` 到 5MB 就轮转(§4.11),而中毒的热锁
/// (比如播放进度)每次访问都吼一句,会把真正有价值的那条 panic 消息挤出日志。
/// 故只在前 8 次、之后每 1024 次说话,每次都带调用点与累计次数。
#[cold]
fn note_poison(loc: &'static Location<'static>) {
    let n = RECOVERED.fetch_add(1, Ordering::Relaxed) + 1;
    if n <= 8 || n.is_multiple_of(1024) {
        tracing::warn!(
            at = %loc,
            recovered = n,
            "锁中毒后恢复:此前有线程持锁 panic,这块状态可能是半更新的(去日志里找那次 panic)"
        );
    }
}

/// `std::sync::Mutex` 的解毒取锁。
pub trait LockExt<T: ?Sized> {
    /// 取锁;中毒则记一句 `warn!` 后把数据拿回来接着用,**永不 panic**。
    #[track_caller]
    fn lk(&self) -> MutexGuard<'_, T>;
}

impl<T: ?Sized> LockExt<T> for Mutex<T> {
    #[track_caller]
    fn lk(&self) -> MutexGuard<'_, T> {
        match self.lock() {
            Ok(g) => g,
            Err(p) => {
                note_poison(Location::caller());
                p.into_inner()
            }
        }
    }
}

/// `std::sync::RwLock` 的解毒取锁。
pub trait RwLockExt<T: ?Sized> {
    /// 读锁;中毒则记一句 `warn!` 后接着用,**永不 panic**。
    #[track_caller]
    fn rd(&self) -> RwLockReadGuard<'_, T>;
    /// 写锁;中毒则记一句 `warn!` 后接着用,**永不 panic**。
    #[track_caller]
    fn wr(&self) -> RwLockWriteGuard<'_, T>;
}

impl<T: ?Sized> RwLockExt<T> for RwLock<T> {
    #[track_caller]
    fn rd(&self) -> RwLockReadGuard<'_, T> {
        match self.read() {
            Ok(g) => g,
            Err(p) => {
                note_poison(Location::caller());
                p.into_inner()
            }
        }
    }

    #[track_caller]
    fn wr(&self) -> RwLockWriteGuard<'_, T> {
        match self.write() {
            Ok(g) => g,
            Err(p) => {
                note_poison(Location::caller());
                p.into_inner()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// 病的复现 + 药的验证:一个线程持锁 panic 之后,`.lock()` 永久报错、`.lk()` 照常给数据。
    #[test]
    fn poisoned_mutex_still_usable_via_lk() {
        let m = Arc::new(Mutex::new(vec![1, 2, 3]));
        let m2 = m.clone();
        let _ = std::thread::spawn(move || {
            let mut g = m2.lk();
            g.push(4);
            panic!("持锁 panic");
        })
        .join();

        // 病:标准库这条路从此永久 Err。
        assert!(m.lock().is_err(), "锁应已中毒");
        assert!(m.is_poisoned());
        // 药:数据拿回来了,而且是 panic 前那次修改的结果(= 可能半更新,但不是死的)。
        assert_eq!(&*m.lk(), &[1, 2, 3, 4]);
        // 中毒不会自愈,但每次都能取到 —— 不再是「一次 panic 废一整块」。
        assert_eq!(m.lk().len(), 4);
    }

    #[test]
    fn poisoned_rwlock_still_usable_via_rd_wr() {
        let l = Arc::new(RwLock::new(0i32));
        let l2 = l.clone();
        let _ = std::thread::spawn(move || {
            // 必须把 guard 绑到变量上跨过 panic —— `*l2.wr() = 7;` 那种临时 guard
            // 在语句末就 drop 了,panic 时压根没持锁、也就不会中毒。
            let mut g = l2.wr();
            *g = 7;
            panic!("持写锁 panic");
        })
        .join();

        assert!(l.write().is_err(), "锁应已中毒");
        assert_eq!(*l.rd(), 7);
        *l.wr() += 1;
        assert_eq!(*l.rd(), 8);
    }

    /// 没中毒时就是普通取锁,零行为差异。
    #[test]
    fn healthy_locks_behave_normally() {
        let m = Mutex::new(String::from("a"));
        m.lk().push('b');
        assert_eq!(&*m.lk(), "ab");
        assert!(!m.is_poisoned());

        let l = RwLock::new(1u8);
        *l.wr() = 2;
        assert_eq!(*l.rd(), 2);
    }
}
