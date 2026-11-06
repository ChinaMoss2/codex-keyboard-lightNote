//! 从 Codex rollout 文件补偿缺失的 `SubagentStop` 事件。
//!
//! 调用方应在父会话为 `running` 或 `waiting`、或仍有活动 agent 时按秒调用
//! [`CompletionReader::reconcile`]。本模块只缓存 rollout 路径、文件位置、inode 和
//! 最近一条任务边界事件；不会保存 rollout 正文或 `last_agent_message`。

use chrono::DateTime;
use libc::{c_char, c_int, c_uchar, c_void};
use serde_json::{Map, Value};
use std::{
    collections::{HashMap, HashSet},
    ffi::{CStr, CString},
    fs::File,
    io::{BufRead, BufReader, Seek, SeekFrom},
    marker::PhantomData,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    ptr::{self, NonNull},
};

const SQLITE_OK: c_int = 0;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;
const SQLITE_OPEN_READONLY: c_int = 0x0000_0001;
const ROLLOUT_PATH_QUERY: &[u8] = b"SELECT rollout_path FROM threads WHERE id = ?1 LIMIT 1\0";
const SPAWNED_CHILDREN_QUERY: &[u8] =
    b"SELECT child_thread_id FROM thread_spawn_edges WHERE parent_thread_id = ?1\0";

#[repr(C)]
struct Sqlite3 {
    _private: [u8; 0],
}

#[repr(C)]
struct Sqlite3Statement {
    _private: [u8; 0],
}

type SqliteDestructor = Option<unsafe extern "C" fn(*mut c_void)>;

#[link(name = "sqlite3")]
extern "C" {
    fn sqlite3_open_v2(
        filename: *const c_char,
        database: *mut *mut Sqlite3,
        flags: c_int,
        vfs: *const c_char,
    ) -> c_int;
    fn sqlite3_prepare_v2(
        database: *mut Sqlite3,
        sql: *const c_char,
        length: c_int,
        statement: *mut *mut Sqlite3Statement,
        tail: *mut *const c_char,
    ) -> c_int;
    fn sqlite3_bind_text(
        statement: *mut Sqlite3Statement,
        index: c_int,
        value: *const c_char,
        length: c_int,
        destructor: SqliteDestructor,
    ) -> c_int;
    fn sqlite3_step(statement: *mut Sqlite3Statement) -> c_int;
    fn sqlite3_column_text(statement: *mut Sqlite3Statement, column: c_int) -> *const c_uchar;
    fn sqlite3_finalize(statement: *mut Sqlite3Statement) -> c_int;
    fn sqlite3_close(database: *mut Sqlite3) -> c_int;
}

/// 复用的只读 SQLite 连接。连接和预处理语句均由 `Drop` 关闭。
struct SqliteConnection {
    raw: NonNull<Sqlite3>,
}

impl SqliteConnection {
    fn open_readonly(path: &Path) -> Option<Self> {
        let path = CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut raw = ptr::null_mut();
        let result = unsafe {
            // `path` 在本次调用期间保持有效，SQLite 不会保留文件名指针。
            sqlite3_open_v2(path.as_ptr(), &mut raw, SQLITE_OPEN_READONLY, ptr::null())
        };
        if result != SQLITE_OK || raw.is_null() {
            if !raw.is_null() {
                unsafe {
                    sqlite3_close(raw);
                }
            }
            return None;
        }
        Some(Self {
            raw: NonNull::new(raw)?,
        })
    }

