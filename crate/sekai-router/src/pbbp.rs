//! pbbp2.Frame 的 protobuf wire 编解码（飞书长连接，见 docs/sekai-feishu.md §3.3）。
//!
//! 零外部依赖：只实现本协议用到的 wire 子集——varint（wire type 0）与
//! length-delimited（wire type 2）。未知字段号**静默跳过**（protobuf 向前
//! 兼容）；wire type 1/5（定长 64/32 位）在本协议流量中不出现，遇到时因
//! 无法安全前进而报 [`PbbpError::Wire`]。编码侧按字段号升序写出，并按
//! proto3 惯例省略默认值字段（varint 0、空 payload）；`Option` 字段只要
//! 是 `Some` 就写出（含空串），保证 `decode(encode(f)) == f` 往返成立。

/// 键值对头（repeated message，字段 key=1、value=2，均 string）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub key: String,
    pub value: String,
}

/// pbbp2.Frame（protobuf 消息；字段号即 wire tag，见 §3.3 字段表）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Frame {
    /// 字段 1，varint：连接内单调递增。
    pub seq_id: u64,
    /// 字段 2，varint：旧日志 id（可 0）。
    pub log_id: u64,
    /// 字段 3，varint：服务标识。
    pub service: u64,
    /// 字段 4，varint：0 = CONTROL（[`METHOD_CONTROL`]），1 = DATA（[`METHOD_DATA`]）。
    pub method: u64,
    /// 字段 5，repeated message：键值对头。
    pub headers: Vec<Header>,
    /// 字段 6，string：如 `json`。
    pub payload_encoding: Option<String>,
    /// 字段 7，string：如 `event/v2.0`。
    pub payload_type: Option<String>,
    /// 字段 8，bytes：事件 JSON 字节。
    pub payload: Vec<u8>,
    /// 字段 9，string：新日志 id。
    pub log_id_new: Option<String>,
}

/// method 取值：CONTROL 帧（应用层 ping/pong/设置，见 §3.4）。
pub const METHOD_CONTROL: u64 = 0;
/// method 取值：DATA 帧（事件推送，见 §3.3/§3.5）。
pub const METHOD_DATA: u64 = 1;

/// wire 编解码错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PbbpError {
    /// 输入在字段中间被截断。
    Truncated(String),
    /// wire 类型/值非法。
    Wire(String),
}

impl std::fmt::Display for PbbpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PbbpError::Truncated(detail) => write!(f, "输入被截断：{detail}"),
            PbbpError::Wire(detail) => write!(f, "wire 格式非法：{detail}"),
        }
    }
}

impl std::error::Error for PbbpError {}

impl Header {
    /// 编码为字段 5 的内嵌消息字节（proto3 惯例：空串省略）。
    fn encode_into(&self, out: &mut Vec<u8>) {
        if !self.key.is_empty() {
            write_bytes(out, 1, self.key.as_bytes());
        }
        if !self.value.is_empty() {
            write_bytes(out, 2, self.value.as_bytes());
        }
    }

    /// 从内嵌消息字节解码（未知字段同样静默跳过）。
    fn decode(bytes: &[u8]) -> Result<Self, PbbpError> {
        let mut reader = Reader::new(bytes);
        let mut header = Header {
            key: String::new(),
            value: String::new(),
        };
        while !reader.done() {
            let (field_no, wire_type) = reader.key()?;
            match field_no {
                1 => {
                    expect_wire(field_no, 2, wire_type)?;
                    header.key = reader.string()?;
                }
                2 => {
                    expect_wire(field_no, 2, wire_type)?;
                    header.value = reader.string()?;
                }
                _ => reader.skip(wire_type)?,
            }
        }
        Ok(header)
    }
}

