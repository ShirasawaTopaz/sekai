//! Six bounded Feishu Calendar v4 tools; no arbitrary HTTP/API execution.
//!
//! Transport owns authentication and HTTP status handling and returns the complete
//! Feishu `{code,msg,data}` envelope. This module validates before any request and
//! enforces `allow_write`; Feishu remains authoritative for calendar/event ACLs.
//! Times are Unix **seconds as strings**, never RFC3339 datetimes or milliseconds;
//! all-day dates are Gregorian `YYYY-MM-DD`, with an exclusive end date, in UTC.
//! A finite IANA timezone allowlist keeps validation local without a tzdb dependency.
//! Official contracts: <https://open.feishu.cn/document/server-docs/calendar-v4/calendar-event/create.md>,
//! `calendar-event/{list,get,patch,delete}.md`, and
//! <https://open.feishu.cn/document/uAjLw4CM/ukTMukTMukTM/reference/calendar-v4/calendar/list.md>.

use crate::{json::Json, llm::ToolDef};
use std::sync::Arc;

const BASE: &str = "/open-apis/calendar/v4/calendars";
const NAMES: [&str; 6] = [
    "feishu_calendar_list",
    "feishu_event_list",
    "feishu_event_get",
    "feishu_event_create",
    "feishu_event_update",
    "feishu_event_delete",
];
const ZONES: &[&str] = &[
    "UTC",
    "Etc/UTC",
    "Asia/Shanghai",
    "Asia/Hong_Kong",
    "Asia/Taipei",
    "Asia/Tokyo",
    "Asia/Seoul",
    "Asia/Singapore",
    "Asia/Kolkata",
    "Europe/London",
    "Europe/Paris",
    "Europe/Berlin",
    "America/New_York",
    "America/Chicago",
    "America/Denver",
    "America/Los_Angeles",
    "Australia/Sydney",
    "Pacific/Auckland",
];

/// Append to the agent prompt when these tools are available.
pub const SYSTEM_PROMPT: &str = "Feishu calendar tools use the current app identity (tenant token), with no personal OAuth. Always call feishu_calendar_list first and confirm the user's intended calendar_id from the result; writing requires role writer/owner, type primary/shared, not deleted or third-party. An app's primary calendar is NOT the user's personal primary calendar. Never guess a personal calendar or claim an app-calendar event was added to the user's personal calendar. If the intended calendar is unavailable, explain and ask for a shared writable calendar. Use Unix seconds encoded as decimal strings (NOT milliseconds or RFC3339 datetimes); timed events default to Asia/Shanghai unless the user specifies another supported timezone. All-day dates use YYYY-MM-DD, UTC, and an exclusive end date. Resolve ambiguous dates/times with the user. Creation requires an idempotency_key of 32..128 characters, stable across retries of the same logical operation and different for different events. Only report success after ok:true, name the actual calendar, and include the returned event_id and app_link when available. Range event listing may be truncated at page_size and cannot paginate; use anchor_time with page_token for exhaustive traversal. Description updates may discard existing client rich-text formatting. These tools do not add attendees or reserve meeting rooms.";

/// Injectable authenticated transport. GET/DELETE without a body receive `Json::Null`.
/// `Ok` must contain the original Feishu `{code,msg,data}` response envelope.
pub trait FeishuApi: Send + Sync {
    /// Send one bounded request; HTTP/authentication failures must return `Err`.
    fn request(&self, method: &str, path: &str, body: &Json) -> Result<Json, String>;
}

/// Calendar tools sharing one authenticated API transport.
pub struct FeishuTools {
    api: Arc<dyn FeishuApi>,
}
impl FeishuTools {
    /// Construct tools; no requests are made until execution.
    pub fn new(api: Arc<dyn FeishuApi>) -> Self {
        Self { api }
    }

    /// Whether this exact tool name is one of the six supported operations.
    pub fn handles(&self, name: &str) -> bool {
        NAMES.contains(&name)
    }

