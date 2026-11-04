//! 工具参数的 UTF-8 文本词法与取值解析。
//!
//! 协议规定 Tool 参数对协议层不透明，因此 GPIO 工具自定义一套**严格**的文本
//! 语法：ASCII 空白分词、不支持引号/转义、键值大小写不敏感、引脚名规范化输出。

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use sekai::sekai_wire::{CommandError, ErrorCode};

use crate::config::{Direction, Level, Pull};
use crate::pin::Port;
use crate::port::GpioError;

/// 参数最大字节数。
pub const MAX_ARGS_BYTES: usize = 256;

/// 结果最大字节数。
pub const MAX_RESULT_BYTES: usize = 1024;

/// 把字节参数转换为文本并检查长度上限。
pub fn args_text(args: &[u8]) -> Result<&str, GpioError> {
    if args.len() > MAX_ARGS_BYTES {
        return Err(GpioError::invalid(format!(
            "参数超过 {MAX_ARGS_BYTES} 字节（实际 {} 字节）",
            args.len()
        )));
    }
    core::str::from_utf8(args).map_err(|_| GpioError::invalid("参数不是合法 UTF-8"))
}

/// 按 ASCII 空白分词；不支持引号与转义。
pub fn tokens(text: &str) -> impl Iterator<Item = &str> {
    text.split_ascii_whitespace()
}

/// 拆分 `key=value`；没有 `=` 时返回 `None`。
pub fn split_key_value(token: &str) -> Option<(&str, &str)> {
    token.split_once('=')
}

/// 键名比较（大小写不敏感）。
pub fn key_eq(key: &str, expected: &str) -> bool {
    key.eq_ignore_ascii_case(expected)
}

/// 解析方向：`in`/`input`/`out`/`output`。
pub fn parse_direction(value: &str) -> Result<Direction, GpioError> {
    if value.eq_ignore_ascii_case("in") || value.eq_ignore_ascii_case("input") {
        Ok(Direction::Input)
    } else if value.eq_ignore_ascii_case("out") || value.eq_ignore_ascii_case("output") {
        Ok(Direction::Output)
    } else {
        Err(GpioError::invalid(format!(
            "方向 `{value}` 非法；应为 in/input/out/output"
        )))
    }
}

/// 解析电平：`0`/`low`/`off` 或 `1`/`high`/`on`。
pub fn parse_level(value: &str) -> Result<Level, GpioError> {
    if value.eq_ignore_ascii_case("0")
        || value.eq_ignore_ascii_case("low")
        || value.eq_ignore_ascii_case("off")
    {
        Ok(Level::Low)
    } else if value.eq_ignore_ascii_case("1")
        || value.eq_ignore_ascii_case("high")
        || value.eq_ignore_ascii_case("on")
    {
        Ok(Level::High)
    } else {
        Err(GpioError::invalid(format!(
            "电平 `{value}` 非法；应为 0/1/low/high/on/off"
        )))
    }
}

/// 解析上下拉：`none`/`up`/`down`。
pub fn parse_pull(value: &str) -> Result<Pull, GpioError> {
    if value.eq_ignore_ascii_case("none") {
        Ok(Pull::None)
    } else if value.eq_ignore_ascii_case("up") {
        Ok(Pull::Up)
    } else if value.eq_ignore_ascii_case("down") {
        Ok(Pull::Down)
    } else {
        Err(GpioError::invalid(format!(
            "上下拉 `{value}` 非法；应为 none/up/down"
        )))
    }
}

/// 解析布尔值：`0|1|true|false|on|off`。
pub fn parse_bool(value: &str) -> Result<bool, GpioError> {
    if value.eq_ignore_ascii_case("0")
        || value.eq_ignore_ascii_case("false")
        || value.eq_ignore_ascii_case("off")
    {
        Ok(false)
    } else if value.eq_ignore_ascii_case("1")
        || value.eq_ignore_ascii_case("true")
        || value.eq_ignore_ascii_case("on")
    {
        Ok(true)
    } else {
        Err(GpioError::invalid(format!(
            "布尔值 `{value}` 非法；应为 0/1/true/false/on/off"
        )))
    }
}

/// 解析 STM32 端口字母：`A`..`K`（大小写不敏感）。
pub fn parse_port(value: &str) -> Result<Port, GpioError> {
    let bytes = value.as_bytes();
    if bytes.len() == 1
        && let Some(port) = Port::from_ascii(bytes[0])
    {
        return Ok(port);
    }
    Err(GpioError::invalid(format!(
        "端口 `{value}` 非法；应为 A..K"
    )))
}

/// 拼接为逗号分隔文本；列表为空时返回 `none`。
pub fn join_or_none(names: &[&str]) -> String {
    if names.is_empty() {
        String::from("none")
    } else {
        names.join(",")
    }
}

