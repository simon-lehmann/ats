//! TranscriptWatcher (plan §4.2): find a session's Claude Code JSONL and
//! classify quiet sessions from its tail.
//!
//! Zero-cost heuristics only — the classifier is consulted when the
//! heartbeat finds a session quiet, so "last assistant message has a pending
//! tool_use" means it's waiting on a permission prompt, not mid-tool-run.
//! Everything here degrades gracefully: no transcript → plain `Idle`.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use ats_core::state::SessionState;
use serde_json::Value;

/// How much of the file tail to inspect.
const TAIL_BYTES: u64 = 64 * 1024;
const DETAIL_MAX: usize = 120;

pub struct Classification {
    pub state: SessionState,
    pub detail: Option<String>,
}

/// Claude Code encodes a project cwd into a directory name under
/// `~/.claude/projects/` by replacing every non-alphanumeric character
/// with `-` (e.g. `/home/x/repo` → `-home-x-repo`). Verified against a real
/// installation; discovery falls back to "newest jsonl" anyway.
pub fn project_dir_for_cwd(claude_home: &Path, cwd: &str) -> PathBuf {
    let encoded: String = cwd
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    claude_home.join("projects").join(encoded)
}

pub fn default_claude_home() -> PathBuf {
    if let Ok(p) = std::env::var("CLAUDE_CONFIG_DIR") {
        return p.into();
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    Path::new(&home).join(".claude")
}

/// Newest `*.jsonl` in the project dir modified at/after `not_before`
/// (unix secs) — the transcript belonging to a session we just spawned.
pub fn discover_transcript(
    claude_home: &Path,
    cwd: &str,
    not_before: i64,
) -> Option<PathBuf> {
    let dir = project_dir_for_cwd(claude_home, cwd);
    let mut best: Option<(i64, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()? {
        let entry = entry.ok()?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let mtime = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)?;
        if mtime + 5 >= not_before && best.as_ref().map(|(m, _)| mtime > *m).unwrap_or(true) {
            best = Some((mtime, path));
        }
    }
    best.map(|(_, p)| p)
}

/// Read the tail of a transcript and classify (plan §4.2 heuristics):
/// - last assistant text ends with `?`            → needs_input (question verbatim)
/// - last assistant message has a pending tool_use → needs_input (permission)
/// - last assistant message is plain text          → finished (text as detail)
/// - anything else                                 → idle
pub fn classify_file(path: &Path) -> Classification {
    let Some(lines) = read_tail_lines(path) else {
        return Classification { state: SessionState::Idle, detail: None };
    };
    classify_lines(lines.iter().map(String::as_str))
}

fn read_tail_lines(path: &Path) -> Option<Vec<String>> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = String::new();
    f.read_to_string(&mut buf).ok()?;
    // first line may be cut in half by the seek; drop it
    let skip = if start > 0 { 1 } else { 0 };
    Some(buf.lines().skip(skip).map(str::to_owned).collect())
}

pub fn classify_lines<'a>(lines: impl DoubleEndedIterator<Item = &'a str>) -> Classification {
    for line in lines.rev() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "assistant" => return classify_assistant(&v),
            // a user line (tool result / prompt) means the model owes a
            // response; quiet here is just inference latency or a stall
            "user" => return Classification { state: SessionState::Idle, detail: None },
            // bookkeeping lines (summary, permission-mode, pr-link, …)
            _ => continue,
        }
    }
    Classification { state: SessionState::Idle, detail: None }
}

fn classify_assistant(v: &Value) -> Classification {
    let empty = Vec::new();
    let content = v
        .pointer("/message/content")
        .and_then(Value::as_array)
        .unwrap_or(&empty);

    let mut last_text: Option<&str> = None;
    let mut pending_tool: Option<&str> = None;
    for block in content {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    if !t.trim().is_empty() {
                        last_text = Some(t);
                        pending_tool = None;
                    }
                }
            }
            Some("tool_use") => {
                pending_tool = block.get("name").and_then(Value::as_str).or(Some("tool"));
            }
            _ => {}
        }
    }

    // quiet + trailing tool_use = waiting on a permission decision
    if let Some(tool) = pending_tool {
        return Classification {
            state: SessionState::NeedsInput,
            detail: Some(truncate(&format!("wants to run: {tool}"))),
        };
    }
    if let Some(text) = last_text {
        let trimmed = text.trim_end();
        if trimmed.ends_with('?') {
            let question = trimmed.lines().last().unwrap_or(trimmed);
            return Classification {
                state: SessionState::NeedsInput,
                detail: Some(truncate(question)),
            };
        }
        let summary = trimmed.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or(trimmed);
        return Classification {
            state: SessionState::Finished,
            detail: Some(truncate(summary)),
        };
    }
    Classification { state: SessionState::Idle, detail: None }
}

