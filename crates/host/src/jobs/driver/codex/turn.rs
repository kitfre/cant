use crate::jobs::driver::{ProgressCallback, WorkerOutcome};
use serde_json::Value;

enum Answer {
    Streaming(String),
    Completed(String),
    Final(String),
    Ignored,
}

pub(super) struct Turn {
    thread: String,
    turn: String,
    structured: bool,
    // First-seen item order survives turn/completed replaying an older snapshot.
    messages: Vec<(Option<String>, Answer)>,
    usage: Option<Value>,
}

impl Turn {
    pub(super) fn new(thread: String, turn: String, structured: bool) -> Self {
        Self {
            thread,
            turn,
            structured,
            messages: Vec::new(),
            usage: None,
        }
    }

    fn message(&mut self, id: Option<&str>) -> &mut Answer {
        let index = id
            .and_then(|id| {
                self.messages
                    .iter()
                    .rposition(|(key, _)| key.as_deref() == Some(id))
            })
            .unwrap_or_else(|| {
                self.messages
                    .push((id.map(str::to_owned), Answer::Streaming(String::new())));
                self.messages.len() - 1
            });
        &mut self.messages[index].1
    }

    fn item(&mut self, item: &Value) {
        if item["type"] != "agentMessage" {
            return;
        }
        *self.message(item["id"].as_str()) = match (item["phase"].as_str(), item["text"].as_str()) {
            (Some("final_answer"), Some(text)) => Answer::Final(text.into()),
            (None | Some("partial_answer"), Some(text)) => Answer::Completed(text.into()),
            _ => Answer::Ignored,
        };
    }

