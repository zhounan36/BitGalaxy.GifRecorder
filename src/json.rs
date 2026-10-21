//! 极简 JSON 解析与序列化（CLI 结果输出 / MCP 服务器共用，零依赖）。
//! 覆盖完整 JSON 语法（含 \uXXXX 代理对），对象保持插入顺序，输出稳定单行。

/// JSON 值
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
    // ---------- 构造辅助 ----------

    pub fn obj(pairs: Vec<(&str, Json)>) -> Json {
        Json::Obj(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    pub fn arr(v: Vec<Json>) -> Json {
        Json::Arr(v)
    }

    pub fn s(v: impl Into<String>) -> Json {
        Json::Str(v.into())
    }

    pub fn i(v: i64) -> Json {
        Json::Num(v as f64)
    }

    pub fn u(v: u64) -> Json {
        Json::Num(v as f64)
    }

    // ---------- 读取辅助 ----------

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_arr(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(v) => Some(v),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }

    // ---------- 序列化 ----------

    pub fn to_string(&self) -> String {
        let mut s = String::new();
        self.write(&mut s);
        s
    }

    /// 带缩进的多行输出（2 空格），用于写回用户配置文件
    pub fn to_string_pretty(&self) -> String {
        let mut s = String::new();
        self.write_pretty(&mut s, 0);
        s
    }

    fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Num(n) => write_num(*n, out),
            Json::Str(s) => write_str(s, out),
            Json::Arr(v) => {
                out.push('[');
                for (i, item) in v.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Json::Obj(m) => {
                out.push('{');
                for (i, (k, v)) in m.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_str(k, out);
                    out.push(':');
                    v.write(out);
                }
                out.push('}');
            }
        }
    }
    fn write_pretty(&self, out: &mut String, depth: usize) {
        let pad = |out: &mut String, d: usize| {
            for _ in 0..d {
                out.push_str("  ");
            }
        };
        match self {
            Json::Arr(v) if !v.is_empty() => {
                out.push_str("[\n");
                for (i, item) in v.iter().enumerate() {
                    pad(out, depth + 1);
                    item.write_pretty(out, depth + 1);
                    if i + 1 < v.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                pad(out, depth);
                out.push(']');
            }
            Json::Obj(m) if !m.is_empty() => {
                out.push_str("{\n");
                for (i, (k, v)) in m.iter().enumerate() {
                    pad(out, depth + 1);
                    write_str(k, out);
                    out.push_str(": ");
                    v.write_pretty(out, depth + 1);
                    if i + 1 < m.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                pad(out, depth);
                out.push('}');
            }
            // 空数组/空对象/标量沿用紧凑形式
            _ => self.write(out),
        }
    }
}

/// 整数值用整数格式输出；其余用 Rust 最短浮点表示
fn write_num(n: f64, out: &mut String) {
    if !n.is_finite() {
        out.push_str("null"); // JSON 不允许 NaN/Inf
    } else if n == n.trunc() && n.abs() <= 9_007_199_254_740_992.0 {
        out.push_str(&format!("{}", n as i64));
    } else {
        out.push_str(&format!("{n}"));
    }
}

fn write_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

// ============================================================
// 解析
// ============================================================

const MAX_DEPTH: usize = 64;

pub fn parse(text: &str) -> Result<Json, String> {
    let mut p = Parser { b: text.as_bytes(), i: 0 };
    p.ws();
    let v = p.value(0)?;
    p.ws();
    if p.i != p.b.len() {
        return Err(format!("JSON 尾部有多余字符（位置 {}）", p.i));
    }
    Ok(v)
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, String> {
        if depth > MAX_DEPTH {
            return Err("JSON 嵌套过深".into());
        }
        let Some(&c) = self.b.get(self.i) else {
            return Err("JSON 意外结束".into());
        };
        match c {
            b'{' => self.object(depth),
            b'[' => self.array(depth),
            b'"' => Ok(Json::Str(self.string()?)),
            b't' => self.lit("true", Json::Bool(true)),
            b'f' => self.lit("false", Json::Bool(false)),
            b'n' => self.lit("null", Json::Null),
            b'-' | b'0'..=b'9' => self.number(),
            c => Err(format!("非法字符 '{}'（位置 {}）", c as char, self.i)),
        }
    }

    fn lit(&mut self, word: &str, v: Json) -> Result<Json, String> {
        if self.b[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            Ok(v)
        } else {
            Err(format!("非法字面量（位置 {}）", self.i))
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, String> {
        self.i += 1; // '{'
        let mut m = Vec::new();
        self.ws();
        if self.b.get(self.i) == Some(&b'}') {
            self.i += 1;
            return Ok(Json::Obj(m));
        }
        loop {
            self.ws();
            if self.b.get(self.i) != Some(&b'"') {
                return Err(format!("期望对象键（位置 {}）", self.i));
            }
            let k = self.string()?;
            self.ws();
            self.eat(b':')?;
            self.ws(); // "key": value —— 冒号后允许空白（pretty-printed 配置）
            let v = self.value(depth + 1)?;
            m.push((k, v));
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Json::Obj(m));
                }
                _ => return Err(format!("期望 ',' 或 '}}'（位置 {}）", self.i)),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Json, String> {
        self.i += 1; // '['
        let mut v = Vec::new();
        self.ws();
        if self.b.get(self.i) == Some(&b']') {
            self.i += 1;
            return Ok(Json::Arr(v));
        }
        loop {
            self.ws(); // 逗号/左括号后允许空白
            v.push(self.value(depth + 1)?);
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Json::Arr(v));
                }
                _ => return Err(format!("期望 ',' 或 ']'（位置 {}）", self.i)),
            }
        }
    }

    fn eat(&mut self, c: u8) -> Result<(), String> {
        if self.b.get(self.i) == Some(&c) {
            self.i += 1;
            Ok(())
        } else {
            Err(format!("期望 '{}'（位置 {}）", c as char, self.i))
        }
    }

    /// 解析字符串字面量（调用时已位于开引号）
    fn string(&mut self) -> Result<String, String> {
        self.i += 1; // '"'
        let mut out = String::new();
        loop {
            let Some(&c) = self.b.get(self.i) else {
                return Err("字符串未闭合".into());
            };
            match c {
                b'"' => {
                    self.i += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.i += 1;
                    let Some(&e) = self.b.get(self.i) else {
                        return Err("转义序列不完整".into());
                    };
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let ch = if (0xD800..0xDC00).contains(&hi) {
                                // 高代理项必须跟 \uXXXX 低代理项，合成码点
                                if self.b.get(self.i) == Some(&b'\\')
                                    && self.b.get(self.i + 1) == Some(&b'u')
                                {
                                    self.i += 2;
                                    let lo = self.hex4()?;
                                    if (0xDC00..0xE000).contains(&lo) {
                                        let cp = 0x10000
                                            + ((hi as u32 - 0xD800) << 10)
                                            + (lo as u32 - 0xDC00);
                                        char::from_u32(cp).unwrap_or('\u{FFFD}')
                                    } else {
                                        '\u{FFFD}'
                                    }
                                } else {
                                    '\u{FFFD}'
                                }
                            } else {
                                char::from_u32(hi as u32).unwrap_or('\u{FFFD}')
                            };
                            out.push(ch);
                        }
                        _ => return Err(format!("非法转义 '\\{}'（位置 {}）", e as char, self.i - 2)),
                    }
                }
                0x00..=0x1F => return Err("字符串含未转义控制字符".into()),
                _ => {
                    // 普通字符（含多字节 UTF-8）原样收集；只在 ASCII 特殊字符处断开
                    let start = self.i;
                    while let Some(&c) = self.b.get(self.i) {
                        if c < 0x20 || c == b'"' || c == b'\\' {
                            break;
                        }
                        self.i += 1;
                    }
                    let s = std::str::from_utf8(&self.b[start..self.i])
                        .map_err(|_| "非法 UTF-8".to_string())?;
                    out.push_str(s);
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u16, String> {
        if self.i + 4 > self.b.len() {
            return Err("\\u 转义不完整".into());
        }
        let s = std::str::from_utf8(&self.b[self.i..self.i + 4])
            .map_err(|_| "\\u 转义非法".to_string())?;
        let v = u16::from_str_radix(s, 16).map_err(|_| format!("非法 \\u 转义 '{s}'"))?;
        self.i += 4;
        Ok(v)
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.i;
        if self.b.get(self.i) == Some(&b'-') {
            self.i += 1;
        }
        while matches!(
            self.b.get(self.i),
            Some(c) if c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-')
        ) {
            self.i += 1;
        }
        if start == self.i {
            return Err(format!("非法数字（位置 {}）", start));
        }
        let s = std::str::from_utf8(&self.b[start..self.i]).map_err(|_| "非法数字".to_string())?;
        s.parse::<f64>()
            .map(Json::Num)
            .map_err(|_| format!("非法数字 '{s}'"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_roundtrip() {
        let src = r#"{"a":1,"b":[true,false,null,-2.5],"c":"中文\n\"引号\"","d":{"e":1e3}}"#;
        let v = parse(src).unwrap();
        assert_eq!(v.get("a").unwrap().as_f64(), Some(1.0));
        assert_eq!(v.get("b").unwrap().as_arr().unwrap().len(), 4);
        assert_eq!(v.get("c").unwrap().as_str().unwrap(), "中文\n\"引号\"");
        assert_eq!(v.get("d").unwrap().get("e").unwrap().as_f64(), Some(1000.0));
        // 序列化 → 再解析，保持等价
        let v2 = parse(&v.to_string()).unwrap();
        assert_eq!(v, v2);
    }

    #[test]
    fn test_surrogate_pair() {
        // 😀 = U+1F600：代理对转义合成一个码点（源码拼出转义文本，避免字面序列被改写）
        let bs: String = std::iter::once('\\').collect();
        let src = format!("\"{bs}ud83d{bs}ude00\"");
        assert_eq!(parse(&src).unwrap().as_str(), Some("\u{1F600}"));
        // 孤立高代理项 → 替换字符，不崩溃
        let lone = format!("\"{bs}ud83d\"");
        assert_eq!(parse(&lone).unwrap().as_str(), Some("\u{FFFD}"));
        // 原生 UTF-8 直传
        assert_eq!(parse(r#""😀""#).unwrap().as_str(), Some("\u{1F600}"));
    }

    #[test]
    fn test_parse_whitespace() {
        // pretty-printed JSON（冒号/逗号后有空格、多行）必须可解析
        let src = "{\n  \"a\": 1,\n  \"b\": [ 1, 2, 3 ],\n  \"c\": { \"d\": \"x\" }\n}";
        let v = parse(src).unwrap();
        assert_eq!(v.get("a").unwrap().as_f64(), Some(1.0));
        assert_eq!(v.get("b").unwrap().as_arr().unwrap().len(), 3);
    }

    #[test]
    fn test_parse_errors() {
        assert!(parse("{").is_err());
        assert!(parse("{} x").is_err());
        assert!(parse(r#""\q""#).is_err());
        assert!(parse("[1,]").is_err());
        assert!(parse("").is_err());
        assert!(parse(r#"{"a" 1}"#).is_err());
    }

    #[test]
    fn test_serialize() {
        assert_eq!(Json::Num(5.0).to_string(), "5");
        assert_eq!(Json::Num(-2.5).to_string(), "-2.5");
        assert_eq!(Json::s("a\"b").to_string(), "\"a\\\"b\"");
        assert_eq!(Json::s("x\u{1}y").to_string(), "\"x\\u0001y\"");
        assert_eq!(Json::obj(vec![]).to_string(), "{}");
        assert_eq!(Json::arr(vec![]).to_string(), "[]");
    }

    #[test]
    fn test_pretty() {
        let v = Json::obj(vec![
            ("a", Json::i(1)),
            ("b", Json::obj(vec![("c", Json::arr(vec![Json::s("x")]))])),
        ]);
        let pretty = v.to_string_pretty();
        assert_eq!(
            pretty,
            "{\n  \"a\": 1,\n  \"b\": {\n    \"c\": [\n      \"x\"\n    ]\n  }\n}"
        );
        // 内层空对象/数组保持紧凑，外层仍多行
        assert_eq!(
            Json::obj(vec![("e", Json::obj(vec![]))]).to_string_pretty(),
            "{\n  \"e\": {}\n}"
        );
        // pretty 输出可再解析
        assert_eq!(parse(&pretty).unwrap(), v);
    }
}