/// Flatten the transcript tail into readable dialogue for orchestrator
/// prompts: assistant text, the developer's messages, and tool names.
/// Capped at `max_chars`, keeping the newest content.
pub fn recent_dialogue(path: &Path, max_chars: usize) -> String {
    let Some(lines) = read_tail_lines(path) else { return String::new() };
    let mut parts: Vec<String> = Vec::new();
    for line in &lines {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
        if kind != "assistant" && kind != "user" {
            continue;
        }
        let empty = Vec::new();
        let content = v
            .pointer("/message/content")
            .and_then(Value::as_array)
            .unwrap_or(&empty);
        for block in content {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(t) = block.get("text").and_then(Value::as_str) {
                        if !t.trim().is_empty() {
                            parts.push(format!("{kind}: {t}"));
                        }
                    }
                }
                Some("tool_use") => {
                    if let Some(n) = block.get("name").and_then(Value::as_str) {
                        parts.push(format!("assistant: [runs {n}]"));
                    }
                }
                _ => {}
            }
        }
    }
    let mut out = String::new();
    for p in parts.iter().rev() {
        if out.len() + p.len() + 1 > max_chars {
            break;
        }
        out = format!("{p}\n{out}");
    }
    out
}

/// Rolling actions-per-minute over the last `window_secs`, computed from the
/// file tail. **Raw** APM: every tool the agent invoked counts equally — a
/// `Read` is worth the same as an `Edit` — because we're measuring pace, not
/// "productivity". Reads only the same 64 KB tail the classifier uses, so it's
/// cheap; under a burst large enough to overflow the tail it errs low. `0.0`
/// when there's no transcript yet.
pub fn apm_from_file(path: &Path, window_secs: i64, now: i64) -> f32 {
    let Some(lines) = read_tail_lines(path) else { return 0.0 };
    let n = actions_in_window(lines.iter().map(String::as_str), window_secs, now);
    n as f32 / (window_secs as f32 / 60.0)
}

/// Count `tool_use` blocks across assistant lines whose top-level `timestamp`
/// falls in the inclusive window `[now - window_secs, now]`.
pub fn actions_in_window<'a>(
    lines: impl Iterator<Item = &'a str>,
    window_secs: i64,
    now: i64,
) -> u32 {
    let floor = now - window_secs;
    let mut count = 0u32;
    for line in lines {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if v.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(ts) = v.get("timestamp").and_then(Value::as_str).and_then(parse_iso_epoch)
        else {
            continue;
        };
        if ts < floor || ts > now {
            continue;
        }
        let empty = Vec::new();
        let content = v
            .pointer("/message/content")
            .and_then(Value::as_array)
            .unwrap_or(&empty);
        count += content
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
            .count() as u32;
    }
    count
}

/// Parse a Claude Code transcript timestamp (`2026-06-14T16:06:02.123Z`, always
/// UTC) to unix seconds. Fractional seconds and any trailing offset are ignored
/// — Claude Code writes `Z`. Returns `None` on anything unexpected.
fn parse_iso_epoch(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    // YYYY-MM-DD(T| )HH:MM:SS — check the fixed separators, then the fields
    let sep_ok = b[4] == b'-'
        && b[7] == b'-'
        && matches!(b[10], b'T' | b't' | b' ')
        && b[13] == b':'
        && b[16] == b':';
    if !sep_ok {
        return None;
    }
    let num = |start: usize, len: usize| s.get(start..start + len)?.parse::<i64>().ok();
    let (year, month, day) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    let (hour, min, sec) = (num(11, 2)?, num(14, 2)?, num(17, 2)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + min * 60 + sec)
}

/// Days since the unix epoch for a proleptic-Gregorian date (Howard Hinnant's
/// `days_from_civil`). Dependency-free so we don't pull in a date crate.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The final assistant text in the transcript, if any — digest input.
pub fn final_text(path: &Path) -> Option<String> {
    let lines = read_tail_lines(path)?;
    for line in lines.iter().rev() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if v.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let content = v.pointer("/message/content").and_then(Value::as_array)?;
        let text: Vec<&str> = content
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect();
        if !text.is_empty() {
            return Some(text.join("\n"));
        }
    }
    None
}