impl Frame {
    /// 编码为 protobuf wire 字节：字段按号升序写出，默认值字段省略。
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        write_varint_field(&mut out, 1, self.seq_id);
        write_varint_field(&mut out, 2, self.log_id);
        write_varint_field(&mut out, 3, self.service);
        write_varint_field(&mut out, 4, self.method);
        for header in &self.headers {
            let mut inner = Vec::new();
            header.encode_into(&mut inner);
            write_bytes(&mut out, 5, &inner);
        }
        if let Some(encoding) = &self.payload_encoding {
            write_bytes(&mut out, 6, encoding.as_bytes());
        }
        if let Some(payload_type) = &self.payload_type {
            write_bytes(&mut out, 7, payload_type.as_bytes());
        }
        if !self.payload.is_empty() {
            write_bytes(&mut out, 8, &self.payload);
        }
        if let Some(log_id_new) = &self.log_id_new {
            write_bytes(&mut out, 9, log_id_new.as_bytes());
        }
        out
    }

    /// 从 wire 字节解码；未知字段号按其 wire type 静默跳过（protobuf
    /// 向前兼容）。已知字段遇到不支持的 wire type 报 [`PbbpError::Wire`]。
    pub fn decode(bytes: &[u8]) -> Result<Self, PbbpError> {
        let mut reader = Reader::new(bytes);
        let mut frame = Frame::default();
        while !reader.done() {
            let (field_no, wire_type) = reader.key()?;
            match field_no {
                1 => {
                    expect_wire(field_no, 0, wire_type)?;
                    frame.seq_id = reader.varint()?;
                }
                2 => {
                    expect_wire(field_no, 0, wire_type)?;
                    frame.log_id = reader.varint()?;
                }
                3 => {
                    expect_wire(field_no, 0, wire_type)?;
                    frame.service = reader.varint()?;
                }
                4 => {
                    expect_wire(field_no, 0, wire_type)?;
                    frame.method = reader.varint()?;
                }
                5 => {
                    expect_wire(field_no, 2, wire_type)?;
                    frame.headers.push(Header::decode(reader.bytes()?)?);
                }
                6 => {
                    expect_wire(field_no, 2, wire_type)?;
                    frame.payload_encoding = Some(reader.string()?);
                }
                7 => {
                    expect_wire(field_no, 2, wire_type)?;
                    frame.payload_type = Some(reader.string()?);
                }
                8 => {
                    expect_wire(field_no, 2, wire_type)?;
                    frame.payload = reader.bytes()?.to_vec();
                }
                9 => {
                    expect_wire(field_no, 2, wire_type)?;
                    frame.log_id_new = Some(reader.string()?);
                }
                _ => reader.skip(wire_type)?,
            }
        }
        Ok(frame)
    }

    /// 取首个匹配 header 的值；不存在返回 `None`。
    pub fn header(&self, key: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|header| header.key == key)
            .map(|header| header.value.as_str())
    }

    /// 设置 header：key 已存在则覆盖首个匹配项，不存在则追加。
    pub fn set_header(&mut self, key: &str, value: impl Into<String>) {
        let value = value.into();
        match self.headers.iter_mut().find(|header| header.key == key) {
            Some(header) => header.value = value,
            None => self.headers.push(Header {
                key: key.to_string(),
                value,
            }),
        }
    }
}

