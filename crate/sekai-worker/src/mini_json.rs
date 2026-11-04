//! 零依赖 JSON 解析与序列化（有意复制的子集实现）。
//!
//! 来源：**复制改写**自 `crate/sekai-router/src/json.rs`。sekai-worker 同样
//! 奉行零第三方依赖策略，而 LLM prompt 通道（`llm_channel`）需要与 Router
//! ingress / DSH 插件交换 JSONL，因此这里有意复制一份实现，而不是让
//! sekai-worker 依赖 sekai-router（Router 与 Worker 是对等进程， crate 间
//! 依赖会造成不必要的耦合）。复制时保持算法与测试一致，仅调整模块路径
//! （`$crate::mini_json`）与宏名（`mini_json!`）。JSONL 的线上契约由协议
//! 文档冻结，双方各自维护实现，本副本不自动跟随 Router 侧演进。
//!
//! - 解析严格遵循 JSON 语法（拒绝尾随内容、非法转义、控制字符）；
//! - 对象保持**插入顺序**（LLM 协议不要求键顺序，但确定性输出便于测试）；
//! - 数字用 f64 表示；受 JSON 双精度限制，cmd_id 等值在 2^53 内安全；
//! - 提供 mini_json! 宏构造嵌套结构，减少样板代码。
//!
//! 该模块只处理 UTF-8 JSON 文本，不做 I/O。

use std::fmt;

/// JSON 值。
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    /// null。
    Null,
    /// 布尔值。
    Bool(bool),
    /// 数字；JSON 不区分整数/浮点，本实现统一用 f64 保存。
    Number(f64),
    /// 字符串。
    String(String),
    /// 数组。
    Array(Vec<Json>),
    /// 对象；保持插入顺序。
    Object(Vec<(String, Json)>),
}

/// JSON 解析错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonError {
    /// 可读错误信息。
    pub message: String,
    /// 出错字节偏移（从 0 开始）。
    pub offset: usize,
}

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JSON 解析失败（偏移 {}）：{}", self.offset, self.message)
    }
}

impl std::error::Error for JsonError {}

impl Json {
    /// 构造对象；参数为 (键, 值) 序列。
    pub fn object(entries: Vec<(&str, Json)>) -> Self {
        Self::Object(
            entries
                .into_iter()
                .map(|(k, v)| (String::from(k), v))
                .collect(),
        )
    }

    /// 构造数组。
    pub fn array(items: Vec<Json>) -> Self {
        Self::Array(items)
    }

    /// 构造字符串。
    pub fn string(value: impl Into<String>) -> Self {
        Self::String(value.into())
    }

    /// 构造数字（整数）。
    pub fn num(value: i64) -> Self {
        Self::Number(value as f64)
    }

    /// 构造数字（浮点）。
    pub fn float(value: f64) -> Self {
        Self::Number(value)
    }

    /// 是否为 null。
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// 按对象键取值；非对象或键不存在返回 None。
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Self::Object(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// 按路径逐层取值；对象用键，数组可用十进制下标。
    ///
    /// 例如 get_path(&["choices", "0", "message", "content"])。
    pub fn get_path(&self, path: &[&str]) -> Option<&Json> {
        let mut current = self;
        for key in path {
            current = match current {
                Self::Array(items) => {
                    let index: usize = key.parse().ok()?;
                    items.get(index)?
                }
                _ => current.get(key)?,
            };
        }
        Some(current)
    }

    /// 字符串值。
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    /// 布尔值。
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }

