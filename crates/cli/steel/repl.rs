//! Persistent, opt-in Steel workspace. Introspection never compiles or calls Scheme.
//! Native redefinition semantics and partial state after errors are deliberately preserved.
use crate::{
    bridge::Bridge,
    install, process_setup, json, register,
    wire::{self, Message, ReplOp, ReplOrigin, ReplRequest},
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::File,
    io,
    sync::{Arc, Mutex},
};
use steel::{
    gc::Gc,
    rvals::Result as SteelResult,
    steel_vm::{builtin::Arity, engine::Engine},
    SteelVal,
};

const MAX_EVALUATIONS: usize = 1000;
const MAX_SOURCES: usize = 8 * 1024 * 1024;
const MAX_TEXT: usize = 8192;
const MACRO_COVERAGE: &str =
    "syntax-rules only; executable macro registry is not exposed by Steel's public Engine API";

type CurrentInputs = Arc<Mutex<BTreeMap<String, String>>>;

struct Workspace {
    engine: Engine,
    inputs: CurrentInputs,
    baseline: BTreeMap<String, SteelVal>,
    baseline_slots: BTreeMap<String, usize>,
    baseline_macros: BTreeMap<String, String>,
    evaluations: usize,
    submitted_bytes: usize,
}

impl Workspace {
    fn new(
        mut engine: Engine,
        bridge: Arc<Mutex<Bridge>>,
        inputs: Value,
        bindings: wire::Bindings,
    ) -> SteelResult<Self> {
        install(&mut engine, &bridge, inputs.clone(), bindings)?;
        let slot: CurrentInputs = Arc::new(Mutex::new(
            serde_json::from_value(inputs).map_err(|e| json::error(e.to_string()))?,
        ));
        let current = slot.clone();
        // Install BEFORE the first guest compilation. This callback, including saved copies,
        // consults host-owned data; neither redefinition nor set! of `inputs` can stale it.
        register(&mut engine, "input", move |args| {
            let [key] = args else {
                return Err(json::arity_error("input", "1", args.len()));
            };
            let (SteelVal::StringV(key) | SteelVal::SymbolV(key)) = key else {
                return Err(json::type_error("input key", "a string or symbol", key));
            };
            current
                .lock()
                .map_err(|_| json::error("Current input lock failed"))?
                .get(key.as_str())
                .map(|v| SteelVal::StringV(v.clone().into()))
                .ok_or_else(|| {
                    json::error(format!(
                        "missing record field {}",
                        json::inspect(&SteelVal::StringV(key.clone()))
                    ))
                })
        });
        let baseline_slots = engine
            .globals()
            .iter()
            .enumerate()
            .map(|(slot, name)| (name.resolve().to_string(), slot))
            .collect();
        let baseline = engine
            .readable_globals(0)
            .iter()
            .filter_map(|name| {
                let name = name.resolve().to_string();
                engine.extract_value(&name).ok().map(|value| (name, value))
            })
            .collect();
        let baseline_macros = engine
            .in_scope_macros()
            .iter()
            .map(|(name, m)| {
                // Source IDs/spans are passive metadata, not expansion or AST pretty-printers.
                (name.resolve().to_string(), format!("{:?}", m.span()))
            })
            .collect();
        Ok(Self {
            engine,
            inputs: slot,
            baseline,
            baseline_slots,
            baseline_macros,
            evaluations: 0,
            submitted_bytes: 0,
        })
    }

    fn execute(
        &mut self,
        request: &ReplRequest,
        source: &str,
        inputs: Value,
    ) -> io::Result<SteelResult<Value>> {
        // Include bootstrap/compiler sources, not just successful user submissions.
        if self.engine.report_engine_stats().sources_size > MAX_SOURCES {
            return Err(io::Error::other(
                "REPL retained-source lifetime budget exceeded; workspace lost",
            ));
        }
        if request.op == ReplOp::Eval {
            if self.evaluations >= MAX_EVALUATIONS
                || source.len() > MAX_SOURCES.saturating_sub(self.submitted_bytes)
            {
                return Err(io::Error::other(
                    "REPL evaluation/source lifetime budget exceeded; workspace lost",
                ));
            }
            self.evaluations += 1;
            self.submitted_bytes += source.len();
            let result = (|| {
                let values = serde_json::from_value(inputs.clone())
                    .map_err(|e| json::error(e.to_string()))?;
                *self
                    .inputs
                    .lock()
                    .map_err(|_| json::error("Current input lock failed"))? = values;
                // Compatibility global is current for new code. Old explicit captures retain
                // native Steel semantics; the canonical input callback never reads this slot.
                let compatibility_inputs = json::from_json(&inputs)?;
                // Do not append host globals after failed compilation: Steel may have
                // reserved symbol slots without allocating their runtime values yet.
                // Appending here could accidentally fill a different pending definition.
                if self.engine.extract_value("inputs").is_ok() {
                    self.engine
                        .update_value("inputs", compatibility_inputs.clone());
                    self.baseline.insert("inputs".into(), compatibility_inputs);
                    if let Some(slot) = self.current_slot("inputs") {
                        self.baseline_slots.insert("inputs".into(), slot);
                    }
                }
                let values = self.engine.compile_and_run_raw_program(source.to_owned())?;
                Ok(eval_result(values.last().unwrap_or(&SteelVal::Void)))
            })();
            if self.engine.report_engine_stats().sources_size > MAX_SOURCES {
                return Err(io::Error::other(
                    "REPL retained-source lifetime budget exceeded; workspace lost",
                ));
            }
            Ok(result)
        } else {
            Ok(self.metadata(request))
        }
    }

