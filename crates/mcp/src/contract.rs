//! WHIQ: the "no dead stops" contract for agents in long unattended runs.
//!
//! - Every error is a [`ToolError`]: a `code`, a one-line `reason` and a `next_step` the agent can
//!   act on (plus details such as `retry_after_ms`, `closest`, `recovered`).
//! - Inputs are forgiving: tool names, command ids and argument names match case-insensitively
//!   and in either spelling (`saveDocument`, `save-document`, `Save_Document`); arguments outside
//!   the schema come back as warnings on a successful call; numbers and booleans given as strings
//!   are read as such.
//! - Oversized results are trimmed with `truncated: true` and a `continue_from` to ask for the
//!   rest ([`trim`]).

use serde_json::{Map, Value, json};

/// A tool error in the contract's shape.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolError {
    pub code: &'static str,
    pub reason: String,
    pub next_step: String,
    /// More fields for the agent (`retry_after_ms`, `closest`, `recovered`, `job_id`…).
    pub extra: Map<String, Value>,
}

impl ToolError {
    pub fn new(code: &'static str, reason: impl Into<String>, next_step: impl Into<String>) -> Self {
        let reason: String = reason.into();
        // One line: the first line of the engine's message.
        let reason = reason.lines().next().unwrap_or("").trim().to_string();
        Self { code, reason, next_step: next_step.into(), extra: Map::new() }
    }

    pub fn with(mut self, key: &str, v: Value) -> Self {
        self.extra.insert(key.to_string(), v);
        self
    }

    /// `{code, reason, next_step, …extra}`.
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("code".into(), json!(self.code));
        m.insert("reason".into(), json!(self.reason));
        m.insert("next_step".into(), json!(self.next_step));
        for (k, v) in &self.extra {
            m.insert(k.clone(), v.clone());
        }
        Value::Object(m)
    }

    pub fn busy(retry_after_ms: u64, what: &str, job: Option<&str>) -> Self {
        let next = match job {
            Some(j) => format!("retry after {retry_after_ms} ms, or call job_status {{\"job_id\":\"{j}\"}} to follow the running job"),
            None => format!("retry the same call after {retry_after_ms} ms"),
        };
        let mut e = ToolError::new("busy", format!("the server is busy with {what}"), next).with("retry_after_ms", json!(retry_after_ms));
        if let Some(j) = job {
            e = e.with("job_id", json!(j));
        }
        e
    }
}

/// A name in comparable form: lower case, without `_`, `-`, `.`, spaces.
pub fn norm(s: &str) -> String {
    s.chars().filter(|c| !matches!(c, '_' | '-' | '.' | ' ' | '/')).flat_map(char::to_lowercase).collect()
}

/// Levenshtein distance (bounded inputs: names), on characters.
pub fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().take(128).collect();
    let b: Vec<char> = b.chars().take(128).collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Up to `n` candidates closest to `want` (by normalised edit distance, substrings first).
pub fn closest<'a>(want: &str, candidates: impl IntoIterator<Item = &'a str>, n: usize) -> Vec<String> {
    let w = norm(want);
    let mut scored: Vec<(usize, &str)> = candidates
        .into_iter()
        .map(|c| {
            let nc = norm(c);
            let d = if !w.is_empty() && (nc.contains(&w) || w.contains(&nc)) { 0 } else { distance(&w, &nc) };
            (d, c)
        })
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.len().cmp(&b.1.len())));
    scored.dedup_by(|a, b| a.1 == b.1);
    let limit = (w.chars().count() / 2).max(3);
    scored.into_iter().filter(|(d, _)| *d <= limit).take(n).map(|(_, c)| c.to_string()).collect()
}

/// The one candidate equal to `want` once normalised, if exactly one is.
pub fn unique_match<'a>(want: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let w = norm(want);
    let mut found = None;
    for c in candidates {
        if norm(c) == w {
            if found.is_some_and(|f| f != c) {
                return None;
            }
            found = Some(c);
        }
    }
    found
}

/// Phrases of `haystack` (runs of as many words as `needle` has) closest to `needle`, best
/// first, for a `not_found` answer: (phrase, edit distance).
pub fn closest_phrases(haystack: &str, needle: &str, n: usize) -> Vec<(String, usize)> {
    let want: Vec<&str> = needle.split_whitespace().collect();
    let k = want.len().clamp(1, 32);
    let words: Vec<&str> = haystack.split_whitespace().take(200_000).collect();
    let target = want.join(" ").to_lowercase();
    let mut scored: Vec<(String, usize)> = words
        .windows(k.min(words.len().max(1)))
        .map(|w| {
            let p = w.join(" ");
            let d = distance(&p.to_lowercase(), &target);
            (p, d)
        })
        .collect();
    scored.sort_by_key(|x| x.1);
    let mut out: Vec<(String, usize)> = Vec::new();
    for (p, d) in scored {
        if out.len() >= n {
            break;
        }
        if d <= target.chars().count().max(4) / 2 + 1 && !out.iter().any(|(q, _)| *q == p) {
            out.push((p, d));
        }
    }
    out
}

