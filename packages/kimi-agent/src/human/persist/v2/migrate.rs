use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine;
use chrono::{NaiveDate, NaiveDateTime, TimeZone};
use futures_util::FutureExt;
use serde_json::{Map, Number, Value, json};
use tokio::fs;

use crate::human::persist::v2::convert::{V2BlobResolver, convert_v2_message};
use crate::human::persist::v2::fold::fold_v2_wire_records;
use crate::human::persist::v2::wire::{V2WireError, read_v2_wire_records};
use crate::human::session::events::{
    AGENT_OPENED_TYPE, SESSION_LOG_BRANCH, SESSION_META_UPDATED_TYPE, agent_opened,
    session_meta_updated,
};
use crate::human::store::backend::node::NodeBackend;
use crate::human::store::branch::Branch;
use crate::human::store::store::TreeStore;
use crate::human::store::types::{AppendInput, StoreError, TreeStoreOptions};
use crate::wire::record::WireAsyncError;

pub const V2_SESSION_TREE_NAME: &str = "session";

#[derive(Debug)]
pub struct V2MigrationError {
    pub code: String,
    pub message: String,
}

impl V2MigrationError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for V2MigrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for V2MigrationError {}

impl From<std::io::Error> for V2MigrationError {
    fn from(error: std::io::Error) -> Self {
        Self::new("io", error.to_string())
    }
}

impl From<StoreError> for V2MigrationError {
    fn from(error: StoreError) -> Self {
        Self::new(error.code, error.message)
    }
}

impl From<V2WireError> for V2MigrationError {
    fn from(error: V2WireError) -> Self {
        Self::new(error.code, error.message)
    }
}

impl From<serde_json::Error> for V2MigrationError {
    fn from(error: serde_json::Error) -> Self {
        Self::new("io", error.to_string())
    }
}

impl From<WireAsyncError> for V2MigrationError {
    fn from(error: WireAsyncError) -> Self {
        Self::new("io", error.to_string())
    }
}

pub struct V2MigrationResult {
    pub tree_name: String,
    pub agents: Vec<String>,
    pub blob_error: Option<String>,
}

async fn path_is_directory(path: impl AsRef<Path>) -> bool {
    fs::metadata(path)
        .await
        .map(|metadata| metadata.is_dir())
        .unwrap_or(false)
}

async fn path_is_file(path: impl AsRef<Path>) -> bool {
    fs::metadata(path)
        .await
        .map(|metadata| metadata.is_file())
        .unwrap_or(false)
}

async fn merge_blobs_into(source: &Path, target: &Path) -> Option<String> {
    let result = async {
        fs::create_dir_all(target).await?;
        let mut entries = fs::read_dir(source).await?;
        while let Some(entry) = entries.next_entry().await? {
            let from = entry.path();
            if !path_is_file(&from).await {
                continue;
            }
            let to = target.join(entry.file_name());
            if path_is_file(&to).await {
                continue;
            }
            if fs::rename(&from, &to).await.is_err() {
                let data = fs::read(&from).await?;
                fs::write(&to, data).await?;
            }
        }
        Ok::<(), std::io::Error>(())
    }
    .await;
    result.err().map(|error| error.to_string())
}

