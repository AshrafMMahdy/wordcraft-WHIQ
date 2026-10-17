//! WHIQ: review tools for agents (on the engine's `review.edit`, `review.changeReport`,
//! `review.resolve`, `review.find` and `review.compare`):
//!
//! - `review_edit`: a batch of replace / insert / insert_paragraph / delete_paragraph / delete /
//!   comment operations located by anchor text (exact, ignoring case, ignoring typography, or
//!   fuzzy, with a match report), as tracked changes by the caller, all or nothing (unless
//!   `keep_going`); the answer lists every change with its location and ids;
//! - `list_changes`: the change report (ids, kinds, authors, dates, text, locations, comments);
//! - `accept_changes` / `reject_changes`: by id, author, kind, or all;
//! - `find_anchor`: where an anchor is, without changing anything;
//! - `compare_documents`: the open document (or `original`) compared with `revised`, as tracked
//!   changes, with the change report.

use serde_json::{Value, json};

use crate::backend::Backend;
use crate::contract::{self, ToolError};
use crate::whiq::{Limits, Out};

fn s(desc: &str) -> Value {
    json!({"type": "string", "description": desc})
}
fn b(desc: &str) -> Value {
    json!({"type": "boolean", "description": desc})
}

fn def(name: &str, title: &str, desc: &str, props: Value, req: &[&str]) -> Value {
    json!({"name": name, "title": title, "description": desc, "inputSchema": {"type": "object", "properties": props, "required": req}})
}

const NAMES: &[&str] = &["review_edit", "list_changes", "accept_changes", "reject_changes", "find_anchor", "compare_documents"];

pub fn handles(name: &str) -> bool {
    NAMES.contains(&name)
}

pub fn tool_definitions() -> Vec<Value> {
    let which = json!({
        "ids": {"type": "array", "items": {"type": "string"}, "description": "Change ids from list_changes or review_edit"},
        "author": s("Every change by this author"),
        "kind": s("insert | delete | format | moveFrom | moveTo"),
        "all": b("Every change"),
    });
    vec![
        def(
            "review_edit",
            "Review edit",
            "Edit like a reviewer: a batch of operations located by anchor text, applied as tracked changes by the caller (dated now), touching nothing else. Operations: {op: \"replace\", anchor, with, occurrence?: n|\"all\"}, {op: \"insert\", anchor, text, position?: before|after}, {op: \"insert_paragraph\", anchor, text, position?: before|after, style?}, {op: \"delete_paragraph\", anchor}, {op: \"delete\", anchor}, {op: \"comment\", anchor, text}. Anchors match exactly, else ignoring case, else ignoring quote/dash/space differences, else fuzzily (score ≥ min_score); every result reports how. All or nothing unless keep_going. The answer lists each change with its location and change ids (for accept_changes / reject_changes).",
            json!({
                "ops": {"type": "array", "items": {"type": "object"}, "description": "The operations, applied in order"},
                "tracked": b("Track the edits (default true)"),
                "fuzzy": b("Allow fuzzy anchor matches (default true)"),
                "min_score": {"type": "number", "description": "Lowest fuzzy score accepted, 0.5–1 (default 0.8)"},
                "keep_going": b("Apply the operations that can be and report the others (default: all or nothing)"),
            }),
            &["ops"],
        ),
        def(
            "list_changes",
            "List changes",
            "Every tracked change: a stable id, kind, author, date, text, paragraph, location and context; and the comments. Filter by author or kind.",
            json!({"author": s("Only this author's changes"), "kind": s("insert | delete | format | moveFrom | moveTo")}),
            &[],
        ),
        def("accept_changes", "Accept changes", "Accept tracked changes by id, by author, by kind, or all.", which.clone(), &[]),
        def("reject_changes", "Reject changes", "Reject tracked changes by id, by author, by kind, or all.", which, &[]),
        def(
            "find_anchor",
            "Find anchor",
            "Where an anchor text is (exact, ignoring case or typography, or fuzzy), with the closest phrases when it isn't there. Changes nothing.",
            json!({"anchor": s("The text to find"), "fuzzy": b("Allow fuzzy matches (default true)"), "match_case": b("Exact case only")}),
            &["anchor"],
        ),
        def(
            "compare_documents",
            "Compare documents",
            "Compare the open document (or `original`) with `revised`: the open document becomes the comparison, with the differences as tracked changes by `author`; the answer lists them (list_changes format).",
            json!({
                "revised": s("Path of the revised document"),
                "original": s("Path of the original (default: the open document)"),
                "author": s("Author of the changes (default: the caller)"),
                "level": s("word (default) | character"),
            }),
            &["revised"],
        ),
    ]
}