    fn current_slot(&self, name: &str) -> Option<usize> {
        self.engine
            .globals()
            .iter()
            .rposition(|s| s.resolve() == name)
    }

    fn runtime_origin(&self, name: &str, value: Option<&SteelVal>) -> (&'static str, bool) {
        let builtin = self.baseline_slots.contains_key(name);
        // Slot identity catches equal-valued redefinitions. For opaque values lacking a
        // public identity accessor, unchanged slot/type is conservative (in-place mutation
        // is not a new binding). Never invoke structural equality to refine this.
        let unchanged = builtin
            && self.baseline_slots.get(name).copied() == self.current_slot(name)
            && match (self.baseline.get(name), value) {
                (Some(a), Some(b)) => {
                    same_identity(a, b)
                        || (!has_identity(a)
                            && std::mem::discriminant(a) == std::mem::discriminant(b))
                }
                (None, None) => true,
                _ => false,
            };
        (
            if unchanged { "builtin" } else { "user" },
            builtin && !unchanged,
        )
    }

    fn named(&self, name: &str, preview_limit: usize, describe: bool) -> Value {
        let extracted = self.engine.extract_value(name).ok();
        let macros = self.engine.in_scope_macros();
        let known_macro = macros.get(&name.into());
        // Macro namespace takes precedence because this is the binding used in head position.
        let mut result = if let Some(m) = known_macro {
            let unchanged = self
                .baseline_macros
                .get(name)
                .is_some_and(|span| span == &format!("{:?}", m.span()));
            let mut value = json!({"kind":"macro","type":"syntax-rules","initialized":true,"origin":if unchanged {"builtin"} else {"user"},"shadowsBuiltin":self.baseline_macros.contains_key(name) && !unchanged});
            if describe {
                value["source"] = m.span().source_id().and_then(|id| self.engine.get_source(&id)).map(|source| {
                    let text: &str = source.as_ref().as_ref();
                    json!({"text":bounded(text, MAX_TEXT),"truncated":text.len()>MAX_TEXT,"provenance":"syntax-rules submission"})
                }).unwrap_or(Value::Null);
            }
            value
        } else {
            let (origin, shadows) = self.runtime_origin(name, extracted.as_ref());
            let mut value = json!({"kind":if self.engine.global_exists(name) {"binding"} else {"missing"},"type":"unknown","initialized":extracted.is_some(),"origin":origin,"shadowsBuiltin":shadows});
            if let Some(current) = &extracted {
                value["type"] = json!(json::type_name(current));
                let preview = json::inspect(current);
                value["preview"] = json!(bounded(&preview, preview_limit));
                value["previewTruncated"] =
                    json!(preview.len() > preview_limit || preview.ends_with("..."));
                if let Some(size) = size(current) {
                    value["size"] = size;
                }
                if let SteelVal::Closure(closure) = current {
                    let variadic = closure.is_multi_arity();
                    value["arity"] = json!({"minimum":closure.arity().saturating_sub(usize::from(variadic)),"variadic":variadic});
                    value["captureCount"] = json!(closure.captures().len());
                } else if let Some(metadata) = self
                    .engine
                    .builtin_modules()
                    .inner()
                    .values()
                    .find_map(|module| module.search(current.clone()))
                {
                    let (minimum, maximum, variadic) = match metadata.arity {
                        Arity::Exact(n) => (n, Some(n), false),
                        Arity::AtLeast(n) => (n, None, true),
                        Arity::AtMost(n) => (0, Some(n), false),
                        Arity::Range(min, max) => (min, Some(max), false),
                    };
                    value["arity"] =
                        json!({"minimum":minimum,"maximum":maximum,"variadic":variadic});
                }
            }
            value
        };
        result["name"] = json!(bounded(name, 256));
        result["nameTruncated"] = json!(name.len() > 256);
        if describe {
            result["macroCoverage"] = json!(MACRO_COVERAGE);
            result["documentation"] = if known_macro.is_none() {
                extracted
                    .as_ref()
                    .and_then(|v| self.documentation(name, v))
                    .unwrap_or(Value::Null)
            } else {
                Value::Null
            };
            if result.get("source").is_none() {
                result["source"] = Value::Null;
            }
        }
        result
    }

