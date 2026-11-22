//! REPL regression tests: metadata requests must never enter the evaluator.
use super::*;
use crate::wire::{Bindings, ReplRequest};

fn fixture() -> Workspace {
    let input = File::open("/dev/null").unwrap();
    let output = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/null")
        .unwrap();
    let bridge = Arc::new(Mutex::new(Bridge::new(input, output)));
    Workspace::new(
        Engine::new_sandboxed(),
        bridge,
        json!({"text":"first"}),
        serde_json::from_value::<Bindings>(json!({"apiPaths":[]})).unwrap(),
    )
    .unwrap()
}
fn submit(w: &mut Workspace, request: Value, source: &str, inputs: Value) -> SteelResult<Value> {
    w.execute(
        &serde_json::from_value::<ReplRequest>(request).unwrap(),
        source,
        inputs,
    )
    .unwrap()
}
fn eval(w: &mut Workspace, source: &str) -> SteelResult<Value> {
    submit(w, json!({"op":"eval"}), source, json!({"text":"first"}))
}
fn metadata(w: &mut Workspace, name: &str) -> Value {
    submit(
        w,
        json!({"op":"inspect","name":name}),
        "(error \"must not run\")",
        json!({}),
    )
    .unwrap()
}

#[test]
fn repl_string_escape_hint_preserves_errors_and_literal_input_recovery() {
    let mut w = fixture();
    let error = eval(&mut w, r#""\s""#).unwrap_err();
    let message = crate::diagnostics::render(&w.engine, error, &[]).message;
    assert!(message.contains("invalid escape 's'"));
    assert!(message.contains("(system/exec \"rg\" (list \"-n\" (input 'pattern) \"src\"))"));
    let recovered = submit(
        &mut w,
        json!({"op":"eval"}),
        "(input 'pattern)",
        json!({"pattern":r"\s"}),
    )
    .unwrap();
    assert_eq!(recovered["preview"], r"\s");
    let unrelated = eval(&mut w, "missing-diagnostic-binding").unwrap_err();
    assert!(!crate::diagnostics::render(&w.engine, unrelated, &[]).message.contains("Hint:"));
}

#[test]
fn persistence_native_redefinition_and_error_recovery() {
    let mut w = fixture();
    eval(&mut w, "(define repl_x 10) (define (repl_read) repl_x) (define repl_b (box 1)) (define-syntax repl_twice (syntax-rules () ((_ x) (+ x x))))").unwrap();
    assert_eq!(
        eval(
            &mut w,
            "(define repl_x 20) (list repl_x (repl_read) (repl_twice 3))"
        )
        .unwrap(),
        json!({"kind":"scheme","preview":"(20 10 6)","type":"list","truncated":false})
    );
    assert!(eval(
        &mut w,
        "(set-box! repl_b 9) (error \"failed\") (define repl_uninitialized 5)"
    )
    .is_err());
    assert_eq!(eval(&mut w, "(unbox repl_b)").unwrap()["preview"], "9");
    assert_eq!(metadata(&mut w, "repl_uninitialized")["initialized"], false);
    assert!(eval(
        &mut w,
        "(define-syntax repl_partial (syntax-rules () ((_ x) x))) repl_missing"
    )
    .is_err());
    assert_eq!(eval(&mut w, "(repl_partial 42)").unwrap()["preview"], "42");
}

#[test]
fn saved_input_helpers_consult_current_host_slot_even_after_global_shadowing() {
    let mut w = fixture();
    eval(&mut w, "(define repl_saved_input input) (define (repl_reader) (input 'text)) (define inputs (obj (text \"fake\")))").unwrap();
    let result = submit(
        &mut w,
        json!({"op":"eval"}),
        "(list (input 'text) (repl_saved_input 'text) (repl_reader) (get inputs 'text))",
        json!({"text":"second"}),
    )
    .unwrap();
    assert_eq!(
        result["preview"],
        "(\"second\" \"second\" \"second\" \"second\")"
    );
    assert!(submit(&mut w, json!({"op":"eval"}), "(repl_reader)", json!({})).is_err());
}

#[test]
fn non_json_and_oversized_results_are_successful_bounded_scheme_values() {
    let mut w = fixture();
    for (source, kind) in [
        ("(lambda (x) x)", "procedure"),
        ("void", "void"),
        ("'hello", "symbol"),
        ("(list void)", "list"),
        ("+inf.0", "number"),
    ] {
        let value = eval(&mut w, source).unwrap();
        assert_eq!(value["kind"], "scheme", "{source}");
        assert_eq!(value["type"], kind);
        assert!(value["preview"].as_str().unwrap().len() <= 32768);
    }
    let value = eval(&mut w, "(make-string 300000 #\\x)").unwrap();
    assert_eq!(value["kind"], "scheme");
    assert!(value["preview"].as_str().unwrap().len() <= 32768);
    assert_eq!(
        eval(&mut w, "json-null").unwrap(),
        json!({"kind":"scheme","preview":"json-null","type":"custom","truncated":false})
    );
}

#[test]
fn passive_metadata_reports_arity_sources_docs_and_shadows() {
    let mut w = fixture();
    eval(&mut w, "(define repl_f (let ((b (box 1))) (lambda (x . rest) (list b x rest)))) (define (value->string x) (error \"printer ran\")) (define-syntax repl_macro (syntax-rules () ((_ x) x)))").unwrap();
    let value = metadata(&mut w, "repl_f");
    assert_eq!(value["arity"], json!({"minimum":1,"variadic":true}));
    assert_eq!(value["captureCount"], 1);
    let source = submit(
        &mut w,
        json!({"op":"describe","name":"repl_macro"}),
        "",
        json!({}),
    )
    .unwrap();
    assert!(source["source"]["text"]
        .as_str()
        .unwrap()
        .contains("define-syntax repl_macro"));
    let docs = submit(
        &mut w,
        json!({"op":"describe","name":"split-many"}),
        "",
        json!({}),
    )
    .unwrap();
    assert!(docs["documentation"]["text"]
        .as_str()
        .is_some_and(|text| !text.is_empty()));
    eval(&mut w, "(define split-many (lambda (x) x))").unwrap();
    let shadow = submit(
        &mut w,
        json!({"op":"describe","name":"split-many"}),
        "",
        json!({}),
    )
    .unwrap();
    assert_eq!(shadow["origin"], "user");
    assert_eq!(shadow["shadowsBuiltin"], true);
    assert!(shadow["documentation"].is_null());
    assert_eq!(metadata(&mut w, "unknown-name")["initialized"], false);
}

#[test]
fn inventory_is_sorted_paginated_bounded_and_user_default() {
    let mut w = fixture();
    eval(&mut w, "(define repl_z 1) (define repl_a 2)").unwrap();
    let inventory = submit(&mut w, json!({"op":"inspect"}), "", json!({})).unwrap();
    let names: Vec<_> = inventory["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"repl_a") && names.contains(&"repl_z"));
    assert!(!names.contains(&"map"));
    assert!(names.windows(2).all(|pair| pair[0] <= pair[1]));
    let page = submit(&mut w, json!({"op":"search","limit":999}), "", json!({})).unwrap();
    assert_eq!(page["items"].as_array().unwrap().len(), 50);
    assert_eq!(page["nextOffset"], 50);
    assert_eq!(page["hasMore"], true);
    let empty = submit(
        &mut w,
        json!({"op":"search","offset":999999}),
        "",
        json!({}),
    )
    .unwrap();
    assert_eq!(empty["items"], json!([]));
    assert_eq!(empty["nextOffset"], Value::Null);
    let found = submit(
        &mut w,
        json!({"op":"search","query":"repl_a"}),
        "",
        json!({}),
    )
    .unwrap();
    assert_eq!(found["total"], 1);
}

#[test]
fn lifetime_budgets_are_fatal_not_scheme_errors() {
    let mut w = fixture();
    let request: ReplRequest = serde_json::from_value(json!({"op":"eval"})).unwrap();
    w.evaluations = MAX_EVALUATIONS;
    assert!(w.execute(&request, "42", json!({})).is_err());
    w.evaluations = 0;
    w.submitted_bytes = MAX_SOURCES;
    assert!(w.execute(&request, "42", json!({})).is_err());
}

#[test]
fn initial_inventory_is_empty_and_equal_valued_redefinitions_are_user_bindings() {
    let mut w = fixture();
    let initial = submit(&mut w, json!({"op":"inspect"}), "", json!({})).unwrap();
    assert_eq!(initial["total"], 0, "{initial}");
    eval(&mut w, "(define code-mode-interface-version 3)").unwrap();
    let value = metadata(&mut w, "code-mode-interface-version");
    assert_eq!(value["origin"], "user");
    assert_eq!(value["shadowsBuiltin"], true);
}

#[test]
fn saved_host_callbacks_route_current_submission_and_error_table_is_fresh() {
    use std::{
        io::{BufRead, BufReader, Write},
        os::unix::net::UnixStream,
    };
    let (guest, mut host) = UnixStream::pair().unwrap();
    let input = File::from(std::os::fd::OwnedFd::from(guest.try_clone().unwrap()));
    let output = File::from(std::os::fd::OwnedFd::from(guest));
    let bridge = Arc::new(Mutex::new(Bridge::new(input, output)));
    bridge
        .lock()
        .unwrap()
        .begin_submission("first".into());
    let mut w = Workspace::new(
        Engine::new_sandboxed(),
        bridge.clone(),
        json!({}),
        serde_json::from_value(json!({"apiPaths":[["probe","call"]]})).unwrap(),
    )
    .unwrap();
    let worker = std::thread::spawn(move || {
        let mut reader = BufReader::new(host.try_clone().unwrap());
        for run in ["first", "second"] {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let message: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(message["runId"], run);
            assert_eq!(message["callId"], "call_1");
            let reply = json!({"type":"tool_result","runId":run,"callId":"call_1","ok":false,"error":{"name":"ProbeError","message":run}});
            writeln!(host, "{reply}").unwrap();
        }
    });
    let value = eval(&mut w, "(define repl_host tools.probe.call) (define (repl_caller) (repl_host (obj))) (with-handler error-info (repl_caller))").unwrap();
    assert!(value["preview"]
        .as_str()
        .unwrap()
        .contains("\"message\" \"first\""));
    bridge
        .lock()
        .unwrap()
        .begin_submission("second".into());
    assert!(bridge.lock().unwrap().host_errors.is_empty());
    let value = eval(&mut w, "(with-handler error-info (repl_caller))").unwrap();
    assert!(value["preview"]
        .as_str()
        .unwrap()
        .contains("\"message\" \"second\""));
    worker.join().unwrap();
}

#[test]
fn runtime_documentation_fills_module_gaps_and_tracks_alias_identity() {
    let mut w = fixture();
    let docs = submit(&mut w, json!({"op":"describe","name":"map"}), "", json!({})).unwrap();
    assert!(docs["documentation"]["text"]
        .as_str()
        .is_some_and(|text| text.contains("map")));
    eval(&mut w, "(define saved-map map)").unwrap();
    eval(&mut w, "(define map (lambda (x) x))").unwrap();
    let alias = submit(
        &mut w,
        json!({"op":"describe","name":"saved-map"}),
        "",
        json!({}),
    )
    .unwrap();
    assert!(alias["documentation"]["text"]
        .as_str()
        .is_some_and(|text| text.contains("map")));
    let shadow = submit(&mut w, json!({"op":"describe","name":"map"}), "", json!({})).unwrap();
    assert!(shadow["documentation"].is_null());
}

#[test]
fn native_function_metadata_describes_arity_and_alias_docs_without_execution() {
    let mut w = fixture();
    let native = submit(
        &mut w,
        json!({"op":"describe","name":"split-many"}),
        "",
        json!({}),
    )
    .unwrap();
    assert_eq!(
        native["arity"],
        json!({"minimum":2,"maximum":2,"variadic":false})
    );
    eval(&mut w, "(define saved-split split-many)").unwrap();
    let alias = submit(
        &mut w,
        json!({"op":"describe","name":"saved-split"}),
        "",
        json!({}),
    )
    .unwrap();
    assert_eq!(alias["arity"], native["arity"]);
    assert!(alias["documentation"]["text"]
        .as_str()
        .is_some_and(|text| text.contains("split-many")));
}

#[test]
fn eval_output_is_native_for_json_values_too_and_reports_real_truncation() {
    let mut w = fixture();
    for (source, expected) in [
        ("42", "42"),
        ("#t", "#t"),
        ("'()", "()"),
        ("(immutable-vector 1 2)", "#(1 2)"),
        (
            "(list 1 #f 'hello json-null void)",
            "(1 #f hello json-null #<void>)",
        ),
        ("\"hello...\"", "hello..."),
        ("(hash \"a\" #t)", "(hash \"a\" #t)"),
    ] {
        let result = eval(&mut w, source).unwrap();
        assert_eq!(result["kind"], "scheme", "{source}");
        assert_eq!(result["preview"], expected, "{source}");
        assert_eq!(result["truncated"], false);
        assert!(result.get("value").is_none(), "no duplicate JSON result");
    }
    let full = eval(&mut w, "(make-string 10000 #\\x)").unwrap();
    assert_eq!(
        full["truncated"], false,
        "REPL output should exceed tiny inspection budget"
    );
    let large = eval(&mut w, "(make-string 100000 #\\λ)").unwrap();
    assert_eq!(large["truncated"], true);
    assert!(large["preview"].as_str().unwrap().len() <= 32768);
    let wide = eval(&mut w, "(range 10000)").unwrap();
    assert_eq!(wide["truncated"], true);
    eval(&mut w,"(define (value->string x) (error \"printer ran\")) (define (display x) (error \"display ran\"))").unwrap();
    assert_eq!(
        eval(&mut w, "(list 'still 'native)").unwrap()["preview"],
        "(still native)"
    );
}

#[test]
fn top_level_text_is_displayed_but_nested_and_inspected_strings_stay_quoted() {
    let mut w = fixture();
    let text = "first line\n\tsecond \"quoted\" \\ path λ";
    let inputs = json!({"text":text});
    let displayed = submit(
        &mut w,
        json!({"op":"eval"}),
        "(input 'text)",
        inputs.clone(),
    )
    .unwrap();
    assert_eq!(displayed["preview"], text);
    assert_eq!(displayed["type"], "string");
    assert_eq!(displayed["truncated"], false);
    let nested = submit(&mut w, json!({"op":"eval"}), "(list (input 'text))", inputs).unwrap();
    assert_eq!(
        nested["preview"],
        "(\"first line\\n\\tsecond \\\"quoted\\\" \\\\ path λ\")"
    );
    assert_eq!(
        json::inspect(&SteelVal::StringV(text.into())),
        "\"first line\\n\\tsecond \\\"quoted\\\" \\\\ path λ\""
    );
    let controls = json::repl_display(&SteelVal::StringV("a\u{1b}[2J\u{0}\tb\n".into()));
    assert_eq!(controls.text, "a\\u{1b}[2J\\u{0}\tb\n");
    let large = json::repl_display(&SteelVal::StringV("λ\n".repeat(20000).into()));
    assert!(large.truncated && large.text.len() <= 32768);
    assert!(!large.text.starts_with('"'));
    assert_eq!(eval(&mut w, "\"\"").unwrap()["preview"], "");
}
