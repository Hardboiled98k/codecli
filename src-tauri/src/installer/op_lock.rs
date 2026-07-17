// SPDX-License-Identifier: MPL-2.0
//! 全局写操作互斥，防止并发 install/config 互踩

use std::sync::{Mutex, MutexGuard};

static OP_LOCK: Mutex<()> = Mutex::new(());

pub fn with_op_lock<T, F>(f: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T, String>,
{
    let _guard: MutexGuard<'_, ()> = match OP_LOCK.try_lock() {
        Ok(g) => g,
        Err(std::sync::TryLockError::WouldBlock) => {
            return Err("已有安装/配置任务进行中，请稍候再试".into());
        }
        Err(std::sync::TryLockError::Poisoned(p)) => {
            // panic 后恢复锁，避免永久不可用
            p.into_inner()
        }
    };
    // purge 的 Prepared/Quarantined marker 是跨进程 durable 状态。
    // 任意后续写操作都必须先完成它；否则新 ownership/journal 可能写回
    // 一个之后会被 purge 恢复流程删除的旧 inode。
    super::config::recover_pending_state_dir_purge()?;
    // 所有当前调用点都是 Tauri 公共写操作边界。获锁后清除上一次
    // 操作留下的 cancel；运行期间新的 cancel 仍会被子进程循环观察到。
    super::cmd::clear_cancel();
    f()
}

/// 公共写操作入口：先获取全局锁，再清掉「上一次」操作留下的取消标志。
///
/// 只能在最外层 command 边界使用。组合操作（如批量升级）内部必须调用
/// `*_sync` 函数，不得再清标志，否则会吞掉用户在批量中途发出的取消。
pub fn with_new_operation<T, F>(f: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T, String>,
{
    with_op_lock(f)
}
