//! 与平台、IPC、设备无关的任务状态机。
//!
//! 调用者注入单调时钟时间；没有新快照时仍可通过 [`StateMachine::status`]
//! 推进提示的到期时间。首次连接和应用重新打开只建立基线，不重放历史提示。
//!
//! ```
//! use std::time::Duration;
//! use codex_status_light::state::{Snapshot, StateMachine, Status, Task};
//!
//! let mut machine = StateMachine::default();
//! let snapshot = Snapshot {
//!     app_open: true,
//!     tasks: vec![Task { id: "main".into(), running: true, ..Task::default() }],
//! };
//! assert_eq!(machine.observe(snapshot, Duration::ZERO)?, Status::Running { roots: 1 });
//! # Ok::<(), anyhow::Error>(())
//! ```

use std::{collections::HashMap, time::Duration};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

/// 一个任务的规范化状态。子代理保留等待和错误，但不增加主任务数量。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub running: bool,
    pub waiting: bool,
    /// 主任务为 `None`；子代理可填父任务 ID 或来源标识。
    pub parent: Option<String>,
    /// 完成事件的稳定标识，通常为 turn ID；用于避免重复播放提示。
    pub completed: Option<String>,
    /// 错误事件的稳定标识，不包含错误正文。
    pub error: Option<String>,
}

/// 一次完整观察。应用退出时必须使用 `app_open = false` 和空任务列表。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub app_open: bool,
    pub tasks: Vec<Task>,
}

/// 设备无关的输出状态；颜色、速度和协议编号由设备适配器解释。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Status {
    Closed,
    Idle,
    Running { roots: usize },
    Waiting,
    Completed,
    Error,
}

/// 短时提示的持续时间，与设备动画速度无关。
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub completed: Duration,
    pub error: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            completed: Duration::from_secs(3),
            error: Duration::from_secs(5),
        }
    }
}

/// 聚合主任务、等待请求及去重后的完成/错误事件。
///
/// 优先级为：应用退出 > 错误 > 等待介入 > 完成提示 > 运行/空闲。
/// 被高优先级状态覆盖的提示仍按原时间到期，不排队补播。
#[derive(Debug, Default)]
pub struct StateMachine {
    tasks: HashMap<String, Task>,
    app_open: bool,
    initialized: bool,
    completed_until: Duration,
    error_until: Duration,
    last_observed: Duration,
    timing: Timing,
}

impl StateMachine {
    pub fn new(timing: Timing) -> Self {
        Self {
            timing,
            ..Self::default()
        }
    }

    /// 接受完整快照并返回当前状态。
    ///
    /// `now` 是从固定起点计算的单调时间。重复 ID、倒退时间和矛盾的
    /// 应用退出快照都会返回错误，并保持原状态不变。
    pub fn observe(&mut self, snapshot: Snapshot, now: Duration) -> Result<Status> {
        ensure!(now >= self.last_observed, "状态机时间不能倒退");
        ensure!(
            snapshot.app_open || snapshot.tasks.is_empty(),
            "应用已退出但快照仍包含任务"
        );
        let mut current = HashMap::new();
        for task in snapshot.tasks {
            ensure!(!task.id.is_empty(), "任务 ID 不能为空");
            ensure!(
                !current.contains_key(&task.id),
                "快照包含重复任务 ID: {}",
                task.id
            );
            current.insert(task.id.clone(), task);
        }
        let mut completed_until = self.completed_until;
        let mut error_until = self.error_until;
        if snapshot.app_open && self.initialized && self.app_open {
            for task in current.values() {
                // 新发现的历史任务也以其当前状态为基线，避免首次订阅时闪灯。
                let Some(previous) = self.tasks.get(&task.id) else {
                    continue;
                };
                if task.error.is_some() && task.error != previous.error {
                    error_until = now
                        .checked_add(self.timing.error)
                        .ok_or_else(|| anyhow::anyhow!("错误提示时间溢出"))?;
                }
                if task.parent.is_none()
                    && task.completed.is_some()
                    && task.completed != previous.completed
                {
                    completed_until = now
                        .checked_add(self.timing.completed)
                        .ok_or_else(|| anyhow::anyhow!("完成提示时间溢出"))?;
                }
            }
        }
        if !snapshot.app_open {
            completed_until = Duration::ZERO;
            error_until = Duration::ZERO;
        }
        self.tasks = current;
        self.app_open = snapshot.app_open;
        self.initialized = true;
        self.last_observed = now;
        self.completed_until = completed_until;
        self.error_until = error_until;
        Ok(self.status(now))
    }