    /// 数字值。
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Number(value) => Some(*value),
            _ => None,
        }
    }

    /// 非负整数值；小数或越界返回 None。
    pub fn as_u64(&self) -> Option<u64> {
        let value = self.as_f64()?;
        if value.is_finite()
            && value >= 0.0
            && value.fract() == 0.0
            && value <= 9_007_199_254_740_992.0
        {
            Some(value as u64)
        } else {
            None
        }
    }

    /// 整数值（可为负）；小数或越界返回 None。
    pub fn as_i64(&self) -> Option<i64> {
        let value = self.as_f64()?;
        if value.is_finite()
            && value.fract() == 0.0
            && (-9_007_199_254_740_992.0..=9_007_199_254_740_992.0).contains(&value)
        {
            Some(value as i64)
        } else {
            None
        }
    }

    /// 数组值。
    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Self::Array(items) => Some(items),
            _ => None,
        }
    }

    /// 对象值（保持插入顺序）。
    pub fn as_object(&self) -> Option<&[(String, Json)]> {
        match self {
            Self::Object(entries) => Some(entries),
            _ => None,
        }
    }

    /// 对象可变引用；非对象返回 None。
    pub fn as_object_mut(&mut self) -> Option<&mut Vec<(String, Json)>> {
        match self {
            Self::Object(entries) => Some(entries),
            _ => None,
        }
    }

    /// 在对象中设置字段；非对象时先转换为空对象。
    pub fn set(&mut self, key: impl Into<String>, value: Json) -> &mut Self {
        if !matches!(self, Self::Object(_)) {
            *self = Self::Object(Vec::new());
        }
        let entries = match self {
            Self::Object(entries) => entries,
            _ => unreachable!(),
        };
        let key = key.into();
        if let Some(slot) = entries.iter_mut().find(|(existing, _)| *existing == key) {
            slot.1 = value;
        } else {
            entries.push((key, value));
        }
        self
    }

    /// 对象中删除字段，返回被删除的值。
    pub fn remove(&mut self, key: &str) -> Option<Json> {
        let entries = self.as_object_mut()?;
        let index = entries.iter().position(|(existing, _)| existing == key)?;
        Some(entries.remove(index).1)
    }

    /// 序列化为紧凑 JSON 文本。
    pub fn to_compact_string(&self) -> String {
        let mut out = String::new();
        write_compact(self, &mut out);
        out
    }

    /// 序列化为带两空格缩进的 JSON 文本。
    pub fn to_pretty_string(&self) -> String {
        let mut out = String::new();
        write_pretty(self, 0, &mut out);
        out
    }

    /// 解析 JSON 文本。
    pub fn parse(text: &str) -> Result<Json, JsonError> {
        let mut parser = Parser {
            bytes: text.as_bytes(),
            pos: 0,
            depth: 0,
        };
        parser.skip_whitespace();
        let value = parser.parse_value()?;
        parser.skip_whitespace();
        if parser.pos != parser.bytes.len() {
            return Err(parser.error("JSON 值之后存在多余内容"));
        }
        Ok(value)
    }
}

impl fmt::Display for Json {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_compact_string())
    }
}

impl From<&str> for Json {
    fn from(value: &str) -> Self {
        Self::String(String::from(value))
    }
}

