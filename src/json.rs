use std::fmt::Write as _;

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn obj(entries: Vec<(&str, Json)>) -> Self {
        Json::Obj(
            entries
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        )
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        if let Json::Obj(entries) = self {
            for (k, v) in entries {
                if k == key {
                    return Some(v);
                }
            }
        }
        None
    }

    pub fn get_mut_or_insert(&mut self, key: &str, default: Json) -> &mut Json {
        if let Json::Obj(entries) = self {
            if let Some(pos) = entries.iter().position(|(k, _)| k == key) {
                return &mut entries[pos].1;
            }
            entries.push((key.to_string(), default));
            let last = entries.len() - 1;
            return &mut entries[last].1;
        }
        panic!("not an object")
    }

    pub fn set(&mut self, key: &str, val: Json) {
        if let Json::Obj(entries) = self {
            for (k, v) in entries.iter_mut() {
                if k == key {
                    *v = val;
                    return;
                }
            }
            entries.push((key.to_string(), val));
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        if let Json::Str(s) = self {
            Some(s.as_str())
        } else {
            None
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        if let Json::Bool(b) = self {
            Some(*b)
        } else {
            None
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        if let Json::Num(n) = self {
            Some(*n as i64)
        } else {
            None
        }
    }

    pub fn as_arr(&self) -> Option<&[Json]> {
        if let Json::Arr(a) = self {
            Some(a.as_slice())
        } else {
            None
        }
    }

    pub fn as_obj(&self) -> Option<&[(String, Json)]> {
        if let Json::Obj(o) = self {
            Some(o.as_slice())
        } else {
            None
        }
    }

    pub fn dump(&self) -> String {
        let mut out = String::new();
        self.write_compact(&mut out);
        out
    }

    pub fn dump_pretty(&self) -> String {
        let mut out = String::new();
        self.write_pretty(&mut out, 0);
        out
    }

    fn write_compact(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Num(n) => {
                if n.is_finite() {
                    if n.fract() == 0.0 && n.abs() < 1e15 {
                        let _ = write!(out, "{}", *n as i64);
                    } else {
                        let _ = write!(out, "{n}");
                    }
                } else {
                    out.push_str("null");
                }
            }
            Json::Str(s) => write_escaped_str(out, s),
            Json::Arr(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write_compact(out);
                }
                out.push(']');
            }
            Json::Obj(entries) => {
                out.push('{');
                for (i, (k, v)) in entries.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_escaped_str(out, k);
                    out.push(':');
                    v.write_compact(out);
                }
                out.push('}');
            }
        }
    }

    fn write_pretty(&self, out: &mut String, indent: usize) {
        let pad = "  ".repeat(indent);
        let next_pad = "  ".repeat(indent + 1);
        match self {
            Json::Arr(items) if items.is_empty() => out.push_str("[]"),
            Json::Obj(entries) if entries.is_empty() => out.push_str("{}"),
            Json::Arr(items) => {
                out.push_str("[\n");
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(",\n");
                    }
                    out.push_str(&next_pad);
                    item.write_pretty(out, indent + 1);
                }
                out.push('\n');
                out.push_str(&pad);
                out.push(']');
            }
            Json::Obj(entries) => {
                out.push_str("{\n");
                for (i, (k, v)) in entries.iter().enumerate() {
                    if i > 0 {
                        out.push_str(",\n");
                    }
                    out.push_str(&next_pad);
                    write_escaped_str(out, k);
                    out.push_str(": ");
                    v.write_pretty(out, indent + 1);
                }
                out.push('\n');
                out.push_str(&pad);
                out.push('}');
            }
            _ => self.write_compact(out),
        }
    }
}

