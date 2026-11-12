//! 文件系统变更通知（spec §5.1：`FspFileSystemNotify`）。
//!
//! 写/删除/重命名回调把事件推进 [`NotifyHub`] 队列；winfsp-rs 的 notify
//! timer（[`crate::mount`] 以固定周期轮询）调用 `should_notify` 取走事件、
//! 经 `notify` 发给内核，shell/编辑器的目录监视因此可见。
//!
//! 过滤器与动作常量见 [`crate::win`]；名字一律用挂载内绝对形式
//! （`\wt-1\src\a.rs`）。

use std::collections::VecDeque;

use parking_lot::Mutex;
use winfsp::U16CString;
use winfsp::filesystem::WideNameInfo;
use winfsp::notify::{Notifier, NotifyingFileSystemContext, NotifyInfo};

use crate::win::NOTIFY_FILTER;

/// 单条变更事件：挂载内路径 + `FILE_ACTION_*`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyEvent {
    /// 挂载内绝对路径（反斜杠，如 `\wt-1\src\a.rs`）。
    pub name: String,
    /// `FILE_ACTION_*` 常量（[`crate::win`]）。
    pub action: u32,
}

pub struct NotifyHub {
    queue: Mutex<VecDeque<NotifyEvent>>,
}

impl NotifyHub {
    pub fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
        }
    }

    /// 入队一条事件。
    pub fn push(&self, name: &str, action: u32) {
        let mut q = self.queue.lock();
        // 有界：极端积压时丢弃最旧事件（监视器会做重新枚举兜底）。
        if q.len() >= 4096 {
            q.pop_front();
        }
        q.push_back(NotifyEvent {
            name: name.to_string(),
            action,
        });
    }

    /// timer 轮询取走一条。
    pub fn pop(&self) -> Option<NotifyEvent> {
        self.queue.lock().pop_front()
    }

    pub fn len(&self) -> usize {
        self.queue.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for NotifyHub {
    fn default() -> Self {
        Self::new()
    }
}

/// winfsp-rs notify 框架的接入点：`R = NotifyEvent`（哨兵携带事件本体）。
impl NotifyingFileSystemContext<NotifyEvent> for crate::context::WtfsContext {
    fn should_notify(&self) -> Option<NotifyEvent> {
        self.notify_hub().pop()
    }

    fn notify(&self, event: NotifyEvent, notifier: &Notifier) {
        let Ok(name) = U16CString::from_str(&event.name) else {
            tracing::warn!(name = %event.name, "notify name not convertible");
            return;
        };
        let mut info = NotifyInfo::<255>::new();
        info.filter = NOTIFY_FILTER;
        info.action = event.action;
        if info.set_name_cstr(&name).is_err() {
            tracing::warn!(name = %event.name, "notify name too long");
            return;
        }
        notifier.notify(&mut info);
    }
}

// 模块级契约：notify 事件名必须是挂载内形式（由 notify_change 构造保证）。

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win::{FILE_ACTION_ADDED, FILE_ACTION_REMOVED};

    #[test]
    fn hub_fifo_and_bounded() {
        let hub = NotifyHub::new();
        assert!(hub.is_empty());
        hub.push("\\wt-1\\a.txt", FILE_ACTION_ADDED);
        hub.push("\\wt-1\\b.txt", FILE_ACTION_REMOVED);
        assert_eq!(hub.len(), 2);
        assert_eq!(
            hub.pop(),
            Some(NotifyEvent {
                name: "\\wt-1\\a.txt".into(),
                action: FILE_ACTION_ADDED
            })
        );
        assert_eq!(
            hub.pop(),
            Some(NotifyEvent {
                name: "\\wt-1\\b.txt".into(),
                action: FILE_ACTION_REMOVED
            })
        );
        assert!(hub.pop().is_none());
        assert!(hub.is_empty());
    }
}
