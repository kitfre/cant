use serde_json::{Map, Number, Value};
use steel::{
    gc::{Gc, ShareableMut},
    rerrs::{ErrorKind, SteelErr},
    rvals::{Custom, CustomType, Result},
    SteelVal,
};

const SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
const MAX_NODES: usize = 65_536;

// Opaque to Scheme: unlike void, this value may cross every JSON boundary.
#[derive(Debug)]
struct JsonNull;
impl Custom for JsonNull {
    fn equality_hint(&self, other: &dyn CustomType) -> bool {
        other.as_any_ref().is::<Self>()
    }
}
pub fn null() -> SteelVal {
    SteelVal::Custom(Gc::new_mut(Box::new(JsonNull)))
}
fn is_null(value: &SteelVal) -> bool {
    matches!(value, SteelVal::Custom(value) if value.try_read().ok().is_some_and(|value| value.as_any_ref().is::<JsonNull>()))
}
pub fn error(message: impl Into<String>) -> SteelErr {
    SteelErr::new(ErrorKind::Generic, message.into())
}
pub fn type_error(context: &str, expected: &str, value: &SteelVal) -> SteelErr {
    SteelErr::new(
        ErrorKind::TypeMismatch,
        format!("{context} expected {expected}, received {}", type_name(value)),
    )
}
pub fn arity_error(name: &str, expected: &str, received: usize) -> SteelErr {
    let suffix = if expected == "1" { "" } else { "s" };
    SteelErr::new(
        ErrorKind::ArityMismatch,
        format!("{name} expected {expected} argument{suffix}, received {received}"),
    )
}
pub fn argument_error(message: impl Into<String>) -> SteelErr {
    SteelErr::new(ErrorKind::ContractViolation, message.into())
}
pub fn type_name(value: &SteelVal) -> &'static str {
    match value {
        SteelVal::Void => "void",
        SteelVal::BoolV(_) => "boolean",
        SteelVal::IntV(_) | SteelVal::NumV(_) | SteelVal::BigNum(_) => "number",
        SteelVal::StringV(_) => "string",
        SteelVal::SymbolV(_) => "symbol",
        SteelVal::CharV(_) => "character",
        SteelVal::ListV(_) => "list",
        SteelVal::VectorV(_) => "vector",
        SteelVal::HashMapV(_) => "hash",
        SteelVal::Pair(_) => "pair",
        SteelVal::Boxed(_) | SteelVal::HeapAllocated(_) => "box",
        SteelVal::MutableVector(_) => "mutable-vector",
        SteelVal::Closure(_)
        | SteelVal::FuncV(_)
        | SteelVal::BoxedFunction(_)
        | SteelVal::MutFunc(_)
        | SteelVal::BuiltIn(_)
        | SteelVal::FutureFunc(_)
        | SteelVal::ContinuationFunction(_) => "procedure",
        SteelVal::CustomStruct(_) => "structure",
        SteelVal::SyntaxObject(_) => "syntax",
        SteelVal::Custom(_) => "custom",
        _ => "opaque",
    }
}