    pub(super) fn event(
        &mut self,
        method: &str,
        params: &Value,
        progress: &ProgressCallback,
    ) -> Result<Option<WorkerOutcome>, String> {
        let id = if method == "turn/completed" {
            &params["turn"]["id"]
        } else {
            &params["turnId"]
        };
        if params["threadId"].as_str() != Some(&self.thread) || id.as_str() != Some(&self.turn) {
            return Ok(None);
        }
        match method {
            "item/agentMessage/delta" => {
                if let (Some(id), Some(delta)) =
                    (params["itemId"].as_str(), params["delta"].as_str())
                {
                    if let Answer::Streaming(text) = self.message(Some(id)) {
                        text.push_str(delta);
                    }
                    progress(delta.into());
                }
            }
            "item/completed" => self.item(&params["item"]),
            "thread/tokenUsage/updated" => self.usage = params.get("tokenUsage").cloned(),
            "error" if params["willRetry"] != true => {
                return Err(params["error"]["message"]
                    .as_str()
                    .unwrap_or("Codex turn error")
                    .into());
            }
            "turn/completed" => {
                let turn = &params["turn"];
                let status = turn["status"].as_str().unwrap_or("unknown");
                if status != "completed" {
                    let reason = turn["error"]["message"].as_str().unwrap_or("no details");
                    return Err(format!("Codex turn {status}: {reason}"));
                }
                if let Some(items) = turn["items"].as_array() {
                    for item in items {
                        self.item(item);
                    }
                }
                let text = self
                    .messages
                    .drain(..)
                    .filter_map(|(_, answer)| match answer {
                        Answer::Final(text) => Some((true, text)),
                        Answer::Streaming(text) | Answer::Completed(text) => Some((false, text)),
                        Answer::Ignored => None,
                    })
                    .max_by_key(|(is_final, _)| *is_final)
                    .map(|(_, text)| text)
                    .ok_or("Codex turn completed without an agent answer")?;
                let parsed = serde_json::from_str(&text);
                let data = if self.structured {
                    Some(
                        parsed
                            .map_err(|e| format!("Invalid JSON in Codex structured output: {e}"))?,
                    )
                } else {
                    parsed.ok()
                };
                return Ok(Some(WorkerOutcome {
                    text,
                    data,
                    usage: self.usage.take(),
                }));
            }
            _ => {}
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    fn scoped(payload: Value) -> Value {
        let mut p = payload;
        p["threadId"] = json!("th");
        p["turnId"] = json!("tu");
        p
    }

    fn completed(status: &str, items: Value) -> Value {
        json!({"threadId":"th","turn":{"id":"tu","status":status,"items":items}})
    }

    fn message(id: &str, phase: Value, text: &str) -> Value {
        json!({"type":"agentMessage","id":id,"phase":phase,"text":text})
    }

    #[test]
    fn streams_scoped_deltas_and_returns_only_the_final_answer() {
        let mut t = Turn::new("th".into(), "tu".into(), true);
        let chunks = Arc::new(Mutex::new(Vec::new()));
        let cb: ProgressCallback = Arc::new({
            let chunks = chunks.clone();
            move |s| chunks.lock().unwrap().push(s)
        });
        for (thread, turn) in [("other", "tu"), ("th", "other"), ("th", "tu")] {
            let p = json!({"threadId":thread,"turnId":turn,"itemId":"answer","delta":"{"});
            assert!(t
                .event("item/agentMessage/delta", &p, &cb)
                .unwrap()
                .is_none());
        }
        assert_eq!(*chunks.lock().unwrap(), ["{"]);
        let answer = message("answer", json!("final_answer"), "{\"n\":7}");
        for item in [answer, message("aside", json!("commentary"), "checking")] {
            t.event("item/completed", &scoped(json!({"item":item})), &cb)
                .unwrap();
        }
        let usage = json!({"last":{"totalTokens":8},"total":{"totalTokens":9}});
        t.event(
            "thread/tokenUsage/updated",
            &scoped(json!({"tokenUsage":usage})),
            &cb,
        )
        .unwrap();
        let mut end = completed("completed", json!([]));
        end["turn"]["id"] = json!("other");
        assert!(t.event("turn/completed", &end, &cb).unwrap().is_none());
        end["turn"]["id"] = json!("tu");
        let out = t.event("turn/completed", &end, &cb).unwrap().unwrap();
        assert_eq!(out.text, "{\"n\":7}");
        assert_eq!(out.data, Some(json!({"n":7})));
        assert_eq!(out.usage, Some(usage));
    }

    #[test]
    fn delta_only_fallback_does_not_merge_separate_messages() {
        let mut t = Turn::new("th".into(), "tu".into(), false);
        let cb: ProgressCallback = Arc::new(|_| {});
        for (id, delta) in [("a", "preamble"), ("b", "an"), ("b", "swer")] {
            t.event(
                "item/agentMessage/delta",
                &scoped(json!({"itemId":id,"delta":delta})),
                &cb,
            )
            .unwrap();
        }
        let out = t
            .event("turn/completed", &completed("completed", json!([])), &cb)
            .unwrap()
            .unwrap();
        assert_eq!(out.text, "answer");
    }

    #[test]
    fn newer_delta_supersedes_partial_unless_classified_as_commentary() {
        let cb: ProgressCallback = Arc::new(|_| {});
        for commentary in [false, true] {
            let mut t = Turn::new("th".into(), "tu".into(), false);
            let partial = message("old", json!("partial_answer"), "old");
            t.event("item/completed", &scoped(json!({"item": partial})), &cb)
                .unwrap();
            t.event(
                "item/agentMessage/delta",
                &scoped(json!({"itemId":"new","delta":"new"})),
                &cb,
            )
            .unwrap();
            let mut items = vec![partial];
            if commentary {
                items.push(message("new", json!("commentary"), "new"));
            }
            let out = t
                .event("turn/completed", &completed("completed", json!(items)), &cb)
                .unwrap()
                .unwrap();
            assert_eq!(out.text, if commentary { "old" } else { "new" });
        }
    }

    #[test]
    fn newer_completed_partial_supersedes_older_unfinished_delta() {
        let cb: ProgressCallback = Arc::new(|_| {});
        let mut t = Turn::new("th".into(), "tu".into(), false);
        t.event(
            "item/agentMessage/delta",
            &scoped(json!({"itemId":"old","delta":"old"})),
            &cb,
        )
        .unwrap();
        let partial = message("new", Value::Null, "new");
        t.event("item/completed", &scoped(json!({"item":partial})), &cb)
            .unwrap();
        let out = t
            .event("turn/completed", &completed("completed", json!([])), &cb)
            .unwrap()
            .unwrap();
        assert_eq!(out.text, "new");
    }

    #[test]
    fn snapshot_can_supply_a_phase_less_legacy_answer() {
        let cb: ProgressCallback = Arc::new(|_| {});
        let end = completed("completed", json!([message("a", Value::Null, "hello")]));
        for structured in [false, true] {
            let mut t = Turn::new("th".into(), "tu".into(), structured);
            let result = t.event("turn/completed", &end, &cb);
            if structured {
                assert!(result.is_err());
            } else {
                let out = result.unwrap().unwrap();
                assert_eq!(out.text, "hello");
                assert_eq!(out.data, None);
            }
        }
    }

    #[test]
    fn retryable_errors_are_not_terminal_but_failure_and_interruption_are() {
        let mut t = Turn::new("th".into(), "tu".into(), false);
        let cb: ProgressCallback = Arc::new(|_| {});
        let mut p = scoped(json!({"error":{"message":"offline"},"willRetry":true}));
        assert!(t.event("error", &p, &cb).unwrap().is_none());
        p["willRetry"] = json!(false);
        assert!(t.event("error", &p, &cb).unwrap_err().contains("offline"));
        for status in ["interrupted", "failed", "inProgress"] {
            let mut end = completed(status, json!([]));
            end["turn"]["error"] = json!({"message":"stopped"});
            assert!(t.event("turn/completed", &end, &cb).is_err());
        }
    }
}
