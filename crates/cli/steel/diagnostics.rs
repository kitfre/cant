//! Compact diagnostics shared by the fresh evaluator and persistent REPL.
use serde::Serialize;
use serde_json::Value;
use steel::{
    parser::span::Span,
    rerrs::{ErrorKind, SteelErr},
    steel_vm::engine::Engine,
};

const TRUNCATED: &str = "… [diagnostic truncated]";
const STRING_ESCAPE_HINT: &str = "\nHint: pass regex/shell text via literal inputs instead of Scheme string escapes, e.g. (system/exec \"rg\" (list \"-n\" (input 'pattern) \"src\")).";

#[derive(Serialize)]
pub struct Diagnostic {
    pub name: String,
    pub message: String,
}

fn bounded(text: &str, bytes: usize, lines: usize) -> String {
    if text.len() <= bytes && text.lines().count() <= lines {
        return text.to_owned();
    }
    let mut line = 1;
    let end = text
        .char_indices()
        .find_map(|(i, ch)| {
            if ch == '\n' {
                line += 1;
            }
            (i + ch.len_utf8() > bytes - TRUNCATED.len() || line > lines).then_some(i)
        })
        .unwrap_or(text.len());
    format!("{}{TRUNCATED}", text[..end].trim_end())
}

fn location(engine: &Engine, span: Span) -> Option<String> {
    let id = span.source_id()?;
    let source = engine.get_source(&id)?;
    // Spans are UTF-8 byte offsets. Invalid/missing locations never replace the error.
    let prefix = source.get(..span.start as usize)?;
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = prefix.rsplit('\n').next()?.chars().count() + 1;
    let path = engine.get_path_for_source_id(&id);
    let path = path
        .map(|path| format!("{}:", bounded(&path.to_string_lossy(), 160, 1)))
        .unwrap_or_default();
    let excerpt = source
        .get(span.start as usize..span.end as usize)
        .unwrap_or("")
        .trim();
    let excerpt = bounded(excerpt, 100, 1);
    Some(if excerpt.is_empty() {
        format!("at {path}{line}:{column}")
    } else {
        format!("at {path}{line}:{column}: {excerpt:?}")
    })
}