fn at(path: &str, message: &str) -> SteelErr {
    error(format!("{path} {message}"))
}
fn field_path(path: &str, key: &str) -> String {
    if !key.is_empty()
        && key
            .bytes()
            .enumerate()
            .all(|(i, b)| b.is_ascii_alphabetic() || b == b'_' || (i > 0 && b.is_ascii_digit()))
    {
        format!("{path}.{key}")
    } else {
        format!("{path}[{}]", serde_json::to_string(key).unwrap())
    }
}
fn budget(depth: usize, remaining: &mut usize, path: &str) -> Result<()> {
    if depth > 64 || *remaining == 0 {
        return Err(at(path, "JSON value exceeds depth/node limits"));
    }
    *remaining -= 1;
    Ok(())
}
pub fn from_json(value: &Value) -> Result<SteelVal> {
    fn convert(value: &Value, depth: usize, left: &mut usize, path: &str) -> Result<SteelVal> {
        budget(depth, left, path)?;
        Ok(match value {
            Value::Null => null(),
            Value::Bool(value) => SteelVal::BoolV(*value),
            Value::String(value) => SteelVal::StringV(value.clone().into()),
            Value::Number(value) => {
                let number = value
                    .as_f64()
                    .ok_or_else(|| at(path, "contains an invalid JSON number"))?;
                if !number.is_finite() || (number.fract() == 0.0 && number.abs() > SAFE_INTEGER) {
                    return Err(at(path, "JSON integers must be JavaScript-safe"));
                }
                if let Some(integer) = value.as_i64() {
                    SteelVal::IntV(integer as isize)
                } else {
                    SteelVal::NumV(number)
                }
            }
            Value::Array(values) => SteelVal::ListV(
                values
                    .iter()
                    .enumerate()
                    .map(|(i, value)| convert(value, depth + 1, left, &format!("{path}[{i}]")))
                    .collect::<Result<Vec<_>>>()?
                    .into(),
            ),
            Value::Object(values) => {
                let mut map = steel::HashMap::new();
                for (key, value) in values {
                    map.insert(
                        SteelVal::StringV(key.clone().into()),
                        convert(value, depth + 1, left, &field_path(path, key))?,
                    );
                }
                SteelVal::HashMapV(Gc::new(map).into())
            }
        })
    }
    let mut remaining = MAX_NODES;
    convert(value, 0, &mut remaining, "value")
}
pub fn to_json(value: &SteelVal) -> Result<Value> {
    to_json_at(value, "result")
}
pub fn to_json_at(value: &SteelVal, root: &str) -> Result<Value> {
    fn convert(value: &SteelVal, depth: usize, left: &mut usize, path: &str) -> Result<Value> {
        budget(depth, left, path)?;
        Ok(match value {
            value if is_null(value) => Value::Null,
            SteelVal::Void => return Err(at(path, "contains void; use json-null for JSON null")),
            SteelVal::BoolV(value) => Value::Bool(*value),
            SteelVal::StringV(value) => Value::String(value.to_string()),
            SteelVal::IntV(value) if (*value as f64).abs() <= SAFE_INTEGER => {
                Value::Number(Number::from(*value as i64))
            }
            SteelVal::NumV(value)
                if value.is_finite() && (value.fract() != 0.0 || value.abs() <= SAFE_INTEGER) =>
            {
                Value::Number(Number::from_f64(*value).unwrap())
            }
            SteelVal::IntV(_) | SteelVal::NumV(_) | SteelVal::BigNum(_) => {
                return Err(at(
                    path,
                    "contains a non-finite or non-JavaScript-safe JSON number",
                ))
            }
            SteelVal::ListV(values) => Value::Array(
                values
                    .iter()
                    .enumerate()
                    .map(|(i, value)| convert(value, depth + 1, left, &format!("{path}[{i}]")))
                    .collect::<Result<Vec<_>>>()?,
            ),
            SteelVal::VectorV(values) => Value::Array(
                values
                    .iter()
                    .enumerate()
                    .map(|(i, value)| convert(value, depth + 1, left, &format!("{path}[{i}]")))
                    .collect::<Result<Vec<_>>>()?,
            ),
            SteelVal::HashMapV(values) => {
                let mut object = Map::new();
                for (key, value) in values.iter() {
                    let SteelVal::StringV(key) = key else {
                        return Err(at(path, "contains a non-string JSON object key"));
                    };
                    object.insert(
                        key.to_string(),
                        convert(value, depth + 1, left, &field_path(path, key))?,
                    );
                }
                Value::Object(object)
            }
            value => {
                return Err(at(
                    path,
                    &format!(
                    "contains {}; value is not JSON-safe (use a string for JSON tags/enum values)",
                    kind(value)
                ),
                ))
            }
        })
    }
    let mut remaining = MAX_NODES;
    convert(value, 0, &mut remaining, root)
}

fn kind(value: &SteelVal) -> &'static str {
    match value {
        SteelVal::Closure(_)
        | SteelVal::FuncV(_)
        | SteelVal::BoxedFunction(_)
        | SteelVal::MutFunc(_)
        | SteelVal::BuiltIn(_)
        | SteelVal::FutureFunc(_)
        | SteelVal::ContinuationFunction(_) => "a procedure",
        SteelVal::SymbolV(_) => "a symbol",
        SteelVal::CustomStruct(_) => "a structure",
        SteelVal::SyntaxObject(_) => "syntax",
        SteelVal::Custom(_) => "a custom value",
        _ => "an unsupported Scheme value",
    }
}

