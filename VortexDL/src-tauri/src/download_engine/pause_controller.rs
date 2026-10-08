//! pause_controller.rs - 真暂停/恢复/取消的统一控制器
//!
//! 设计目标:
//! 1. 废除 downloader.rs 旧版 pause_download 用的 `handle.abort()` (会导致进度回调
//!    与 pause 状态竞争, 前端进度条抽搐 - 详见 project_memory 2026-08-14 决策)
//! 2. HTTP 下载走新 dynamic_engine: 通过 watch::channel 广播 EngineState,
//!    workers 主动 select! 在 resume_notify 上等待, 真暂停不写文件不读网络
//! 3. BT 下载兼容旧引擎: stop_notify 唤醒 + cancel_flag 终止, 恢复时 spawn 新任务
//!
//! 与前端契约的状态机:
//!   starting → running ⇄ paused → completed | failed | canceled
//! 暂停态下 progress_loop 不再发射 download-progress 事件, 前端只接收一次 paused 切换.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{watch, Notify};

use swiftfetch::dynamic_engine::EngineState;

// ============================================================
// PauseController - 双模 (HTTP 新引擎 / BT 旧引擎)
// ============================================================

pub enum PauseController {
    /// HTTP 下载: 新 dynamic_engine 的状态广播 + Notify 恢复 + cancel_flag 取消
    Http {
        state_tx: watch::Sender<EngineState>,
        cancel_flag: Arc<AtomicBool>,
        resume_notify: Arc<Notify>,
    },
    /// BT 下载: 旧 EngineContext 的 stop_notify + cancel_flag
    /// (BT 引擎内部模块从 stop_notify 退出, 无需 state 广播)
    Bt {
        stop_notify: Arc<Notify>,
        cancel_flag: Arc<AtomicBool>,
    },
}

impl PauseController {
    // ---------- HTTP 模式 ----------
    pub fn new_http(
        state_tx: watch::Sender<EngineState>,
        cancel_flag: Arc<AtomicBool>,
        resume_notify: Arc<Notify>,
    ) -> Self {
        Self::Http { state_tx, cancel_flag, resume_notify }
    }

    // ---------- BT 模式 ----------
    pub fn new_bt(stop_notify: Arc<Notify>, cancel_flag: Arc<AtomicBool>) -> Self {
        Self::Bt { stop_notify, cancel_flag }
    }

    // ---------- 公共 API ----------

    /// 暂停下载
    /// - HTTP: 广播 EngineState::Paused, workers 主动进入 select! 等待
    /// - BT: 通知 stop_notify, 各模块从 select! 退出 (与 abort 等效但不 panic-safe)
    pub fn pause(&self) {
        match self {
            Self::Http { state_tx, .. } => {
                // 广播 Paused: 所有 worker 在下一次循环检查时进入 select!
                let _ = state_tx.send(EngineState::Paused);
            }
            Self::Bt { stop_notify, .. } => {
                // 通知所有 select! 中的 BT 模块退出 (但保留 cancel_flag=false)
                stop_notify.notify_waiters();
            }
        }
    }

    /// 恢复下载
    /// - HTTP: 广播 EngineState::Running + 唤醒 resume_notify, workers 立即继续
    /// - BT: 不直接支持热恢复, 调用方 (commands::resume_download) 需 spawn 新任务
    ///   此处仅清 cancel_flag, 实际由 commands.rs 重新 spawn_download
    pub fn resume(&self) {
        match self {
            Self::Http { state_tx, cancel_flag, resume_notify } => {
                // 清 cancel (防止上次 cancel 未清)
                cancel_flag.store(false, Ordering::Relaxed);
                let _ = state_tx.send(EngineState::Running);
                resume_notify.notify_waiters();
            }
            Self::Bt { cancel_flag, .. } => {
                // BT 旧引擎: 仅清 cancel_flag, 实际恢复由 spawn_download 完成
                cancel_flag.store(false, Ordering::Relaxed);
            }
        }
    }

    /// 取消下载
    /// - HTTP: 广播 Canceled + cancel_flag=true + 唤醒所有等待中的 worker
    /// - BT: cancel_flag=true + stop_notify 唤醒, 各模块退出
    pub fn cancel(&self) {
        match self {
            Self::Http { state_tx, cancel_flag, resume_notify } => {
                cancel_flag.store(true, Ordering::Relaxed);
                let _ = state_tx.send(EngineState::Canceled);
                resume_notify.notify_waiters();
            }
            Self::Bt { stop_notify, cancel_flag } => {
                cancel_flag.store(true, Ordering::Relaxed);
                stop_notify.notify_waiters();
            }
        }
    }

    /// 当前是否已取消
    pub fn is_canceled(&self) -> bool {
        match self {
            Self::Http { cancel_flag, .. } | Self::Bt { cancel_flag, .. } => {
                cancel_flag.load(Ordering::Relaxed)
            }
        }
    }

    /// 当前状态 (HTTP 模式从 watch 读取; BT 模式返回 Running 由调用方维护)
    pub fn current_state(&self) -> EngineState {
        match self {
            Self::Http { state_tx, .. } => state_tx.borrow().clone(),
            Self::Bt { .. } => EngineState::Running,
        }
    }
}

// ============================================================
// 单元测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_http_pause_resume_cancel() {
        let (state_tx, mut state_rx) = watch::channel(EngineState::Starting);
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let resume_notify = Arc::new(Notify::new());
        let pc = PauseController::new_http(state_tx, cancel_flag.clone(), resume_notify.clone());

        // 初始 Starting
        assert_eq!(*state_rx.borrow(), EngineState::Starting);
        assert!(!pc.is_canceled());

        // Pause
        pc.pause();
        assert_eq!(*state_rx.borrow(), EngineState::Paused);

        // Resume
        pc.resume();
        assert_eq!(*state_rx.borrow(), EngineState::Running);
        assert!(!cancel_flag.load(Ordering::Relaxed));

        // Cancel
        pc.cancel();
        assert!(pc.is_canceled());
        assert_eq!(*state_rx.borrow(), EngineState::Canceled);
    }

    #[tokio::test]
    async fn test_bt_cancel() {
        let stop_notify = Arc::new(Notify::new());
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let pc = PauseController::new_bt(stop_notify, cancel_flag.clone());

        assert!(!pc.is_canceled());
        pc.cancel();
        assert!(pc.is_canceled());
        // BT current_state 永远 Running (由 task.state 维护)
        assert_eq!(pc.current_state(), EngineState::Running);
    }

    #[tokio::test]
    async fn test_resume_clears_cancel_flag() {
        let (state_tx, _) = watch::channel(EngineState::Paused);
        let cancel_flag = Arc::new(AtomicBool::new(true));
        let resume_notify = Arc::new(Notify::new());
        let pc = PauseController::new_http(state_tx, cancel_flag.clone(), resume_notify);

        assert!(pc.is_canceled());
        pc.resume();
        assert!(!pc.is_canceled());
    }
}