    /// 按当前时间查询输出，计时到期不依赖新任务消息。
    pub fn status(&self, now: Duration) -> Status {
        if !self.app_open {
            return Status::Closed;
        }
        if now < self.error_until {
            return Status::Error;
        }
        if self.tasks.values().any(|t| t.waiting) {
            return Status::Waiting;
        }
        if now < self.completed_until {
            return Status::Completed;
        }
        let roots = self
            .tasks
            .values()
            .filter(|t| t.running && t.parent.is_none())
            .count();
        if roots == 0 {
            Status::Idle
        } else {
            Status::Running { roots }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn t(id: &str) -> Task {
        Task {
            id: id.into(),
            ..Task::default()
        }
    }
    fn at(n: u64) -> Duration {
        Duration::from_secs(n)
    }
    fn observe(m: &mut StateMachine, tasks: Vec<Task>, n: u64) -> Status {
        m.observe(
            Snapshot {
                app_open: true,
                tasks,
            },
            at(n),
        )
        .unwrap()
    }
    #[test]
    fn roots_only_but_child_waits_count() {
        let mut m = StateMachine::default();
        let a = Task {
            running: true,
            ..t("a")
        };
        let b = Task {
            running: true,
            ..t("b")
        };
        let child = Task {
            running: true,
            parent: Some("a".into()),
            ..t("child")
        };
        assert_eq!(
            observe(&mut m, vec![a.clone(), child.clone()], 0),
            Status::Running { roots: 1 }
        );
        assert_eq!(
            observe(&mut m, vec![a.clone(), b.clone(), child.clone()], 1),
            Status::Running { roots: 2 }
        );
        assert_eq!(
            observe(
                &mut m,
                vec![
                    a,
                    b,
                    Task {
                        waiting: true,
                        ..child
                    }
                ],
                2
            ),
            Status::Waiting
        );
    }
    #[test]
    fn priority_expiry_and_no_replay() {
        let mut m = StateMachine::default();
        let old = Task {
            completed: Some("old".into()),
            error: Some("old".into()),
            ..t("a")
        };
        assert_eq!(observe(&mut m, vec![old], 0), Status::Idle);
        let done = Task {
            completed: Some("new".into()),
            ..t("a")
        };
        assert_eq!(observe(&mut m, vec![done.clone()], 1), Status::Completed);
        assert_eq!(m.status(Duration::from_millis(3999)), Status::Completed);
        assert_eq!(m.status(at(4)), Status::Idle);
        let wait = Task {
            waiting: true,
            ..t("b")
        };
        assert_eq!(
            observe(&mut m, vec![done.clone(), wait.clone()], 5),
            Status::Waiting
        );
        let failure = Task {
            error: Some("e1".into()),
            ..done
        };
        assert_eq!(
            observe(&mut m, vec![failure.clone(), wait.clone()], 6),
            Status::Error
        );
        assert_eq!(
            observe(&mut m, vec![failure.clone(), wait.clone()], 10),
            Status::Error
        );
        assert_eq!(m.status(at(11)), Status::Waiting);
        assert_eq!(observe(&mut m, vec![failure], 12), Status::Idle);
    }
    #[test]
    fn closing_clears_notifications_and_reopening_baselines() {
        let mut m = StateMachine::default();
        observe(&mut m, vec![t("a")], 0);
        let done = Task {
            completed: Some("done".into()),
            ..t("a")
        };
        assert_eq!(observe(&mut m, vec![done.clone()], 1), Status::Completed);
        assert_eq!(
            m.observe(Snapshot::default(), at(2)).unwrap(),
            Status::Closed
        );
        assert_eq!(observe(&mut m, vec![done], 3), Status::Idle);
    }
    #[test]
    fn invalid_snapshot_is_atomic() {
        let mut m = StateMachine::default();
        observe(
            &mut m,
            vec![Task {
                running: true,
                ..t("a")
            }],
            2,
        );
        assert!(
            m.observe(
                Snapshot {
                    app_open: true,
                    tasks: vec![t("a"), t("a")]
                },
                at(3)
            )
            .is_err()
        );
        assert!(m.observe(Snapshot::default(), at(1)).is_err());
        assert_eq!(m.status(at(3)), Status::Running { roots: 1 });
    }
    #[test]
    fn child_completion_and_new_history_do_not_pulse() {
        let mut m = StateMachine::default();
        let child = Task {
            parent: Some("root".into()),
            ..t("child")
        };
        observe(&mut m, vec![child.clone()], 0);
        let done = Task {
            completed: Some("done".into()),
            ..child
        };
        let history = Task {
            completed: Some("historic".into()),
            ..t("history")
        };
        assert_eq!(observe(&mut m, vec![done, history], 1), Status::Idle);
    }
}
