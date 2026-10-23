//! 正在处理的对话请求计数，以及在线更新期间的闸门。
//! 更新会重启程序：有请求未完成时不更新；更新开始后新请求一律拒收，避免中断生成、丢失用量统计
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static BUSY: AtomicUsize = AtomicUsize::new(0);
static PAUSED: AtomicBool = AtomicBool::new(false);

/// 存活期间计为一个进行中的请求。同一请求可同时持有多个（处理函数一个、流式响应一个），只用于判断是否为零
pub struct Busy(());

impl Busy {
    pub fn new() -> Self {
        BUSY.fetch_add(1, Ordering::SeqCst);
        Self(())
    }
}

impl Default for Busy {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        BUSY.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 当前是否有请求在处理
pub fn any() -> bool {
    BUSY.load(Ordering::SeqCst) > 0
}

/// 关上闸门：之后收到的对话请求被拒绝
pub fn pause() {
    PAUSED.store(true, Ordering::SeqCst);
}

/// 打开闸门
pub fn resume() {
    PAUSED.store(false, Ordering::SeqCst);
}

/// 闸门是否关着
pub fn paused() -> bool {
    PAUSED.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_counts_while_it_is_alive() {
        let before = BUSY.load(Ordering::SeqCst);
        let a = Busy::new();
        let b = Busy::new();
        assert_eq!(BUSY.load(Ordering::SeqCst), before + 2);
        assert!(any());
        drop(a);
        drop(b);
        assert_eq!(BUSY.load(Ordering::SeqCst), before);
    }

    #[test]
    fn the_gate_opens_and_closes() {
        assert!(!paused());
        pause();
        assert!(paused());
        resume();
        assert!(!paused());
    }
}
