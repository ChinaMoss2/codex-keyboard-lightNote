//! 与现有状态文件兼容；只在业务状态改变时更新时间，避免每次工具调用落盘。
use serde_json::{json, Value};
use std::{collections::BTreeSet, fs};

fn alive(e: &Value) -> bool {
    let Some(pid) = e["owner"][0].as_u64() else {
        return true;
    };
    let Some(start) = e["owner"][1].as_str() else {
        return false;
    };
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| {
            s.rsplit_once(')')
                .map(|(_, v)| v.split_whitespace().nth(19).unwrap_or("").to_string())
        })
        .is_some_and(|v| v == start)
}
fn has_agents(e: &Value) -> bool {
    e["agents"].as_object().is_some_and(|v| !v.is_empty())
}
pub fn prune(s: &mut Value, now: f64) {
    if !s.is_object() {
        *s = json!({});
    }
    s.as_object_mut().unwrap().retain(|_, e| {
        if has_agents(e) {
            alive(e)
        } else if e["state"] == "done" {
            now - e["updated"].as_f64().unwrap_or(0.0) < 5.0
        } else {
            alive(e)
        }
    });
}
pub fn selected(s: &Value) -> String {
    let entries: Vec<&Value> = s
        .as_object()
        .map(|o| o.values().collect())
        .unwrap_or_default();
    if entries.iter().any(|e| {
        e["state"] == "waiting"
            || e["agents"]
                .as_object()
                .is_some_and(|a| a.values().any(|v| v["state"] == "waiting"))
    }) {
        return "waiting".into();
    }
    if entries.iter().any(|e| {
        e["input_pending"] == true && matches!(e["state"].as_str(), Some("running" | "waiting"))
    }) {
        return if entries.iter().any(|e| {
            e["input_pending"] == true
                && matches!(e["state"].as_str(), Some("running" | "waiting"))
                && has_agents(e)
        }) {
            "input_waiting_subagents"
        } else {
            "input_waiting"
        }
        .into();
    }
    if entries.iter().any(|e| has_agents(e)) {
        if entries
            .iter()
            .any(|e| matches!(e["state"].as_str(), Some("running" | "waiting")))
        {
            return "running_subagents".into();
        }
        return "subagents_only".into();
    }
    for name in ["running", "done"] {
        if entries.iter().any(|e| e["state"] == name) {
            return name.into();
        }
    }
    "idle".into()
}
pub fn activity_counts(s: &Value) -> (usize, usize) {
    s.as_object()
        .map(|entries| {
            entries
                .values()
                .fold((0, 0), |(main, agent_groups), entry| {
                    let main = main + usize::from(entry["state"] == "running");
                    let agent_groups = agent_groups
                        + usize::from(entry["agents"].as_object().is_some_and(|agents| {
                            agents.values().any(|agent| agent["state"] == "running")
                        }));
                    (main, agent_groups)
                })
        })
        .unwrap_or((0, 0))
}
fn aliases(e: &mut Value) {
    if !e["aliases"].is_object() {
        e["aliases"] = json!({});
    }
    let unbound: Vec<String> = e["aliases"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(_, v)| v.is_null())
        .map(|(k, _)| k.clone())
        .collect();
    let used: BTreeSet<String> = e["aliases"]
        .as_object()
        .unwrap()
        .values()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    let ids: Vec<String> = e["known_agents"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .filter(|id| !used.contains(*id))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    if unbound.len() == 1 && ids.len() == 1 {
        e["aliases"][&unbound[0]] = json!(ids[0]);
    }
}
fn known(e: &mut Value, id: &str) {
    let mut ids: BTreeSet<String> = e["known_agents"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    ids.insert(id.into());
    e["known_agents"] = json!(ids);
    aliases(e);
}
fn obj(v: &Value) -> Value {
    if let Some(s) = v.as_str() {
        serde_json::from_str(s).unwrap_or(Value::Null)
    } else {
        v.clone()
    }
}
pub fn reduce(s: &mut Value, p: &Value, now: f64, owner: Value) {
    prune(s, now);
    let Some(session) = p["session_id"].as_str() else {
        return;
    };
    let Some(event) = p["hook_event_name"].as_str() else {
        return;
    };
    if ![
        "UserPromptSubmit",
        "PermissionRequest",
        "PreToolUse",
        "PostToolUse",
        "Stop",
        "Interrupt",
        "SessionEnd",
        "SubagentStart",
        "SubagentStop",
    ]
    .contains(&event)
    {
        return;
    }
    let turn = p["turn_id"].clone();
    let agent = p["agent_id"].as_str();
    let mut entry = s.get(session).cloned().unwrap_or_else(
        || json!({"state":"running","updated":now,"owner":owner,"turn":null,"agents":{}}),
    );
    if !entry["agents"].is_object() {
        entry["agents"] = json!({});
    }
    if let Some(aid) = agent {
        let old = entry["agents"].get(aid).cloned();
        if ["SubagentStop", "Stop", "Interrupt", "SessionEnd"].contains(&event) {
            let Some(old) = old else { return };
            if !turn.is_null() && !old["turn"].is_null() && turn != old["turn"] {
                return;
            }
            entry["agents"].as_object_mut().unwrap().remove(aid);
            if !has_agents(&entry) && entry["state"] == "done" {
                entry["updated"] = json!(now);
            }
        } else {
            let mode = if event == "PermissionRequest" {
                "waiting"
            } else {
                "running"
            };
            if old
                .as_ref()
                .is_some_and(|o| o["turn"] == turn && o["state"] == mode)
            {
                return;
            }
            entry["agents"][aid] = json!({"state":mode,"turn":turn,"updated":now});
            known(&mut entry, aid);
        }
        s[session] = entry;
        return;
    }
    if event == "UserPromptSubmit" || (!turn.is_null() && entry["turn"] != turn) {
        entry.as_object_mut().unwrap().remove("input_pending");
    }
    if event.starts_with("Subagent") {
        return;
    }
    if event == "PostToolUse" {
        let name = p["tool_name"].as_str().unwrap_or("");
        let input = obj(&p["tool_input"]);
        let response = obj(&p["tool_response"]);
        if name == "collaborationspawn_agent" {
            if let Some(name) = response["task_name"].as_str() {
                if !entry["aliases"].is_object() {
                    entry["aliases"] = json!({});
                }
                entry["aliases"][name] = Value::Null;
                aliases(&mut entry);
            }
        }
        if name == "collaborationfollowup_task"
            && response.get("error").is_none()
            && response["isError"] != true
        {
            if let Some(target) = input["target"].as_str() {
                let id = entry["aliases"][target]
                    .as_str()
                    .unwrap_or(target)
                    .to_owned();
                if entry["known_agents"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|x| x == &id))
                {
                    entry["agents"][&id] = json!({"state":"running","turn":null,"updated":now});
                }
            }
        }
    }
    if ["Stop", "Interrupt"].contains(&event)
        && !turn.is_null()
        && !entry["turn"].is_null()
        && turn != entry["turn"]
    {
        return;
    }
    if event == "Interrupt" || event == "SessionEnd" {
        if event == "SessionEnd" && entry["state"] == "done" && !has_agents(&entry) {
            s[session] = entry;
        } else {
            s.as_object_mut().unwrap().remove(session);
        }
        return;
    }
    let mode = match event {
        "PermissionRequest" => "waiting",
        "Stop" => "done",
        _ => "running",
    };
    if entry["state"] != mode || entry["turn"] != turn {
        entry["updated"] = json!(now);
        entry["event"] = json!(event);
    }
    entry["state"] = json!(mode);
    entry["turn"] = turn;
    entry["owner"] = owner;
    s[session] = entry;
}

#[cfg(test)]
mod tests {
    use super::*;
    fn event(s: &mut Value, name: &str, agent: Option<&str>, turn: &str, t: f64) -> String {
        reduce(
            s,
            &json!({"hook_event_name":name,"session_id":"p","agent_id":agent,"turn_id":turn}),
            t,
            Value::Null,
        );
        selected(s)
    }
    #[test]
    fn input_question_overrides_main_without_hiding_agents() {
        let mut s = json!({"p":{"state":"running","input_pending":true,"agents":{}}});
        assert_eq!(selected(&s), "input_waiting");
        s["p"]["agents"]["a"] = json!({"state":"running"});
        assert_eq!(selected(&s), "input_waiting_subagents");
        s["q"] = json!({"state":"waiting"});
        assert_eq!(selected(&s), "waiting");
        s.as_object_mut().unwrap().remove("q");
        s["p"]["input_pending"] = json!(false);
        assert_eq!(selected(&s), "running_subagents");
        s["p"]["state"] = json!("done");
        s["p"]["input_pending"] = json!(true);
        assert_eq!(selected(&s), "subagents_only");
    }
    #[test]
    fn parent_child_and_priority() {
        let mut s = json!({});
        assert_eq!(event(&mut s, "UserPromptSubmit", None, "p", 1.), "running");
        assert_eq!(
            event(&mut s, "SubagentStart", Some("a"), "a", 2.),
            "running_subagents"
        );
        event(&mut s, "SubagentStart", Some("b"), "b", 3.);
        assert_eq!(
            event(&mut s, "PermissionRequest", Some("a"), "a", 4.),
            "waiting"
        );
        assert_eq!(
            event(&mut s, "PostToolUse", Some("a"), "a", 5.),
            "running_subagents"
        );
        event(&mut s, "Stop", None, "p", 6.);
        event(&mut s, "SubagentStop", Some("a"), "a", 7.);
        assert_eq!(event(&mut s, "SubagentStop", Some("b"), "b", 8.), "done");
        event(&mut s, "SessionEnd", None, "p", 9.);
        prune(&mut s, 14.);
        assert_eq!(selected(&s), "idle");
    }
    #[test]
    fn stale_child_does_not_end_parent() {
        let mut s = json!({});
        event(&mut s, "UserPromptSubmit", None, "p", 1.);
        event(&mut s, "SubagentStart", Some("a"), "one", 2.);
        event(&mut s, "SubagentStop", Some("a"), "one", 3.);
        event(&mut s, "PreToolUse", Some("a"), "two", 4.);
        event(&mut s, "SubagentStop", Some("a"), "one", 5.);
        assert_eq!(selected(&s), "running_subagents");
        assert_eq!(s["p"]["turn"], "p");
        event(&mut s, "Interrupt", None, "p", 6.);
        assert_eq!(selected(&s), "idle");
    }

    #[test]
    fn parent_done_keeps_only_green_subagent_zone() {
        let mut s = json!({"p":{"state":"done","updated":1.0,"agents":{
            "a":{"state":"running","turn":"child","updated":2.0}}}});
        assert_eq!(selected(&s), "subagents_only");
        s["p"]["agents"] = json!({});
        assert_eq!(selected(&s), "done");
    }
    #[test]
    fn activity_counts_deduplicates_running_agents_per_parent() {
        let s = json!({
            "p1": {"state": "running", "agents": {
                "a": {"state": "running"},
                "b": {"state": "running"}
            }},
            "p2": {"state": "running", "agents": {}}
        });
        assert_eq!(activity_counts(&s), (2, 1));
    }
    #[test]
    fn activity_counts_counts_two_running_agent_groups() {
        let s = json!({
            "p1": {"state": "running", "agents": {"a": {"state": "running"}}},
            "p2": {"state": "running", "agents": {"b": {"state": "running"}}}
        });
        assert_eq!(activity_counts(&s), (2, 2));
    }
    #[test]
    fn activity_counts_keeps_agent_group_after_parent_stops() {
        let mut s = json!({});
        reduce(
            &mut s,
            &json!({"hook_event_name":"UserPromptSubmit","session_id":"p1","turn_id":"p1"}),
            1.,
            Value::Null,
        );
        reduce(
            &mut s,
            &json!({"hook_event_name":"UserPromptSubmit","session_id":"p2","turn_id":"p2"}),
            2.,
            Value::Null,
        );
        for (agent, t) in [("a", 3.), ("b", 4.)] {
            reduce(
                &mut s,
                &json!({"hook_event_name":"SubagentStart","session_id":"p1","agent_id":agent,"turn_id":agent}),
                t,
                Value::Null,
            );
        }
        assert_eq!(activity_counts(&s), (2, 1));
        reduce(
            &mut s,
            &json!({"hook_event_name":"Stop","session_id":"p1","turn_id":"p1"}),
            5.,
            Value::Null,
        );
        assert_eq!(activity_counts(&s), (1, 1));
        reduce(
            &mut s,
            &json!({"hook_event_name":"SubagentStop","session_id":"p1","agent_id":"a","turn_id":"a"}),
            6.,
            Value::Null,
        );
        assert_eq!(activity_counts(&s), (1, 1));
        reduce(
            &mut s,
            &json!({"hook_event_name":"SubagentStop","session_id":"p1","agent_id":"b","turn_id":"b"}),
            7.,
            Value::Null,
        );
        assert_eq!(activity_counts(&s), (1, 0));
    }
    #[test]
    fn activity_counts_excludes_waiting_states() {
        let s = json!({
            "main_waiting": {"state": "waiting", "agents": {}},
            "agent_waiting": {"state": "done", "agents": {"a": {"state": "waiting"}}},
            "running_with_waiting_agent": {"state": "running", "agents": {"b": {"state": "waiting"}}}
        });
        assert_eq!(activity_counts(&s), (1, 0));
    }
    #[test]
    fn repeated_tool_does_not_write() {
        let mut s = json!({});
        event(&mut s, "UserPromptSubmit", None, "p", 1.);
        let old = s.clone();
        event(&mut s, "PreToolUse", None, "p", 2.);
        event(&mut s, "PostToolUse", None, "p", 3.);
        assert_eq!(s, old);
    }
}