    /// Strict JSON schemas; prompts require calendar discovery before choosing a target.
    pub fn tool_defs(&self) -> Vec<ToolDef> {
        let descriptions = [
            "List current identity's calendars and role/type with pagination. Call first; tenant token app primary is not a personal primary calendar. Confirm intended writable calendar_id (writer/owner, primary/shared).",
            "List events. Unix-second start_time/end_time strings must be paired and cannot mix with tokens/anchor_time; range results may be truncated. Use anchor_time plus page_token to paginate exhaustively.",
            "Get one event, including event_id and app_link when available.",
            "Create in the confirmed writable calendar. Requires write permission and a stable idempotency_key (32..128 chars) reused only for retries of this creation. Returns event_id/app_link. Does not invite attendees.",
            "Patch an event in the confirmed writable calendar. Requires write permission and at least one changed field. Times must be supplied together; end must exceed start. Description updates may lose rich text.",
            "Delete an event in the confirmed writable calendar. Requires write permission; Feishu also requires organizer rights. need_notification defaults to the API's true.",
        ];
        NAMES.iter().enumerate().map(|(i, name)| {
            let (fields, required) = fields(i);
            let properties = Json::Object(fields.iter().map(|key| {
                let schema = if ["start_time", "end_time"].contains(key) {
                    if i == 1 { string_schema("Unix seconds string, 0..253402300799; NOT RFC3339 or milliseconds.", 1, 12) }
                    else { crate::json!({"oneOf": crate::json!([time_schema(false), time_schema(true)])}) }
                } else { field_schema(key) };
                ((*key).into(), schema)
            }).collect());
            ToolDef { name: (*name).into(), description: descriptions[i].into(), parameters: crate::json!({
                "type": "object", "additionalProperties": false, "properties": properties,
                "required": Json::array(required.iter().map(|s| Json::string(*s)).collect())
            }) }
        }).collect()
    }

    /// Validate arguments and policy before I/O; return `{ok,data}` or `{ok,error}`.
    /// Calendar ACL checks are enforced remotely, never inferred from caller fields.
    pub fn execute(&self, name: &str, arguments: &str, allow_write: bool) -> Json {
        let Some(i) = NAMES.iter().position(|n| *n == name) else {
            return error("invalid_arguments", "Unknown Feishu tool");
        };
        let parsed = Json::parse(arguments)
            .map_err(|e| e.to_string())
            .and_then(|args| prepare(i, &args));
        let (method, path, body) = match parsed {
            Ok(request) => request,
            Err(message) => return error("invalid_arguments", &message),
        };
        if i >= 3 && !allow_write {
            return error("policy_denied", "Feishu writes require allow_write");
        }
        let response = match self.api.request(method, &path, &body) {
            Ok(response) => response,
            Err(message) => return error("feishu_api_error", &message),
        };
        if response.get("code").and_then(Json::as_i64) != Some(0) {
            return error(
                "feishu_api_error",
                &format!(
                    "Feishu code={}: {}",
                    response.get("code").unwrap_or(&Json::Null),
                    response
                        .get("msg")
                        .and_then(Json::as_str)
                        .unwrap_or("invalid API response")
                ),
            );
        }
        let Some(data) = response.get("data").filter(|v| v.as_object().is_some()) else {
            return error("feishu_api_error", "Missing API data object");
        };
        if i <= 1
            && data
                .get(if i == 0 { "calendar_list" } else { "items" })
                .and_then(Json::as_array)
                .is_none()
        {
            return error("feishu_api_error", "Missing API calendar/event list");
        }
        if (2..=4).contains(&i)
            && data
                .get_path(&["event", "event_id"])
                .and_then(Json::as_str)
                .is_none_or(|s| s.trim().is_empty())
        {
            return error(
                "feishu_api_error",
                "Missing event_id; success cannot be confirmed",
            );
        }
        // Preserve event.app_link and pagination exactly; never fabricate event links.
        crate::json!({ "ok": true, "data": data.clone() })
    }
}