/// 检查结果长度后返回；超过 1024 字节映射为 `ResultTooLarge`。
pub fn finish_result(result: Vec<u8>) -> Result<Vec<u8>, CommandError> {
    if result.len() > MAX_RESULT_BYTES {
        return Err(CommandError::new(
            ErrorCode::ResultTooLarge,
            "GPIO 结果过大",
        ));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::{
        args_text, finish_result, join_or_none, key_eq, parse_bool, parse_direction, parse_level,
        parse_port, parse_pull, split_key_value, tokens,
    };
    use crate::config::{Direction, Level, Pull};
    use crate::pin::Port;
    use crate::port::GpioError;
    use sekai::sekai_wire::ErrorCode;

    #[test]
    fn tokenizes_on_ascii_whitespace() {
        let parsed: alloc::vec::Vec<&str> = tokens("  IO4\tout\n level=0  ").collect();
        assert_eq!(parsed, alloc::vec!["IO4", "out", "level=0"]);
        assert_eq!(tokens("").count(), 0);
    }

    #[test]
    fn splits_key_value_pairs() {
        assert_eq!(split_key_value("level=0"), Some(("level", "0")));
        assert_eq!(split_key_value("open_drain=1"), Some(("open_drain", "1")));
        assert_eq!(split_key_value("out"), None);
        assert_eq!(split_key_value("level="), Some(("level", "")));
        assert!(key_eq("LEVEl", "level"));
        assert!(!key_eq("lev", "level"));
    }

    #[test]
    fn parses_levels_and_directions() {
        assert_eq!(parse_level("0").unwrap(), Level::Low);
        assert_eq!(parse_level("LOW").unwrap(), Level::Low);
        assert_eq!(parse_level("off").unwrap(), Level::Low);
        assert_eq!(parse_level("1").unwrap(), Level::High);
        assert_eq!(parse_level("High").unwrap(), Level::High);
        assert_eq!(parse_level("ON").unwrap(), Level::High);
        assert!(matches!(
            parse_level("2"),
            Err(GpioError::InvalidArgument(_))
        ));

        assert_eq!(parse_direction("IN").unwrap(), Direction::Input);
        assert_eq!(parse_direction("input").unwrap(), Direction::Input);
        assert_eq!(parse_direction("out").unwrap(), Direction::Output);
        assert_eq!(parse_direction("Output").unwrap(), Direction::Output);
        assert!(parse_direction("inout").is_err());
    }

    #[test]
    fn parses_pull_and_bool_and_port() {
        assert_eq!(parse_pull("none").unwrap(), Pull::None);
        assert_eq!(parse_pull("UP").unwrap(), Pull::Up);
        assert_eq!(parse_pull("Down").unwrap(), Pull::Down);
        assert!(parse_pull("float").is_err());

        assert!(!parse_bool("0").unwrap());
        assert!(!parse_bool("FALSE").unwrap());
        assert!(!parse_bool("off").unwrap());
        assert!(parse_bool("1").unwrap());
        assert!(parse_bool("True").unwrap());
        assert!(parse_bool("on").unwrap());
        assert!(parse_bool("yes").is_err());

        assert_eq!(parse_port("A").unwrap(), Port::A);
        assert_eq!(parse_port("k").unwrap(), Port::K);
        assert!(parse_port("").is_err());
        assert!(parse_port("AA").is_err());
        assert!(parse_port("L").is_err());
    }

    #[test]
    fn args_length_and_utf8_are_checked() {
        assert!(args_text(b"IO4 out level=0").is_ok());
        assert!(matches!(
            args_text(&[0xff, 0xfe]),
            Err(GpioError::InvalidArgument(_))
        ));
        let long = alloc::vec![b'a'; super::MAX_ARGS_BYTES + 1];
        let error = args_text(&long).unwrap_err();
        assert!(error.message().contains("256"), "{}", error.message());
        let exact = alloc::vec![b'a'; super::MAX_ARGS_BYTES];
        assert!(args_text(&exact).is_ok());
    }

    #[test]
    fn results_are_bounded() {
        assert!(finish_result(alloc::vec![b'x'; super::MAX_RESULT_BYTES]).is_ok());
        let error = finish_result(alloc::vec![b'x'; super::MAX_RESULT_BYTES + 1]).unwrap_err();
        assert_eq!(error.code, ErrorCode::ResultTooLarge);
        assert_eq!(error.message, "GPIO 结果过大");
    }

    #[test]
    fn joins_names() {
        assert_eq!(join_or_none(&[]), "none");
        assert_eq!(join_or_none(&["in", "out"]), "in,out");
    }
}