async fn list_v2_agent_ids(dir: &str) -> Vec<String> {
    let agents_dir = Path::new(dir).join("agents");
    let mut entries = match fs::read_dir(&agents_dir).await {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut names = Vec::new();
    loop {
        match entries.next_entry().await {
            Ok(Some(entry)) => names.push(entry.file_name().to_string_lossy().into_owned()),
            Ok(None) => break,
            Err(_) => return Vec::new(),
        }
    }
    names.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
    let mut ids = Vec::new();
    for name in names {
        if path_is_file(agents_dir.join(&name).join("wire.jsonl")).await {
            ids.push(name);
        }
    }
    ids
}

pub async fn is_v2_session_dir(dir: &str) -> bool {
    if path_is_directory(Path::new(dir).join("trees")).await {
        return false;
    }
    if !path_is_file(Path::new(dir).join("state.json")).await {
        return false;
    }
    !list_v2_agent_ids(dir).await.is_empty()
}

struct IsoDateTime {
    date: NaiveDate,
    time: Option<(u32, u32, u32, u32)>,
    offset_seconds: Option<i32>,
}

fn decimal(input: &str, start: usize, length: usize) -> Option<u32> {
    let end = start.checked_add(length)?;
    let bytes = input.as_bytes();
    if end > bytes.len() {
        return None;
    }
    let mut value = 0_u32;
    for byte in &bytes[start..end] {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value * 10 + u32::from(byte - b'0');
    }
    Some(value)
}

fn parse_iso_date_time(input: &str) -> Option<IsoDateTime> {
    let bytes = input.as_bytes();
    let (year, mut index) = match bytes.first() {
        Some(b'+' | b'-') => {
            let negative = bytes[0] == b'-';
            let value = decimal(input, 1, 6)? as i32;
            if negative && value == 0 {
                return None;
            }
            (if negative { -value } else { value }, 7)
        }
        _ => (decimal(input, 0, 4)? as i32, 4),
    };
    if index == bytes.len() {
        return Some(IsoDateTime {
            date: NaiveDate::from_ymd_opt(year, 1, 1)?,
            time: None,
            offset_seconds: None,
        });
    }
    if bytes.get(index) != Some(&b'-') {
        return None;
    }
    let month = decimal(input, index + 1, 2)?;
    index += 3;
    if index == bytes.len() {
        return Some(IsoDateTime {
            date: NaiveDate::from_ymd_opt(year, month, 1)?,
            time: None,
            offset_seconds: None,
        });
    }
    if bytes.get(index) != Some(&b'-') {
        return None;
    }
    let day = decimal(input, index + 1, 2)?;
    index += 3;
    let date = NaiveDate::from_ymd_opt(year, month, day)?;
    if index == bytes.len() {
        return Some(IsoDateTime {
            date,
            time: None,
            offset_seconds: None,
        });
    }
    if !matches!(bytes.get(index), Some(b'T' | b't' | b' ')) {
        return None;
    }
    index += 1;
    let hour = decimal(input, index, 2)?;
    if bytes.get(index + 2) != Some(&b':') {
        return None;
    }
    let minute = decimal(input, index + 3, 2)?;
    index += 5;
    let mut second = 0;
    if bytes.get(index) == Some(&b':') {
        second = decimal(input, index + 1, 2)?;
        index += 3;
    }
    let mut millisecond = 0;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let fraction_start = index;
        let mut digits = 0_usize;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            if digits < 3 {
                millisecond = millisecond * 10 + u32::from(bytes[index] - b'0');
            }
            digits += 1;
            index += 1;
        }
        if index == fraction_start {
            return None;
        }
        while digits < 3 {
            millisecond *= 10;
            digits += 1;
        }
    }
    let offset_seconds = if index == bytes.len() {
        None
    } else if matches!(bytes.get(index), Some(b'Z' | b'z')) {
        index += 1;
        if index != bytes.len() {
            return None;
        }
        Some(0)
    } else if matches!(bytes.get(index), Some(b'+' | b'-')) {
        let negative = bytes[index] == b'-';
        index += 1;
        let hours = decimal(input, index, 2)?;
        index += 2;
        let mut minutes = 0;
        if index < bytes.len() {
            if bytes[index] == b':' {
                index += 1;
            }
            minutes = decimal(input, index, 2)?;
            index += 2;
        }
        if index != bytes.len() || hours > 23 || minutes > 59 {
            return None;
        }
        let seconds = (hours * 3600 + minutes * 60) as i32;
        Some(if negative { -seconds } else { seconds })
    } else {
        return None;
    };
    if minute > 59 || second > 59 || hour > 24 {
        return None;
    }
    let (date, hour) = if hour == 24 {
        if minute != 0 || second != 0 || millisecond != 0 {
            return None;
        }
        (date.succ_opt()?, 0)
    } else {
        (date, hour)
    };
    Some(IsoDateTime {
        date,
        time: Some((hour, minute, second, millisecond)),
        offset_seconds,
    })
}