    fn prepare(&mut self, sql: &[u8]) -> Option<SqliteStatement<'_>> {
        let mut raw = ptr::null_mut();
        let result = unsafe {
            sqlite3_prepare_v2(
                self.raw.as_ptr(),
                sql.as_ptr().cast(),
                -1,
                &mut raw,
                ptr::null_mut(),
            )
        };
        if result != SQLITE_OK || raw.is_null() {
            if !raw.is_null() {
                unsafe {
                    sqlite3_finalize(raw);
                }
            }
            return None;
        }
        Some(SqliteStatement {
            raw: NonNull::new(raw)?,
            _connection: PhantomData,
        })
    }

    fn rollout_path(&mut self, agent: &str) -> Option<PathBuf> {
        // `None` 表示 SQLITE_STATIC。`agent` 在 `statement` 前创建，因而会在
        // `sqlite3_finalize` 之后才释放，满足 SQLite 对绑定文本的生命周期要求。
        let agent = CString::new(agent).ok()?;
        let statement = self.prepare(ROLLOUT_PATH_QUERY)?;
        let bound =
            unsafe { sqlite3_bind_text(statement.raw.as_ptr(), 1, agent.as_ptr(), -1, None) };
        if bound != SQLITE_OK {
            return None;
        }
        if unsafe { sqlite3_step(statement.raw.as_ptr()) } != SQLITE_ROW {
            return None;
        }
        let text = unsafe { sqlite3_column_text(statement.raw.as_ptr(), 0) };
        if text.is_null() {
            return None;
        }
        let text = unsafe { CStr::from_ptr(text.cast()) }.to_str().ok()?;
        Some(PathBuf::from(text))
    }

    fn spawned_children(&mut self, parent: &str) -> Option<Vec<String>> {
        // 故意不读取或解释 edge.status：`open` 只代表边仍存在，不能证明子代理运行。
        let parent = CString::new(parent).ok()?;
        let statement = self.prepare(SPAWNED_CHILDREN_QUERY)?;
        let bound =
            unsafe { sqlite3_bind_text(statement.raw.as_ptr(), 1, parent.as_ptr(), -1, None) };
        if bound != SQLITE_OK {
            return None;
        }

        let mut children = Vec::new();
        loop {
            match unsafe { sqlite3_step(statement.raw.as_ptr()) } {
                SQLITE_ROW => {
                    let text = unsafe { sqlite3_column_text(statement.raw.as_ptr(), 0) };
                    let text = (!text.is_null())
                        .then(|| unsafe { CStr::from_ptr(text.cast()) })?
                        .to_str()
                        .ok()?;
                    children.push(text.to_owned());
                }
                SQLITE_DONE => return Some(children),
                _ => return None,
            }
        }
    }
}

impl Drop for SqliteConnection {
    fn drop(&mut self) {
        unsafe {
            sqlite3_close(self.raw.as_ptr());
        }
    }
}

/// 让语句无法比其连接活得更久。
struct SqliteStatement<'connection> {
    raw: NonNull<Sqlite3Statement>,
    _connection: PhantomData<&'connection mut SqliteConnection>,
}