pub fn render(engine: &Engine, error: SteelErr, host_errors: &[(String, Value)]) -> Diagnostic {
    // Steel 0.8.3's message() is private, but Display has a stable, source-free prefix.
    // Strip only that prefix; don't infer error kinds from arbitrary message text.
    let raw = error.to_string();
    let prefix = format!("Error: {}: ", error.kind());
    let body = raw.strip_prefix(&prefix).unwrap_or(&raw);
    let body = if error.kind() == ErrorKind::Parse {
        body.strip_prefix("Syntax Error: ").unwrap_or(body)
    } else {
        body
    };
    let host = (error.kind() == ErrorKind::Generic)
        .then(|| host_errors.iter().find(|(text, _)| text == body))
        .flatten();
    let (name, body) = if let Some((_, host)) = host {
        let name = host["name"].as_str().unwrap_or("SteelError");
        // The parent interprets AbortError as cancellation, not an ordinary guest error.
        // Preserve that distinction while still showing the tool's original error name.
        if name == "AbortError" {
            (
                "SteelError",
                format!("AbortError: {}", host["message"].as_str().unwrap_or(body)),
            )
        } else {
            (name, host["message"].as_str().unwrap_or(body).to_owned())
        }
    } else {
        let name = match error.kind() {
            ErrorKind::TypeMismatch => "TypeError",
            ErrorKind::ArityMismatch => "ArityError",
            ErrorKind::FreeIdentifier => "NameError",
            ErrorKind::Parse | ErrorKind::BadSyntax | ErrorKind::UnexpectedToken => "SyntaxError",
            ErrorKind::ContractViolation => "ContractError",
            ErrorKind::ConversionError => "ConversionError",
            ErrorKind::Io => "IOError",
            _ => "SteelError",
        };
        (name, body.trim().to_owned())
    };
    let mut message = bounded(&body, 2048, 12);
    let mut spans = Vec::new();
    let mut locations = Vec::new();
    let frames = error
        .stack_trace()
        .iter()
        .flat_map(|trace| trace.trace().iter().rev())
        .filter_map(|frame| *frame.span());
    // Keep the primary location and two distinct nearby callers, never a source dump.
    for span in error.span().into_iter().chain(frames).take(32) {
        if spans.contains(&span) {
            continue;
        }
        if let Some(location) = location(engine, span) {
            if locations.len() == 3 {
                locations.push("… more frames".into());
                break;
            }
            spans.push(span);
            locations.push(location);
        }
    }
    if !locations.is_empty() {
        message.push('\n');
        message.push_str(&bounded(&locations.join("\n"), 1024, 4));
    }
    if error.kind() == ErrorKind::Parse
        && raw.starts_with("Error: Parse: Syntax Error: invalid escape '")
    {
        message.push_str(STRING_ESCAPE_HINT);
    }
    Diagnostic {
        name: bounded(name, 128, 1),
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_errors_keep_the_cause_location_and_targeted_hint() {
        let mut engine = Engine::new_sandboxed();
        for source in [r#""\s""#, r#""\(""#] {
            let error = engine.compile_and_run_raw_program(source).unwrap_err();
            let diagnostic = render(&engine, error, &[]);
            assert_eq!(diagnostic.name, "SyntaxError");
            assert!(diagnostic.message.starts_with("invalid escape '"));
            assert!(diagnostic.message.contains("at 1:"));
            assert!(diagnostic.message.ends_with(STRING_ESCAPE_HINT));
            assert!(!diagnostic.message.contains('│'));
        }
        // The useful message must survive even when the source line dwarfs the output budget.
        let source = format!("\"λ\" {}missing-diagnostic-binding", " ".repeat(20_000));
        let error = engine.compile_and_run_raw_program(source).unwrap_err();
        let diagnostic = render(&engine, error, &[]);
        assert_eq!(diagnostic.name, "NameError");
        assert!(diagnostic.message.contains("missing-diagnostic-binding"));
        assert!(
            diagnostic.message.contains("at 1:20005:"),
            "{}",
            diagnostic.message
        );
        assert!(diagnostic.message.len() < 300);
        for source in ["(+ 1", r#"(error "Syntax Error: invalid escape 's'")"#] {
            let error = engine.compile_and_run_raw_program(source).unwrap_err();
            assert!(!render(&engine, error, &[]).message.contains("Hint:"));
        }
    }

    #[test]
    fn multiline_and_unicode_diagnostics_are_explicitly_bounded_without_losing_hints() {
        let engine = Engine::new_sandboxed();
        for text in ["λ".repeat(10_000), "line\n".repeat(100)] {
            let error = SteelErr::new(
                ErrorKind::Parse,
                format!("Syntax Error: invalid escape 's': {text}"),
            );
            let diagnostic = render(&engine, error, &[]);
            assert!(diagnostic.message.starts_with("invalid escape 's'"));
            assert!(diagnostic.message.contains(TRUNCATED));
            assert!(diagnostic.message.ends_with(STRING_ESCAPE_HINT));
            assert!(diagnostic.message.len() <= 2048 + STRING_ESCAPE_HINT.len());
            assert!(diagnostic.message.lines().count() <= 13);
        }
    }

    #[test]
    fn host_errors_unwrap_only_exact_current_records_and_never_change_cancellation_semantics() {
        let engine = Engine::new_sandboxed();
        for name in ["NestedToolError", "AbortError"] {
            let text = format!("[run_1:call_1] {name}: files.read failed: ENOENT");
            let host_errors = vec![(
                text.clone(),
                serde_json::json!({"name":name,"message":"files.read failed: ENOENT"}),
            )];
            let diagnostic = render(
                &engine,
                SteelErr::new(ErrorKind::Generic, text.clone()),
                &host_errors,
            );
            assert_eq!(
                diagnostic.name,
                if name == "AbortError" {
                    "SteelError"
                } else {
                    name
                }
            );
            assert!(diagnostic.message.ends_with("files.read failed: ENOENT"));
            assert!(!diagnostic.message.contains("run_1"));
            for (kind, body) in [
                (ErrorKind::Generic, format!("other {text}")),
                (ErrorKind::TypeMismatch, text.clone()),
            ] {
                assert!(render(&engine, SteelErr::new(kind, body), &host_errors)
                    .message
                    .contains(&text));
            }
            // Old/caught errors must not be mistaken for a new submission's host failure.
            assert_eq!(
                render(
                    &engine,
                    SteelErr::new(ErrorKind::Generic, text.clone()),
                    &[]
                )
                .message,
                text
            );
        }
    }

    #[test]
    fn saved_function_errors_keep_original_locations_and_compact_caller_context() {
        let mut engine = Engine::new_sandboxed();
        engine.register_value(
            "inputs",
            crate::json::from_json(&serde_json::json!({})).unwrap(),
        );
        crate::prelude::install(&mut engine).unwrap();
        engine
            .compile_and_run_raw_program("(define (saved x)\n  (get x 'field))")
            .unwrap();
        let error = engine
            .compile_and_run_raw_program("(saved #f)")
            .unwrap_err();
        let diagnostic = render(&engine, error, &[]);
        assert_eq!(diagnostic.name, "TypeError");
        assert!(
            diagnostic.message.contains("at 2:"),
            "{}",
            diagnostic.message
        );
        assert!(diagnostic.message.len() < 600);
        assert!(!diagnostic.message.contains('│'));
    }
}