impl From<String> for Json {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<bool> for Json {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<i32> for Json {
    fn from(value: i32) -> Self {
        Self::Number(f64::from(value))
    }
}

impl From<u32> for Json {
    fn from(value: u32) -> Self {
        Self::Number(f64::from(value))
    }
}

impl From<i64> for Json {
    fn from(value: i64) -> Self {
        Self::Number(value as f64)
    }
}

impl From<u64> for Json {
    fn from(value: u64) -> Self {
        Self::Number(value as f64)
    }
}

impl From<usize> for Json {
    fn from(value: usize) -> Self {
        Self::Number(value as f64)
    }
}

impl From<f64> for Json {
    fn from(value: f64) -> Self {
        Self::Number(value)
    }
}

impl From<Vec<Json>> for Json {
    fn from(value: Vec<Json>) -> Self {
        Self::Array(value)
    }
}

/// 构造 JSON 值。
///
/// 示例（伪代码）：
/// mini_json!({ "ok": true, "count": 2i64, "workers": ["a", "b"] })
///
/// 注意：嵌套对象/数组字面量请再包一层 mini_json!()，例如
/// mini_json!({ "items": mini_json!([ "a", 3i64 ]) })；否则会被当成普通 Rust 表达式。
/// 嵌套的 mini_json!() 调用本身可以作为值直接使用（Json: From<Json>）。
#[macro_export]
macro_rules! mini_json {
    (null) => {
        $crate::mini_json::Json::Null
    };
    (true) => {
        $crate::mini_json::Json::Bool(true)
    };
    (false) => {
        $crate::mini_json::Json::Bool(false)
    };
    ([ $($item:expr),* $(,)? ]) => {
        $crate::mini_json::Json::Array(vec![ $( $crate::mini_json::Json::from($item) ),* ])
    };
    ({ $($key:literal : $value:expr),* $(,)? }) => {
        $crate::mini_json::Json::Object(vec![
            $( (String::from($key), $crate::mini_json::Json::from($value)) ),*
        ])
    };
    ($value:expr) => {
        $crate::mini_json::Json::from($value)
    };
}

// ---------------------------------------------------------------------------
// 解析
// ---------------------------------------------------------------------------

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
    depth: usize,
}

const MAX_DEPTH: usize = 128;

impl<'a> Parser<'a> {
    fn error(&self, message: impl Into<String>) -> JsonError {
        JsonError {
            message: message.into(),
            offset: self.pos,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.pos += 1;
        Some(byte)
    }

    fn skip_whitespace(&mut self) {
        while let Some(byte) = self.peek() {
            if matches!(byte, b' ' | b'\t' | b'\n' | b'\r') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn expect_literal(&mut self, literal: &[u8], name: &str) -> Result<(), JsonError> {
        if self.bytes[self.pos..].starts_with(literal) {
            self.pos += literal.len();
            Ok(())
        } else {
            Err(self.error(format!("期望 {name}")))
        }
    }

    fn parse_value(&mut self) -> Result<Json, JsonError> {
        if self.depth >= MAX_DEPTH {
            return Err(self.error("JSON 嵌套层级过深"));
        }
        match self.peek() {
            Some(b'n') => {
                self.bump();
                self.expect_literal(b"ull", "null")?;
                Ok(Json::Null)
            }
            Some(b't') => {
                self.bump();
                self.expect_literal(b"rue", "true")?;
                Ok(Json::Bool(true))
            }
            Some(b'f') => {
                self.bump();
                self.expect_literal(b"alse", "false")?;
                Ok(Json::Bool(false))
            }
            Some(b'"') => {
                self.bump();
                Ok(Json::String(self.parse_string()?))
            }
            Some(b'[') => {
                self.bump();
                self.depth += 1;
                let mut items = Vec::new();
                self.skip_whitespace();
                if self.peek() == Some(b']') {
                    self.bump();
                    self.depth -= 1;
                    return Ok(Json::Array(items));
                }
                loop {
                    self.skip_whitespace();
                    items.push(self.parse_value()?);
                    self.skip_whitespace();
                    match self.bump() {
                        Some(b',') => continue,
                        Some(b']') => break,
                        _ => return Err(self.error("数组缺少 ',' 或 ']'")),
                    }
                }
                self.depth -= 1;
                Ok(Json::Array(items))
            }
            Some(b'{') => {
                self.bump();
                self.depth += 1;
                let mut entries: Vec<(String, Json)> = Vec::new();
                self.skip_whitespace();
                if self.peek() == Some(b'}') {
                    self.bump();
                    self.depth -= 1;
                    return Ok(Json::Object(entries));
                }
                loop {
                    self.skip_whitespace();
                    if self.peek() != Some(b'"') {
                        return Err(self.error("对象键必须是字符串"));
                    }
                    self.bump();
                    let key = self.parse_string()?;
                    self.skip_whitespace();
                    if self.bump() != Some(b':') {
                        return Err(self.error("对象键之后缺少 ':'"));
                    }
                    self.skip_whitespace();
                    let value = self.parse_value()?;
                    entries.push((key, value));
                    self.skip_whitespace();
                    match self.bump() {
                        Some(b',') => continue,
                        Some(b'}') => break,
                        _ => return Err(self.error("对象缺少 ',' 或 '}'")),
                    }
                }
                self.depth -= 1;
                Ok(Json::Object(entries))
            }
            Some(byte) if byte == b'-' || byte.is_ascii_digit() => self.parse_number(),
            Some(byte) => Err(self.error(format!("非法 JSON 值起始字符 0x{byte:02X}"))),
            None => Err(self.error("JSON 值意外结束")),
        }
    }

    fn parse_string(&mut self) -> Result<String, JsonError> {
        let mut out = String::new();
        loop {
            let byte = self.bump().ok_or_else(|| self.error("字符串未闭合"))?;
            match byte {
                b'"' => return Ok(out),
                b'\\' => self.parse_escape(&mut out)?,
                byte if byte < 0x20 => {
                    return Err(self.error("字符串中包含未转义的控制字符"));
                }
                byte if byte < 0x80 => out.push(byte as char),
                first => {
                    // 非 ASCII：从当前位置回退，按 UTF-8 读取一个完整字符。
                    self.pos -= 1;
                    let rest = &self.bytes[self.pos..];
                    let text = std::str::from_utf8(rest)
                        .map_err(|_| self.error("字符串不是合法 UTF-8"))?;
                    let ch = text
                        .chars()
                        .next()
                        .ok_or_else(|| self.error("字符串未闭合"))?;
                    out.push(ch);
                    self.pos += ch.len_utf8();
                    let _ = first;
                }
            }
        }
    }

    fn parse_escape(&mut self, out: &mut String) -> Result<(), JsonError> {
        let escaped = self.bump().ok_or_else(|| self.error("转义序列未完成"))?;
        match escaped {
            b'"' => out.push('"'),
            b'\\' => out.push('\\'),
            b'/' => out.push('/'),
            b'b' => out.push('\u{0008}'),
            b'f' => out.push('\u{000C}'),
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            b'u' => {
                let first = self.parse_hex_u16()?;
                let code = if (0xD800..=0xDBFF).contains(&first) {
                    if self.bump() != Some(b'\\') || self.bump() != Some(b'u') {
                        return Err(self.error("高位代理项之后缺少低位代理"));
                    }
                    let second = self.parse_hex_u16()?;
                    if !(0xDC00..=0xDFFF).contains(&second) {
                        return Err(self.error("非法 UTF-16 低位代理"));
                    }
                    0x10000 + (((first as u32 - 0xD800) << 10) | (second as u32 - 0xDC00))
                } else if (0xDC00..=0xDFFF).contains(&first) {
                    return Err(self.error("孤立的 UTF-16 低位代理"));
                } else {
                    first as u32
                };
                let ch = char::from_u32(code).ok_or_else(|| self.error("非法 Unicode 码点"))?;
                out.push(ch);
            }
            other => {
                return Err(self.error(format!("非法转义字符 '\\{}'", other as char)));
            }
        }
        Ok(())
    }

    fn parse_hex_u16(&mut self) -> Result<u16, JsonError> {
        let mut value: u16 = 0;
        for _ in 0..4 {
            let byte = self.bump().ok_or_else(|| self.error("\\u 转义不完整"))?;
            let digit = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => return Err(self.error("\\u 转义包含非十六进制字符")),
            };
            value = value * 16 + u16::from(digit);
        }
        Ok(value)
    }

    fn parse_number(&mut self) -> Result<Json, JsonError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.bump();
        }
        let mut digits = 0usize;
        let mut leading_zero = false;
        while let Some(byte) = self.peek() {
            if byte.is_ascii_digit() {
                if digits == 0 && byte == b'0' {
                    leading_zero = true;
                }
                self.bump();
                digits += 1;
            } else {
                break;
            }
        }
        if digits == 0 {
            return Err(self.error("数字缺少整数部分"));
        }
        if leading_zero && digits > 1 {
            return Err(self.error("数字不允许前导零"));
        }
        if self.peek() == Some(b'.') {
            self.bump();
            let mut fraction = 0usize;
            while let Some(byte) = self.peek() {
                if byte.is_ascii_digit() {
                    self.bump();
                    fraction += 1;
                } else {
                    break;
                }
            }
            if fraction == 0 {
                return Err(self.error("数字缺少小数部分"));
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            self.bump();
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.bump();
            }
            let mut exponent = 0usize;
            while let Some(byte) = self.peek() {
                if byte.is_ascii_digit() {
                    self.bump();
                    exponent += 1;
                } else {
                    break;
                }
            }
            if exponent == 0 {
                return Err(self.error("数字缺少指数部分"));
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| self.error("数字不是合法 UTF-8"))?;
        let value: f64 = text.parse().map_err(|_| self.error("数字超出可解析范围"))?;
        if !value.is_finite() {
            return Err(self.error("数字超出 f64 范围"));
        }
        Ok(Json::Number(value))
    }
}

// ---------------------------------------------------------------------------
// 序列化
// ---------------------------------------------------------------------------

fn write_compact(value: &Json, out: &mut String) {
    match value {
        Json::Null => out.push_str("null"),
        Json::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Json::Number(number) => write_number(*number, out),
        Json::String(text) => write_string(text, out),
        Json::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_compact(item, out);
            }
            out.push(']');
        }
        Json::Object(entries) => {
            out.push('{');
            for (index, (key, item)) in entries.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_string(key, out);
                out.push(':');
                write_compact(item, out);
            }
            out.push('}');
        }
    }
}