fn truncate(s: &str) -> String {
    if s.chars().count() <= DETAIL_MAX {
        return s.to_string();
    }
    let cut: String = s.chars().take(DETAIL_MAX - 1).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant_line(content: &str) -> String {
        format!(r#"{{"type":"assistant","message":{{"role":"assistant","content":[{content}]}}}}"#)
    }

    fn assistant_line_ts(ts: &str, content: &str) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","message":{{"role":"assistant","content":[{content}]}}}}"#
        )
    }

    #[test]
    fn parse_iso_epoch_matches_known_instants() {
        assert_eq!(parse_iso_epoch("1970-01-01T00:00:00Z"), Some(0));
        // 2021-01-01T00:00:00Z = 1609459200
        assert_eq!(parse_iso_epoch("2021-01-01T00:00:00.000Z"), Some(1_609_459_200));
        // fractional seconds and the trailing offset are ignored
        assert_eq!(parse_iso_epoch("2021-01-01T00:00:01.999Z"), Some(1_609_459_201));
        assert_eq!(parse_iso_epoch("garbage"), None);
        assert_eq!(parse_iso_epoch(""), None);
    }

    #[test]
    fn actions_in_window_counts_tool_use_within_window_only() {
        let now = parse_iso_epoch("2026-06-14T16:10:00Z").unwrap();
        let window = 300; // 5 min
        let two = r#"{"type":"tool_use","name":"Read"},{"type":"tool_use","name":"Edit"}"#;
        let lines = [
            assistant_line_ts("2026-06-14T16:09:30Z", two), // 30s ago → +2
            assistant_line_ts("2026-06-14T16:06:00Z", r#"{"type":"tool_use","name":"Bash"}"#), // 4m ago → +1
            assistant_line_ts("2026-06-14T16:00:00Z", two), // 10m ago → outside window
            assistant_line_ts("2026-06-14T16:09:59Z", r#"{"type":"text","text":"hi"}"#), // no tool_use
            // a user/tool_result line is never an action
            r#"{"type":"user","timestamp":"2026-06-14T16:09:31Z","message":{"role":"user","content":[{"type":"tool_result"}]}}"#.to_string(),
        ];
        let n = actions_in_window(lines.iter().map(String::as_str), window, now);
        assert_eq!(n, 3, "two recent + one mid-window tool_use, rest excluded");
    }

    #[test]
    fn apm_from_missing_file_is_zero() {
        assert_eq!(apm_from_file(Path::new("/nonexistent.jsonl"), 300, 1), 0.0);
    }

    #[test]
    fn project_dir_encoding_matches_claude_code() {
        let d = project_dir_for_cwd(Path::new("/home/u/.claude"), "/home/dev/projects/repo");
        assert!(d.ends_with("projects/-home-dev-projects-repo"), "{d:?}");
    }

    #[test]
    fn question_means_needs_input_with_verbatim_question() {
        let line = assistant_line(
            r#"{"type":"text","text":"Done with refactor.\nShould I keep the legacy API?"}"#,
        );
        let c = classify_lines([line.as_str()].into_iter());
        assert_eq!(c.state, SessionState::NeedsInput);
        assert_eq!(c.detail.as_deref(), Some("Should I keep the legacy API?"));
    }

    #[test]
    fn pending_tool_use_means_needs_input() {
        let line = assistant_line(
            r#"{"type":"text","text":"Let me run the tests."},{"type":"tool_use","name":"Bash","input":{}}"#,
        );
        let c = classify_lines([line.as_str()].into_iter());
        assert_eq!(c.state, SessionState::NeedsInput);
        assert_eq!(c.detail.as_deref(), Some("wants to run: Bash"));
    }

    #[test]
    fn plain_text_means_finished_with_summary() {
        let line = assistant_line(r#"{"type":"text","text":"All tests pass. PR is ready."}"#);
        let c = classify_lines([line.as_str()].into_iter());
        assert_eq!(c.state, SessionState::Finished);
        assert_eq!(c.detail.as_deref(), Some("All tests pass. PR is ready."));
    }

    #[test]
    fn thinking_blocks_are_ignored() {
        let line = assistant_line(
            r#"{"type":"thinking","thinking":"hmm?"},{"type":"text","text":"Shipped."}"#,
        );
        let c = classify_lines([line.as_str()].into_iter());
        assert_eq!(c.state, SessionState::Finished);
    }

    #[test]
    fn user_tool_result_means_idle() {
        let lines = [
            assistant_line(r#"{"type":"text","text":"running"},{"type":"tool_use","name":"Bash"}"#),
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result"}]}}"#.to_string(),
        ];
        let c = classify_lines(lines.iter().map(String::as_str));
        assert_eq!(c.state, SessionState::Idle);
    }

    #[test]
    fn bookkeeping_lines_are_skipped() {
        let lines = [
            assistant_line(r#"{"type":"text","text":"Done."}"#),
            r#"{"type":"pr-link","prNumber":1}"#.to_string(),
            r#"{"type":"permission-mode","permissionMode":"auto"}"#.to_string(),
        ];
        let c = classify_lines(lines.iter().map(String::as_str));
        assert_eq!(c.state, SessionState::Finished);
    }

    #[test]
    fn long_detail_is_truncated() {
        let long = "x".repeat(500) + "?";
        let line = assistant_line(&format!(r#"{{"type":"text","text":"{long}"}}"#));
        let c = classify_lines([line.as_str()].into_iter());
        assert_eq!(c.state, SessionState::NeedsInput);
        assert!(c.detail.unwrap().chars().count() <= DETAIL_MAX);
    }

    #[test]
    fn empty_or_missing_file_is_idle() {
        let c = classify_file(Path::new("/nonexistent/file.jsonl"));
        assert_eq!(c.state, SessionState::Idle);
    }

    #[test]
    fn discover_picks_newest_after_spawn() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = "/work/proj";
        let proj = project_dir_for_cwd(tmp.path(), cwd);
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("a.jsonl"), "{}").unwrap();
        std::fs::write(proj.join("b.jsonl"), "{}").unwrap();
        // both fresh; not_before far in the past picks one of them
        let found = discover_transcript(tmp.path(), cwd, 0).unwrap();
        assert!(found.extension().unwrap() == "jsonl");
        // not_before far in the future finds nothing
        assert!(discover_transcript(tmp.path(), cwd, i64::MAX).is_none());
    }
}