    fn documentation(&self, name: &str, value: &SteelVal) -> Option<Value> {
        // Loaded metadata only: never resolve/import modules or invoke Scheme get-doc.
        // Compare the CURRENT binding identity, not just its spelling (including aliases).
        let mut modules: Vec<_> = self
            .engine
            .builtin_modules()
            .inner()
            .iter()
            .map(|(name, module)| (name.clone(), module.clone()))
            .collect();
        modules.sort_by(|(a, _), (b, _)| a.as_ref().cmp(b.as_ref()));
        for (module_name, module) in modules {
            if module
                .try_get_ref(name)
                .is_some_and(|v| same_identity(value, &v))
            {
                if let Some(text) = module.get_documentation(name) {
                    return Some(
                        json!({"text":bounded(&text, MAX_TEXT),"truncated":text.len()>MAX_TEXT,"provenance":"builtin-module","module":module_name.as_ref()}),
                    );
                }
            }
            if let Some(metadata) = module.search(value.clone()) {
                if let Some(doc) = metadata.doc {
                    return Some(
                        json!({"text":bounded(&doc.0, MAX_TEXT),"truncated":doc.0.len()>MAX_TEXT,"provenance":"builtin-function","module":module_name.as_ref(),"definitionName":metadata.name.as_ref()}),
                    );
                }
            }
        }
        // Host Engine lookup reads the closure doc table without executing Scheme.
        // Module lookup comes first to avoid Steel's order-sensitive early-return gap.
        self.engine.get_doc_for_identifier(name).map(|text|
            json!({"text":bounded(&text, MAX_TEXT),"truncated":text.len()>MAX_TEXT,"provenance":"runtime-binding"}))
    }

    fn metadata(&self, request: &ReplRequest) -> SteelResult<Value> {
        if request.name.as_ref().is_some_and(|n| n.len() > 4096)
            || request.query.as_ref().is_some_and(|q| q.len() > 4096)
        {
            return Err(json::error("REPL name/query exceeds 4096-byte limit"));
        }
        match request.op {
            ReplOp::Describe => {
                let name = request
                    .name
                    .as_deref()
                    .filter(|n| !n.is_empty())
                    .ok_or_else(|| json::error("describe requires a name"))?;
                return Ok(self.named(name, MAX_TEXT, true));
            }
            ReplOp::Inspect if request.name.is_some() => {
                return Ok(self.named(request.name.as_deref().unwrap(), MAX_TEXT, false))
            }
            _ => {}
        }
        let origin = request.origin.unwrap_or(if request.op == ReplOp::Search {
            ReplOrigin::All
        } else {
            ReplOrigin::User
        });
        let query = request.query.as_deref().unwrap_or("").to_lowercase();
        let mut names: Vec<_> = self
            .engine
            .readable_globals(0)
            .iter()
            .map(|s| s.resolve().to_string())
            .collect();
        names.extend(
            self.engine
                .in_scope_macros()
                .keys()
                .map(|s| s.resolve().to_string()),
        );
        names.sort();
        names.dedup();
        let offset = request.offset.unwrap_or(0);
        let limit = request.limit.unwrap_or(20).clamp(1, 50);
        let mut total = 0;
        let mut items = Vec::new();
        for name in names {
            let item = self.named(&name, 256, false);
            if origin == ReplOrigin::User && item["origin"] != "user" {
                continue;
            }
            if !query.is_empty() && !name.to_lowercase().contains(&query) {
                let doc = if request.op == ReplOp::Search {
                    self.engine
                        .extract_value(&name)
                        .ok()
                        .and_then(|v| self.documentation(&name, &v))
                } else {
                    None
                };
                if !doc
                    .as_ref()
                    .and_then(|d| d["text"].as_str())
                    .is_some_and(|text| text.to_lowercase().contains(&query))
                {
                    continue;
                }
            }
            if total >= offset && items.len() < limit {
                items.push(item);
            }
            total += 1;
        }
        let next = offset.saturating_add(items.len());
        let has_more = next < total;
        Ok(
            json!({"items":items,"total":total,"nextOffset":if has_more {Some(next)} else {None},"hasMore":has_more}),
        )
    }
}