fn valid_epoch_ms(value: i64) -> Option<f64> {
    (-8_640_000_000_000_000..=8_640_000_000_000_000)
        .contains(&value)
        .then_some(value as f64)
}

fn local_timestamp_ms(date_time: NaiveDateTime) -> Option<i64> {
    let resolve = |date_time: NaiveDateTime| {
        match chrono::Local.from_local_datetime(&date_time) {
            chrono::LocalResult::Single(value) => Some(value.timestamp_millis()),
            chrono::LocalResult::Ambiguous(first, second) => Some(
                first
                    .timestamp_millis()
                    .min(second.timestamp_millis()),
            ),
            chrono::LocalResult::None => None,
        }
    };
    if let Some(value) = resolve(date_time) {
        return Some(value);
    }
    for minutes in 1..=24 * 60 {
        if let Some(value) = resolve(date_time + chrono::Duration::minutes(minutes)) {
            return Some(value);
        }
    }
    None
}

fn js_trim(value: &str) -> &str {
    value.trim_matches(|character| {
        matches!(
            character,
            '\u{0009}'..='\u{000d}'
                | ' '
                | '\u{00a0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
        )
    })
}

fn parse_js_date(input: &str) -> Option<f64> {
    let input = js_trim(input);
    if input.is_empty() {
        return None;
    }
    if let Some(parsed) = parse_iso_date_time(input) {
        let (hour, minute, second, millisecond) = parsed.time.unwrap_or((0, 0, 0, 0));
        let date_time = parsed
            .date
            .and_hms_nano_opt(hour, minute, second, millisecond * 1_000_000)?;
        let timestamp = if parsed.time.is_none() {
            date_time.and_utc().timestamp_millis()
        } else if let Some(offset) = parsed.offset_seconds {
            date_time.and_utc().timestamp_millis() - i64::from(offset) * 1000
        } else {
            local_timestamp_ms(date_time)?
        };
        return valid_epoch_ms(timestamp);
    }
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(input) {
        return valid_epoch_ms(parsed.timestamp_millis());
    }
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc2822(input) {
        return valid_epoch_ms(parsed.timestamp_millis());
    }
    None
}

fn to_epoch_ms(value: Option<&Value>) -> f64 {
    match value {
        Some(Value::Number(number)) => number.as_f64().filter(|value| value.is_finite()).unwrap_or(0.0),
        Some(Value::String(text)) => parse_js_date(text).unwrap_or(0.0),
        _ => 0.0,
    }
}

fn number(value: f64) -> Value {
    Number::from_f64(value).map(Value::Number).unwrap_or(Value::Null)
}

fn normalize_title(raw: &Map<String, Value>) -> (Option<String>, Option<String>) {
    let title = raw.get("title").and_then(Value::as_str).map(str::to_string);
    if title.is_some() && raw.get("isCustomTitle").and_then(Value::as_bool) == Some(true) {
        return (title, Some("custom".to_string()));
    }
    if title.is_some()
        && raw
            .get("titleKind")
            .and_then(Value::as_str)
            .is_some_and(|kind| matches!(kind, "replaceable" | "generated" | "custom"))
    {
        return (
            title,
            raw.get("titleKind")
                .and_then(Value::as_str)
                .map(str::to_string),
        );
    }
    if title.is_some() && raw.get("isCustomTitle").and_then(Value::as_bool) == Some(false) {
        return (title, Some("replaceable".to_string()));
    }
    if let Some(custom_title) = raw.get("customTitle").and_then(Value::as_str) {
        return (Some(custom_title.to_string()), Some("custom".to_string()));
    }
    (title.clone(), title.map(|_| "replaceable".to_string()))
}

