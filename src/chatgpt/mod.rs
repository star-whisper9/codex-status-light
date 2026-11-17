//! 本机桌面任务状态：只读 SQLite 发现会话，再订阅内部 IPC 状态流。
//! 按实际消息结构校验兼容性，不以协议版本号拒绝状态流。
mod history;
mod ipc;

use crate::state::{Snapshot, Task};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    time::{Duration, Instant},
};

struct Live {
    state: Value,
    task: Task,
    revision: u64,
    owner: String,
}

#[derive(Default)]
struct Streams {
    subscribed: HashSet<String>,
    live: HashMap<String, Live>,
    resync: HashMap<String, Instant>,
}
impl Streams {
    fn change(&mut self, mut message: Value) -> Result<Option<String>> {
        let tid = message["params"]["conversationId"]
            .as_str()
            .context("状态消息缺少 conversationId")?
            .to_owned();
        if !self.subscribed.contains(&tid) {
            return Ok(None);
        }
        let owner = message["sourceClientId"]
            .as_str()
            .context("状态消息缺少 sourceClientId")?
            .to_owned();
        let change = &mut message["params"]["change"];
        let revision = change["revision"]
            .as_u64()
            .context("状态消息缺少 revision")?;
        match change["type"].as_str() {
            Some("snapshot") => {
                let state = change
                    .get_mut("conversationState")
                    .context("状态快照缺少 conversationState")?
                    .take();
                ensure!(
                    state["id"].as_str() == Some(tid.as_str()),
                    "状态快照与会话 ID 不一致"
                );
                let task = task_from_state(&tid, &state)?;
                self.live.insert(
                    tid.to_owned(),
                    Live {
                        state,
                        task,
                        revision,
                        owner,
                    },
                );
                self.resync.remove(&tid);
            }
            Some("patches") => {
                let base = change["baseRevision"]
                    .as_u64()
                    .context("增量消息缺少 baseRevision")?;
                ensure!(revision > base, "增量 revision 未递增");
                let matches = self
                    .live
                    .get(&tid)
                    .is_some_and(|live| live.revision == base && live.owner == owner);
                if !matches {
                    if !self.resync.contains_key(&tid) {
                        self.resync
                            .insert(tid.to_owned(), Instant::now() + Duration::from_secs(3));
                        return Ok(Some(tid.to_owned()));
                    }
                    return Ok(None);
                }
                let live = self.live.get_mut(&tid).unwrap();
                ipc::apply_patches(&mut live.state, &change["patches"])?;
                live.task = task_from_state(&tid, &live.state)?;
                live.revision = revision;
            }
            _ => bail!("未知桌面状态变更类型"),
        }
        Ok(None)
    }
    fn owner_disconnected(&mut self, owner: &str) -> Result<()> {
        for live in self.live.values().filter(|live| live.owner == owner) {
            let task = &live.task;
            ensure!(
                !task.running && !task.waiting,
                "活动任务的桌面状态拥有者已断开"
            );
        }
        self.live.retain(|_, live| live.owner != owner);
        Ok(())
    }

    fn check_deadlines(&self) -> Result<()> {
        ensure!(
            !self
                .resync
                .values()
                .any(|deadline| Instant::now() >= *deadline),
            "桌面状态重同步超时"
        );
        Ok(())
    }
}