/// Byte offset of the `occurrence`-th (1-based) case-insensitive match of `needle` in `hay`.
pub fn find_ci(hay: &str, needle: &str, occurrence: usize) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    let lower_needle: Vec<char> = needle.chars().flat_map(char::to_lowercase).collect();
    let mut seen = 0;
    for (i, _) in hay.char_indices() {
        let mut it = hay.get(i..)?.chars().flat_map(char::to_lowercase);
        if lower_needle.iter().all(|c| it.next() == Some(*c)) {
            seen += 1;
            if seen == occurrence.max(1) {
                return Some(i);
            }
        }
    }
    None
}

/// Arguments every tool takes besides its own (see [`crate::tools::tool_definitions`]).
pub const COMMON_ARGS: &[&str] = &["caller", "background", "overwrite", "continue_from", "max_bytes"];

/// Arguments after matching names to the schema's `properties` (case and spelling), reading
/// numbers and booleans given as strings, with a warning for each argument outside the schema
/// (dropped) and each name read as another. `Err` when a required argument is missing.
pub fn normalise_args(args: &Value, schema: &Value) -> Result<(Value, Vec<String>), ToolError> {
    let props = schema.get("properties").and_then(Value::as_object).cloned().unwrap_or_default();
    let mut names: Vec<String> = props.keys().cloned().collect();
    names.extend(COMMON_ARGS.iter().map(|s| s.to_string()));
    let mut out = Map::new();
    let mut warnings = Vec::new();
    match args {
        Value::Null => {}
        Value::Object(m) => {
            for (k, v) in m {
                let key = if names.iter().any(|n| n == k) {
                    k.clone()
                } else if let Some(n) = unique_match(k, names.iter().map(String::as_str)) {
                    warnings.push(format!("argument `{k}` read as `{n}`"));
                    n.to_string()
                } else {
                    let near = closest(k, names.iter().map(String::as_str), 3);
                    let hint = if near.is_empty() { String::new() } else { format!(" (did you mean {}?)", near.join(", ")) };
                    warnings.push(format!("argument `{k}` is not one this tool takes and was ignored{hint}"));
                    continue;
                };
                let ty = props.get(&key).and_then(|p| p.get("type")).and_then(Value::as_str).unwrap_or("");
                out.insert(key, coerce(v, ty));
            }
        }
        other => {
            return Err(ToolError::new(
                "bad_params",
                format!("arguments must be an object, got {}", kind(other)),
                "send the arguments as a JSON object, e.g. {\"path\": \"out.docx\"}",
            ));
        }
    }
    let required: Vec<String> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    let missing: Vec<&String> = required.iter().filter(|r| !out.contains_key(*r)).collect();
    if !missing.is_empty() {
        let list: Vec<String> = missing.iter().map(|m| format!("`{m}`")).collect();
        let props_doc: Vec<String> = props
            .iter()
            .map(|(k, p)| {
                format!("{k}: {}", p.get("description").and_then(Value::as_str).or_else(|| p.get("type").and_then(Value::as_str)).unwrap_or(""))
            })
            .collect();
        return Err(ToolError::new(
            "bad_params",
            format!("missing required argument {}", list.join(", ")),
            format!("call again with {}; arguments: {}", list.join(" and "), props_doc.join("; ")),
        ));
    }
    Ok((Value::Object(out), warnings))
}

fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// `v` read as `ty` where that is safe: `"2"` as 2 for a number, `"true"` as true for a boolean.
fn coerce(v: &Value, ty: &str) -> Value {
    match (ty, v) {
        ("number" | "integer", Value::String(s)) => {
            s.trim().parse::<f64>().ok().and_then(|n| serde_json::Number::from_f64(n).map(Value::Number)).unwrap_or_else(|| v.clone())
        }
        ("boolean", Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" | "on" => Value::Bool(true),
            "false" | "no" | "0" | "off" => Value::Bool(false),
            _ => v.clone(),
        },
        ("string", Value::Number(n)) => Value::String(n.to_string()),
        _ => v.clone(),
    }
}

