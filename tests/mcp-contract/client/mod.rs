//! A small MCP client for the contract tests: starts `wordcraft-cli mcp` with a fresh write root,
//! sends JSON-RPC lines and reads the replies (with a timeout, so a hang fails the test instead
//! of stalling it). Every `tools/call` result is checked to carry `structuredContent`, and every
//! error `code`, `reason` and `next_step`.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use serde_json::{Value, json};

/// The binary under test: `$WORDCRAFT_CLI` (e.g. the release build) or the one cargo built.
pub fn binary() -> PathBuf {
    std::env::var_os("WORDCRAFT_CLI").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_wordcraft-cli")))
}

/// A fresh, empty folder (removed when dropped).
pub struct Dir(pub PathBuf);

impl Dir {
    pub fn new(tag: &str) -> Dir {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!("whiq-mcp-{tag}-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Dir(plain_path(std::fs::canonicalize(&p).unwrap()))
    }
    pub fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
    pub fn s(&self, name: &str) -> String {
        self.path(name).to_string_lossy().into_owned()
    }
    /// File names in the folder, sorted.
    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(&self.0).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub struct Mcp {
    pub child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    next: u64,
    /// Replies read while waiting for another one.
    early: std::collections::BTreeMap<u64, Value>,
    /// Notifications the server sent (progress), in order.
    pub notifications: Vec<Value>,
}

pub const WAIT: Duration = Duration::from_secs(60);

impl Mcp {
    /// `wordcraft-cli mcp` confined to `root` (read and write), with `extra` flags and the test
    /// hooks on.
    pub fn start(root: &Path, extra: &[&str]) -> Mcp {
        let r = root.to_string_lossy().into_owned();
        let mut args = vec!["mcp".to_string(), "--automation-read-root".into(), r.clone(), "--automation-write-root".into(), r];
        args.extend(extra.iter().map(|s| s.to_string()));
        Mcp::start_args(&args, root)
    }

    pub fn start_args(args: &[String], cwd: &Path) -> Mcp {
        let mut child = Command::new(binary())
            .args(args)
            .current_dir(cwd)
            .env("WORDCRAFT_MCP_TEST_HOOKS", "1")
            .env("WORDCRAFT_REGION", "en-US")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start wordcraft-cli mcp");
        let stdout = child.stdout.take().unwrap();
        let stdin = child.stdin.take();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(l) = line else { break };
                if tx.send(l).is_err() {
                    break;
                }
            }
        });
        let mut m = Mcp { child, stdin, lines: rx, next: 1, early: Default::default(), notifications: Vec::new() };
        let init = m.request(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "whiq-contract-test", "version": "1"}}),
        );
        assert_eq!(init["result"]["serverInfo"]["name"], "wordcraft", "{init}");
        m.notify("notifications/initialized", json!({}));
        m
    }

    pub fn send(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next;
        self.next += 1;
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
        id
    }

    pub fn notify(&mut self, method: &str, params: Value) {
        let line = json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string();
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
    }

    /// The reply with `id` (replies to other ids that arrive first are kept for later).
    pub fn reply(&mut self, id: u64) -> Value {
        self.reply_within(id, WAIT).unwrap_or_else(|| panic!("no reply to request {id} within {WAIT:?}: the server hung"))
    }

    /// The reply with `id`, if it comes within `wait`.
    pub fn reply_within(&mut self, id: u64, wait: Duration) -> Option<Value> {
        if let Some(v) = self.early.remove(&id) {
            return Some(v);
        }
        let end = std::time::Instant::now() + wait;
        loop {
            let left = end.saturating_duration_since(std::time::Instant::now());
            let line = self.lines.recv_timeout(left).ok()?;
            let v: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("reply is not JSON ({e}): {line}"));
            match v["id"].as_u64() {
                Some(got) if got == id => return Some(v),
                Some(got) => {
                    self.early.insert(got, v);
                }
                None if v.get("method").is_some() => self.notifications.push(v),
                None => panic!("a reply without an id: {v}"),
            }
        }
    }

    pub fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params);
        self.reply(id)
    }

    /// `tools/call` → the full result, checked against the contract's shape.
    pub fn call(&mut self, name: &str, args: Value) -> Value {
        let r = self.request("tools/call", json!({"name": name, "arguments": args}));
        let res = r.get("result").cloned().unwrap_or_else(|| panic!("{name}: no result: {r}"));
        check_shape(name, &res);
        res
    }

    /// A call expected to succeed: its structured content.
    pub fn ok(&mut self, name: &str, args: Value) -> Value {
        let res = self.call(name, args.clone());
        assert_eq!(res["isError"], false, "{name} {args}: {res}");
        res["structuredContent"].clone()
    }

    /// A call expected to fail with `code`: its error.
    pub fn err(&mut self, name: &str, args: Value, code: &str) -> Value {
        let res = self.call(name, args.clone());
        assert_eq!(res["isError"], true, "{name} {args} should fail: {res}");
        let e = res["structuredContent"]["error"].clone();
        assert_eq!(e["code"], code, "{name} {args}: {e}");
        e
    }

    /// Close stdin (the client goes away) and wait for the server to end.
    pub fn close(mut self) -> std::process::ExitStatus {
        drop(self.stdin.take());
        wait_exit(&mut self.child)
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}

/// Wait for `child` to end (bounded).
pub fn wait_exit(child: &mut Child) -> std::process::ExitStatus {
    for _ in 0..600 {
        if let Some(s) = child.try_wait().unwrap() {
            return s;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    panic!("the server did not end within 60 s");
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The error codes every craft app answers with (CONTRACT-PARITY.md): no other code may appear.
pub const CODES: &[&str] = &[
    "busy",
    "cancelled",
    "disk_full",
    "evicted",
    "failed",
    "internal",
    "invalid_argument",
    "invalid_request",
    "locked",
    "no_document",
    "not_available",
    "not_found",
    "out_of_range",
    "outside_root",
    "password_required",
    "too_large",
    "unsupported",
    "wrong_password",
];

/// Every tool result carries `structuredContent` (an object); an error carries `code` (one of
/// [`CODES`]), `reason` and `next_step`, all non-empty.
pub fn check_shape(name: &str, res: &Value) {
    assert!(res["structuredContent"].is_object(), "{name}: no structuredContent: {res}");
    assert!(res["content"].as_array().is_some_and(|c| !c.is_empty()), "{name}: no content: {res}");
    if res["isError"] == true {
        let e = &res["structuredContent"]["error"];
        for k in ["code", "reason", "next_step"] {
            assert!(e[k].as_str().is_some_and(|s| !s.trim().is_empty()), "{name}: error without {k}: {res}");
        }
        assert!(CODES.contains(&e["code"].as_str().unwrap_or("")), "{name}: `{}` is not a shared error code: {res}", e["code"]);
    }
}

/// The canonical path in the form the server reports it: on Windows, canonicalize gives the verbatim `\\?\C:\...` form, while
/// the server (and the person reading its answers) uses the plain `C:\...` path to the same folder. A UNC path keeps its prefix.
pub fn plain_path(p: PathBuf) -> PathBuf {
    if let Some(rest) = p.to_str().and_then(|s| s.strip_prefix(r"\\?\"))
        && !rest.starts_with(r"UNC\")
    {
        return PathBuf::from(rest);
    }
    p
}