impl Drop for SqliteStatement<'_> {
    fn drop(&mut self) {
        unsafe {
            sqlite3_finalize(self.raw.as_ptr());
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EventKind {
    TaskStarted,
    TaskComplete,
    TurnAborted,
}

#[derive(Clone, Debug, PartialEq)]
struct CompletionEvent {
    kind: EventKind,
    turn: Option<String>,
    timestamp: f64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

struct AgentCache {
    rollout_path: PathBuf,
    offset: u64,
    identity: Option<FileIdentity>,
    event: Option<CompletionEvent>,
    input_pending: bool,
}

impl AgentCache {
    fn new(rollout_path: PathBuf) -> Self {
        Self {
            rollout_path,
            offset: 0,
            identity: None,
            event: None,
            input_pending: false,
        }
    }

    fn reset(&mut self, identity: FileIdentity) {
        self.offset = 0;
        self.identity = Some(identity);
        self.event = None;
        self.input_pending = false;
    }
}

/// 读取 rollout 的终态证据，补偿未送达的子代理停止 Hook。
pub struct CompletionReader {
    database: PathBuf,
    connection: Option<SqliteConnection>,
    agents: HashMap<String, AgentCache>,
    database_identity: Option<FileIdentity>,
}

impl CompletionReader {
    pub fn new(database: PathBuf) -> Self {
        Self {
            database,
            connection: None,
            agents: HashMap::new(),
            database_identity: None,
        }
    }

    /// 就地发现有运行证据的子代理，并清除已有同轮 `task_complete` 证据的 agent。
    ///
    /// `now` 保留为服务循环的统一调用接口。运行和终态证据始终来自 rollout 中的
    /// RFC3339 时间戳，因此不会用轮询时刻或 edge.status 推断状态。
    pub fn reconcile(&mut self, sessions: &mut Value, now: f64) {
        let _ = now;
        let identity = std::fs::metadata(&self.database)
            .ok()
            .map(|m| FileIdentity {
                device: m.dev(),
                inode: m.ino(),
            });
        if identity != self.database_identity {
            self.connection = None;
            self.agents.clear();
            self.database_identity = identity;
        }
        let Some(entries) = sessions.as_object_mut() else {
            return;
        };

        for (parent, entry) in entries.iter_mut() {
            self.reconcile_active_agents(entry);
            self.discover_agents(parent, entry);
            let pending = self.agents.get(parent).is_some_and(|cache| {
                cache.input_pending
                    && entry
                        .get("state")
                        .and_then(Value::as_str)
                        .is_some_and(|state| matches!(state, "running" | "waiting"))
                    && entry
                        .get("turn")
                        .and_then(Value::as_str)
                        .is_none_or(|turn| {
                            cache.event.as_ref().and_then(|event| event.turn.as_deref())
                                == Some(turn)
                        })
            });
            entry["input_pending"] = Value::Bool(pending);
        }
        // 历史候选也保留读取偏移，避免每秒从头重读；没有会话时清空。
        // 极长会话采用有界缓存，单个缓存条目仅含路径、偏移和一个事件。
        if entries.is_empty() || self.agents.len() > 2048 {
            self.agents.clear();
        }
    }

    fn reconcile_active_agents(&mut self, entry: &mut Value) {
        for agent in agent_ids(entry) {
            let Some(event) = self.latest_event(&agent) else {
                continue;
            };
            if !matches!(event.kind, EventKind::TaskComplete | EventKind::TurnAborted)
                || !completion_is_current(entry, &agent, &event)
            {
                continue;
            }

            let empty = entry
                .get_mut("agents")
                .and_then(Value::as_object_mut)
                .map(|agents| {
                    agents.remove(&agent);
                    agents.is_empty()
                })
                .unwrap_or(false);

            if empty && entry.get("state").and_then(Value::as_str) == Some("done") {
                entry["updated"] = Value::from(event.timestamp);
            }
        }
    }

    fn discover_agents(&mut self, parent: &str, entry: &mut Value) {
        if !is_discoverable_parent(entry) {
            return;
        }
        let Some(parent_event) = self.latest_event(parent) else {
            return;
        };
        let Some(parent_start) = parent_current_turn_start(entry, &parent_event) else {
            return;
        };

        // edge 是候选来源，known_agents 则覆盖已经发现过、但 edge 尚未可见的子代理。
        let mut candidates = self
            .spawned_children(parent)
            .unwrap_or_default()
            .into_iter()
            .collect::<HashSet<_>>();
        candidates.extend(known_agent_ids(entry));
        let active = agent_ids(entry).into_iter().collect::<HashSet<_>>();

        for child in candidates {
            if child == parent || active.contains(&child) {
                continue;
            }
            let Some(event) = self.latest_event(&child) else {
                continue;
            };
            if child_started_during_parent_turn(&event, &parent_start) {
                insert_discovered_agent(entry, child, &event);
            }
        }
    }

    fn latest_event(&mut self, agent: &str) -> Option<CompletionEvent> {
        if !self.agents.contains_key(agent) {
            let path = self.rollout_path(agent)?;
            self.agents.insert(agent.to_owned(), AgentCache::new(path));
        }
        read_new_events(self.agents.get_mut(agent)?)
    }

    fn rollout_path(&mut self, agent: &str) -> Option<PathBuf> {
        if self.connection.is_none() {
            self.connection = SqliteConnection::open_readonly(&self.database);
        }
        self.connection.as_mut()?.rollout_path(agent)
    }

    fn spawned_children(&mut self, parent: &str) -> Option<Vec<String>> {
        if self.connection.is_none() {
            self.connection = SqliteConnection::open_readonly(&self.database);
        }
        self.connection.as_mut()?.spawned_children(parent)
    }
}

fn agent_ids(entry: &Value) -> Vec<String> {
    entry
        .get("agents")
        .and_then(Value::as_object)
        .map(|agents| agents.keys().cloned().collect())
        .unwrap_or_default()
}

fn known_agent_ids(entry: &Value) -> Vec<String> {
    entry
        .get("known_agents")
        .and_then(Value::as_array)
        .map(|agents| {
            agents
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn is_discoverable_parent(entry: &Value) -> bool {
    matches!(
        entry.get("state").and_then(Value::as_str),
        Some("running" | "waiting")
    )
}

/// rollout 的最后一个边界事件必须是当前父 turn 的 `task_started`。
fn parent_current_turn_start(entry: &Value, event: &CompletionEvent) -> Option<CompletionEvent> {
    if event.kind != EventKind::TaskStarted {
        return None;
    }
    match entry.get("turn") {
        None | Some(Value::Null) => Some(event.clone()),
        Some(Value::String(turn)) if event.turn.as_deref() == Some(turn.as_str()) => {
            Some(event.clone())
        }
        Some(_) => None,
    }
}

/// edge 或 known_agents 只能提供候选身份；实际运行证据必须是本父 turn 内的开始事件。
fn child_started_during_parent_turn(
    event: &CompletionEvent,
    parent_start: &CompletionEvent,
) -> bool {
    event.kind == EventKind::TaskStarted && event.timestamp >= parent_start.timestamp
}

fn insert_discovered_agent(entry: &mut Value, agent: String, event: &CompletionEvent) {
    let Some(entry) = entry.as_object_mut() else {
        return;
    };
    if !entry.contains_key("agents") {
        entry.insert("agents".to_owned(), Value::Object(Map::new()));
    }
    {
        let Some(agents) = entry.get_mut("agents").and_then(Value::as_object_mut) else {
            return;
        };
        if agents.contains_key(&agent) {
            return;
        }
        agents.insert(
            agent.clone(),
            Value::Object(Map::from_iter([
                ("state".to_owned(), Value::String("running".to_owned())),
                (
                    "turn".to_owned(),
                    event.turn.clone().map(Value::String).unwrap_or(Value::Null),
                ),
                ("updated".to_owned(), Value::from(event.timestamp)),
            ])),
        );
    }

    if !entry.contains_key("known_agents") {
        entry.insert("known_agents".to_owned(), Value::Array(Vec::new()));
    }
    if let Some(known_agents) = entry.get_mut("known_agents").and_then(Value::as_array_mut) {
        if !known_agents
            .iter()
            .any(|known| known.as_str() == Some(agent.as_str()))
        {
            known_agents.push(Value::String(agent));
        }
    }
}

/// 只接受与当前活动标记匹配、且不早于该标记的完成事件。
/// 这和 Python 版本的两个保护条件一致，旧的完成记录无法清理新一轮任务。
fn completion_is_current(entry: &Value, agent: &str, event: &CompletionEvent) -> bool {
    let Some(active) = entry
        .get("agents")
        .and_then(Value::as_object)
        .and_then(|agents| agents.get(agent))
    else {
        return false;
    };
    let Some(updated) = active.get("updated").and_then(Value::as_f64) else {
        return false;
    };
    if !updated.is_finite() || event.timestamp < updated {
        return false;
    }
    match active.get("turn") {
        None | Some(Value::Null) => true,
        Some(Value::String(turn)) => event.turn.as_deref() == Some(turn.as_str()),
        Some(_) => false,
    }
}

/// 从上次确认的换行边界继续读取。半行不前移 offset，等待下一次补全后重读。
fn read_new_events(cache: &mut AgentCache) -> Option<CompletionEvent> {
    let mut stream = File::open(&cache.rollout_path).ok()?;
    let metadata = stream.metadata().ok()?;
    let identity = FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    };
    if cache.identity != Some(identity) || metadata.len() < cache.offset {
        cache.reset(identity);
    }

    stream.seek(SeekFrom::Start(cache.offset)).ok()?;
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line).ok()?;
        if read == 0 {
            break;
        }
        if !line.ends_with(b"\n") {
            // 当前写入尚未完成，下一轮从同一 offset 重新读取。
            break;
        }
        cache.offset = cache.offset.checked_add(read as u64)?;
        observe_input_record(cache, &line);
        if let Some(event) = parse_event(&line) {
            let terminal = matches!(
                event.kind,
                EventKind::TaskStarted | EventKind::TaskComplete | EventKind::TurnAborted
            );
            cache.event = Some(event);
            if terminal {
                cache.input_pending = false;
            }
        }
    }
    cache.event.clone()
}

fn observe_input_record(cache: &mut AgentCache, line: &[u8]) {
    let Ok(record) = serde_json::from_slice::<Value>(line) else {
        return;
    };
    if record.get("type").and_then(Value::as_str) != Some("response_item") {
        return;
    }
    let Some(payload) = record.get("payload").and_then(Value::as_object) else {
        return;
    };
    match payload.get("type").and_then(Value::as_str) {
        Some("function_call")
            if payload.get("name").and_then(Value::as_str) == Some("request_user_input") =>
        {
            cache.input_pending = true;
        }
        Some("function_call_output") => {
            cache.input_pending = false;
        }
        _ => {}
    }
}

/// 只保留任务边界事件，不保留 record/payload 及其中的对话正文。
fn parse_event(line: &[u8]) -> Option<CompletionEvent> {
    let record: Value = serde_json::from_slice(line).ok()?;
    if record.get("type").and_then(Value::as_str) != Some("event_msg") {
        return None;
    }
    let payload = record.get("payload")?.as_object()?;
    let kind = match payload.get("type").and_then(Value::as_str) {
        Some("task_started") => EventKind::TaskStarted,
        Some("task_complete") => EventKind::TaskComplete,
        Some("turn_aborted") => EventKind::TurnAborted,
        _ => return None,
    };
    let timestamp = parse_timestamp(record.get("timestamp")?.as_str()?)?;
    Some(CompletionEvent {
        kind,
        turn: payload
            .get("turn_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        timestamp,
    })
}

fn parse_timestamp(value: &str) -> Option<f64> {
    let timestamp = DateTime::parse_from_rfc3339(value).ok()?;
    let seconds = timestamp.timestamp() as f64
        + f64::from(timestamp.timestamp_subsec_nanos()) / 1_000_000_000.0;
    seconds.is_finite().then_some(seconds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        fs,
        io::Write,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn temp_file(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "completion-reader-{name}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn event(kind: &str, turn: &str, timestamp: &str) -> String {
        json!({
            "timestamp": timestamp,
            "type": "event_msg",
            "payload": {"type": kind, "turn_id": turn, "last_agent_message": "不应缓存"}
        })
        .to_string()
    }

    #[test]
    fn interrupted_child_is_terminal_and_does_not_clear_a_new_turn() {
        let line = event(
            "turn_aborted",
            "interrupted-turn",
            "2026-09-25T04:09:56.120Z",
        );
        let terminal = parse_event(line.as_bytes()).expect("中断回合必须是终态证据");
        let entry = json!({"agents":{"a":{"turn":"interrupted-turn","updated":1790309237.406871}}});
        assert!(completion_is_current(&entry, "a", &terminal));
        let resumed = json!({"agents":{"a":{"turn":"new-turn","updated":1790309900.0}}});
        assert!(!completion_is_current(&resumed, "a", &terminal));
        assert!(
            parent_current_turn_start(&json!({"turn":"interrupted-turn"}), &terminal).is_none()
        );
    }

    #[test]
    fn only_current_completion_can_remove_an_agent() {
        let entry = json!({
            "agents": {"agent": {"turn": "new", "updated": 20.0}}
        });
        let old = CompletionEvent {
            kind: EventKind::TaskComplete,
            turn: Some("old".to_owned()),
            timestamp: 21.0,
        };
        let current = CompletionEvent {
            kind: EventKind::TaskComplete,
            turn: Some("new".to_owned()),
            timestamp: 21.0,
        };
        assert!(!completion_is_current(&entry, "agent", &old));
        assert!(completion_is_current(&entry, "agent", &current));
    }

    #[test]
    fn discovery_needs_the_current_parent_turn_and_a_new_child_start() {
        let entry = json!({"state": "running", "turn": "parent-turn", "agents": {}});
        let parent_start = CompletionEvent {
            kind: EventKind::TaskStarted,
            turn: Some("parent-turn".to_owned()),
            timestamp: 100.0,
        };
        let wrong_parent_turn = CompletionEvent {
            turn: Some("older-parent-turn".to_owned()),
            ..parent_start.clone()
        };
        let stale_child = CompletionEvent {
            kind: EventKind::TaskStarted,
            turn: Some("child-turn".to_owned()),
            timestamp: 99.0,
        };
        let current_child = CompletionEvent {
            timestamp: 100.0,
            ..stale_child.clone()
        };

        assert!(parent_current_turn_start(&entry, &parent_start).is_some());
        assert!(parent_current_turn_start(&entry, &wrong_parent_turn).is_none());
        assert!(!child_started_during_parent_turn(
            &stale_child,
            &parent_start
        ));
        assert!(child_started_during_parent_turn(
            &current_child,
            &parent_start
        ));
    }

    #[test]
    fn discovery_adds_the_agent_to_known_agents() {
        let mut entry = json!({"state": "running", "agents": {}});
        let started = CompletionEvent {
            kind: EventKind::TaskStarted,
            turn: Some("child-turn".to_owned()),
            timestamp: 100.0,
        };

        insert_discovered_agent(&mut entry, "child".to_owned(), &started);

        assert_eq!(entry["agents"]["child"]["state"], "running");
        assert_eq!(entry["agents"]["child"]["turn"], "child-turn");
        assert_eq!(entry["agents"]["child"]["updated"], 100.0);
        assert_eq!(entry["known_agents"], json!(["child"]));
    }

    #[test]
    fn partial_line_is_retried_without_advancing_offset() {
        let path = temp_file("partial");
        let started = event("task_started", "turn-1", "2026-09-24T00:00:01Z");
        fs::write(&path, &started).unwrap();

        let mut cache = AgentCache::new(path.clone());
        assert_eq!(read_new_events(&mut cache), None);
        assert_eq!(cache.offset, 0);

        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file).unwrap();
        let observed = read_new_events(&mut cache).unwrap();
        assert_eq!(observed.kind, EventKind::TaskStarted);
        assert_eq!(observed.turn.as_deref(), Some("turn-1"));
        assert_eq!(cache.offset, fs::metadata(&path).unwrap().len());

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn truncation_discards_the_previous_terminal_event() {
        let path = temp_file("truncate");
        let complete = event("task_complete", "turn-1", "2026-09-24T00:00:02Z");
        fs::write(&path, format!("{complete}\n")).unwrap();

        let mut cache = AgentCache::new(path.clone());
        assert_eq!(
            read_new_events(&mut cache).unwrap().kind,
            EventKind::TaskComplete
        );
        fs::write(
            &path,
            format!(
                "{}\n",
                event("task_started", "turn-2", "2026-09-24T00:00:03Z")
            ),
        )
        .unwrap();

        let observed = read_new_events(&mut cache).unwrap();
        assert_eq!(observed.kind, EventKind::TaskStarted);
        assert_eq!(observed.turn.as_deref(), Some("turn-2"));

        fs::remove_file(path).unwrap();
    }
}