fn fields(i: usize) -> (&'static [&'static str], &'static [&'static str]) {
    match i {
        0 => (&["page_size", "page_token", "sync_token"], &[]),
        1 => (
            &[
                "calendar_id",
                "page_size",
                "page_token",
                "sync_token",
                "anchor_time",
                "start_time",
                "end_time",
            ],
            &["calendar_id"],
        ),
        2 => (&["calendar_id", "event_id"], &["calendar_id", "event_id"]),
        3 => (
            &[
                "calendar_id",
                "summary",
                "start_time",
                "end_time",
                "description",
                "need_notification",
                "idempotency_key",
            ],
            &[
                "calendar_id",
                "summary",
                "start_time",
                "end_time",
                "idempotency_key",
            ],
        ),
        4 => (
            &[
                "calendar_id",
                "event_id",
                "summary",
                "start_time",
                "end_time",
                "description",
                "need_notification",
            ],
            &["calendar_id", "event_id"],
        ),
        _ => (
            &["calendar_id", "event_id", "need_notification"],
            &["calendar_id", "event_id"],
        ),
    }
}
fn string_schema(description: &str, min: i64, max: i64) -> Json {
    crate::json!({"type": "string", "description": description, "minLength": min, "maxLength": max})
}
fn field_schema(key: &str) -> Json {
    match key {
        "page_size" => crate::json!({"type": "integer", "minimum": 50, "maximum": 1000}),
        "need_notification" => crate::json!({"type": "boolean"}),
        "anchor_time" => string_schema(
            "Unix seconds string; enables event pagination; cannot mix with range.",
            1,
            12,
        ),
        "summary" => string_schema("Nonblank event title.", 1, 1000),
        "description" => string_schema(
            "Event description; updating can discard existing rich text.",
            0,
            40960,
        ),
        "idempotency_key" => string_schema(
            "Stable unique key for this creation; reuse on retries, 32..128 characters.",
            32,
            128,
        ),
        "calendar_id" | "event_id" => string_schema(
            "Exact opaque ID from calendar/event list; never guess a personal calendar.",
            1,
            512,
        ),
        _ => string_schema(
            "Opaque pagination/sync token returned by Feishu; omit on first request.",
            1,
            8192,
        ),
    }
}
fn time_schema(date: bool) -> Json {
    let key = if date { "date" } else { "timestamp" };
    let value = if date {
        string_schema(
            "All-day Gregorian YYYY-MM-DD (1970..9999); end date is exclusive.",
            10,
            10,
        )
    } else {
        string_schema(
            "Unix seconds string, 0..253402300799. NOT RFC3339, milliseconds or a JSON number.",
            1,
            12,
        )
    };
    let zones = if date { &["UTC", "Etc/UTC"][..] } else { ZONES };
    crate::json!({"type": "object", "additionalProperties": false,
        "properties": Json::object(vec![(key, value), ("timezone", crate::json!({"type": "string",
            "description": "Supported IANA zone; timed default Asia/Shanghai, all-day default UTC.",
            "enum": Json::array(zones.iter().map(|z| Json::string(*z)).collect())}))]),
        "required": crate::json!([key])})
}
fn error(code: &str, message: &str) -> Json {
    crate::json!({"ok": false, "error": crate::json!({"code": code, "message": message})})
}
fn object<'a>(value: &'a Json, allowed: &[&str]) -> Result<&'a [(String, Json)], String> {
    let entries = value.as_object().ok_or("Expected JSON object")?;
    for (n, (key, _)) in entries.iter().enumerate() {
        if !allowed.contains(&key.as_str()) {
            return Err(format!("Unknown field: {key}"));
        }
        if entries[..n].iter().any(|(k, _)| k == key) {
            return Err(format!("Duplicate field: {key}"));
        }
    }
    Ok(entries)
}
fn text<'a>(args: &'a Json, key: &str, min: usize, max: usize) -> Result<&'a str, String> {
    let s = args
        .get(key)
        .and_then(Json::as_str)
        .ok_or_else(|| format!("{key} must be a string"))?;
    if !(min..=max).contains(&s.chars().count()) || (min > 0 && s.trim().is_empty()) {
        return Err(format!(
            "{key} length must be {min}..{max}, nonblank when required"
        ));
    }
    Ok(s)
}
fn seconds(value: &str) -> Result<i64, String> {
    if value.is_empty()
        || !value.bytes().all(|b| b.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return Err("Expected canonical nonnegative Unix seconds string".into());
    }
    value
        .parse::<i64>()
        .ok()
        .filter(|v| *v <= 253_402_300_799)
        .ok_or_else(|| {
            "Unix seconds out of range (0..253402300799); milliseconds are not accepted".into()
        })
}
fn date_value(s: &str) -> Result<i64, String> {
    let b = s.as_bytes();
    if b.len() != 10
        || b[4] != b'-'
        || b[7] != b'-'
        || !b
            .iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
    {
        return Err("Expected YYYY-MM-DD".into());
    }
    let y: i64 = s[..4].parse().unwrap();
    let m: usize = s[5..7].parse().unwrap();
    let d: i64 = s[8..].parse().unwrap();
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if !(1970..=9999).contains(&y) || !(1..=12).contains(&m) || d < 1 || d > days[m - 1] {
        return Err("Invalid Gregorian date (supported years 1970..9999)".into());
    }
    Ok(y * 10000 + m as i64 * 100 + d)
}
fn time(value: &Json) -> Result<(Json, i64, bool), String> {
    object(value, &["timestamp", "date", "timezone"])?;
    let date = value.get("date").is_some();
    if date == value.get("timestamp").is_some() {
        return Err("Time requires exactly one of date/timestamp".into());
    }
    let key = if date { "date" } else { "timestamp" };
    let s = text(value, key, 1, 12)?;
    let ordinal = if date { date_value(s)? } else { seconds(s)? };
    let zone = if value.get("timezone").is_some() {
        text(value, "timezone", 1, 64)?
    } else if date {
        "UTC"
    } else {
        "Asia/Shanghai"
    };
    if !ZONES.contains(&zone) || (date && !["UTC", "Etc/UTC"].contains(&zone)) {
        return Err(
            "Unsupported IANA timezone; all-day events require UTC (see schema enum)".into(),
        );
    }
    Ok((
        Json::object(vec![
            (key, Json::string(s)),
            ("timezone", Json::string(zone)),
        ]),
        ordinal,
        date,
    ))
}
fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}
fn id(args: &Json, key: &str) -> Result<String, String> {
    let s = text(args, key, 1, 512)?;
    if s == "." || s == ".." || s.trim() != s || s.chars().any(char::is_control) {
        return Err(format!("Invalid {key}"));
    }
    Ok(encode(s))
}
fn query(path: &mut String, key: &str, value: &str) {
    path.push(if path.contains('?') { '&' } else { '?' });
    path.push_str(key);
    path.push('=');
    path.push_str(&encode(value));
}
fn prepare(i: usize, args: &Json) -> Result<(&'static str, String, Json), String> {
    let (allowed, required) = fields(i);
    object(args, allowed)?;
    for key in required {
        if args.get(key).is_none() {
            return Err(format!("Missing {key}"));
        }
    }
    let mut path = BASE.to_string();
    if i > 0 {
        path.push_str(&format!("/{}/events", id(args, "calendar_id")?));
    }
    if i == 2 || i >= 4 {
        path.push_str(&format!("/{}", id(args, "event_id")?));
    }
    let mut body = Json::Null;
    if i <= 1 {
        if let Some(v) = args.get("page_size") {
            let n = v
                .as_u64()
                .filter(|n| (50..=1000).contains(n))
                .ok_or("page_size must be integer 50..1000")?;
            query(&mut path, "page_size", &n.to_string());
        }
        if args.get("page_token").is_some() && args.get("sync_token").is_some() {
            return Err("page_token and sync_token are mutually exclusive".into());
        }
        for key in ["page_token", "sync_token"] {
            if args.get(key).is_some() {
                let token = text(args, key, 1, 8192)?;
                if token.chars().any(char::is_control) {
                    return Err(format!("Invalid {key}"));
                }
                query(&mut path, key, token);
            }
        }
        if i == 1 {
            let start = args.get("start_time");
            let end = args.get("end_time");
            if start.is_some() != end.is_some() {
                return Err("start_time/end_time must be paired".into());
            }
            if start.is_some() {
                if ["page_token", "sync_token", "anchor_time"]
                    .iter()
                    .any(|k| args.get(k).is_some())
                {
                    return Err(
                        "Time range cannot mix with pagination, sync_token or anchor_time".into(),
                    );
                }
                let a = text(args, "start_time", 1, 12)?;
                let b = text(args, "end_time", 1, 12)?;
                if seconds(b)? <= seconds(a)? {
                    return Err("end_time must exceed start_time".into());
                }
                query(&mut path, "start_time", a);
                query(&mut path, "end_time", b);
            }
            if args.get("anchor_time").is_some() {
                let anchor = text(args, "anchor_time", 1, 12)?;
                seconds(anchor)?;
                query(&mut path, "anchor_time", anchor);
            }
        }
    } else if i == 3 || i == 4 {
        body = Json::object(vec![]);
        for (key, min, max) in [("summary", 1, 1000), ("description", 0, 40960)] {
            if args.get(key).is_some() {
                body.set(key, Json::string(text(args, key, min, max)?));
            }
        }
        let start = args.get("start_time");
        let end = args.get("end_time");
        if start.is_some() != end.is_some() {
            return Err("start_time/end_time must be paired".into());
        }
        if let (Some(start), Some(end)) = (start, end) {
            let (a, av, ad) = time(start)?;
            let (b, bv, bd) = time(end)?;
            if ad != bd || bv <= av {
                return Err("Times must use the same kind and end must exceed start".into());
            }
            body.set("start_time", a);
            body.set("end_time", b);
        }
        if i == 3 {
            let key = text(args, "idempotency_key", 32, 128)?;
            if key.chars().any(char::is_control) {
                return Err("Invalid idempotency_key".into());
            }
            query(&mut path, "idempotency_key", key);
        }
        if i == 4 && body.as_object().unwrap().is_empty() && args.get("need_notification").is_none()
        {
            return Err("Update requires at least one changed field".into());
        }
    }
    if let Some(value) = args.get("need_notification") {
        let flag = value.as_bool().ok_or("need_notification must be boolean")?;
        if i == 5 {
            query(
                &mut path,
                "need_notification",
                if flag { "true" } else { "false" },
            );
        } else {
            body.set("need_notification", Json::Bool(flag));
        }
    }
    Ok((
        match i {
            3 => "POST",
            4 => "PATCH",
            5 => "DELETE",
            _ => "GET",
        },
        path,
        body,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    struct Fake {
        calls: Mutex<Vec<(String, String, Json)>>,
        reply: Result<Json, String>,
    }
    impl FeishuApi for Fake {
        fn request(&self, method: &str, path: &str, body: &Json) -> Result<Json, String> {
            self.calls
                .lock()
                .unwrap()
                .push((method.into(), path.into(), body.clone()));
            self.reply.clone()
        }
    }
    fn fixture(reply: Result<Json, String>) -> (FeishuTools, Arc<Fake>) {
        let api = Arc::new(Fake {
            calls: Mutex::new(vec![]),
            reply,
        });
        (FeishuTools::new(api.clone()), api)
    }
    fn success() -> Json {
        Json::parse(r#"{"code":0,"data":{"calendar_list":[],"items":[],"event":{"event_id":"ev_0","app_link":"https://example/event"}}}"#).unwrap()
    }
    fn create() -> Json {
        Json::parse(r#"{"calendar_id":"cal@host","summary":"会议","start_time":{"timestamp":"1790000000"},"end_time":{"timestamp":"1790003600"},"idempotency_key":"25fdf41b-8c80-2ce1-e94c-de8b5e7aa7e6"}"#).unwrap()
    }
    fn assert_error(value: &Json, code: &str) {
        assert_eq!(value.get("ok"), Some(&Json::Bool(false)));
        assert_eq!(
            value.get_path(&["error", "code"]).and_then(Json::as_str),
            Some(code)
        );
    }
    #[test]
    fn schemas_are_bounded_and_advertise_discovery() {
        let (tools, _) = fixture(Ok(success()));
        let defs = tools.tool_defs();
        assert_eq!(defs.len(), 6);
        for d in defs {
            assert!(tools.handles(&d.name));
            assert_eq!(
                d.parameters.get("additionalProperties"),
                Some(&Json::Bool(false))
            );
        }
        assert!(!tools.handles("feishu_api"));
        assert!(SYSTEM_PROMPT.contains("NOT the user's personal"));
    }
    #[test]
    fn all_six_http_mappings_and_event_identity() {
        let (tools, fake) = fixture(Ok(success()));
        let inputs = ["{}".into(), r#"{"calendar_id":"cal@host","page_size":50,"anchor_time":"0","page_token":"a+/=?&中"}"#.into(),
            r#"{"calendar_id":"cal@host","event_id":"ev/0?#%"}"#.into(), create().to_compact_string(),
            r#"{"calendar_id":"cal@host","event_id":"ev_0","summary":"新标题"}"#.into(),
            r#"{"calendar_id":"cal@host","event_id":"ev_0","need_notification":false}"#.into()];
        for (i, input) in inputs.iter().enumerate() {
            let result = tools.execute(NAMES[i], input, true);
            assert_eq!(result.get("ok"), Some(&Json::Bool(true)), "{result}");
            assert_eq!(
                result
                    .get_path(&["data", "event", "event_id"])
                    .and_then(Json::as_str),
                Some("ev_0")
            );
            assert_eq!(
                result
                    .get_path(&["data", "event", "app_link"])
                    .and_then(Json::as_str),
                Some("https://example/event")
            );
        }
        let calls = fake.calls.lock().unwrap();
        assert_eq!(
            calls.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(),
            ["GET", "GET", "GET", "POST", "PATCH", "DELETE"]
        );
        assert_eq!(calls[0], ("GET".into(), BASE.into(), Json::Null));
        assert_eq!(
            calls[1].1,
            format!(
                "{BASE}/cal%40host/events?page_size=50&page_token=a%2B%2F%3D%3F%26%E4%B8%AD&anchor_time=0"
            )
        );
        assert_eq!(
            calls[2].1,
            format!("{BASE}/cal%40host/events/ev%2F0%3F%23%25")
        );
        assert_eq!(
            calls[3].1,
            format!(
                "{BASE}/cal%40host/events?idempotency_key=25fdf41b-8c80-2ce1-e94c-de8b5e7aa7e6"
            )
        );
        let mut expected = create();
        expected.remove("calendar_id");
        expected.remove("idempotency_key");
        expected.set(
            "start_time",
            crate::json!({"timestamp":"1790000000", "timezone":"Asia/Shanghai"}),
        );
        expected.set(
            "end_time",
            crate::json!({"timestamp":"1790003600", "timezone":"Asia/Shanghai"}),
        );
        assert_eq!(calls[3].2, expected);
        assert_eq!(calls[4].1, format!("{BASE}/cal%40host/events/ev_0"));
        assert_eq!(calls[4].2, crate::json!({"summary":"新标题"}));
        assert_eq!(
            calls[5].1,
            format!("{BASE}/cal%40host/events/ev_0?need_notification=false")
        );
        assert_eq!(calls[5].2, Json::Null);
    }
    #[test]
    fn denied_writes_never_reach_transport() {
        let (tools, fake) = fixture(Ok(success()));
        for (name, args) in [
            (NAMES[3], create().to_compact_string()),
            (
                NAMES[4],
                r#"{"calendar_id":"c","event_id":"e","summary":"s"}"#.into(),
            ),
            (NAMES[5], r#"{"calendar_id":"c","event_id":"e"}"#.into()),
        ] {
            assert_error(&tools.execute(name, &args, false), "policy_denied");
        }
        assert!(fake.calls.lock().unwrap().is_empty());
        assert_eq!(
            tools.execute(NAMES[0], "{}", false).get("ok"),
            Some(&Json::Bool(true))
        );
    }
    #[test]
    fn malformed_arguments_never_reach_transport() {
        let (tools, fake) = fixture(Ok(success()));
        for input in [
            "",
            "[]",
            "null",
            "{} trailing",
            r#"{"unknown":1}"#,
            r#"{"page_size":50,"page_size":60}"#,
            r#"{"page_size":49}"#,
            r#"{"page_size":1001}"#,
            r#"{"page_size":50.5}"#,
            r#"{"page_size":"50"}"#,
            r#"{"page_token":null}"#,
            r#"{"page_token":"a","sync_token":"b"}"#,
        ] {
            assert_error(&tools.execute(NAMES[0], input, true), "invalid_arguments");
        }
        for (key, value) in [
            ("calendar_id", Json::string("..")),
            ("summary", Json::string("  ")),
            ("idempotency_key", Json::string("short")),
            ("need_notification", Json::string("false")),
            ("start_time", crate::json!({"timestamp":"1790000000000"})),
            (
                "start_time",
                crate::json!({"timestamp":"1790000000","timezone":"Mars/Olympus"}),
            ),
            (
                "start_time",
                crate::json!({"timestamp":"1790000000","date":"2026-01-01"}),
            ),
            (
                "start_time",
                crate::json!({"timestamp":"1790000000","extra":true}),
            ),
            ("start_time", crate::json!({"timestamp":1790000000})),
            ("end_time", crate::json!({"timestamp":"1789999999"})),
        ] {
            let mut args = create();
            args.set(key, value);
            assert_error(
                &tools.execute(NAMES[3], &args.to_compact_string(), true),
                "invalid_arguments",
            );
        }
        for input in [
            r#"{"calendar_id":"c","event_id":"e"}"#,
            r#"{"calendar_id":"c","event_id":"e","start_time":{"timestamp":"1"}}"#,
        ] {
            assert_error(&tools.execute(NAMES[4], input, true), "invalid_arguments");
        }
        assert_error(
            &tools.execute("feishu_arbitrary", "{}", true),
            "invalid_arguments",
        );
        assert!(fake.calls.lock().unwrap().is_empty());
    }
    #[test]
    fn dates_seconds_timezones_and_ranges_are_strict() {
        for s in [
            "2023-02-29",
            "2100-02-29",
            "2024-04-31",
            "2024-00-01",
            "2024-01-00",
            "1969-01-01",
            "2024-1-01",
            "２０２４-01-01",
        ] {
            assert!(date_value(s).is_err(), "{s}");
        }
        for s in ["2024-02-29", "2000-02-29", "9999-12-31"] {
            assert!(date_value(s).is_ok());
        }
        for s in [
            "-1",
            "01",
            "1.0",
            "1e9",
            "2026-01-01T00:00:00Z",
            "253402300800",
            "1790000000000",
        ] {
            assert!(seconds(s).is_err(), "{s}");
        }
        assert_eq!(seconds("253402300799"), Ok(253402300799));
        let (tools, fake) = fixture(Ok(success()));
        let mut args = create();
        args.set("start_time", crate::json!({"date":"2024-02-29"}));
        args.set("end_time", crate::json!({"date":"2024-03-01"}));
        assert_eq!(
            tools
                .execute(NAMES[3], &args.to_compact_string(), true)
                .get("ok"),
            Some(&Json::Bool(true))
        );
        assert_eq!(
            fake.calls.lock().unwrap()[0]
                .2
                .get_path(&["start_time", "timezone"])
                .and_then(Json::as_str),
            Some("UTC")
        );
        for time in [
            crate::json!({"date":"2024-03-01"}),
            crate::json!({"timestamp":"1790003600"}),
            crate::json!({"date":"2024-02-29","timezone":"Asia/Shanghai"}),
        ] {
            args.set("start_time", time);
            assert_error(
                &tools.execute(NAMES[3], &args.to_compact_string(), true),
                "invalid_arguments",
            );
        }
        for input in [
            r#"{"calendar_id":"c","start_time":"1"}"#,
            r#"{"calendar_id":"c","start_time":"2","end_time":"1"}"#,
            r#"{"calendar_id":"c","start_time":"1","end_time":"2","page_token":"p"}"#,
            r#"{"calendar_id":"c","start_time":"1","end_time":"2","anchor_time":"0"}"#,
        ] {
            assert_error(&tools.execute(NAMES[1], input, true), "invalid_arguments");
        }
        assert_eq!(fake.calls.lock().unwrap().len(), 1);
    }
    #[test]
    fn query_modes_boundaries_and_update_body() {
        let (tools, fake) = fixture(Ok(success()));
        for (name, input) in [
            (NAMES[0], r#"{"page_size":1000,"sync_token":"sync+/=&"}"#),
            (
                NAMES[1],
                r#"{"calendar_id":"c","start_time":"0","end_time":"1"}"#,
            ),
            (
                NAMES[4],
                r#"{"calendar_id":"c","event_id":"e","description":"","need_notification":false,"start_time":{"timestamp":"0","timezone":"UTC"},"end_time":{"timestamp":"1","timezone":"UTC"}}"#,
            ),
        ] {
            assert_eq!(
                tools.execute(name, input, true).get("ok"),
                Some(&Json::Bool(true))
            );
        }
        let calls = fake.calls.lock().unwrap();
        assert_eq!(
            calls[0].1,
            format!("{BASE}?page_size=1000&sync_token=sync%2B%2F%3D%26")
        );
        assert_eq!(
            calls[1].1,
            format!("{BASE}/c/events?start_time=0&end_time=1")
        );
        assert_eq!(calls[1].2, Json::Null);
        assert_eq!(
            calls[2].2.get("description").and_then(Json::as_str),
            Some("")
        );
        assert_eq!(
            calls[2].2.get("need_notification"),
            Some(&Json::Bool(false))
        );
        assert_eq!(
            calls[2]
                .2
                .get_path(&["end_time", "timestamp"])
                .and_then(Json::as_str),
            Some("1")
        );
        drop(calls);
        for key in ["x".repeat(31), "x".repeat(129)] {
            let mut args = create();
            args.set("idempotency_key", Json::string(key));
            assert_error(
                &tools.execute(NAMES[3], &args.to_compact_string(), true),
                "invalid_arguments",
            );
        }
        let mut args = create();
        args.set(
            "idempotency_key",
            Json::string(format!("{}+/=&", "x".repeat(28))),
        );
        assert_eq!(
            tools
                .execute(NAMES[3], &args.to_compact_string(), true)
                .get("ok"),
            Some(&Json::Bool(true))
        );
        assert!(fake.calls.lock().unwrap()[3].1.ends_with("%2B%2F%3D%26"));
    }
    #[test]
    fn transport_api_and_malformed_responses_never_claim_success() {
        for reply in [
            Err("HTTP 403".into()),
            Ok(crate::json!({"code":193002,"msg":"no permission"})),
            Ok(Json::Null),
            Ok(crate::json!({"code":0})),
            Ok(crate::json!({"code":"0","data":crate::json!({})})),
            Ok(crate::json!({"code":0,"data":crate::json!({})})),
        ] {
            let (tools, fake) = fixture(reply);
            assert_error(
                &tools.execute(NAMES[3], &create().to_compact_string(), true),
                "feishu_api_error",
            );
            assert_eq!(fake.calls.lock().unwrap().len(), 1);
        }
    }
}