/// An engine error message in the contract's shape. `tool` and `args` name the call (for the
/// next step); `commands` lists command ids, for an unknown one.
pub fn classify(msg: &str, tool: &str, args: &Value, commands: &[String]) -> ToolError {
    let lower = msg.to_ascii_lowercase();
    // Stopped by `cancel` at a safe point (the command was undone).
    if let Some(what) = msg.strip_prefix("cancelled: ") {
        return ToolError::new(
            "cancelled",
            format!("cancelled: {what}"),
            format!("the document is as it was before `{tool}`: call it again to do it"),
        );
    }
    let path = args.get("path").and_then(Value::as_str).unwrap_or("");
    if let Some(id) = msg.strip_prefix("unknown command `").and_then(|r| r.split('`').next()) {
        let near = closest(id, commands.iter().map(String::as_str), 5);
        return ToolError::new(
            "unknown_command",
            format!("no command `{id}`"),
            format!("use one of {} (list_commands {{\"query\": \"…\"}} lists them all)", list_or(&near, "list_commands")),
        )
        .with("closest", json!(near));
    }
    if lower.contains("outside the read root")
        || lower.contains("outside the write root")
        || lower.contains("authority is absent")
        || lower.contains("automation path rejected")
    {
        let (code, next) = if lower.contains("write") {
            ("path_refused", "save inside the write root: give a file name or a path under it (relative paths are resolved inside it)")
        } else {
            ("path_refused", "open files from inside the read root (server_status shows the roots)")
        };
        return ToolError::new(code, msg, next);
    }
    if lower.contains("no such file") || lower.contains("os error 2") || lower.contains("not found") || lower.contains("cannot find") {
        let near = nearby_files(path);
        let next =
            if near.is_empty() { "check the path (server_status shows the roots)".to_string() } else { format!("did you mean {}?", near.join(", ")) };
        return ToolError::new("not_found", msg, next).with("closest", json!(near));
    }
    if let Some(n) = lower.strip_prefix("no page ") {
        return ToolError::new(
            "out_of_range",
            msg,
            format!("ask for an existing page (inspect_document or server_status gives the page count); page {} doesn't exist", n.trim()),
        );
    }
    if lower.starts_with("bad parameters") || lower.contains("missing `") || lower.contains("must be") || lower.contains("required") {
        let next = if tool == "execute" {
            let cmd = args.get("command").and_then(Value::as_str).unwrap_or("");
            format!("fix the parameters; list_commands {{\"query\": \"{cmd}\"}} shows what `{cmd}` takes")
        } else {
            format!("fix the arguments; tools/list shows what `{tool}` takes")
        };
        return ToolError::new("bad_params", msg, next);
    }
    if lower.starts_with("not available") {
        return ToolError::new(
            "not_available",
            msg,
            "the command can't run in the current state (selection, view or protection): change that first, or use another command",
        );
    }
    if lower.contains("needs the desktop app") {
        return ToolError::new(
            "needs_app",
            msg,
            "this tool drives the desktop app: start `wordcraft --control 7981` and `wordcraft-cli mcp --connect 127.0.0.1:7981`, or use render_page instead of screenshots",
        );
    }
    if lower.contains("password") {
        return ToolError::new("needs_password", msg, "call again with the document's `password`");
    }
    if lower.contains("panicked") {
        return ToolError::new("internal", msg, "the document is unchanged; try another way to do this (the failure was logged)");
    }
    ToolError::new("failed", msg, "check the document with inspect_document, then retry with other arguments or another command")
}

fn list_or(v: &[String], fallback: &str) -> String {
    if v.is_empty() { fallback.to_string() } else { v.iter().map(|s| format!("`{s}`")).collect::<Vec<_>>().join(", ") }
}

/// File names in `path`'s folder closest to its name (for a `not_found`).
fn nearby_files(path: &str) -> Vec<String> {
    if path.is_empty() {
        return Vec::new();
    }
    let p = std::path::Path::new(path);
    let dir = p.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
    let want = p.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let names: Vec<String> = entries.filter_map(Result::ok).take(2000).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    closest(&want, names.iter().map(String::as_str), 5).into_iter().map(|n| dir.join(n).to_string_lossy().into_owned()).collect()
}