/// 活动标志与人工请求共同决定等待；普通 item/tool/call 不表示需要用户介入。
fn task_from_state(id: &str, state: &Value) -> Result<Task> {
    let runtime = state
        .get("threadRuntimeStatus")
        .context("桌面状态缺少 threadRuntimeStatus")?;
    let kind = runtime["type"].as_str().context("运行状态缺少 type")?;
    ensure!(
        ["active", "idle", "notLoaded", "systemError"].contains(&kind),
        "未知桌面运行状态：{kind}"
    );
    let mut waiting = false;
    if kind == "active" {
        for flag in runtime["activeFlags"]
            .as_array()
            .context("active 状态缺少 activeFlags")?
        {
            let flag = flag.as_str().context("activeFlags 包含非字符串")?;
            waiting |= matches!(flag, "waitingOnUserInput" | "waitingOnApproval");
        }
    }
    if let Some(requests) = state.get("requests") {
        for request in requests.as_array().context("requests 必须是数组")? {
            let method = request["method"].as_str().context("请求缺少 method")?;
            waiting |= request["completed"] != true
                && matches!(
                    method,
                    "item/commandExecution/requestApproval"
                        | "item/fileChange/requestApproval"
                        | "item/permissions/requestApproval"
                        | "item/tool/requestUserInput"
                        | "item/tool/requestOptionPicker"
                        | "mcpServer/elicitation/request"
                );
        }
    }
    let (canonical, legacy) = if state["turnHistory"]["kind"] == "canonical" {
        (
            Some(
                state["turnHistory"]["history"]["entitiesByKey"]
                    .as_object()
                    .context("canonical 历史缺少 entitiesByKey")?,
            ),
            None,
        )
    } else {
        (
            None,
            state
                .get("turns")
                .map(|value| value.as_array().context("turns 必须是数组"))
                .transpose()?,
        )
    };
    // 单次遍历保留 max_by_key 的末项同值规则，并验证多轮历史的开始时间。
    let turns = canonical
        .into_iter()
        .flat_map(|turns| turns.values())
        .chain(legacy.into_iter().flat_map(|turns| turns.iter()))
        .filter(|turn| turn["turnId"].is_string());
    let mut latest = None;
    let mut latest_time = None;
    let mut turn_count = 0;
    let mut missing_time = false;
    for turn in turns {
        let started = turn["turnStartedAtMs"].as_i64();
        turn_count += 1;
        missing_time |= started.is_none();
        if latest.is_none() || started >= latest_time {
            latest = Some(turn);
            latest_time = started;
        }
    }
    ensure!(
        turn_count <= 1 || !missing_time,
        "多轮历史缺少可靠的轮次开始时间"
    );
    let turn_id = latest.and_then(|turn| turn["turnId"].as_str());
    let mut completed = None;
    let mut error = None;
    if let Some(turn) = latest {
        let status = turn["status"].as_str().context("轮次缺少 status")?;
        ensure!(
            ["inProgress", "completed", "failed", "interrupted"].contains(&status),
            "未知轮次状态：{status}"
        );
        if status == "completed" {
            completed = turn_id.map(str::to_owned);
        }
        if status == "failed" || turn.get("error").is_some_and(|error| !error.is_null()) {
            error = turn_id.map(str::to_owned);
        }
    }
    if let Some(items) = latest.and_then(|turn| turn.get("items")) {
        let items = items.as_array().context("轮次 items 必须是数组")?;
        if let Some(marker) = items
            .iter()
            .rev()
            .find(|item| item["type"] == "autoReviewInterruptionWarning")
        {
            let marker_id = marker["id"].as_str().context("自动审批中断标记缺少 id")?;
            error = Some(format!(
                "{}:auto-review:{}",
                turn_id.unwrap_or(id),
                marker_id
            ));
        }
    }
    if kind == "systemError" {
        error = Some(format!("{}:systemError", turn_id.unwrap_or(id)));
    }
    Ok(Task {
        id: id.to_owned(),
        running: kind == "active",
        waiting,
        parent: None,
        completed,
        error,
    })
}

/// 持有一个只读桌面订阅连接。实例应由同一个采集线程调用。
pub struct Source {
    home: PathBuf,
    #[cfg(target_os = "macos")]
    connection: Option<ipc::Connection>,
    #[cfg(target_os = "macos")]
    client_id: Option<String>,
    streams: Streams,
}
impl Source {
    pub fn new(home: PathBuf) -> Self {
        Self {
            home,
            streams: Streams::default(),
            #[cfg(target_os = "macos")]
            connection: None,
            #[cfg(target_os = "macos")]
            client_id: None,
        }
    }

    #[cfg(not(target_os = "macos"))]
    pub fn snapshot(&mut self) -> Result<Snapshot> {
        bail!("ChatGPT 桌面状态采集目前仅支持 macOS")
    }

