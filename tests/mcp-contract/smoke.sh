#!/usr/bin/env bash
# WHIQ MCP smoke test: runs the headless binary end to end over stdio, the way the platform's
# supervisor does (one process, read/write roots set to a scratch folder, author set), and checks
# every answer. Usage:
#   tests/mcp-contract/smoke.sh [path/to/wordcraft-cli]
# Default binary: target/release/wordcraft-cli (build it with `cargo build --release -p wordcraft-cli`;
# set CRAFT_FONTS_DIR to a craft-fonts checkout so the Arabic fallback face is embedded).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
bin="${1:-${CARGO_TARGET_DIR:-$repo/target}/release/wordcraft-cli}"
[ -x "$bin" ] || { echo "no binary at $bin (cargo build --release -p wordcraft-cli)"; exit 2; }
bin="$(cd "$(dirname "$bin")" && pwd)/$(basename "$bin")"   # the server is exec'd after a cd into the scratch folder, so a relative path must be made absolute first
work="$(mktemp -d "${TMPDIR:-/tmp}/whiq-smoke.XXXXXX")"
trap 'rm -rf "$work"' EXIT
fail=0
coproc MCP { cd "$work" && exec "$bin" mcp --automation-read-root "$work" --automation-write-root "$work" --author "Smoke Agent" 2>"$work/stderr.log"; }
id=0
# send METHOD PARAMS-JSON -> prints the reply line
send() {
  id=$((id + 1))
  printf '{"jsonrpc":"2.0","id":%d,"method":"%s","params":%s}\n' "$id" "$1" "$2" >&"${MCP[1]}"
  local line
  IFS= read -r -t 60 line <&"${MCP[0]}" || { echo "FAIL: no answer to $1 within 60 s"; fail=1; return; }
  printf '%s' "$line"
}
# check NAME ARGS-JSON EXPECT-REGEX: a tools/call whose answer must match
check() {
  local out
  out="$(send tools/call "{\"name\":\"$1\",\"arguments\":$2}")"
  if [[ "$out" =~ $3 ]] && [[ "$out" == *structuredContent* ]]; then
    echo "ok   $1"
  else
    echo "FAIL $1 $2"; echo "     ${out:0:400}"; fail=1
  fi
}
out="$(send initialize '{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"1"}}')"
[[ "$out" == *'"name":"wordcraft"'* ]] && echo "ok   initialize" || { echo "FAIL initialize: $out"; fail=1; }
printf '{"jsonrpc":"2.0","method":"notifications/initialized"}\n' >&"${MCP[1]}"
out="$(send tools/list '{}')"
[[ "$out" == *'"readOnlyHint":true'* && "$out" == *'"destructiveHint":true'* && "$out" == *server_status* ]] && echo "ok   tools/list" || { echo "FAIL tools/list"; fail=1; }
check new_document '{}' '"isError":false'
check type_text '{"text":"Quarterly report\nمرحبا بالعالم\nSales grew.","paragraphs":true}' '"isError":false'
check save_document '{"path":"report.docx"}' '"saved":true'
check save_document '{"path":"report.docx"}' 'OLD_report.docx'
check select_text '{"text":"sales grew"}' 'ignoring case'
check execute '{"command":"FORMAT_BOLD"}' 'read as `format.bold`'
check execute '{"command":"format.blod"}' '"code":"not_found"'
check save_document '{"path":"../escape.docx"}' '"code":"outside_root"'
check render_page '{"page":1,"dpi":48}' '"type":"image"'
check export_pdf '{"path":"report.pdf","markup":true}' '"isError":false'
check inspect_document '{"max_bytes":2000}' '"isError":false'
check review_edit '{"caller":"Smoke Reviewer","ops":[{"op":"replace","anchor":"Sales grew","with":"Sales rose"},{"op":"comment","anchor":"Quarterly","text":"Check the title"}]}' '"applied":2'
check list_changes '{}' '"author":"Smoke Reviewer"'
check accept_changes '{"author":"Smoke Reviewer"}' '"resolved":\['
check roundtrip_check '{"path":"report.docx"}' '"lossless":true'
check import_markdown '{"markdown":"---\ntitle: Smoke\n---\n\n[TOC]\n\n# One\n\n| A | B |\n|---|---|\n| 1 | 2 |\n\n> [!NOTE]\n> Callout.\n\n\\pagebreak\n\n## Two\n"}' '"callouts":1'
check render_page '{"pages":"1-2","dpi":36}' '"total_pages"'
check save_document '{"path":"markdown.docx"}' '"saved":true'
check server_status '{}' '"faces":[1-9][0-9]*,"families":\["'
check server_status '{}' '"measured":true'
check list_jobs '{}' '"jobs":\[\]'
check job_status '{"job_id":"job-1"}' '"code":"not_found"'
check cancel '{}' '"status":"not_running"'
check evict '{}' '"evicted":\[\{'
check get_text '{}' '"code":"evicted"'
exec {MCP[1]}>&-
wait "$MCP_PID" || { echo "FAIL server exit status $?"; fail=1; }
for f in report.docx OLD_report.docx report.pdf; do
  [ -s "$work/$f" ] && echo "ok   file $f" || { echo "FAIL missing $f"; fail=1; }
done
[ "$fail" = 0 ] && echo "SMOKE OK" || { echo "SMOKE FAILED"; cat "$work/stderr.log"; }
exit "$fail"