/// 解码游标：封装 varint / 字段 key / length-delimited 的顺序读取。
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Reader { bytes, pos: 0 }
    }

    /// 是否已消费完输入。
    fn done(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    /// 读一个 varint。最多 10 字节：第 10 字节仍带延续位（超长）或值
    /// 超出 u64 范围 → [`PbbpError::Wire`]；输入中途用尽 → [`PbbpError::Truncated`]。
    fn varint(&mut self) -> Result<u64, PbbpError> {
        let start = self.pos;
        let mut value: u64 = 0;
        for index in 0..10u32 {
            let byte = *self.bytes.get(self.pos).ok_or_else(|| {
                PbbpError::Truncated(format!(
                    "偏移 {start} 处 varint 在第 {} 字节被截断",
                    index + 1
                ))
            })?;
            self.pos += 1;
            if index == 9 {
                // 前 9 字节已占满低 63 位；第 10 字节最多再贡献最高 1 位。
                if byte & 0x80 != 0 {
                    return Err(PbbpError::Wire(format!(
                        "偏移 {start} 处 varint 第 10 字节仍带延续位，超过 10 字节上限"
                    )));
                }
                if byte > 0x01 {
                    return Err(PbbpError::Wire(format!(
                        "偏移 {start} 处 varint 第 10 字节 0x{byte:02x} 超出 u64 表示范围"
                    )));
                }
            }
            value |= u64::from(byte & 0x7F) << (index * 7);
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(PbbpError::Wire(format!(
            "偏移 {start} 处 varint 超过 10 字节上限"
        )))
    }

    /// 读字段 key（varint），拆成（字段号, wire 类型）。字段号 0 为
    /// protobuf 保留值，直接判非法。
    fn key(&mut self) -> Result<(u32, u8), PbbpError> {
        let start = self.pos;
        let key = self.varint()?;
        let field_no = u32::try_from(key >> 3)
            .map_err(|_| PbbpError::Wire(format!("偏移 {start} 处字段号超出 u32 范围")))?;
        if field_no == 0 {
            return Err(PbbpError::Wire(format!(
                "偏移 {start} 处字段号为 0（保留值）"
            )));
        }
        Ok((field_no, (key & 0x07) as u8))
    }

    /// 读 length-delimited（wire type 2）：长度 varint + 内容切片。
    /// 长度越过输入末尾 → [`PbbpError::Truncated`]。
    fn bytes(&mut self) -> Result<&'a [u8], PbbpError> {
        let start = self.pos;
        let len = self.varint()?;
        let end = usize::try_from(len)
            .ok()
            .and_then(|len| self.pos.checked_add(len))
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| {
                PbbpError::Truncated(format!(
                    "偏移 {start} 处 length-delimited 长度 {len} 越过输入末尾（{} 字节）",
                    self.bytes.len()
                ))
            })?;
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    /// 读 length-delimited 字符串（protobuf string 必须是合法 UTF-8）。
    fn string(&mut self) -> Result<String, PbbpError> {
        let start = self.pos;
        let bytes = self.bytes()?;
        String::from_utf8(bytes.to_vec())
            .map_err(|_| PbbpError::Wire(format!("偏移 {start} 处 string 含非法 UTF-8")))
    }

    /// 按既有 wire 类型跳过未知字段：0 读 varint、2 读长度后跳内容；
    /// 其他 wire 类型无法安全前进 → [`PbbpError::Wire`]。
    fn skip(&mut self, wire_type: u8) -> Result<(), PbbpError> {
        match wire_type {
            0 => self.varint().map(|_| ()),
            2 => self.bytes().map(|_| ()),
            other => Err(PbbpError::Wire(format!(
                "wire 类型 {other} 不受支持（仅 0=varint 与 2=length-delimited），无法跳过"
            ))),
        }
    }
}

/// 已知字段的 wire 类型校验：不符即 [`PbbpError::Wire`]。
fn expect_wire(field_no: u32, want: u8, got: u8) -> Result<(), PbbpError> {
    if want == got {
        Ok(())
    } else {
        Err(PbbpError::Wire(format!(
            "字段 {field_no} 期望 wire 类型 {want}，实际 {got}"
        )))
    }
}

/// 写 varint：每字节低 7 位有效，最高位为延续位（小端 7 位组）。
fn write_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            return;
        }
    }
}

/// 写字段 key：字段号 << 3 | wire 类型。
fn write_key(out: &mut Vec<u8>, field_no: u32, wire_type: u8) {
    write_varint(out, (u64::from(field_no) << 3) | u64::from(wire_type));
}