fn normalize_v2_session_meta(raw: &Map<String, Value>, fallback_id: &str) -> Map<String, Value> {
    let work_dir = raw.get("workDir").and_then(Value::as_str);
    let mut rest = raw.clone();
    for key in [
        "workDir",
        "titleSource",
        "isCustomTitle",
        "customTitle",
        "createdAt",
        "updatedAt",
    ] {
        rest.remove(key);
    }
    let cwd = rest
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| work_dir.filter(|work_dir| !work_dir.is_empty()).map(str::to_string));
    let (title, title_kind) = normalize_title(raw);
    let id = rest
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or(fallback_id)
        .to_string();
    rest.insert("id".to_string(), Value::String(id));
    rest.insert("version".to_string(), json!(2));
    match cwd {
        Some(cwd) => rest.insert("cwd".to_string(), Value::String(cwd)),
        None => rest.remove("cwd"),
    };
    match title {
        Some(title) => rest.insert("title".to_string(), Value::String(title)),
        None => rest.remove("title"),
    };
    match title_kind {
        Some(kind) => rest.insert("titleKind".to_string(), Value::String(kind)),
        None => rest.remove("titleKind"),
    };
    rest.insert("createdAt".to_string(), number(to_epoch_ms(raw.get("createdAt"))));
    rest.insert("updatedAt".to_string(), number(to_epoch_ms(raw.get("updatedAt"))));
    rest.insert(
        "archived".to_string(),
        Value::Bool(raw.get("archived").and_then(Value::as_bool) == Some(true)),
    );
    rest
}

fn event(event_type: &str, payload: Value) -> Value {
    let mut event = payload.as_object().cloned().unwrap_or_default();
    event.insert("type".to_string(), Value::String(event_type.to_string()));
    event.insert(
        "time".to_string(),
        json!(chrono::Utc::now().timestamp_millis().max(0)),
    );
    Value::Object(event)
}

fn append_input(event_type: &str, data: Value) -> AppendInput {
    AppendInput {
        type_: event_type.to_string(),
        kind: "event".to_string(),
        data: Some(data),
    }
}

fn to_base36(mut value: u64) -> String {
    if value == 0 {
        return "0".to_string();
    }
    let mut output = Vec::new();
    while value > 0 {
        let digit = (value % 36) as u8;
        output.push(if digit < 10 {
            b'0' + digit
        } else {
            b'a' + digit - 10
        } as char);
        value /= 36;
    }
    output.iter().rev().collect()
}