fn bounded(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit.saturating_sub(3);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

// No SteelVal equality: structural/custom equality could execute guest code or walk cycles.
fn has_identity(value: &SteelVal) -> bool {
    matches!(
        value,
        SteelVal::Closure(_)
            | SteelVal::BoxedFunction(_)
            | SteelVal::Custom(_)
            | SteelVal::FuncV(_)
            | SteelVal::MutFunc(_)
            | SteelVal::BuiltIn(_)
            | SteelVal::Void
            | SteelVal::BoolV(_)
            | SteelVal::IntV(_)
            | SteelVal::NumV(_)
            | SteelVal::StringV(_)
            | SteelVal::SymbolV(_)
    )
}

fn same_identity(a: &SteelVal, b: &SteelVal) -> bool {
    match (a, b) {
        (SteelVal::Closure(a), SteelVal::Closure(b)) => Gc::ptr_eq(a, b),
        (SteelVal::BoxedFunction(a), SteelVal::BoxedFunction(b)) => Gc::ptr_eq(a, b),
        (SteelVal::Custom(a), SteelVal::Custom(b)) => Gc::ptr_eq(a, b),
        (SteelVal::FuncV(a), SteelVal::FuncV(b)) => std::ptr::fn_addr_eq(*a, *b),
        (SteelVal::MutFunc(a), SteelVal::MutFunc(b)) => std::ptr::fn_addr_eq(*a, *b),
        (SteelVal::BuiltIn(a), SteelVal::BuiltIn(b)) => std::ptr::fn_addr_eq(*a, *b),
        (SteelVal::Void, SteelVal::Void) => true,
        (SteelVal::BoolV(a), SteelVal::BoolV(b)) => a == b,
        (SteelVal::IntV(a), SteelVal::IntV(b)) => a == b,
        (SteelVal::NumV(a), SteelVal::NumV(b)) => a.to_bits() == b.to_bits(),
        (SteelVal::StringV(a), SteelVal::StringV(b))
        | (SteelVal::SymbolV(a), SteelVal::SymbolV(b)) => a == b,
        _ => false,
    }
}

fn size(value: &SteelVal) -> Option<Value> {
    let (unit, count) = match value {
        SteelVal::StringV(v) | SteelVal::SymbolV(v) => ("bytes", v.len()),
        SteelVal::ListV(v) => ("items", v.len()),
        SteelVal::VectorV(v) => ("items", v.len()),
        SteelVal::HashMapV(v) => ("entries", v.len()),
        _ => return None,
    };
    Some(json!({"unit":unit,"value":count}))
}

fn eval_result(value: &SteelVal) -> Value {
    let display = json::repl_display(value);
    json!({"kind":"scheme","preview":display.text,"truncated":display.truncated,"type":json::type_name(value)})
}

pub fn run(input: File, output: File) -> io::Result<()> {
    let mut bridge = Bridge::new(input, output);
    process_setup::resource_limits()?;
    let mut engine = Some(Engine::new_sandboxed());
    #[cfg(target_os = "linux")]
    crate::linux_seccomp::install()?;
    wire::write(
        &mut bridge.output,
        &json!({"type":"ready","protocolVersion":1,"runner":{"id":"steel-repl","programFormat":"scheme"}}),
    )?;
    let bridge = Arc::new(Mutex::new(bridge));
    let mut workspace: Option<Workspace> = None;
    loop {
        let message = {
            let mut bridge = bridge
                .lock()
                .map_err(|_| io::Error::other("Protocol lock failed"))?;
            wire::read(&mut bridge.input)?
        };
        let Message::Execute {
            run_id,
            program,
            inputs,
            bindings,
        } = message
        else {
            if matches!(message, Message::Shutdown) {
                return Ok(());
            }
            return Err(io::Error::other("Expected execute or shutdown"));
        };
        if run_id.is_empty() || program.format != "scheme" || program.source.len() > wire::MAX_VALUE
        {
            return Err(io::Error::other("Invalid Steel program"));
        }
        bridge
            .lock()
            .map_err(|_| io::Error::other("Protocol lock failed"))?
            .begin_submission(run_id.clone());
        let inputs = serde_json::to_value(inputs)?;
        if workspace.is_none() {
            workspace = Some(
                Workspace::new(
                    engine.take().unwrap(),
                    bridge.clone(),
                    inputs.clone(),
                    bindings,
                )
                .map_err(|e| io::Error::other(e.to_string()))?,
            );
        }
        // Binding descriptors are frozen by the parent. Saved host callbacks retain this
        // bridge, never the old dispatcher; every request carries its current run_id.
        let workspace = workspace.as_mut().unwrap();
        let result = workspace.execute(&program.repl, &program.source, inputs)?;
        let mut bridge = bridge
            .lock()
            .map_err(|_| io::Error::other("Protocol lock failed"))?;
        let message = match result {
            Ok(value) => json!({"type":"complete","runId":run_id,"ok":true,"value":value}),
            Err(error) => {
                let error = crate::diagnostics::render(&workspace.engine, error, &bridge.host_errors);
                json!({"type":"complete","runId":run_id,"ok":false,"error":error})
            }
        };
        wire::write(&mut bridge.output, &message)?;
    }
}

#[cfg(test)]
#[path = "repl_tests.rs"]
mod tests;
