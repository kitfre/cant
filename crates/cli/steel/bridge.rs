//! A single-threaded, bounded request pump. All operations still use the shared host contract.
use crate::{
    json,
    wire::{self, Message},
};
use serde_json::Value;
use std::{collections::BTreeMap, fs::File, io::BufReader};
use steel::{rvals::Result, SteelVal};

pub const MAX_PARALLEL_BYTES: usize = 1024 * 1024;

pub fn request_size(request: &Value) -> Result<usize> {
    let size = serde_json::to_vec(request)
        .map_err(|e| json::error(e.to_string()))?
        .len();
    if size > wire::MAX_VALUE {
        return Err(json::error("Host arguments exceed the JSON byte limit"));
    }
    Ok(size)
}

pub struct Bridge {
    pub input: BufReader<File>,
    pub output: File,
    pub run_id: String,
    next_call: usize,
    pub console_bytes: usize,
    pub host_errors: Vec<(String, Value)>,
}
impl Bridge {
    pub fn new(input: File, output: File) -> Self {
        Self {
            input: BufReader::new(input),
            output,
            run_id: String::new(),
            next_call: 0,
            console_bytes: 0,
            host_errors: Vec::new(),
        }
    }

    pub fn begin_submission(&mut self, run_id: String) {
        self.run_id = run_id;
        self.next_call = 0;
        self.console_bytes = 0;
        self.host_errors.clear();
    }

    pub fn close_protocol(&mut self, message: impl Into<String>) -> ! {
        // Notify the parent, then exit without returning control to guest handlers.
        let _ = wire::write(
            &mut self.output,
            &serde_json::json!({
                "type":"fatal", "error":{"name":"SteelRunnerError","message":message.into()}
            }),
        );
        std::process::exit(1);
    }

    pub fn host_call(&mut self, request: Value) -> Result<SteelVal> {
        Ok(self.calls(&[request], 1, None)?.remove(0))
    }

    pub fn parallel_calls(&mut self, requests: &[Value], limit: usize) -> Result<SteelVal> {
        Ok(SteelVal::ListV(
            self.calls(requests, limit, Some(MAX_PARALLEL_BYTES))?
                .into_iter()
                .collect(),
        ))
    }

    fn send(&mut self, request: &Value) -> String {
        self.next_call += 1;
        let call_id = format!("call_{}", self.next_call);
        if let Err(error) = wire::write(
            &mut self.output,
            &serde_json::json!({"type":"tool_call","runId":self.run_id,"callId":call_id,"request":request}),
        ) {
            self.close_protocol(error.to_string());
        }
        call_id
    }

    // No guest code runs while the pump owns the bridge. Validate the entire group before sending,
    // refill on any response, and settle every planned call even after a normal host/value error.
    // Cancellation and protocol faults are terminal instead: the parent aborts active host work.
    fn calls(
        &mut self,
        requests: &[Value],
        limit: usize,
        budget: Option<usize>,
    ) -> Result<Vec<SteelVal>> {
        if !(1..=16).contains(&limit) {
            return Err(json::error(
                "parallel-calls #:limit must be an integer from 1 through 16",
            ));
        }
        let mut request_bytes = 0;
        for request in requests {
            request_bytes += request_size(request)?;
        }
        if budget.is_some_and(|max| request_bytes > max) {
            return Err(json::error(
                "parallel-calls arguments exceed the 1 MiB group byte limit",
            ));
        }
        let mut pending = BTreeMap::new();
        let mut next = 0;
        let mut results: Vec<Option<Result<SteelVal>>> =
            (0..requests.len()).map(|_| None).collect();
        let mut result_bytes = 0;
        while next < requests.len() || !pending.is_empty() {
            while next < requests.len() && pending.len() < limit {
                pending.insert(self.send(&requests[next]), next);
                next += 1;
            }
            match wire::read(&mut self.input) {
                Ok(Message::ToolResult {
                    run_id,
                    call_id,
                    ok,
                    value,
                    error,
                }) if run_id == self.run_id && pending.contains_key(&call_id) => {
                    let index = pending.remove(&call_id).unwrap();
                    let payload = if ok {
                        value
                    } else if let Some(error) = error {
                        serde_json::json!({"kind":"host","name":error.name,"message":error.message})
                    } else {
                        self.close_protocol("Invalid host error response");
                    };
                    // Bound all retained reply payloads (including errors) before conversion.
                    // Every ordinary value/serialization error stays in its slot until all calls settle.
                    results[index] = Some((|| {
                        result_bytes += serde_json::to_vec(&payload)
                            .map_err(|e| json::error(e.to_string()))?
                            .len();
                        if budget.is_some_and(|max| result_bytes > max) {
                            return Err(json::error(
                                "parallel-calls results exceed the 1 MiB group byte limit",
                            ));
                        }
                        if ok {
                            json::from_json(&payload)
                        } else {
                            // A retained caught error must not alias a same-numbered,
                            // identical failure in a later REPL submission.
                            let message = format!(
                                "[{}:{call_id}] {}: {}",
                                self.run_id,
                                payload["name"].as_str().unwrap(),
                                payload["message"].as_str().unwrap()
                            );
                            self.host_errors.push((message.clone(), payload));
                            Err(json::error(message))
                        }
                    })());
                }
                Ok(Message::Cancel { run_id }) if run_id == self.run_id => {
                    self.close_protocol("Invocation aborted");
                }
                _ => {
                    self.close_protocol("Unexpected or malformed host response");
                }
            }
        }
        // Only raise once there is no outstanding work; errors are chosen in source order.
        results
            .into_iter()
            .map(|result| result.expect("all requests settled"))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn submission_refresh_resets_only_per_run_state() {
        let input = File::open("/dev/null").unwrap();
        let mut bridge = Bridge::new(input.try_clone().unwrap(), input);
        bridge.run_id = "old".into();
        bridge.next_call = 64;
        bridge.console_bytes = 65536;
        bridge
            .host_errors
            .push(("old".into(), serde_json::json!({})));
        bridge.begin_submission("next".into());
        assert_eq!(bridge.run_id, "next");
        assert_eq!(bridge.next_call, 0);
        assert_eq!(bridge.console_bytes, 0);
        assert!(bridge.host_errors.is_empty());
    }
}