fn write_pretty(value: &Json, indent: usize, out: &mut String) {
    match value {
        Json::Array(items) if !items.is_empty() => {
            out.push_str("[\n");
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(",\n");
                }
                push_indent(indent + 1, out);
                write_pretty(item, indent + 1, out);
            }
            out.push('\n');
            push_indent(indent, out);
            out.push(']');
        }
        Json::Object(entries) if !entries.is_empty() => {
            out.push_str("{\n");
            for (index, (key, item)) in entries.iter().enumerate() {
                if index > 0 {
                    out.push_str(",\n");
                }
                push_indent(indent + 1, out);
                write_string(key, out);
                out.push_str(": ");
                write_pretty(item, indent + 1, out);
            }
            out.push('\n');
            push_indent(indent, out);
            out.push('}');
        }
        other => write_compact(other, out),
    }
}

fn push_indent(indent: usize, out: &mut String) {
    for _ in 0..indent {
        out.push_str("  ");
    }
}

fn write_number(number: f64, out: &mut String) {
    if number.fract() == 0.0 && number.abs() <= 9_007_199_254_740_992.0 {
        out.push_str(&format!("{}", number as i64));
    } else {
        out.push_str(&format!("{number}"));
    }
}

fn write_string(text: &str, out: &mut String) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", ch as u32));
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::{Json, JsonError};

    #[test]
    fn parse_basics() {
        assert_eq!(Json::parse("null").unwrap(), Json::Null);
        assert_eq!(Json::parse("true").unwrap(), Json::Bool(true));
        assert_eq!(Json::parse(" 42 ").unwrap(), Json::Number(42.0));
        assert_eq!(Json::parse("-1.5e2").unwrap(), Json::Number(-150.0));
        assert_eq!(Json::parse("\"hi\"").unwrap(), Json::from("hi"));
        assert_eq!(
            Json::parse("[1, 2, 3]").unwrap(),
            Json::Array(vec![Json::num(1), Json::num(2), Json::num(3)])
        );
    }

    #[test]
    fn parse_object_keeps_order() {
        let value = Json::parse(r#"{"b": 1, "a": [true, null]}"#).unwrap();
        let entries = value.as_object().unwrap();
        assert_eq!(entries[0].0, "b");
        assert_eq!(entries[1].0, "a");
        assert_eq!(value.get_path(&["a", "1"]).unwrap(), &Json::Null);
    }

    #[test]
    fn parse_escapes_and_unicode() {
        let value = Json::parse(r#""a\nb\u0041\u4e16\u754c\ud83d\ude00""#).unwrap();
        assert_eq!(value.as_str().unwrap(), "a\nbA世界\u{1F600}");
    }

    #[test]
    fn parse_non_ascii_literal() {
        let value = Json::parse("{\"名字\": \"世界\"}").unwrap();
        assert_eq!(value.get("名字").unwrap().as_str(), Some("世界"));
    }

    #[test]
    fn parse_rejects_invalid() {
        for text in [
            "",
            "tru",
            "[1,]",
            "{\"a\" 1}",
            "\"unterminated",
            "01",
            "1.",
            "1e",
            "{} extra",
            "\"\\q\"",
            "\"\\ud800\"",
        ] {
            assert!(Json::parse(text).is_err(), "should reject: {text}");
        }
    }

    #[test]
    fn roundtrip() {
        let text = r#"{"a":1,"b":[true,false,null],"c":"x\ny","d":-2.5}"#;
        let value = Json::parse(text).unwrap();
        let dumped = value.to_compact_string();
        assert_eq!(Json::parse(&dumped).unwrap(), value);
        let pretty = value.to_pretty_string();
        assert!(pretty.contains("\n  \"a\": 1"));
        assert_eq!(Json::parse(&pretty).unwrap(), value);
    }

    #[test]
    fn macros_build_values() {
        let value = crate::mini_json!({
            "ok": true,
            "text": "hello",
            "count": 2i64,
            "items": crate::mini_json!([ "a", 3i64 ]),
            "nested": crate::mini_json!({ "x": Json::Null }),
        });
        assert_eq!(value.get("ok").unwrap().as_bool(), Some(true));
        assert_eq!(value.get("count").unwrap().as_u64(), Some(2));
        assert_eq!(value.get("items").unwrap().as_array().unwrap().len(), 2);
        assert_eq!(value.get_path(&["nested", "x"]).unwrap(), &Json::Null);
    }

    #[test]
    fn set_and_remove() {
        let mut value = crate::mini_json!({ "a": 1i64 });
        value.set("b", Json::from("x"));
        value.set("a", Json::from(2i64));
        assert_eq!(value.get("a").unwrap().as_u64(), Some(2));
        assert_eq!(value.remove("b").unwrap().as_str(), Some("x"));
        assert!(value.remove("b").is_none());
    }

    #[test]
    fn error_reports_offset() {
        let error: JsonError = Json::parse("[1, x]").unwrap_err();
        assert!(error.message.contains("非法"));
        assert_eq!(error.offset, 4);
    }

    #[test]
    fn numbers_render_as_integers_when_integral() {
        assert_eq!(
            Json::num(1790000000123).to_compact_string(),
            "1790000000123"
        );
        assert_eq!(Json::float(1.25).to_compact_string(), "1.25");
    }
}
