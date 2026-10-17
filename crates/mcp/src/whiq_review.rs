//! WHIQ: review tooling (filled in by the review work: replace / insert / delete / comment by
//! anchor, accept / reject, change report).

use serde_json::Value;

use crate::backend::Backend;
use crate::contract::ToolError;
use crate::whiq::{Limits, Out};

pub fn tool_definitions() -> Vec<Value> {
    Vec::new()
}

pub fn handles(_name: &str) -> bool {
    false
}

pub fn run(_b: &mut dyn Backend, name: &str, _a: &Value, _limits: &Limits, _warnings: &mut Vec<String>) -> Result<Out, ToolError> {
    Err(ToolError::new("unknown_tool", format!("no tool `{name}`"), "call tools/list for the tools"))
}
