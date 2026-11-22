//! Explicit direct-call groups, not arbitrary concurrent Scheme branches or guest threads.
use crate::{
    bridge::{self, Bridge},
    json,
};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use steel::{
    gc::{Gc, ShareableMut},
    rvals::{Custom, Result},
    steel_vm::engine::Engine,
    SteelVal,
};

type HostFunction = dyn Fn(&[SteelVal]) -> Result<SteelVal> + Send + Sync;
type RequestFunction = dyn Fn(&[SteelVal]) -> Result<Value> + Send + Sync;

pub struct Capability {
    // Retain the original Arc: pointer identity cannot be recycled after a guest rebinds a name.
    function: Arc<HostFunction>,
    request: Arc<RequestFunction>,
}

pub fn register_capability(
    engine: &mut Engine,
    bridge: &Arc<Mutex<Bridge>>,
    registry: &mut Vec<Capability>,
    name: &str,
    request: impl Fn(&[SteelVal]) -> Result<Value> + Send + Sync + 'static,
) {
    let request: Arc<RequestFunction> = Arc::new(request);
    let normalize = request.clone();
    let bridge = bridge.clone();
    let function: Arc<HostFunction> = Arc::new(move |args| {
        let request = normalize(args)?;
        bridge
            .lock()
            .map_err(|_| json::error("Protocol lock failed"))?
            .host_call(request)
    });
    engine.register_value(name, SteelVal::anonymous_boxed_function(function.clone()));
    registry.push(Capability { function, request });
}

// Opaque, local, and inert until run. Incremental admission bounds retained planning data;
// consuming the vector avoids cloning the entire group or accidentally replaying a used group.
#[derive(Debug)]
struct PreparedGroup {
    limit: usize,
    expected: usize,
    bytes: usize,
    requests: Option<Vec<Value>>,
}
impl Custom for PreparedGroup {}

fn with_group<T>(value: &SteelVal, f: impl FnOnce(&mut PreparedGroup) -> Result<T>) -> Result<T> {
    if let SteelVal::Custom(value) = value {
        if let Ok(mut value) = value.try_write() {
            if let Some(group) = value.as_any_ref_mut().downcast_mut::<PreparedGroup>() {
                return f(group);
            }
        }
    }
    Err(json::error(
        "parallel-calls expects an opaque prepared group",
    ))
}
fn limit(value: &SteelVal) -> Result<usize> {
    match value {
        SteelVal::IntV(n) if (1..=16).contains(n) => Ok(*n as usize),
        _ => Err(json::error(
            "parallel-calls #:limit must be an integer from 1 through 16",
        )),
    }
}

pub fn install(
    engine: &mut Engine,
    bridge: &Arc<Mutex<Bridge>>,
    registry: Vec<Capability>,
) -> Result<()> {
    crate::register(engine, "#%parallel-group", move |args| {
        let [count, SteelVal::ListV(sites)] = args else {
            return Err(json::error(
                "parallel-calls expects a limit and quoted call sites",
            ));
        };
        let limit = limit(count)?;
        Ok(SteelVal::Custom(Gc::new_mut(Box::new(PreparedGroup {
            limit,
            expected: sites.len(),
            bytes: 0,
            requests: Some(Vec::with_capacity(sites.len())),
        }))))
    });
    crate::register(engine, "#%parallel-add!", move |args| {
        let capability = args.get(1).and_then(|value| {
            if let SteelVal::BoxedFunction(function) = value {
                registry.iter().find(|entry| Arc::ptr_eq(&entry.function, &function.function))
            } else { None }
        }).ok_or_else(|| json::error("parallel-calls expects registered capability procedures, not arbitrary Scheme functions or tool-call/tool-raw"))?;
        with_group(&args[0], |group| {
            let requests = group
                .requests
                .as_mut()
                .ok_or_else(|| json::error("parallel-calls group already consumed"))?;
            if requests.len() >= group.expected {
                return Err(json::error("parallel-calls has too many prepared calls"));
            }
            let request = (capability.request)(&args[2..])?;
            let size = bridge::request_size(&request)?;
            if group.bytes + size > bridge::MAX_PARALLEL_BYTES {
                return Err(json::error(
                    "parallel-calls arguments exceed the 1 MiB group byte limit",
                ));
            }
            group.bytes += size;
            requests.push(request);
            Ok(SteelVal::Void)
        })
    });
    let bridge = bridge.clone();
    crate::register(engine, "#%parallel-run", move |args| {
        let [value] = args else {
            return Err(json::error("parallel-calls expects one prepared group"));
        };
        let (limit, requests) = with_group(value, |group| {
            let requests = group
                .requests
                .as_ref()
                .ok_or_else(|| json::error("parallel-calls group already consumed"))?;
            if requests.len() != group.expected {
                return Err(json::error("parallel-calls group is incomplete"));
            }
            Ok((group.limit, group.requests.take().unwrap()))
        })?;
        bridge
            .lock()
            .map_err(|_| json::error("Protocol lock failed"))?
            .parallel_calls(&requests, limit)
    });
    // Quoted call sites provide the expected group size, not name-based authority.
    // Targets and arguments then run once in source order, with incremental byte admission.
    // Steel keyword tokens match directly; syntax-rules' literal list only accepts identifiers.
    engine.compile_and_run_raw_program(
        r#"
(define-syntax parallel-calls
  (syntax-rules ()
    ((_ #:limit count (callee arg ...) ...)
     (let ([group (#%parallel-group count '(callee ...))])
       (#%parallel-add! group callee arg ...) ...
       (#%parallel-run group)))
    ((_ (callee arg ...) ...)
     (parallel-calls #:limit 4 (callee arg ...) ...))))
"#,
    )?;
    Ok(())
}

#[test]
fn bootstrap_compiles_parallel_syntax_before_publishing_a_runner() {
    let mut engine = Engine::new_sandboxed();
    let bridge = Arc::new(Mutex::new(Bridge::new(
        std::fs::File::open("/dev/null").unwrap(),
        std::fs::File::open("/dev/null").unwrap(),
    )));
    crate::install(
        &mut engine,
        &bridge,
        serde_json::json!({}),
        crate::wire::Bindings {
            api_paths: vec![],
            steel_bindings: vec![],
            spread_api_paths: vec![],
            environment_scopes: false,
        },
    )
    .unwrap();
    for source in ["(parallel-calls)", "(parallel-calls #:limit 2)"] {
        let results = engine.compile_and_run_raw_program(source).unwrap();
        assert_eq!(
            json::to_json(results.last().unwrap()).unwrap(),
            serde_json::json!([])
        );
    }
    for source in ["(parallel-calls #:limit 0)", "(parallel-calls #:other 2)"] {
        assert!(
            engine.compile_and_run_raw_program(source).is_err(),
            "{source}"
        );
    }
}