fn exec(b: &mut dyn Backend, id: &str, params: Value) -> Result<Value, String> {
    b.call("engine.execute", json!({"command": id, "params": params}))
}

/// Selection params for `review.resolve`.
fn which(a: &Value, accept: bool) -> Value {
    let mut p = json!({"accept": accept});
    for k in ["ids", "author", "kind", "all"] {
        if let Some(v) = a.get(k) {
            p[k] = v.clone();
        }
    }
    p
}

pub fn run(b: &mut dyn Backend, name: &str, a: &Value, _limits: &Limits, _warnings: &mut Vec<String>) -> Result<Out, ToolError> {
    match name {
        "review_edit" => {
            let mut p = json!({"ops": a.get("ops").cloned().unwrap_or(Value::Null)});
            for (from, to) in [("tracked", "tracked"), ("fuzzy", "fuzzy"), ("min_score", "minScore"), ("keep_going", "keepGoing")] {
                if let Some(v) = a.get(from) {
                    p[to] = v.clone();
                }
            }
            exec(b, "review.edit", p).map(Out::Json).map_err(|e| edit_error(&e))
        }
        "list_changes" => {
            let mut p = json!({});
            for k in ["author", "kind"] {
                if let Some(v) = a.get(k) {
                    p[k] = v.clone();
                }
            }
            exec(b, "review.changeReport", p).map(Out::Json).map_err(|e| contract::classify(&e, name, a, &[]))
        }
        "accept_changes" | "reject_changes" => {
            let v = exec(b, "review.resolve", which(a, name == "accept_changes"))
                .map_err(|e| ToolError::new("bad_params", e, "say which changes: ids (from list_changes), author, kind, or all: true"))?;
            Ok(Out::Json(v))
        }
        "find_anchor" => {
            let mut p = json!({"anchor": a.get("anchor").cloned().unwrap_or(Value::Null)});
            if let Some(v) = a.get("fuzzy") {
                p["fuzzy"] = v.clone();
            }
            if let Some(v) = a.get("match_case") {
                p["matchCase"] = v.clone();
            }
            exec(b, "review.find", p).map(Out::Json).map_err(|e| contract::classify(&e, name, a, &[]))
        }
        "compare_documents" => {
            let mut p = json!({"path": a.get("revised").cloned().unwrap_or(Value::Null)});
            for k in ["original", "author", "level"] {
                if let Some(v) = a.get(k) {
                    p[k] = v.clone();
                }
            }
            let compared = exec(b, "review.compare", p).map_err(|e| contract::classify(&e, name, &json!({"path": a.get("revised")}), &[]))?;
            let mut report = exec(b, "review.changeReport", json!({})).map_err(|e| contract::classify(&e, name, a, &[]))?;
            report["compared"] = compared;
            Ok(Out::Json(report))
        }
        other => Err(ToolError::new("unknown_tool", format!("no tool `{other}`"), "call tools/list for the tools")),
    }
}

/// `review.edit`'s refusal (nothing changed) in the contract's shape, with the failing
/// operation and the closest phrases.
fn edit_error(msg: &str) -> ToolError {
    let (head, detail) = msg.split_once('\n').unwrap_or((msg, ""));
    let detail: Value = serde_json::from_str(detail).unwrap_or(Value::Null);
    let head = head.strip_prefix("bad parameters: ").unwrap_or(head);
    let closest: Vec<String> =
        detail["closest"].as_array().map(|v| v.iter().filter_map(|c| c["found"].as_str().map(str::to_string)).collect()).unwrap_or_default();
    let not_found = head.contains("not found") || head.contains("occurs");
    let at = detail["failedAt"].as_u64().map_or(String::new(), |i| format!("operation {}", i + 1));
    let next = match (not_found, closest.is_empty()) {
        (true, false) => format!(
            "nothing was changed: point {at} at text that is there, e.g. {}, or pass keep_going: true to apply the others",
            closest.iter().map(|c| format!("`{c}`")).collect::<Vec<_>>().join(", ")
        ),
        (true, true) => {
            format!("nothing was changed: check {at}'s anchor with get_text or find_anchor, or pass keep_going: true to apply the others")
        }
        _ => format!(
            "nothing was changed: fix {at} (ops: replace, insert, insert_paragraph, delete_paragraph, delete, comment) and send the batch again"
        ),
    };
    let mut e = ToolError::new(if not_found { "not_found" } else { "bad_params" }, head, next).with("closest", json!(closest));
    if let Some(i) = detail["failedAt"].as_u64() {
        e = e.with("failed_at", json!(i));
    }
    if !detail["closest"].is_null() {
        e = e.with("matches", detail["closest"].clone());
    }
    e
}