    #[cfg(target_os = "macos")]
    pub fn snapshot(&mut self) -> Result<Snapshot> {
        if !app_open()? {
            self.reset();
            return Ok(Snapshot {
                app_open: false,
                tasks: Vec::new(),
            });
        }
        match self.collect() {
            Ok(snapshot) => Ok(snapshot),
            Err(error) => {
                self.reset();
                if !app_open()? {
                    Ok(Snapshot {
                        app_open: false,
                        tasks: Vec::new(),
                    })
                } else {
                    Err(error)
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn reset(&mut self) {
        self.connection = None;
        self.client_id = None;
        self.streams = Streams::default();
    }

    #[cfg(target_os = "macos")]
    fn follow(&mut self, tid: &str, following: bool) -> Result<()> {
        self.connection
            .as_mut()
            .context("IPC 尚未连接")?
            .send(&json!({
                "type":"broadcast", "method":"thread-stream-following-changed", "version":1,
                "sourceClientId":self.client_id,
                "params":{"hostId":"local","conversationId":tid,"following":following}
            }))
    }

    #[cfg(target_os = "macos")]
    fn drain(&mut self, duration: Duration, initializing: bool) -> Result<()> {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            let messages = self
                .connection
                .as_mut()
                .context("IPC 尚未连接")?
                .receive(deadline.saturating_duration_since(Instant::now()))?;
            for message in messages {
                self.receive(message)?;
            }
            self.streams.check_deadlines()?;
            if initializing && self.client_id.is_some() {
                return Ok(());
            }
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn receive(&mut self, message: Value) -> Result<()> {
        match message["type"].as_str().context("IPC 消息缺少 type")? {
            "client-discovery-request" => {
                self.connection.as_mut().unwrap().send(&json!({"type":"client-discovery-response",
                    "requestId":message.get("requestId").context("发现请求缺少 requestId")?,"response":{"canHandle":false}}))?;
            }
            "response" => {
                ensure!(
                    message["requestId"] == "status-observer-init",
                    "收到未知 IPC 请求响应"
                );
                ensure!(
                    message["resultType"] == "success" && message["method"] == "initialize",
                    "桌面 IPC 初始化失败"
                );
                self.client_id = Some(
                    message["result"]["clientId"]
                        .as_str()
                        .context("初始化缺少 clientId")?
                        .to_owned(),
                );
            }
            "broadcast" => match message["method"].as_str().context("广播缺少 method")? {
                "thread-stream-state-changed" if message["params"]["hostId"] == "local" => {
                    if let Some(tid) = self.streams.change(message)? {
                        self.follow(&tid, true)?;
                    }
                }
                "thread-stream-following-status-requested"
                    if message["params"]["hostId"] == "local" =>
                {
                    let tid = message["params"]["conversationId"]
                        .as_str()
                        .context("订阅查询缺少 conversationId")?;
                    if self.streams.subscribed.contains(tid) {
                        self.follow(tid, true)?;
                    }
                }
                "client-status-changed" if message["params"]["status"] == "disconnected" => {
                    let owner = message["params"]["clientId"]
                        .as_str()
                        .context("断连消息缺少 clientId")?;
                    self.streams.owner_disconnected(owner)?;
                }
                _ => {} // 路由还广播编辑器与缓存事件，与任务状态无关。
            },
            other => bail!("未知 IPC 消息类型：{other}"),
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn collect(&mut self) -> Result<Snapshot> {
        let catalog = history::read(&self.home)?;
        if self.connection.is_none() {
            self.connection = Some(ipc::Connection::open(&self.home.join("ipc/ipc.sock"))?);
            self.connection
                .as_mut()
                .unwrap()
                .send(&json!({"type":"request","requestId":"status-observer-init",
                "method":"initialize","version":0,"params":{"clientType":"status-observer"}}))?;
            self.drain(Duration::from_secs(2), true)?;
            ensure!(self.client_id.is_some(), "桌面 IPC 初始化超时");
        }
        let mut wanted = catalog.candidates;
        for (tid, live) in &self.streams.live {
            if live.task.running && catalog.desktop_ids.contains(tid) {
                wanted.insert(tid.clone());
            }
        }
        let new: Vec<_> = wanted
            .difference(&self.streams.subscribed)
            .cloned()
            .collect();
        let removed: Vec<_> = self
            .streams
            .subscribed
            .difference(&wanted)
            .cloned()
            .collect();
        for tid in removed {
            self.follow(&tid, false)?;
            self.streams.resync.remove(&tid);
            self.streams.live.remove(&tid);
        }
        self.streams.subscribed = wanted;
        for tid in &new {
            self.follow(tid, true)?;
        }
        self.drain(
            if new.is_empty() {
                Duration::from_millis(50)
            } else {
                Duration::from_secs(1)
            },
            false,
        )?;
        let mut tasks = Vec::with_capacity(self.streams.live.len());
        for (tid, live) in &self.streams.live {
            if !catalog.desktop_ids.contains(tid) {
                continue;
            }
            let parent = catalog.parents.get(tid);
            // 子代理必须归属于已观察到的桌面根任务；guardian 没有 thread_spawn 归属，不会进入目录。
            let root_observed = catalog
                .roots
                .get(tid)
                .is_some_and(|root| self.streams.live.contains_key(root));
            if parent.is_none() || root_observed {
                let mut task = live.task.clone();
                task.parent = parent.cloned();
                tasks.push(task);
            }
        }
        Ok(Snapshot {
            app_open: true,
            tasks,
        })
    }
}

#[cfg(target_os = "macos")]
fn app_open() -> Result<bool> {
    let output = std::process::Command::new("/bin/ps")
        .args(["-axo", "comm="])
        .output()
        .context("无法读取应用进程列表")?;
    ensure!(output.status.success(), "读取应用进程列表失败");
    let text = String::from_utf8(output.stdout).context("进程列表编码无效")?;
    Ok(text.lines().any(|line| {
        line.trim().ends_with("/ChatGPT.app/Contents/MacOS/ChatGPT")
            || line.trim().ends_with("/Codex.app/Contents/MacOS/Codex")
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn active(requests: Value) -> Value {
        json!({"id":"t","threadRuntimeStatus":{"type":"active","activeFlags":[]},"requests":requests})
    }
    #[test]
    fn user_wait_excludes_tools_and_completed_requests() {
        assert!(
            !task_from_state("t", &active(json!([{"method":"item/tool/call"}])))
                .unwrap()
                .waiting
        );
        assert!(
            task_from_state(
                "t",
                &active(json!([{"method":"item/tool/requestUserInput"}]))
            )
            .unwrap()
            .waiting
        );
        assert!(
            !task_from_state(
                "t",
                &active(json!([{"method":"item/tool/requestUserInput","completed":true}]))
            )
            .unwrap()
            .waiting
        );
        assert!(task_from_state("t", &json!({"threadRuntimeStatus":{"type":"active","activeFlags":["waitingOnApproval"]}})).unwrap().waiting);
    }
    #[test]
    fn missing_runtime_and_unknown_status_fail() {
        assert!(task_from_state("t", &json!({})).is_err());
        assert!(task_from_state("t", &json!({"threadRuntimeStatus":{"type":"active"}})).is_err());
        assert!(task_from_state("t", &json!({"threadRuntimeStatus":{"type":"mystery"}})).is_err());
    }
    #[test]
    fn live_turn_time_beats_map_order_and_old_errors() {
        let state = json!({"threadRuntimeStatus":{"type":"active","activeFlags":[]},"turnHistory":{"kind":"canonical","history":{"entitiesByKey":{
            "z-old":{"turnId":"old","status":"failed","error":{},"turnStartedAtMs":100},
            "a-new":{"turnId":"new","status":"inProgress","turnStartedAtMs":200}
        }}}});
        let task = task_from_state("t", &state).unwrap();
        assert!(task.running);
        assert!(task.error.is_none());
        assert!(task.completed.is_none());
        let mut failed = state.clone();
        failed["threadRuntimeStatus"] = json!({"type":"systemError"});
        assert_eq!(
            task_from_state("t", &failed).unwrap().error.as_deref(),
            Some("new:systemError")
        );
    }
    #[test]
    fn approval_interruption_token_and_ambiguous_history() {
        let state = json!({"threadRuntimeStatus":{"type":"idle"},"turns":[{"turnId":"turn","status":"interrupted","items":[{"type":"autoReviewInterruptionWarning","id":"warning"}]}]});
        assert_eq!(
            task_from_state("t", &state).unwrap().error.as_deref(),
            Some("turn:auto-review:warning")
        );
        let state = json!({"threadRuntimeStatus":{"type":"idle"},"turns":[{"turnId":"a","status":"completed"},{"turnId":"b","status":"failed"}]});
        assert!(task_from_state("t", &state).is_err());
    }

    #[test]
    fn owner_disconnect_never_fabricates_idle() {
        let mut streams = Streams::default();
        streams.live.insert(
            "t".into(),
            Live {
                state: active(json!([])),
                task: task_from_state("t", &active(json!([]))).unwrap(),
                revision: 1,
                owner: "owner".into(),
            },
        );
        assert!(streams.owner_disconnected("unrelated").is_ok());
        assert!(streams.owner_disconnected("owner").is_err());
        assert_eq!(
            streams.live["t"].state["threadRuntimeStatus"]["type"],
            "active"
        );
    }

    #[test]
    fn patches_refresh_cached_wait_and_error_before_collection() {
        let mut streams = Streams::default();
        streams.subscribed.insert("t".into());
        streams.change(json!({"version":11,"sourceClientId":"owner","params":{"conversationId":"t","change":{
            "type":"snapshot","revision":1,"conversationState":{
                "id":"t","threadRuntimeStatus":{"type":"active","activeFlags":[]},"requests":[],
                "turns":[{"turnId":"turn","status":"inProgress","items":[]}]
            }
        }}})).unwrap();
        assert!(!streams.live["t"].task.waiting);
        streams.change(json!({"version":11,"sourceClientId":"owner","params":{"conversationId":"t","change":{
            "type":"patches","baseRevision":1,"revision":2,"patches":[
                {"op":"add","path":["requests",0],"value":{"method":"item/tool/requestUserInput"}}
            ]
        }}})).unwrap();
        assert!(streams.live["t"].task.waiting);
        streams.change(json!({"version":11,"sourceClientId":"owner","params":{"conversationId":"t","change":{
            "type":"patches","baseRevision":2,"revision":3,"patches":[
                {"op":"remove","path":["requests",0]},
                {"op":"replace","path":["threadRuntimeStatus"],"value":{"type":"idle"}},
                {"op":"replace","path":["turns",0,"status"],"value":"interrupted"},
                {"op":"add","path":["turns",0,"items",0],"value":{"type":"autoReviewInterruptionWarning","id":"warning"}}
            ]
        }}})).unwrap();
        let task = &streams.live["t"].task;
        assert!(!task.waiting);
        assert!(!task.running);
        assert_eq!(task.error.as_deref(), Some("turn:auto-review:warning"));
        assert_eq!(
            *task,
            task_from_state("t", &streams.live["t"].state).unwrap()
        );
        assert!(streams.owner_disconnected("owner").is_ok());
    }

    #[test]
    fn invalid_runtime_patch_still_fails_immediately() {
        let mut streams = Streams::default();
        streams.subscribed.insert("t".into());
        streams.change(json!({"version":11,"sourceClientId":"owner","params":{"conversationId":"t","change":{
            "type":"snapshot","revision":1,"conversationState":active(json!([]))
        }}})).unwrap();
        let result = streams.change(
            json!({"version":11,"sourceClientId":"owner","params":{"conversationId":"t","change":{
                "type":"patches","baseRevision":1,"revision":2,"patches":[
                    {"op":"remove","path":["threadRuntimeStatus","activeFlags"]}
                ]
            }}}),
        );
        assert!(result.is_err());
    }

    #[test]
    fn revision_gap_requests_one_resync_and_times_out() {
        let mut streams = Streams::default();
        streams.subscribed.insert("t".into());
        let message = json!({"version":11,"sourceClientId":"owner","params":{"conversationId":"t","change":{"type":"patches","baseRevision":2,"revision":3,"patches":[]}}});
        assert_eq!(
            streams.change(message.clone()).unwrap().as_deref(),
            Some("t")
        );
        assert!(streams.change(message.clone()).unwrap().is_none());
        streams
            .resync
            .insert("t".into(), Instant::now() - Duration::from_secs(1));
        assert!(streams.check_deadlines().is_err());
    }

    #[test]
    fn protocol_version_is_metadata_but_state_contract_is_required() {
        for version in [Some(json!(11)), Some(json!(13)), Some(json!(999)), None] {
            let mut streams = Streams::default();
            streams.subscribed.insert("t".into());
            let mut message = json!({"sourceClientId":"owner","params":{"conversationId":"t","change":{
                "type":"snapshot","revision":1,"conversationState":active(json!([]))
            }}});
            if let Some(version) = version {
                message["version"] = version;
            }
            streams.change(message.clone()).unwrap();
            assert!(streams.live["t"].task.running);
            message["params"]["change"] = json!({"type":"patches","baseRevision":1,"revision":2,"patches":[
                {"op":"replace","path":["threadRuntimeStatus"],"value":{"type":"idle"}}
            ]});
            streams.change(message.clone()).unwrap();
            assert!(!streams.live["t"].task.running);
            message["params"]["change"] =
                json!({"type":"snapshot","revision":3,"conversationState":{"id":"t"}});
            assert!(streams.change(message).is_err());
        }
    }
}
