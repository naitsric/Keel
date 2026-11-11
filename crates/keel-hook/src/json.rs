//! Just enough JSON for the hook: reading the event Claude Code sends, and quoting the note.

pub enum Value {
    Null,
    Bool(bool),
    Number,
    String(String),
    Array,
    Object(Vec<(String, Value)>),
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Self::Object(fields) => fields
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }
}

pub fn parse(text: &str) -> Option<Value> {
    let mut reader = Reader {
        chars: text.chars().collect(),
        pos: 0,
    };
    let value = reader.value(0)?;
    reader.space();
    (reader.pos == reader.chars.len()).then_some(value)
}

/// `text` as a JSON string.
pub fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

struct Reader {
    chars: Vec<char>,
    pos: usize,
}

impl Reader {
    fn space(&mut self) {
        while self
            .chars
            .get(self.pos)
            .is_some_and(|c| matches!(c, ' ' | '\t' | '\n' | '\r'))
        {
            self.pos += 1;
        }
    }

    fn next(&mut self) -> Option<char> {
        let c = *self.chars.get(self.pos)?;
        self.pos += 1;
        Some(c)
    }

    fn word(&mut self, word: &str) -> Option<()> {
        for expected in word.chars() {
            (self.next()? == expected).then_some(())?;
        }
        Some(())
    }

    fn value(&mut self, depth: usize) -> Option<Value> {
        if depth > 64 {
            return None;
        }
        self.space();
        match *self.chars.get(self.pos)? {
            '{' => {
                self.pos += 1;
                let mut fields = Vec::new();
                self.space();
                if self.chars.get(self.pos) == Some(&'}') {
                    self.pos += 1;
                    return Some(Value::Object(fields));
                }
                loop {
                    self.space();
                    (self.next()? == '"').then_some(())?;
                    let name = self.string()?;
                    self.space();
                    (self.next()? == ':').then_some(())?;
                    fields.push((name, self.value(depth + 1)?));
                    self.space();
                    match self.next()? {
                        ',' => {}
                        '}' => return Some(Value::Object(fields)),
                        _ => return None,
                    }
                }
            }
            '[' => {
                self.pos += 1;
                self.space();
                if self.chars.get(self.pos) == Some(&']') {
                    self.pos += 1;
                    return Some(Value::Array);
                }
                loop {
                    self.value(depth + 1)?;
                    self.space();
                    match self.next()? {
                        ',' => {}
                        ']' => return Some(Value::Array),
                        _ => return None,
                    }
                }
            }
            '"' => {
                self.pos += 1;
                self.string().map(Value::String)
            }
            't' => self.word("true").map(|()| Value::Bool(true)),
            'f' => self.word("false").map(|()| Value::Bool(false)),
            'n' => self.word("null").map(|()| Value::Null),
            c if c == '-' || c.is_ascii_digit() => {
                while self
                    .chars
                    .get(self.pos)
                    .is_some_and(|c| c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E'))
                {
                    self.pos += 1;
                }
                Some(Value::Number)
            }
            _ => None,
        }
    }

    /// After the opening quote.
    fn string(&mut self) -> Option<String> {
        let mut out = String::new();
        loop {
            match self.next()? {
                '"' => return Some(out),
                '\\' => match self.next()? {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    'b' => out.push('\u{8}'),
                    'f' => out.push('\u{c}'),
                    'u' => {
                        let high = self.hex()?;
                        let code = if (0xD800..0xDC00).contains(&high) {
                            self.word("\\u")?;
                            let low = self.hex()?;
                            0x10000 + ((high - 0xD800) << 10) + (low.checked_sub(0xDC00)? & 0x3FF)
                        } else {
                            high
                        };
                        out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                    }
                    other => out.push(other),
                },
                other => out.push(other),
            }
        }
    }

    fn hex(&mut self) -> Option<u32> {
        let mut code = 0;
        for _ in 0..4 {
            code = code * 16 + self.next()?.to_digit(16)?;
        }
        Some(code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_an_event_with_escapes_and_nesting() {
        let event = parse(
            r#"{"tool_input":{"command":"cat <<'EOF'\n\"hi\" é 😀\nEOF","n":[1,-2.5e3,true,null,{}]},"cwd":"/w"}"#,
        )
        .expect("valid");
        assert_eq!(
            event
                .get("tool_input")
                .and_then(|i| i.get("command"))
                .and_then(Value::as_str),
            Some("cat <<'EOF'\n\"hi\" é 😀\nEOF")
        );
        assert_eq!(event.get("cwd").and_then(Value::as_str), Some("/w"));
    }

    #[test]
    fn refuses_what_is_not_json() {
        assert!(parse("not json").is_none());
        assert!(parse(r#"{"a":"b""#).is_none());
        assert!(parse(r#"{"a":1} trailing"#).is_none());
    }

    #[test]
    fn quoting_reads_back_the_same() {
        let text = "a \"b\" \\ c\nd\te\u{1}";
        let back = parse(&quote(text)).expect("valid");
        assert_eq!(back.as_str(), Some(text));
    }
}