/// 写 length-delimited 字段：key + 长度 varint + 内容。
fn write_bytes(out: &mut Vec<u8>, field_no: u32, bytes: &[u8]) {
    write_key(out, field_no, 2);
    write_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

/// 写 varint 字段；proto3 惯例：值为 0（默认值）时省略整个字段。
fn write_varint_field(out: &mut Vec<u8>, field_no: u32, value: u64) {
    if value != 0 {
        write_key(out, field_no, 0);
        write_varint(out, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 与 golden_bytes 测试推导保持一致的最小完整帧。
    fn golden_frame() -> Frame {
        Frame {
            seq_id: 7,
            log_id: 0,
            service: 0,
            method: METHOD_DATA,
            headers: vec![Header {
                key: "type".into(),
                value: "event".into(),
            }],
            payload_encoding: Some("json".into()),
            payload_type: Some("event/v2.0".into()),
            payload: b"{}".to_vec(),
            log_id_new: Some("log-1".into()),
        }
    }

    /// 手工 golden 字节（按 protobuf 规则逐步推导）：
    ///
    /// - 字段 1 varint：tag = (1<<3)|0 = `0x08`，seq_id=7 → `08 07`
    /// - 字段 4 varint：tag = (4<<3)|0 = `0x20`，method=1 → `20 01`
    /// - 字段 5 message：tag = (5<<3)|2 = `0x2a`；内嵌 Header{type:"event"}：
    ///   key 字段 1 string：`0a 04` + "type"（74 79 70 65），
    ///   value 字段 2 string：`12 05` + "event"（65 76 65 6e 74），
    ///   内嵌消息共 6+7=13 字节 → 长度前缀 `0x0d`
    /// - 字段 6 string："json" → `32 04 6a 73 6f 6e`
    /// - 字段 7 string："event/v2.0"（10 字节）→ `3a 0a` + ASCII
    /// - 字段 8 bytes：`{}` → `42 02 7b 7d`
    /// - 字段 9 string："log-1" → `4a 05 6c 6f 67 2d 31`
    #[test]
    fn golden_bytes() {
        let seq_only = Frame {
            seq_id: 1,
            ..Frame::default()
        };
        assert_eq!(seq_only.encode(), [0x08, 0x01]);

        let method_only = Frame {
            method: METHOD_DATA,
            ..Frame::default()
        };
        assert_eq!(method_only.encode(), [0x20, 0x01]);

        let golden: Vec<u8> = vec![
            0x08, 0x07, // 字段 1 seq_id=7
            0x20, 0x01, // 字段 4 method=1（DATA）
            0x2a, 0x0d, // 字段 5 headers[0]，内嵌长 13
            0x0a, 0x04, 0x74, 0x79, 0x70, 0x65, //   key="type"
            0x12, 0x05, 0x65, 0x76, 0x65, 0x6e, 0x74, //   value="event"
            0x32, 0x04, 0x6a, 0x73, 0x6f, 0x6e, // 字段 6 payload_encoding="json"
            0x3a, 0x0a, // 字段 7 payload_type，长 10
            0x65, 0x76, 0x65, 0x6e, 0x74, 0x2f, 0x76, 0x32, 0x2e, 0x30, // "event/v2.0"
            0x42, 0x02, 0x7b, 0x7d, // 字段 8 payload=b"{}"
            0x4a, 0x05, 0x6c, 0x6f, 0x67, 0x2d, 0x31, // 字段 9 log_id_new="log-1"
        ];
        let frame = golden_frame();
        assert_eq!(frame.encode(), golden);
        // golden 字节反向 decode 回同一帧（字节级互证）。
        assert_eq!(Frame::decode(&golden).unwrap(), frame);
    }

    /// decode(encode(f)) == f：全空默认帧与全字段填满帧。
    #[test]
    fn roundtrip_default_and_full() {
        let empty = Frame::default();
        assert_eq!(empty.encode(), Vec::<u8>::new());
        assert_eq!(Frame::decode(&[]).unwrap(), empty);

        let rich = Frame {
            seq_id: u64::MAX,
            log_id: 1 << 63,
            service: 42,
            method: METHOD_DATA,
            headers: vec![
                Header {
                    key: "type".into(),
                    value: "event".into(),
                },
                Header {
                    key: "empty".into(),
                    value: String::new(),
                },
            ],
            payload_encoding: Some(String::new()), // Some("") 不得丢 Some
            payload_type: Some("event/v2.0".into()),
            payload: b"{\"k\":1}".to_vec(),
            log_id_new: Some("new-log".into()),
        };
        assert_eq!(Frame::decode(&rich.encode()).unwrap(), rich);
    }

    /// 未知字段跳过：字段 15（varint）与 16（length-delimited）混在已知字段之间。
    ///
    /// - `08 05`：字段 1 seq_id=5
    /// - `78 09`：字段 15 varint（tag=(15<<3)|0=0x78）值 9 → 跳过
    /// - `82 01 03 61 62 63`：字段 16（tag=(16<<3)|2=130，varint 编码
    ///   `82 01`）+ 长度 3 + "abc" → 跳过
    /// - `42 01 58`：字段 8 payload=[0x58]
    #[test]
    fn skips_unknown_fields() {
        let bytes: &[u8] = &[
            0x08, 0x05, // 字段 1 seq_id=5
            0x78, 0x09, // 未知字段 15（varint）
            0x82, 0x01, 0x03, b'a', b'b', b'c', // 未知字段 16（length-delimited）
            0x42, 0x01, 0x58, // 字段 8 payload=[0x58]
        ];
        let frame = Frame::decode(bytes).unwrap();
        assert_eq!(frame.seq_id, 5);
        assert_eq!(frame.payload, [0x58]);
        assert_eq!(frame.method, METHOD_CONTROL);
        assert!(frame.headers.is_empty());
        assert_eq!(frame.payload_encoding, None);

        // 未知字段的 wire type 同样要能正确前进后继续解析后续已知字段。
        let tail = Frame::decode(&[0x78, 0x09, 0x08, 0x2a]).unwrap();
        assert_eq!(tail.seq_id, 0x2a);
    }

    /// varint 边界：0（默认值省略）、u64::MAX 与 1<<63（10 字节）、超长。
    #[test]
    fn varint_boundaries() {
        // u64::MAX = 0xff×9 + 0x01（10 字节 varint）。
        let mut max = vec![0x08];
        max.extend_from_slice(&[0xff; 9]);
        max.push(0x01);
        assert_eq!(Frame::decode(&max).unwrap().seq_id, u64::MAX);

        // 1<<63 = 0x80×9 + 0x01（10 字节 varint）。
        let mut high_bit = vec![0x08];
        high_bit.extend_from_slice(&[0x80; 9]);
        high_bit.push(0x01);
        assert_eq!(Frame::decode(&high_bit).unwrap().seq_id, 1 << 63);

        // 编码侧 0 值字段省略；u64::MAX 往返。
        let zero = Frame {
            seq_id: 0,
            ..Frame::default()
        };
        assert_eq!(zero.encode(), Vec::<u8>::new());
        let max_frame = Frame {
            seq_id: u64::MAX,
            log_id: u64::MAX,
            ..Frame::default()
        };
        assert_eq!(Frame::decode(&max_frame.encode()).unwrap(), max_frame);

        // 第 10 字节仍带延续位 → Wire（超长）。
        let mut over = vec![0x08];
        over.extend_from_slice(&[0xff; 10]);
        assert!(matches!(Frame::decode(&over), Err(PbbpError::Wire(_))));

        // 第 10 字节 0x02：不带延续位但超出 u64 范围 → Wire。
        let mut wide = vec![0x08];
        wide.extend_from_slice(&[0xff; 9]);
        wide.push(0x02);
        assert!(matches!(Frame::decode(&wide), Err(PbbpError::Wire(_))));

        // varint 中途输入用尽 → Truncated。
        assert!(matches!(
            Frame::decode(&[0x08, 0xff]),
            Err(PbbpError::Truncated(_))
        ));
    }

    /// 截断检测：合法编码任意切点要么仍是合法（更短的）帧，要么 Truncated。
    #[test]
    fn truncation_detection() {
        let encoded = golden_frame().encode();
        assert_eq!(encoded.len(), 48);
        for split in 0..encoded.len() {
            match Frame::decode(&encoded[..split]) {
                Ok(_) => {} // 切点恰好落在字段边界：得到合法前缀帧
                Err(PbbpError::Truncated(_)) => {}
                Err(other) => panic!("切点 {split} 意外错误：{other}"),
            }
        }
        // tag 后缺 varint 值。
        assert!(matches!(
            Frame::decode(&encoded[..1]),
            Err(PbbpError::Truncated(_))
        ));
        // 内嵌 header 消息中途被切。
        assert!(matches!(
            Frame::decode(&encoded[..15]),
            Err(PbbpError::Truncated(_))
        ));
        // 长度 varint 指到输入末尾之外（声明 5 字节只给了 1 字节）。
        assert!(matches!(
            Frame::decode(&[0x42, 0x05, 0x7b]),
            Err(PbbpError::Truncated(_))
        ));
    }

    /// header()/set_header()：追加、查询、覆盖、重复 key 取首个。
    #[test]
    fn header_accessors() {
        let mut frame = Frame::default();
        assert_eq!(frame.header("type"), None);

        frame.set_header("type", "event");
        frame.set_header("message_id", "m-1");
        assert_eq!(frame.header("type"), Some("event"));
        assert_eq!(frame.header("message_id"), Some("m-1"));
        assert_eq!(frame.headers.len(), 2);

        // 覆盖已存在的 key：值更新、不追加重复项。
        frame.set_header("type", "pong");
        assert_eq!(frame.header("type"), Some("pong"));
        assert_eq!(frame.headers.len(), 2);

        // 重复 key 时 header() 取首个匹配（即被覆盖的那条）。
        frame.headers.push(Header {
            key: "type".into(),
            value: "second".into(),
        });
        assert_eq!(frame.header("type"), Some("pong"));

        // value 参数 impl Into<String>：&'static str 与 String 均可。
        frame.set_header("trace_id", String::from("t-9"));
        assert_eq!(frame.header("trace_id"), Some("t-9"));
    }

    /// wire 类型/值非法：已知字段错 wire 类型、字段号 0、非法 UTF-8、
    /// 未知字段使用无法安全前进的 wire 类型。
    #[test]
    fn rejects_bad_wire() {
        // 字段 1（varint）收到 wire type 2 → Wire。
        assert!(matches!(
            Frame::decode(&[0x0a, 0x01, b'A']),
            Err(PbbpError::Wire(_))
        ));
        // 字段 8（bytes）收到 wire type 0（tag 0x40）→ Wire。
        assert!(matches!(
            Frame::decode(&[0x40, 0x01]),
            Err(PbbpError::Wire(_))
        ));
        // 字段号 0（tag 0x00）为 protobuf 保留值 → Wire。
        assert!(matches!(
            Frame::decode(&[0x00, 0x01]),
            Err(PbbpError::Wire(_))
        ));
        // string 字段含非法 UTF-8 → Wire。
        assert!(matches!(
            Frame::decode(&[0x32, 0x01, 0xff]),
            Err(PbbpError::Wire(_))
        ));
        // 未知字段 15 使用 wire type 5（tag=(15<<3)|5=0x7d）→ 无法跳过 → Wire。
        assert!(matches!(Frame::decode(&[0x7d]), Err(PbbpError::Wire(_))));
        // Header 内嵌消息里字段 1 用 wire type 0 → Wire。
        let bad_header = [0x2a, 0x02, 0x08, 0x01];
        assert!(matches!(
            Frame::decode(&bad_header),
            Err(PbbpError::Wire(_))
        ));
    }

    /// Display 中文消息与 std::error::Error 兼容。
    #[test]
    fn error_display_and_std_error() {
        let truncated = Frame::decode(&[0x08, 0xff]).unwrap_err();
        assert!(truncated.to_string().contains("截断"), "{truncated}");
        let wire = PbbpError::Wire("字段号非法".into());
        assert!(wire.to_string().contains("wire 格式非法"), "{wire}");
        // 可作为 std::error::Error 使用（Box<dyn Error> 转换）。
        let boxed: Box<dyn std::error::Error> = Box::new(wire);
        assert!(boxed.to_string().contains("字段号非法"));
    }
}
