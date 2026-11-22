#[cfg(not(any(
    all(target_os = "linux", target_pointer_width = "64", any(target_arch = "x86_64", target_arch = "aarch64")),
    all(target_os = "macos", target_arch = "aarch64")
)))]
compile_error!("Cant Steel runner requires Linux x86_64/aarch64 or Apple-silicon macOS");
mod bindings;
mod bridge;
mod diagnostics;
#[cfg(target_os = "linux")]
mod linux_seccomp;
mod process_setup;
mod json;
mod parallel;
mod prelude;
mod repl;
mod wire;

use bridge::Bridge;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use steel::{rvals::Result as SteelResult, steel_vm::engine::Engine, SteelVal};

fn register(
    engine: &mut Engine,
    name: &str,
    function: impl Fn(&[SteelVal]) -> SteelResult<SteelVal> + Send + Sync + 'static,
) {
    engine.register_value(name, SteelVal::anonymous_boxed_function(Arc::new(function)));
}
fn as_string(value: &SteelVal) -> SteelResult<String> {
    match value {
        SteelVal::StringV(value) => Ok(value.to_string()),
        _ => Err(json::type_error("argument", "a string", value)),
    }
}
fn install(
    engine: &mut Engine,
    bridge: &Arc<Mutex<Bridge>>,
    inputs: Value,
    bindings: wire::Bindings,
) -> SteelResult<()> {
    if bindings.environment_scopes {
        return Err(json::error(
            "Steel does not implement scoped environment helpers yet",
        ));
    }
    if bindings.api_paths.len() > 4096 {
        return Err(json::error("Too many bindings"));
    }
    let mut seen = std::collections::HashSet::new();
    for path in &bindings.api_paths {
        if path.is_empty()
            || path.iter().any(|segment| {
                segment.is_empty()
                    || !segment.bytes().enumerate().all(|(index, byte)| {
                        byte.is_ascii_alphabetic()
                            || byte == b'_'
                            || byte == b'$'
                            || (index > 0 && byte.is_ascii_digit())
                    })
            })
            || !seen.insert(path.clone())
        {
            return Err(json::error("Invalid or duplicate binding path"));
        }
    }
    if bindings
        .spread_api_paths
        .iter()
        .any(|path| !seen.contains(path))
    {
        return Err(json::error("Spread binding is not in the API snapshot"));
    }
    bindings::validate(&bindings)?;
    engine.register_value("inputs", json::from_json(&inputs)?);
    engine.register_value("json-null", json::null());
    engine.register_value("code-mode-interface-version", SteelVal::IntV(4));
    prelude::install(engine)?;
    register(engine, "json-parse", |args| {
        if args.len() != 1 {
            return Err(json::arity_error("json-parse", "1", args.len()));
        }
        let source = as_string(&args[0])?;
        if source.len() > wire::MAX_VALUE {
            return Err(json::error("JSON text exceeds limit"));
        }
        json::from_json(
            &serde_json::from_str(&source).map_err(|error| json::error(error.to_string()))?,
        )
    });
    let identities = Arc::new(
        bindings
            .steel_bindings
            .iter()
            .map(|binding| (binding.name.clone(), binding.path.join(".")))
            .collect::<std::collections::BTreeMap<_, _>>(),
    );
    let mut registry = Vec::new();
    for binding in bindings.steel_bindings {
        // Exact fallback descriptors already have the ordinary dotted implementation below.
        if binding.name == format!("tools.{}", binding.path.join(".")) {
            continue;
        }
        let identities = identities.clone();
        let name = binding.name.clone();
        if engine.global_exists(&name) {
            return Err(json::error(format!("Steel binding {name} would shadow a runtime primitive; rebuild with matching reservations")));
        }
        parallel::register_capability(engine, bridge, &mut registry, &name, move |args| {
            let args = bindings::arguments(&binding, args, &identities)?;
            Ok(json!({"kind":"path","path":binding.path,"args":args}))
        });
    }
    {
        let bridge = bridge.clone();
        register(engine, "#%host-error-info", move |args| {
            if args.len() != 1 {
                return Err(json::error("error-info expects one caught error"));
            }
            let message = as_string(&args[0])?;
            let bridge = bridge
                .lock()
                .map_err(|_| json::error("Protocol lock failed"))?;
            let value = bridge
                .host_errors
                .iter()
                .find(|(text, _)| text == &message)
                .map(|(_, value)| value.clone())
                .unwrap_or_else(|| json!({"kind":"steel","name":"SteelError","message":message}));
            json::from_json(&value)
        });
    }
    engine.compile_and_run_raw_program(
        "(define (error-info error) (#%host-error-info (error-object-message error)))",
    )?;
    for path in bindings.api_paths {
        let name = format!("tools.{}", path.join("."));
        let spread = bindings.spread_api_paths.contains(&path);
        let diagnostic_name = name.clone();
        parallel::register_capability(engine, bridge, &mut registry, &name, move |args| {
            let args = if spread {
                Value::Array(
                    args.iter()
                        .enumerate()
                        .map(|(i, value)| {
                            json::to_json_at(
                                value,
                                &format!("{diagnostic_name} argument {}", i + 1),
                            )
                        })
                        .collect::<SteelResult<Vec<_>>>()?,
                )
            } else {
                if args.len() != 1 {
                    return Err(json::error("This binding expects exactly one argument"));
                }
                json::to_json_at(&args[0], &format!("{diagnostic_name} argument 1"))?
            };
            Ok(json!({"kind":"path","path":path,"args":args}))
        });
    }
    parallel::install(engine, bridge, registry)?;
    for (name, kind) in [("tool-call", "tool"), ("tool-raw", "raw")] {
        let bridge = bridge.clone();
        register(engine, name, move |args| {
            if args.len() != 2 {
                return Err(json::error(
                    "Name-based calls expect a tool name and JSON arguments",
                ));
            }
            let target = as_string(&args[0])?;
            bridge
                .lock()
                .map_err(|_| json::error("Protocol lock failed"))?
                .host_call(json!({"kind":kind,"name":target,"args":json::to_json_at(&args[1], &format!("{name} argument 2"))?}))
        });
    }
    for name in ["console-log", "inspect", "print"] {
        let bridge = bridge.clone();
        register(engine, name, move |args| {
            let values = args
                .iter()
                .map(|value| match name {
                    "inspect" => Ok(Value::String(json::inspect(value))),
                    "print" => {
                        let display = json::repl_display(value);
                        Ok(Value::String(if display.truncated {
                            format!("{} [truncated]", display.text)
                        } else {
                            display.text
                        }))
                    }
                    _ => json::to_json(value),
                })
                .collect::<SteelResult<Vec<_>>>()?;
            let mut bridge = bridge
                .lock()
                .map_err(|_| json::error("Protocol lock failed"))?;
            let message =
                json!({"type":"console","runId":bridge.run_id,"level":"log","values":values});
            let bytes = serde_json::to_vec(&message)
                .map_err(|e| json::error(e.to_string()))?
                .len();
            if bridge.console_bytes + bytes > 64 * 1024 {
                return Err(json::error("Console byte limit exceeded"));
            }
            bridge.console_bytes += bytes;
            if let Err(error) = wire::write(&mut bridge.output, &message) {
                bridge.close_protocol(error.to_string());
            }
            Ok(SteelVal::Void)
        });
    }
    Ok(())
}
fn main() {
    // Every invocation gets a fresh process. Abort/error exits are handled by the shared parent.
    // No engine diagnostic is ever allowed onto the protocol stream.
    let Ok((input, output)) = process_setup::private_protocol() else {
        std::process::exit(1)
    };
    let Ok(mut report) = output.try_clone() else {
        std::process::exit(1)
    };
    let result = std::panic::catch_unwind(|| repl::run(input, output));
    let message = match result {
        Ok(Ok(())) => std::process::exit(0),
        Ok(Err(error)) => error.to_string(),
        Err(_) => "Steel runner panicked".to_string(),
    };
    let _ = wire::write(
        &mut report,
        &json!({"type":"fatal","error":{"name":"SteelRunnerError","message":message}}),
    );
    std::process::exit(1);
}