/// Bounded diagnostic rendering, never SteelVal's Display/Debug or custom printers.
/// Mutable/opaque values are deliberately not traversed, so cycles are harmless.
/// Immutable containers are limited to depth 16, 1024 nodes, and 8192 UTF-8 bytes.
pub fn inspect(value: &SteelVal) -> String {
    render(value, 8192, 1024, false).text
}

pub struct Inspection {
    pub text: String,
    pub truncated: bool,
}

/// The actual REPL value, not a lossy JSON round-trip. Same passive traversal as inspect.
pub fn repl_display(value: &SteelVal) -> Inspection {
    render(value, 32 * 1024, 4096, true)
}

fn render(value: &SteelVal, max_bytes: usize, max_nodes: usize, pretty: bool) -> Inspection {
    struct Printer {
        text: String,
        nodes: usize,
        max_bytes: usize,
        pretty: bool,
        truncated: bool,
    }
    impl Printer {
        fn push(&mut self, text: &str) {
            let available = self
                .max_bytes
                .saturating_sub(3)
                .saturating_sub(self.text.len());
            let mut end = text.len().min(available);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            self.text.push_str(&text[..end]);
            if end < text.len() {
                self.truncated = true;
            }
        }
        fn full(&self) -> bool {
            self.truncated || self.nodes == 0
        }
        fn value(&mut self, value: &SteelVal, depth: usize) {
            if self.full() || depth > 16 {
                self.truncated = true;
                return;
            }
            self.nodes -= 1;
            match value {
                value if is_null(value) => self.push("json-null"),
                SteelVal::Void => self.push("#<void>"),
                SteelVal::BoolV(value) => self.push(if *value { "#t" } else { "#f" }),
                SteelVal::IntV(value) => self.push(&value.to_string()),
                SteelVal::NumV(value) => self.push(&value.to_string()),
                SteelVal::CharV(value) => {
                    self.push("#\\");
                    self.push(value.encode_utf8(&mut [0; 4]));
                }
                SteelVal::SymbolV(value) => self.push(value),
                SteelVal::StringV(value) if self.pretty && depth == 0 => {
                    // Display returned text directly; nested strings still use write notation.
                    for ch in value.chars() {
                        if self.truncated {
                            break;
                        }
                        match ch {
                            ch if ch.is_control() && !matches!(ch, '\n' | '\t') => {
                                self.push(&format!("\\u{{{:x}}}", ch as u32))
                            }
                            ch => self.push(ch.encode_utf8(&mut [0; 4])),
                        }
                    }
                }
                SteelVal::StringV(value) => {
                    self.push("\"");
                    for ch in value.chars() {
                        if self.truncated {
                            break;
                        }
                        match ch {
                            '\n' => self.push("\\n"),
                            '\r' => self.push("\\r"),
                            '\t' => self.push("\\t"),
                            '"' => self.push("\\\""),
                            '\\' => self.push("\\\\"),
                            ch if ch.is_control() => self.push(&format!("\\u{{{:x}}}", ch as u32)),
                            ch => self.push(ch.encode_utf8(&mut [0; 4])),
                        }
                    }
                    self.push("\"");
                }
                SteelVal::ListV(values) => self.sequence(values.iter(), "(", ")", depth),
                SteelVal::VectorV(values) => self.sequence(values.iter(), "#(", ")", depth),
                SteelVal::HashMapV(values) => {
                    let multiline = self.pretty && values.len() > 2;
                    self.push("(hash");
                    for (key, value) in values.iter() {
                        if self.full() {
                            self.truncated = true;
                            break;
                        }
                        self.separator(multiline, depth + 1);
                        self.value(key, depth + 1);
                        self.push(" ");
                        self.value(value, depth + 1);
                    }
                    if multiline {
                        self.separator(true, depth);
                    }
                    self.push(")");
                }
                SteelVal::Pair(pair) => {
                    self.push("(");
                    self.value(pair.car_ref(), depth + 1);
                    self.push(" . ");
                    self.value(pair.cdr_ref(), depth + 1);
                    self.push(")");
                }
                SteelVal::CustomStruct(value) => {
                    self.push("#<struct ");
                    self.push(value.name().resolve());
                    // Access fields directly, not the optional Scheme custom printer.
                    let mut index = 0;
                    while let Some(field) = value.get_index(index) {
                        if self.full() {
                            self.truncated = true;
                            break;
                        }
                        self.push(" ");
                        self.value(field, depth + 1);
                        index += 1;
                    }
                    self.push(">");
                }
                SteelVal::Boxed(_) | SteelVal::HeapAllocated(_) => self.push("#<box>"),
                SteelVal::MutableVector(_) => self.push("#<mutable-vector>"),
                value => {
                    self.push("#<");
                    self.push(kind(value).strip_prefix("a ").unwrap_or(kind(value)));
                    self.push(">");
                }
            }
        }
        fn separator(&mut self, multiline: bool, depth: usize) {
            if multiline {
                self.push("\n");
                self.push(&"  ".repeat(depth));
            } else {
                self.push(" ");
            }
        }
        fn sequence<'a>(
            &mut self,
            values: impl Iterator<Item = &'a SteelVal>,
            open: &str,
            close: &str,
            depth: usize,
        ) {
            let multiline = self.pretty && values.size_hint().0 > 6;
            self.push(open);
            for (i, value) in values.enumerate() {
                if self.full() {
                    self.truncated = true;
                    break;
                }
                if i > 0 || multiline {
                    self.separator(multiline, depth + 1);
                }
                self.value(value, depth + 1);
            }
            if multiline {
                self.separator(true, depth);
            }
            self.push(close);
        }
    }
    let mut printer = Printer {
        text: String::new(),
        nodes: max_nodes,
        max_bytes,
        pretty,
        truncated: false,
    };
    printer.value(value, 0);
    if printer.truncated {
        printer.text.push_str("...");
    }
    Inspection {
        text: printer.text,
        truncated: printer.truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspection_never_calls_custom_printers() {
        #[derive(Debug)]
        struct Dangerous;
        impl Custom for Dangerous {
            fn fmt(&self) -> Option<std::result::Result<String, std::fmt::Error>> {
                panic!("inspection called a custom printer")
            }
        }
        let value = SteelVal::Custom(Gc::new_mut(Box::new(Dangerous)));
        assert!(inspect(&value).contains("custom"));
        assert!(to_json(&value).is_err());
    }

    #[test]
    fn inspection_bounds_bytes_depth_nodes_and_accepts_nonfinite() {
        let text = inspect(&SteelVal::StringV("λ".repeat(10000).into()));
        assert!(text.len() <= 8192 && text.ends_with("..."));
        let wide = SteelVal::ListV(vec![SteelVal::BoolV(false); 2000].into());
        assert!(inspect(&wide).ends_with("..."));
        let mut deep = SteelVal::IntV(1);
        for _ in 0..100 {
            deep = SteelVal::ListV(vec![deep].into());
        }
        assert!(inspect(&deep).ends_with("..."));
        assert!(inspect(&SteelVal::NumV(f64::INFINITY)).contains("inf"));
        assert!(inspect(&SteelVal::NumV(f64::NAN)).contains("NaN"));
        let boxed = Gc::new_mut(SteelVal::Void);
        let value = SteelVal::Boxed(boxed.clone());
        *boxed.write() = value.clone();
        assert_eq!(inspect(&value), "#<box>");
        *boxed.write() = SteelVal::Void;
    }

    #[test]
    fn null_is_not_void_and_round_trips() {
        let value = from_json(&Value::Null).unwrap();
        assert!(!matches!(value, SteelVal::Void));
        assert_eq!(to_json(&value).unwrap(), Value::Null);
        assert_eq!(value, null());
        assert_ne!(value, SteelVal::Void);
    }

    #[test]
    fn void_and_procedures_report_the_value_path() {
        let mut map = steel::HashMap::new();
        map.insert(
            SteelVal::StringV("items".into()),
            SteelVal::ListV(vec![SteelVal::IntV(1), SteelVal::Void].into()),
        );
        let err = to_json(&SteelVal::HashMapV(Gc::new(map).into()))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("result.items[1]") && err.contains("void"),
            "{err}"
        );
        assert!(to_json(&SteelVal::Void).is_err());
    }

    #[test]
    fn unsafe_numbers_have_paths() {
        let err = from_json(&serde_json::json!({"n": 9007199254740992_u64}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("value.n"), "{err}");
    }
}
