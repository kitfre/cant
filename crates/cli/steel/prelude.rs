//! The bundled, effect-free interface layer. Install after registering `inputs`.
use crate::{
    bindings, json,
    wire::{SteelBinding, SteelKeyword},
};
use serde::Deserialize;
use std::collections::BTreeMap;
use steel::{gc::Gc, rvals::Result as SteelResult, steel_vm::engine::Engine, SteelVal};

fn key(value: &SteelVal) -> SteelResult<SteelVal> {
    match value {
        SteelVal::StringV(value) | SteelVal::SymbolV(value) => Ok(SteelVal::StringV(value.clone())),
        _ => Err(json::type_error("record key", "a string or symbol", value)),
    }
}
fn object(args: &[SteelVal]) -> SteelResult<SteelVal> {
    if args.len() % 2 != 0 {
        return Err(json::error("object expects key/value pairs"));
    }
    let mut map = steel::HashMap::new();
    for pair in args.chunks_exact(2) {
        let key = key(&pair[0])?;
        if map.contains_key(&key) {
            return Err(json::error(format!(
                "duplicate object key {}",
                json::inspect(&key)
            )));
        }
        map.insert(key, pair[1].clone());
    }
    Ok(SteelVal::HashMapV(Gc::new(map).into()))
}
// The macro only references private native procedures, never caller-visible list/apply/append.
fn object_fields(args: &[SteelVal]) -> SteelResult<SteelVal> {
    let mut pairs = Vec::new();
    for arg in args {
        let SteelVal::HashMapV(fields) = arg else {
            return Err(json::type_error("obj field", "a record (hash)", arg));
        };
        for (key, value) in fields.iter() {
            pairs.push(key.clone());
            pairs.push(value.clone());
        }
    }
    object(&pairs)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Constructor {
    name: String,
    positional: Vec<String>,
    fields: Vec<SteelKeyword>,
    fixed: serde_json::Map<String, serde_json::Value>,
}
fn install_constructors(engine: &mut Engine) -> SteelResult<()> {
    let constructors: Vec<Constructor> = serde_json::from_str(include_str!("constructors.json"))
        .map_err(|error| json::error(error.to_string()))?;
    for constructor in constructors {
        let binding = SteelBinding {
            path: vec![],
            name: constructor.name.clone(),
            mode: "object".into(),
            positional: constructor.positional,
            keywords: constructor.fields,
            signature_args: None,
        };
        crate::register(engine, &constructor.name, move |args| {
            let mut value = bindings::arguments(&binding, args, &BTreeMap::new())?;
            value
                .as_object_mut()
                .unwrap()
                .extend(constructor.fixed.clone());
            json::from_json(&value)
        });
    }
    Ok(())
}
fn get(args: &[SteelVal]) -> SteelResult<SteelVal> {
    if !(2..=3).contains(&args.len()) {
        return Err(json::arity_error("get", "2 or 3", args.len()));
    }
    let SteelVal::HashMapV(record) = &args[0] else {
        return Err(json::type_error("get", "a record (hash)", &args[0]));
    };
    let key = key(&args[1])?;
    record
        .get(&key)
        .or_else(|| args.get(2))
        .cloned()
        .ok_or_else(|| json::error(format!("missing record field {}", json::inspect(&key))))
}
fn value_to_string(args: &[SteelVal]) -> SteelResult<SteelVal> {
    let [value] = args else {
        return Err(json::arity_error("value->string", "1", args.len()));
    };
    Ok(SteelVal::StringV(json::inspect(value).into()))
}

// Steel 0.8.3 already supplies -> and ->>; don't replace standard macros.
// Keeping this source separate preserves user source offsets and prelude diagnostics.
const SOURCE: &str = r#"
(define-syntax obj
  (syntax-rules ()
    ((_ (key value) ...)
     (#%private/object-fields (#%private/object 'key value) ...))))

(define (input key) (get inputs key))
(define (replace-text old new) (object "oldText" old "newText" new))

(define-syntax let-fields
  (syntax-rules ()
    ((_ ([field ...] expression) body ...)
     (let ([record expression])
       (let ([field (get record 'field)] ...)
         body ...)))))
"#;

pub fn install(engine: &mut Engine) -> SteelResult<()> {
    engine.register_value("json-null", json::null());
    engine.register_value("object", SteelVal::FuncV(object));
    engine.register_value("#%private/object", SteelVal::FuncV(object));
    engine.register_value("#%private/object-fields", SteelVal::FuncV(object_fields));
    install_constructors(engine)?;
    engine.register_value("get", SteelVal::FuncV(get));
    // Steel's general printer can call custom printers; this boundary cannot.
    engine.register_value("value->string", SteelVal::FuncV(value_to_string));
    // A separate compilation gets its own SourceId without shifting user lines.
    // The with_path API canonicalizes a real file in Steel 0.8.3, so do not use it.
    engine.compile_and_run_raw_program(SOURCE)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> Engine {
        let mut engine = Engine::new_sandboxed();
        engine.register_value(
            "inputs",
            json::from_json(&serde_json::json!({"source": "literal"})).unwrap(),
        );
        install(&mut engine).unwrap();
        engine
    }
    fn eval(source: &str) -> serde_json::Value {
        let values = engine()
            .compile_and_run_raw_program(source.to_owned())
            .unwrap();
        json::to_json(values.last().unwrap()).unwrap()
    }

    #[test]
    fn records_and_macros() {
        assert_eq!(
            eval("(list (obj) (obj (maxOutputBytes 42)) (replace-text (input 'source) \"new\"))"),
            serde_json::json!([{}, {"maxOutputBytes":42}, {"oldText":"literal", "newText":"new"}])
        );
        assert_eq!(eval("(let ([get (lambda args #f)] [object (lambda args #f)] [record 99]) (let-fields ([a] (obj (a 42))) (list a record)))"), serde_json::json!([42,99]));
        assert_eq!(
            eval("(list (-> 3 (+ 2) (* 4)) (->> '(1 2) (map (lambda (x) (+ x 1))) (apply +)))"),
            serde_json::json!([20, 5])
        );
    }
    #[test]
    fn values_evaluate_once_in_order_and_inspection_handles_guest_cycles() {
        assert_eq!(
            eval(
                r#"
            (define n 0)
            (define made (obj (a (begin (set! n (+ n 1)) n)) (b (begin (set! n (+ n 1)) n))))
            (let-fields ([a b] (begin (set! n (+ n 1)) made)) (list a b n))
        "#
            ),
            serde_json::json!([1, 2, 3])
        );
        assert_eq!(eval("(let-fields ([] (obj)) 42)"), serde_json::json!(42));
        assert_eq!(
            eval("(define cycle (box #f)) (set-box! cycle cycle) (value->string cycle)"),
            serde_json::json!("#<box>")
        );
        assert_eq!(
            eval("(list (value->string +inf.0) (value->string +nan.0) (value->string void))"),
            serde_json::json!(["inf", "NaN", "#<void>"])
        );
        assert_eq!(
            eval("(struct record (text)) (value->string (record 'hello))"),
            serde_json::json!("#<struct record hello>")
        );
    }

    #[test]
    fn helper_errors_report_types_and_counts_without_dumping_values() {
        for (source, kind, message) in [
            (r#"(get "private text" 'field)"#, steel::rerrs::ErrorKind::TypeMismatch, "get expected a record (hash), received string"),
            ("(get (obj) 42)", steel::rerrs::ErrorKind::TypeMismatch, "record key expected a string or symbol, received number"),
            ("(get (obj))", steel::rerrs::ErrorKind::ArityMismatch, "get expected 2 or 3 arguments, received 1"),
        ] {
            let error = engine().compile_and_run_raw_program(source).unwrap_err();
            assert_eq!(error.kind(), kind);
            assert!(error.to_string().contains(message));
            assert!(!error.to_string().contains("private text"));
        }
    }

    #[test]
    fn null_and_missing() {
        assert_eq!(eval("(list (void? json-null) (equal? json-null json-null) (get (obj (nil json-null) (no #f)) 'nil 42) (get (obj (no #f)) 'no 42) (get (obj) 'absent 42))"), serde_json::json!([false,true,null,false,42]));
        for source in [
            "(obj (a 1) (a 2))",
            "(get (obj) 'absent)",
            "(let-fields ([absent] (obj)) absent)",
        ] {
            assert!(
                engine().compile_and_run_raw_program(source).is_err(),
                "{source}"
            );
        }
    }
}