fn write_escaped_str(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

pub fn parse_json(input: &str) -> Result<Json, String> {
    let mut parser = Parser {
        bytes: input.as_bytes(),
        pos: 0,
    };
    parser.skip_ws();
    let val = parser.parse_value()?;
    parser.skip_ws();
    if parser.pos < parser.bytes.len() {
        return Err(format!("trailing characters at byte {}", parser.pos));
    }
    Ok(val)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() {
            match self.bytes[self.pos] {
                b' ' | b'\t' | b'\n' | b'\r' => self.pos += 1,
                _ => break,
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn parse_value(&mut self) -> Result<Json, String> {
        self.skip_ws();
        match self.peek() {
            Some(b'"') => self.parse_string().map(Json::Str),
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
            Some(b't') => self.parse_literal(b"true", Json::Bool(true)),
            Some(b'f') => self.parse_literal(b"false", Json::Bool(false)),
            Some(b'n') => self.parse_literal(b"null", Json::Null),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.parse_number(),
            Some(c) => Err(format!("unexpected byte '{}' at {}", c as char, self.pos)),
            None => Err("unexpected end of JSON".to_string()),
        }
    }

    fn parse_literal(&mut self, lit: &[u8], val: Json) -> Result<Json, String> {
        if self.bytes.get(self.pos..self.pos + lit.len()) == Some(lit) {
            self.pos += lit.len();
            Ok(val)
        } else {
            Err(format!("invalid literal at {}", self.pos))
        }
    }

    fn parse_number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-') {
                self.pos += 1;
            } else {
                break;
            }
        }
        let s = std::str::from_utf8(&self.bytes[start..self.pos]).map_err(|e| e.to_string())?;
        let n: f64 = s.parse().map_err(|_| format!("invalid number '{s}'"))?;
        Ok(Json::Num(n))
    }

    fn parse_hex4(&mut self) -> Result<u16, String> {
        if self.pos + 4 > self.bytes.len() {
            return Err("truncated unicode escape".to_string());
        }
        let slice = &self.bytes[self.pos..self.pos + 4];
        self.pos += 4;
        let s = std::str::from_utf8(slice).map_err(|e| e.to_string())?;
        u16::from_str_radix(s, 16).map_err(|_| format!("invalid hex escape '{s}'"))
    }

    fn parse_string(&mut self) -> Result<String, String> {
        if self.peek() != Some(b'"') {
            return Err("expected '\"'".to_string());
        }
        self.pos += 1;
        let mut out = String::new();
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b == b'"' {
                self.pos += 1;
                return Ok(out);
            }
            if b == b'\\' {
                self.pos += 1;
                if self.pos >= self.bytes.len() {
                    return Err("unterminated escape".to_string());
                }
                let esc = self.bytes[self.pos];
                self.pos += 1;
                match esc {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\u{08}'),
                    b'f' => out.push('\u{0C}'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' => {
                        let code = self.parse_hex4()?;
                        if (0xD800..=0xDBFF).contains(&code) {
                            if self.bytes.get(self.pos..self.pos + 2) == Some(b"\\u") {
                                self.pos += 2;
                                let low = self.parse_hex4()?;
                                let combined = 0x10000
                                    + (((code as u32 - 0xD800) << 10) | (low as u32 - 0xDC00));
                                if let Some(ch) = char::from_u32(combined) {
                                    out.push(ch);
                                }
                            }
                        } else if let Some(ch) = char::from_u32(code as u32) {
                            out.push(ch);
                        }
                    }
                    other => out.push(other as char),
                }
            } else {
                let start = self.pos;
                self.pos += 1;
                while self.pos < self.bytes.len()
                    && self.bytes[self.pos] != b'"'
                    && self.bytes[self.pos] != b'\\'
                {
                    self.pos += 1;
                }
                let chunk =
                    std::str::from_utf8(&self.bytes[start..self.pos]).map_err(|e| e.to_string())?;
                out.push_str(chunk);
            }
        }
        Err("unterminated string".to_string())
    }

    fn parse_array(&mut self) -> Result<Json, String> {
        self.pos += 1;
        self.skip_ws();
        let mut items = Vec::new();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            items.push(self.parse_value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                    self.skip_ws();
                }
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Arr(items));
                }
                _ => return Err(format!("expected ',' or ']' at {}", self.pos)),
            }
        }
    }

    fn parse_object(&mut self) -> Result<Json, String> {
        self.pos += 1;
        self.skip_ws();
        let mut entries = Vec::new();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Obj(entries));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(format!("expected ':' at {}", self.pos));
            }
            self.pos += 1;
            let val = self.parse_value()?;
            entries.push((key, val));
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                    self.skip_ws();
                }
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Obj(entries));
                }
                _ => return Err(format!("expected ',' or '}}' at {}", self.pos)),
            }
        }
    }
}
