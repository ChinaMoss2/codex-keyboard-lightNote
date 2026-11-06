mod completion;
mod device;
mod state;
use serde_json::{json, Value};
use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::{
        fs::{OpenOptionsExt, PermissionsExt},
        io::{AsRawFd, FromRawFd},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}
fn root() -> PathBuf {
    env::var_os("CODEX_KEYBOARD_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/home/pyl/.codex/codex-keyboard-prototype"))
}
fn atomic(path: &Path, v: &Value) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut f = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(serde_json::to_string(v)?.as_bytes())?;
    fs::rename(tmp, path)
}
fn log(root: &Path, name: &str, v: Value) {
    let p = root.join(name);
    if fs::metadata(&p).map(|m| m.len() > 262144).unwrap_or(false) {
        let _ = fs::rename(&p, p.with_extension("previous.jsonl"));
    }
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(p)
    {
        let _ = writeln!(f, "{v}");
    }
}
fn proc_identity(pid: u32) -> Option<(u32, String)> {
    let s = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<_> = s.rsplit_once(')')?.1.split_whitespace().collect();
    Some((fields.get(1)?.parse().ok()?, fields.get(19)?.to_string()))
}
fn owner() -> Value {
    let mut pid = unsafe { libc::getppid() } as u32;
    for _ in 0..24 {
        if pid <= 1 {
            break;
        }
        let Some((parent, start)) = proc_identity(pid) else {
            break;
        };
        if fs::read_to_string(format!("/proc/{pid}/comm"))
            .unwrap_or_default()
            .trim()
            == "codex"
        {
            return json!([pid, start]);
        }
        pid = parent;
    }
    Value::Null
}
fn object(v: &Value) -> Value {
    if let Some(s) = v.as_str() {
        serde_json::from_str(s).unwrap_or(Value::Null)
    } else {
        v.clone()
    }
}
fn sanitize(v: &Value) -> Value {
    let mut p = json!({});
    for key in [
        "hook_event_name",
        "session_id",
        "agent_id",
        "turn_id",
        "tool_name",
        "stop_hook_active",
    ] {
        if let Some(value) = v.get(key) {
            p[key] = value.clone();
        }
    }
    let args = object(&v["tool_input"]);
    let response = object(&v["tool_response"]);
    for (dest, src, keys) in [
        (
            "tool_input",
            &args,
            &["target", "task_name", "id", "agent_id"][..],
        ),
        (
            "tool_response",
            &response,
            &["task_name", "agent_id", "error", "isError"][..],
        ),
    ] {
        let mut obj = json!({});
        for key in keys {
            if let Some(value) = src.get(*key) {
                obj[*key] = if *key == "error" {
                    json!(true)
                } else {
                    value.clone()
                };
            }
        }
        p[dest] = obj;
    }
    p
}
fn hook(root: &Path) -> io::Result<()> {
    let mut buf = Vec::new();
    io::stdin()
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut buf)?;
    if buf.len() > 4 * 1024 * 1024 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Hook 输入过大"));
    }
    let payload: Value = serde_json::from_slice(&buf)?;
    let message = json!({"op":"hook","payload":sanitize(&payload),"owner":owner(),"time":now()});
    match request(root, &message) {
        Ok(_) => Ok(()),
        Err(e) => {
            // 服务短暂不可用时保留精简事件，服务恢复后按序处理。
            let dir = root.join("pending");
            fs::create_dir_all(&dir)?;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
            if fs::read_dir(&dir)?.count() >= 256 {
                return Err(io::Error::other("待处理事件已满"));
            }
            let name = format!(
                "{:020}-{}.json",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                std::process::id()
            );
            atomic(&dir.join(name), &message)?;
            eprintln!("键盘事件已暂存：{e}");
            Ok(())
        }
    }
}
fn request(root: &Path, value: &Value) -> io::Result<Value> {
    let mut s = UnixStream::connect(root.join("control.sock"))?;
    s.set_read_timeout(Some(Duration::from_millis(500)))?;
    s.set_write_timeout(Some(Duration::from_millis(500)))?;
    writeln!(s, "{value}")?;
    s.shutdown(std::net::Shutdown::Write)?;
    let mut reply = String::new();
    s.take(1048576).read_to_string(&mut reply)?;
    Ok(serde_json::from_str(&reply)?)
}
fn load(root: &Path) -> io::Result<Value> {
    match fs::read(root.join("sessions.json")) {
        Ok(b) => Ok(serde_json::from_slice(&b)?),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e),
    }
}
fn apply(root: &Path, sessions: &mut Value, message: &Value) {
    if message["op"] != "hook" {
        return;
    }
    let p = &message["payload"];
    state::reduce(
        sessions,
        p,
        message["time"].as_f64().unwrap_or_else(now),
        message["owner"].clone(),
    );
    if !matches!(
        p["hook_event_name"].as_str(),
        Some("PreToolUse" | "PostToolUse")
    ) {
        let mut v = sanitize(p);
        v["time"] = message["time"].clone();
        log(root, "hook-events.jsonl", v);
    }
}
fn watch_dev() -> io::Result<File> {
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let f = unsafe { File::from_raw_fd(fd) };
    let wd = unsafe {
        libc::inotify_add_watch(
            fd,
            b"/dev\0".as_ptr().cast(),
            libc::IN_CREATE
                | libc::IN_DELETE
                | libc::IN_ATTRIB
                | libc::IN_MOVED_TO
                | libc::IN_MOVED_FROM,
        )
    };
    if wd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(f)
}
fn daemon(root: &Path, dry: bool) -> io::Result<()> {
    fs::create_dir_all(root)?;
    unsafe {
        libc::umask(0o077);
    }
    let lock = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(root.join(".daemon.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::other("已有灯光服务运行"));
    }
    let sock = root.join("control.sock");
    if sock.exists() {
        fs::remove_file(&sock)?;
    }
    let listener = UnixListener::bind(&sock)?;
    listener.set_nonblocking(true)?;
    let mut watch = watch_dev()?;
    let mut sessions = load(root)?;
    state::prune(&mut sessions, now());
    let pending = root.join("pending");
    if pending.exists() {
        let mut paths: Vec<_> = fs::read_dir(&pending)?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        paths.sort();
        for p in paths {
            if let Ok(b) = fs::read(&p) {
                if let Ok(v) = serde_json::from_slice(&b) {
                    apply(root, &mut sessions, &v);
                    fs::remove_file(p)?;
                }
            }
        }
    }
    let mut persisted = Value::Null;
    let mut last: Option<device::Effect> = None;
    let mut animation_start = Instant::now();
    let mut next_frame = Instant::now();
    let mut dev = device::Device::new(dry);
    let mut retry = None;
    let mut last_error = String::new();
    let db = env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env::var_os("HOME").unwrap()).join(".codex"))
        .join("state_5.sqlite");
    let mut reader = completion::CompletionReader::new(db);
    let mut maintenance = Instant::now();
    loop {
        let time = now();
        let completion_expired = sessions.as_object().is_some_and(|entries| {
            entries.values().any(|e| {
                e["state"] == "done"
                    && e["agents"].as_object().is_none_or(|a| a.is_empty())
                    && time - e["updated"].as_f64().unwrap_or(0.0) >= 5.0
            })
        });
        if Instant::now() >= maintenance || completion_expired {
            state::prune(&mut sessions, time);
        }
        let active = sessions.as_object().is_some_and(|s| {
            s.values().any(|v| {
                matches!(v["state"].as_str(), Some("running" | "waiting"))
                    || v["agents"].as_object().is_some_and(|a| !a.is_empty())
            })
        });
        if Instant::now() >= maintenance {
            if active {
                reader.reconcile(&mut sessions, time);
            }
            maintenance = Instant::now() + Duration::from_secs(1);
        }
        if sessions != persisted {
            atomic(&root.join("sessions.json"), &sessions)?;
            persisted = sessions.clone();
        }
        let selected = state::selected(&sessions);
        let counts = state::activity_counts(&sessions);
        let effect = device::Effect::new(selected.clone(), counts);
        let changed = last.as_ref() != Some(&effect);
        let animated = effect.animated();
        if (changed || (animated && Instant::now() >= next_frame))
            && retry.is_none_or(|t| Instant::now() >= t)
        {
            if changed {
                animation_start = Instant::now();
            }
            match dev.set_frame(effect.frame(animation_start.elapsed().as_secs_f64())) {
                Ok(n) => {
                    if changed {
                        log(
                            root,
                            "events.jsonl",
                            json!({"time":now(),"state":selected,"written":n,"driver":"rust","dry_run":dry,
                                "main_spectrum":effect.main_spectrum,"numpad_spectrum":effect.numpad_spectrum,
                                "running_parents":counts.0,"subagent_parents":counts.1}),
                        );
                        println!(
                            "状态：{selected}，主区Spectrum={}，小键盘Spectrum={}",
                            effect.main_spectrum, effect.numpad_spectrum
                        );
                    }
                    last = Some(effect);
                    next_frame = Instant::now() + Duration::from_millis(100);
                    retry = None;
                    last_error.clear();
                }
                Err(e) => {
                    let text = e.to_string();
                    if text != last_error {
                        eprintln!("键盘等待恢复：{text}");
                        last_error = text;
                    }
                    dev.invalidate();
                    retry = Some(Instant::now() + Duration::from_secs(3));
                }
            }
        }
        // 无活动任务/设备异常时无限阻塞；活动时维护进程活性和缺失停止事件。
        let mut timeout: Option<f64> = if sessions.as_object().is_some_and(|s| !s.is_empty()) {
            Some(if active { 1.0 } else { 5.0 })
        } else {
            None
        };
        if let Some(map) = sessions.as_object() {
            for e in map.values() {
                if e["state"] == "done" && e["agents"].as_object().is_none_or(|a| a.is_empty()) {
                    let delay = (e["updated"].as_f64().unwrap_or(time) + 5.0 - time).max(0.001);
                    timeout = Some(timeout.unwrap_or(delay).min(delay));
                }
            }
        }
        if animated && retry.is_none() {
            let delay = next_frame
                .saturating_duration_since(Instant::now())
                .as_secs_f64();
            timeout = Some(timeout.unwrap_or(delay).min(delay));
        }
        if let Some(t) = retry {
            let delay = t.saturating_duration_since(Instant::now()).as_secs_f64();
            timeout = Some(timeout.unwrap_or(delay).min(delay));
        }
        let ms = timeout.map(|v| (v * 1000.0).ceil() as i32).unwrap_or(-1);
        let mut fds = [
            libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: watch.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        if unsafe { libc::poll(fds.as_mut_ptr(), 2, ms) } < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(io::Error::last_os_error());
        }
        if fds[1].revents & libc::POLLIN != 0 {
            let mut buf = [0u8; 8192];
            while let Ok(n) = watch.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let mut offset = 0;
                while offset + std::mem::size_of::<libc::inotify_event>() <= n {
                    let e = unsafe {
                        std::ptr::read_unaligned(
                            buf[offset..].as_ptr().cast::<libc::inotify_event>(),
                        )
                    };
                    let start = offset + std::mem::size_of::<libc::inotify_event>();
                    let end = (start + e.len as usize).min(n);
                    if buf[start..end].starts_with(b"hidraw") || e.mask & libc::IN_Q_OVERFLOW != 0 {
                        dev.invalidate();
                        last = None;
                        retry = None;
                    }
                    offset = end;
                }
            }
        }
        if fds[0].revents & libc::POLLIN != 0 {
            while let Ok((mut stream, _)) = listener.accept() {
                stream.set_read_timeout(Some(Duration::from_millis(200)))?;
                stream.set_write_timeout(Some(Duration::from_millis(200)))?;
                let mut data = Vec::new();
                let parsed = (&mut stream)
                    .take(1048577)
                    .read_to_end(&mut data)
                    .and_then(|_| {
                        if data.len() > 1048576 {
                            return Err(io::Error::other("请求过大"));
                        }
                        serde_json::from_slice::<Value>(&data).map_err(io::Error::other)
                    });
                let reply = match parsed {
                    Ok(v) if v["op"] == "status" => {
                        let counts = state::activity_counts(&sessions);
                        let effect = device::Effect::new(state::selected(&sessions), counts);
                        json!({"state":effect.state,"sessions":sessions,
                                "running_parents":counts.0,"subagent_parents":counts.1,
                                "main_spectrum":effect.main_spectrum,"numpad_spectrum":effect.numpad_spectrum})
                    }
                    Ok(v) => {
                        apply(root, &mut sessions, &v);
                        json!({"ok":true})
                    }
                    Err(e) => json!({"error":e.to_string()}),
                };
                let _ = writeln!(stream, "{reply}");
            }
        }
    }
}
fn main() {
    let args: Vec<_> = env::args().collect();
    let root = root();
    let result = match args.get(1).map(String::as_str) {
        Some("hook") => {
            let r = hook(&root);
            if let Err(e) = r {
                eprintln!("键盘Hook：{e}");
            }
            println!("{{}}");
            Ok(())
        }
        Some("daemon") => daemon(&root, args.iter().any(|v| v == "--dry-run")),
        Some("status") => request(&root, &json!({"op":"status"})).map(|v| println!("{v}")),
        Some("reports") => {
            let s = args.get(2).map(String::as_str).unwrap_or("idle");
            println!(
                "{}",
                json!(device::reports(s)
                    .iter()
                    .map(|p| p.to_vec())
                    .collect::<Vec<_>>())
            );
            Ok(())
        }
        _ => Err(io::Error::other(
            "用法：codex-keyboard hook|daemon [--dry-run]|status|reports 状态",
        )),
    };
    if let Err(e) = result {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