/// Trim `v` to at most `max` bytes of JSON, starting at `from`: the largest list in it (the value
/// itself, or one of its fields) is cut to the items that fit, or its largest text to the
/// characters that fit; the result says `truncated: true`, `continue_from` (pass it back to get
/// the rest) and `total`. Within the limit (and `from` 0), `v` is returned as is.
pub fn trim(v: Value, from: usize, max: usize) -> Value {
    let size = |x: &Value| serde_json::to_string(x).map(|s| s.len()).unwrap_or(0);
    if from == 0 && size(&v) <= max {
        return v;
    }
    // Where the bulk is: the value itself or one field of it.
    let field = match &v {
        Value::Object(m) => m.iter().filter(|(_, x)| x.is_array() || x.is_string()).max_by_key(|(_, x)| size(x)).map(|(k, _)| k.clone()),
        _ => None,
    };
    let (bulk, rest) = match (&field, v) {
        (Some(f), Value::Object(mut m)) => {
            let b = m.remove(f).unwrap_or(Value::Null);
            (b, Some((f.clone(), m)))
        }
        (_, other) => (other, None),
    };
    let overhead = rest.as_ref().map_or(0, |(_, m)| size(&Value::Object(m.clone()))) + 160;
    let room = max.saturating_sub(overhead).max(256);
    let (part, next, total) = match bulk {
        Value::Array(items) => {
            let total = items.len();
            let mut used = 2;
            let mut out = Vec::new();
            for it in items.into_iter().skip(from) {
                let s = size(&it) + 1;
                if used + s > room && !out.is_empty() {
                    break;
                }
                used += s;
                out.push(it);
            }
            let next = from + out.len();
            (Value::Array(out), (next < total).then_some(next), total)
        }
        Value::String(s) => {
            let total = s.chars().count();
            let mut out = String::new();
            let mut n = 0;
            for c in s.chars().skip(from) {
                if out.len() + c.len_utf8() + 8 > room {
                    break;
                }
                out.push(c);
                n += 1;
            }
            let next = from + n;
            (Value::String(out), (next < total).then_some(next), total)
        }
        other => (other, None, 1),
    };
    let mut m = match rest {
        Some((f, mut m)) => {
            m.insert(f, part);
            m
        }
        None => {
            let mut m = Map::new();
            m.insert("items".into(), part);
            m
        }
    };
    m.insert("truncated".into(), json!(next.is_some()));
    if let Some(n) = next {
        m.insert("continue_from".into(), json!(n));
        m.insert("next_step".into(), json!(format!("call the same tool again with continue_from: {n} for the rest")));
    }
    m.insert("total".into(), json!(total));
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_match_in_any_spelling() {
        let tools = ["save_document", "open_document", "server_status"];
        assert_eq!(unique_match("SaveDocument", tools), Some("save_document"));
        assert_eq!(unique_match("save-document", tools), Some("save_document"));
        assert_eq!(unique_match("Server Status", tools), Some("server_status"));
        assert_eq!(unique_match("save", tools), None);
        assert_eq!(closest("save_documnet", tools, 2).first().map(String::as_str), Some("save_document"));
    }

    #[test]
    fn arguments_are_forgiving() {
        let schema = json!({"type": "object", "properties": {"keepGoing": {"type": "boolean"}, "page": {"type": "number"}, "path": {"type": "string"}}, "required": ["path"]});
        let (a, w) = normalise_args(&json!({"keep_going": "true", "PAGE": "3", "path": "x", "bogus": 1}), &schema).unwrap();
        assert_eq!(a, json!({"keepGoing": true, "page": 3.0, "path": "x"}));
        assert_eq!(w.len(), 3, "{w:?}");
        let e = normalise_args(&json!({}), &schema).unwrap_err();
        assert_eq!(e.code, "bad_params");
        assert!(e.next_step.contains("`path`"), "{e:?}");
    }

    #[test]
    fn oversized_lists_and_text_are_trimmed_with_a_continuation() {
        let big: Vec<Value> = (0..1000).map(|i| json!({"i": i, "text": "x".repeat(50)})).collect();
        let t = trim(json!({"blocks": big, "pages": 3}), 0, 4000);
        assert_eq!(t["truncated"], true);
        let n = t["continue_from"].as_u64().unwrap() as usize;
        assert!(n > 10 && n < 1000, "{n}");
        assert_eq!(t["pages"], 3);
        assert!(serde_json::to_string(&t).unwrap().len() <= 4000);
        let t2 = trim(json!({"blocks": (0..1000).map(|i| json!({"i": i, "text": "x".repeat(50)})).collect::<Vec<_>>(), "pages": 3}), n, 4000);
        assert_eq!(t2["blocks"][0]["i"], n);
        let s = trim(json!({"text": "é".repeat(10_000)}), 0, 2000);
        assert_eq!(s["truncated"], true);
        assert!(s["text"].as_str().unwrap().chars().all(|c| c == 'é'));
        // Small results are untouched.
        assert_eq!(trim(json!({"a": 1}), 0, 100), json!({"a": 1}));
    }

    #[test]
    fn engine_errors_get_codes_and_next_steps() {
        let e = classify("unknown command `format.blod`", "execute", &json!({}), &["format.bold".into(), "format.italic".into()]);
        assert_eq!(e.code, "unknown_command");
        assert_eq!(e.extra["closest"][0], "format.bold");
        let e = classify("automation path rejected: outside the write root /w: /etc/x", "save_document", &json!({}), &[]);
        assert_eq!(e.code, "path_refused");
        let e = classify("bad parameters: `rows` must be 1–63", "execute", &json!({"command": "insert.table"}), &[]);
        assert_eq!(e.code, "bad_params");
        assert!(e.next_step.contains("insert.table"));
        assert!(!e.reason.is_empty() && !e.next_step.is_empty());
    }
}