pub async fn migrate_v2_session(dir: &str) -> Result<V2MigrationResult, V2MigrationError> {
    if !is_v2_session_dir(dir).await {
        return Err(V2MigrationError::new(
            "not-v2-session",
            format!("{dir} is not a v2 session directory"),
        ));
    }
    let state_path = Path::new(dir).join("state.json");
    let raw_meta = async {
        let text = fs::read_to_string(&state_path).await?;
        serde_json::from_str::<Value>(&text).map_err(std::io::Error::other)
    }
    .await
    .map_err(|error: std::io::Error| {
        V2MigrationError::new(
            "invalid-state-json",
            format!("cannot read or parse {}: {error}", state_path.to_string_lossy()),
        )
    })?;
    let Some(raw_meta) = raw_meta.as_object() else {
        return Err(V2MigrationError::new(
            "invalid-state-json",
            format!("{} does not contain an object", state_path.to_string_lossy()),
        ));
    };
    let fallback_id = Path::new(dir)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let meta = normalize_v2_session_meta(raw_meta, &fallback_id);
    let agent_ids = list_v2_agent_ids(dir).await;
    let random = to_base36(fastrand::u64(..));
    let temporary_name = format!(
        ".migrate-{}-{}-{}",
        to_base36(std::process::id() as u64),
        to_base36(chrono::Utc::now().timestamp_millis().max(0) as u64),
        random.chars().take(6).collect::<String>()
    );
    let tmp = Path::new(dir).join(temporary_name);
    let mut branches: Vec<Arc<Branch>> = Vec::new();
    let mut committed = false;
    let mut outcome = async {
        let backend = NodeBackend::new(&tmp);
        let store = TreeStore::open(backend.store_backend(), TreeStoreOptions::default()).await?;
        let tree = store.tree(V2_SESSION_TREE_NAME).await?;
        let log = tree.create_branch(SESSION_LOG_BRANCH, None)?;
        branches.push(log.clone());
        for agent_id in &agent_ids {
            let agent_dir = Path::new(dir).join("agents").join(agent_id);
            let records = read_v2_wire_records(
                &agent_dir.join("wire.jsonl").to_string_lossy(),
                agent_id,
            )
            .await?;
            let folded = fold_v2_wire_records(&records)?;
            let branch = tree.create_branch(agent_id, None)?;
            branches.push(branch.clone());
            let blob_dir = agent_dir.join("blobs");
            let resolve_blob: V2BlobResolver = Arc::new(move |hash| {
                let path = blob_dir.join(hash);
                async move {
                    fs::read(path)
                        .await
                        .ok()
                        .map(|data| base64::engine::general_purpose::STANDARD.encode(data))
                }
                .boxed()
            });
            for message in &folded.messages {
                let converted = convert_v2_message(
                    message,
                    folded.assistant_extra(message),
                    resolve_blob.clone(),
                )
                .await?;
                let Some(converted) = converted else {
                    continue;
                };
                branch
                    .append(append_input(
                        "message.appended",
                        event("message.appended", json!({ "message": converted })),
                    ))
                    .await?;
            }
            let last_turn_id = folded.next_turn_id - 1;
            if !folded.todos.is_empty() {
                branch
                    .append(append_input(
                        "state.updated",
                        event(
                            "state.updated",
                            json!({
                                "name": "todo",
                                "value": {
                                    "todos": folded.todos,
                                    "lastWriteTurn": last_turn_id.max(0),
                                }
                            }),
                        ),
                    ))
                    .await?;
            }
            if folded.next_turn_id > 0 {
                branch
                    .append(append_input(
                        "turn.ended",
                        event(
                            "turn.ended",
                            json!({ "turnId": last_turn_id, "outcome": "done" }),
                        ),
                    ))
                    .await?;
            }
            log.append(append_input(
                AGENT_OPENED_TYPE,
                serde_json::to_value(agent_opened(agent_id, agent_id))?,
            ))
            .await?;
        }
        log.append(append_input(
            SESSION_META_UPDATED_TYPE,
            serde_json::to_value(session_meta_updated(Value::Object(meta)))?,
        ))
        .await?;
        for branch_name in tree.branches() {
            tree.open_branch(&branch_name)?.settled().await?;
        }
        fs::rename(tmp.join("trees"), Path::new(dir).join("trees")).await?;
        committed = true;
        let blob_error = if path_is_directory(tmp.join("blobs")).await {
            merge_blobs_into(&tmp.join("blobs"), &Path::new(dir).join("blobs")).await
        } else {
            None
        };
        Ok::<Option<String>, V2MigrationError>(blob_error)
    }
    .await;
    for branch in &branches {
        let _ = branch.settled().await;
    }
    let has_blob_error = outcome.as_ref().ok().is_some_and(Option::is_some);
    if !committed || !has_blob_error {
        if let Err(error) = fs::remove_dir_all(&tmp).await {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(error.into());
            }
        }
    }
    let blob_error = outcome?;
    Ok(V2MigrationResult {
        tree_name: V2_SESSION_TREE_NAME.to_string(),
        agents: agent_ids,
        blob_error,
    })
}
