//! Isolated Monty execution. Only completed, idle sessions are checkpointed;
//! persistence and sequential scheduling are the worker's responsibility.

use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    time::Duration,
};

use futures_util::{StreamExt, stream::FuturesUnordered};
use monty_pool::{Pool, PoolConfig, PoolError, ReplConfig, ResumeValue, TurnEvent, on_print_sync};
use monty_types::{CallArgs, ExcType, MontyException, MontyObject, ObjectRef, ResourceLimits};
use serde_json::{Map, Value};
use tokio::sync::{OnceCell, watch};

use crate::{
    agents::agent_context::AgentContext,
    components::{cancellation::Cancellation, codemode_functions::CodeModeFunction},
    db::{MAX_CODEMODE_SNAPSHOT_BYTES, PrivateEvent, PrivateEventKind},
};

const MAX_OUTPUT_CHARS: usize = 16_000;
const MAX_PRINT_CHARS: usize = 12_000;
const MAX_SOURCE_BYTES: usize = 256 * 1024;
const MAX_HOST_CALLS: usize = 128;
const MAX_PENDING_CALLS: usize = 32;
// The registered tools currently require at most ten seconds of cooperative
// cleanup. Leave another second for draining and returning the result.
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);
const OUTER_CLEANUP_TIMEOUT: Duration = Duration::from_secs(11);
// A registered wait_message call can legitimately wait for 600 seconds;
// leave time for interpreter work and protocol overhead around it.
const WALL_TIMEOUT: Duration = Duration::from_secs(660);
const FEED_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) struct EngineOutput {
    pub text: String,
    pub error: bool,
    pub snapshot: Option<Vec<u8>>,
}

/// One instance is owned by one agent's bundle, and shared by its invocations.
pub(crate) struct MontyEngine {
    binary: PathBuf,
    pool: OnceCell<Pool>,
    shutdown: watch::Sender<bool>,
}

impl MontyEngine {
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            pool: OnceCell::new(),
            shutdown: watch::channel(false).0,
        }
    }

    /// Invalidate retained handles and wake active invocations.
    /// This is synchronous so the bundle can call it from Drop.
    pub fn shutdown(&self) {
        self.shutdown.send_replace(true);
    }

    pub async fn close(&self) {
        self.shutdown();
        if let Some(pool) = self.pool.get() {
            pool.close().await;
        }
    }

    pub async fn execute(
        &self,
        script: &str,
        context: &AgentContext<'_>,
        cancellation: &mut Cancellation,
    ) -> EngineOutput {
        let mut shutdown = self.shutdown.subscribe();
        if *shutdown.borrow() {
            return EngineOutput {
                text: "Code mode has shut down".into(),
                error: true,
                snapshot: None,
            };
        }

        let mut output = BoundedOutput::new(MAX_PRINT_CHARS);
        let (stop, stopped) = watch::channel(false);
        let mut execution = Box::pin(self.execute_script(script, context, &mut output, stopped));
        let result = tokio::select! {
            biased;
            _ = shutdown.changed() => {
                let _ = stop.send(true);
                let _ = tokio::time::timeout(OUTER_CLEANUP_TIMEOUT, &mut execution).await;
                Err("Code mode has shut down".to_owned())
            },
            _ = cancellation.cancelled() => {
                let _ = stop.send(true);
                let _ = tokio::time::timeout(OUTER_CLEANUP_TIMEOUT, &mut execution).await;
                Err("Execution cancelled".to_owned())
            },
            result = tokio::time::timeout(WALL_TIMEOUT, &mut execution) => match result {
                Ok(result) => result,
                Err(_) => {
                    let _ = stop.send(true);
                    let _ = tokio::time::timeout(OUTER_CLEANUP_TIMEOUT, &mut execution).await;
                    Err("Execution timed out".to_owned())
                }
            },
        };
        drop(execution);

        let (snapshot, error_message) = match result {
            Ok((snapshot, error)) => (Some(snapshot), error),
            Err(error) => (None, Some(error)),
        };

        let error = error_message.is_some();
        let mut text = output.finish();
        if let Some(message) = error_message {
            append_error_output(&mut text, &message, snapshot.is_none());
        }
        EngineOutput {
            text,
            error,
            snapshot,
        }
    }

    async fn execute_script(
        &self,
        script: &str,
        context: &AgentContext<'_>,
        output: &mut BoundedOutput,
        mut stopped: watch::Receiver<bool>,
    ) -> Result<(Vec<u8>, Option<String>), String> {
        if script.len() > MAX_SOURCE_BYTES {
            return Err("Script exceeds 256 KiB".into());
        }

        // The scheduler starts this call only after the previous result was committed.
        let snapshot = context
            .load_latest_private_event(PrivateEventKind::CodeModeSnapshot)
            .await
            .map_err(|e| format!("Cannot load checkpoint: {e}"))?
            .map(|event| match event {
                PrivateEvent::CodeModeSnapshot(bytes) => bytes,
            });
        if snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.len() > MAX_CODEMODE_SNAPSHOT_BYTES)
        {
            return Err("Checkpoint exceeds 16 MiB".into());
        }

        let pool = self
            .pool
            .get_or_try_init(|| async {
                let mut config = PoolConfig::subprocess(self.binary.clone());
                config.min_processes = 0;
                config.max_processes = 1;
                // Agents may be retired for a long time; do not retain one
                // subprocess per agent between checkpointed invocations.
                config.max_checkouts_per_worker = Some(1);
                config.checkout_timeout = Some(WALL_TIMEOUT);
                config.request_timeout = Some(FEED_TIMEOUT + Duration::from_secs(2));
                Pool::new(config).await
            })
            .await
            .map_err(|e| format!("Cannot initialize interpreter: {e}"))?;

        let repl = ReplConfig {
            limits: Some(
                ResourceLimits::default()
                    .max_feed_duration(FEED_TIMEOUT)
                    .max_memory(64 * 1024 * 1024),
            ),
            ..Default::default()
        };
        let mut session = pool.checkout(&repl).await.map_err(|e| e.to_string())?;
        let mut print = on_print_sync(|_, text: &str| output.push_str(text));
        if let Some(snapshot) = snapshot {
            let (event, _) = session
                .restore(snapshot, vec![], &mut print)
                .await
                .map_err(|e| format!("Cannot restore checkpoint: {e}"))?;
            if event.is_some() {
                return Err("Refusing suspended checkpoint".into());
            }
        }

        let mut pending = FuturesUnordered::new();
        let mut ready = HashMap::new();
        let (abort, aborted) = watch::channel(false);
        macro_rules! cancellable_turn {
            ($operation:expr) => {
                tokio::select! {
                    biased;
                    _ = stopped.changed() => {
                        let _ = abort.send(true);
                        while pending.next().await.is_some() {}
                        return Err("Execution cancelled".into());
                    }
                    result = $operation => result,
                }
            };
        }
        let mut event = cancellable_turn!(session.feed(script, vec![], vec![], false, &mut print));
        let mut call_count = 0;
        let exception = loop {
            if *stopped.borrow() {
                let _ = abort.send(true);
                while pending.next().await.is_some() {}
                return Err("Execution cancelled".into());
            }
            let current = match event {
                Ok(event) => event,
                Err(PoolError::Runtime(error))
                    if !matches!(
                        error.exc_type(),
                        ExcType::TimeoutError | ExcType::MemoryError
                    ) =>
                {
                    break Some(error.to_string());
                }
                Err(error) => {
                    let _ = abort.send(true);
                    while pending.next().await.is_some() {}
                    return Err(error.to_string());
                }
            };
            event = match current {
                TurnEvent::Complete(_) => break None,
                TurnEvent::FunctionCall {
                    function_name,
                    args,
                    call_id,
                    object_id,
                    ..
                } => {
                    call_count += 1;
                    if call_count > MAX_HOST_CALLS
                        || pending.len() + ready.len() >= MAX_PENDING_CALLS
                        || object_id.is_some()
                    {
                        let error = MontyException::new(
                            ExcType::RuntimeError,
                            Some("Host call limit exceeded or unsupported host object".into()),
                        );
                        cancellable_turn!(session.resume(ResumeValue::Error(error), &mut print))
                    } else {
                        let call = execute_function(context, function_name, args, aborted.clone());
                        pending.push(async move { (call_id, call.await) });
                        cancellable_turn!(session.resume(ResumeValue::Future, &mut print))
                    }
                }
                TurnEvent::ResolveFutures {
                    pending_call_ids, ..
                } => {
                    let (id, result) = loop {
                        if let Some(id) = pending_call_ids.iter().find(|id| ready.contains_key(*id))
                        {
                            break (*id, ready.remove(id).unwrap());
                        }
                        let item = tokio::select! {
                            biased;
                            _ = stopped.changed() => {
                                let _ = abort.send(true);
                                while pending.next().await.is_some() {}
                                return Err("Execution cancelled".into());
                            }
                            item = pending.next() => item,
                        };
                        let Some((id, result)) = item else {
                            return Err("Interpreter requested an unknown host future".into());
                        };
                        ready.insert(id, result);
                    };
                    cancellable_turn!(session.resume_futures(vec![(id, result)], &mut print))
                }
                TurnEvent::NameLookup { .. } => {
                    cancellable_turn!(session.resume_name_lookup(None, &mut print))
                }
                TurnEvent::OsCall { .. } => {
                    cancellable_turn!(session.resume(ResumeValue::NotHandled, &mut print))
                }
            };
        };
        // No detached callbacks may outlive the turn or race checkpoint persistence.
        if !pending.is_empty() || !ready.is_empty() {
            let _ = abort.send(true);
            while pending.next().await.is_some() {}
            return Err(unresolved_calls_error(exception.as_deref()));
        }
        drop(print);
        let snapshot = session
            .dump()
            .await
            .map_err(|e| format!("Cannot serialize checkpoint: {e}"))?;
        if snapshot.len() > MAX_CODEMODE_SNAPSHOT_BYTES {
            return Err("Checkpoint exceeds 16 MiB".into());
        }
        session.finish().await.map_err(|e| e.to_string())?;
        // Runtime errors can also be a rejected host reply, in which case
        // Monty leaves the session suspended rather than ending the feed.
        // Verify the dump on a fresh checkout before allowing persistence;
        // restoring only re-announces a suspension and executes no host call.
        if let Some(message) = &exception
            && let Err(reason) = validate_idle_snapshot(pool, &repl, &snapshot).await
        {
            return Err(format!("Checkpoint discarded: {reason}\n{message}"));
        }
        Ok((snapshot, exception))
    }
}

async fn validate_idle_snapshot(
    pool: &Pool,
    repl: &ReplConfig,
    snapshot: &[u8],
) -> Result<(), String> {
    let mut check = pool.checkout(repl).await.map_err(|e| e.to_string())?;
    let mut no_print = on_print_sync(|_, _: &str| {});
    let (event, _) = check
        .restore(snapshot.to_vec(), vec![], &mut no_print)
        .await
        .map_err(|e| e.to_string())?;
    check.finish().await.map_err(|e| e.to_string())?;
    if event.is_some() {
        return Err("interpreter is suspended".into());
    }
    Ok(())
}

async fn execute_function(
    context: &AgentContext<'_>,
    name: String,
    args: CallArgs,
    mut aborted: watch::Receiver<bool>,
) -> ResumeValue {
    let Some(function) = context
        .codemode_functions()
        .iter()
        .find(|f| f.definition().name == name)
    else {
        return ResumeValue::Error(MontyException::new(
            ExcType::NameError,
            Some(format!("Unknown function: {name}")),
        ));
    };
    let arguments = match parse_call_arguments(&args) {
        Ok(value) => value,
        Err(error) => {
            return ResumeValue::Error(MontyException::new(ExcType::TypeError, Some(error)));
        }
    };
    let Some(mut child) = context.child_invocation() else {
        return ResumeValue::Error(MontyException::new(
            ExcType::RuntimeError,
            Some("Nested invocation unavailable".into()),
        ));
    };
    let (sender, cancellation) = Cancellation::channel();
    let mut invocation = Box::pin(function.execute(arguments, &mut child, cancellation));
    let result = tokio::select! {
        biased;
        _ = aborted.changed() => {
            let _ = sender.send(());
            let _ = tokio::time::timeout(function.cancellation_timeout().min(CLEANUP_TIMEOUT), &mut invocation).await;
            return ResumeValue::Error(MontyException::new(ExcType::RuntimeError, Some("Host call cancelled".into())));
        }
        result = &mut invocation => result,
    };
    match result {
        Ok(value) => match json_to_monty(value) {
            Ok(value) => ResumeValue::Return(value),
            Err(message) => {
                ResumeValue::Error(MontyException::new(ExcType::TypeError, Some(message)))
            }
        },
        Err(error) => {
            let kind = error
                .exception_type
                .parse::<ExcType>()
                .unwrap_or(ExcType::RuntimeError);
            ResumeValue::Error(MontyException::new(kind, Some(error.message)))
        }
    }
}

fn parse_call_arguments(args: &CallArgs) -> Result<Value, String> {
    let mut budget = 100_000;
    let positional: Vec<_> = args.args().collect();
    if positional.len() == 1 && args.kwargs().len() == 0 {
        let value = monty_to_json(positional[0], 0, &mut budget)?;
        return if value.is_object() {
            Ok(value)
        } else {
            Err("Expected an argument object".into())
        };
    }
    if !positional.is_empty() {
        return Err("Use keyword arguments or one argument object".into());
    }
    let mut result = Map::new();
    for (key, value) in args.kwargs() {
        let key = key.as_str().ok_or("Keyword must be a string")?;
        if result
            .insert(key.to_owned(), monty_to_json(value, 0, &mut budget)?)
            .is_some()
        {
            return Err("Duplicate keyword argument".into());
        }
    }
    Ok(Value::Object(result))
}

fn monty_to_json(value: ObjectRef<'_>, depth: usize, budget: &mut usize) -> Result<Value, String> {
    if depth > 64 || *budget == 0 {
        return Err("Argument structure is too large or cyclic".into());
    }
    *budget -= 1;
    match value.type_name() {
        "NoneType" => Ok(Value::Null),
        "bool" => Ok(Value::Bool(value.as_bool().unwrap())),
        "int" => value
            .as_int()
            .map(Value::from)
            .ok_or_else(|| "Integer is outside JSON range".into()),
        "float" => value
            .as_float()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .ok_or_else(|| "Non-finite float".into()),
        "str" => Ok(Value::String(value.as_str().unwrap().to_owned())),
        "list" | "tuple" => value
            .items()
            .unwrap()
            .into_iter()
            .map(|v| monty_to_json(v, depth + 1, budget))
            .collect(),
        "dict" => {
            let mut object = Map::new();
            for (key, value) in value.pairs().unwrap() {
                let key = key.as_str().ok_or("Object keys must be strings")?;
                object.insert(key.to_owned(), monty_to_json(value, depth + 1, budget)?);
            }
            Ok(Value::Object(object))
        }
        _ => Err(format!("Unsupported argument type: {}", value.type_name())),
    }
}

fn json_to_monty(value: Value) -> Result<MontyObject, String> {
    json_to_monty_inner(value, 0, &mut 100_000)
}

fn json_to_monty_inner(
    value: Value,
    depth: usize,
    budget: &mut usize,
) -> Result<MontyObject, String> {
    if depth > 64 || *budget == 0 {
        return Err("Host result structure is too large".into());
    }
    *budget -= 1;
    Ok(match value {
        Value::Null => MontyObject::none(),
        Value::Bool(v) => MontyObject::bool(v),
        Value::Number(v) => {
            if let Some(n) = v.as_i64() {
                MontyObject::int(n)
            } else if v.is_u64() {
                return Err("Integer is outside supported range".into());
            } else {
                MontyObject::float(v.as_f64().ok_or("Invalid number")?)
            }
        }
        Value::String(v) => MontyObject::string(v),
        Value::Array(v) => MontyObject::list(
            v.into_iter()
                .map(|value| json_to_monty_inner(value, depth + 1, budget))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Value::Object(v) => MontyObject::dict(
            v.into_iter()
                .map(|(k, v)| {
                    Ok((
                        MontyObject::string(k),
                        json_to_monty_inner(v, depth + 1, budget)?,
                    ))
                })
                .collect::<Result<Vec<_>, String>>()?,
        ),
    })
}

fn unresolved_calls_error(exception: Option<&str>) -> String {
    const DIAGNOSTIC: &str = "Execution ended with unresolved host calls";
    match exception {
        Some(exception) => format!("{DIAGNOSTIC}\n{exception}"),
        None => DIAGNOSTIC.into(),
    }
}

fn append_error_output(text: &mut String, message: &str, not_saved: bool) {
    const NOT_SAVED: &str = "REPL changes were not saved; external tool effects may have occurred.";
    if !text.is_empty() {
        text.push('\n');
    }
    if not_saved {
        text.push_str(NOT_SAVED);
        text.push('\n');
    }
    let room = MAX_OUTPUT_CHARS.saturating_sub(text.chars().count());
    let mut bounded_error = BoundedOutput::new(room);
    bounded_error.push_str(message);
    text.push_str(&bounded_error.finish());
}

#[cfg(test)]
mod tests;

// The shared middle truncation helper counts bytes; this streaming buffer bounds
// retained output and applies the code-mode limits in Unicode characters instead.
struct BoundedOutput {
    text: String,
    chars: usize,
    tail: VecDeque<char>,
    max: usize,
    truncated: bool,
}

impl BoundedOutput {
    fn new(max: usize) -> Self {
        Self {
            text: String::new(),
            chars: 0,
            tail: VecDeque::new(),
            max,
            truncated: false,
        }
    }
    fn push_str(&mut self, text: &str) {
        let remaining = self.max.saturating_sub(self.chars);
        let end = text
            .char_indices()
            .nth(remaining)
            .map_or(text.len(), |(i, _)| i);
        self.text.push_str(&text[..end]);
        self.chars += text[..end].chars().count();
        self.truncated |= end < text.len();

        // Retain only enough trailing characters for the middle-truncated form.
        let tail_capacity = self.max / 2;
        if tail_capacity == 0 {
            return;
        }
        let start = text
            .char_indices()
            .rev()
            .nth(tail_capacity - 1)
            .map_or(0, |(index, _)| index);
        for character in text[start..].chars() {
            if self.tail.len() == tail_capacity {
                self.tail.pop_front();
            }
            self.tail.push_back(character);
        }
    }
    fn finish(mut self) -> String {
        if self.truncated {
            const MARKER: &str = "\n[output truncated]\n";
            let marker_chars = MARKER.chars().count().min(self.max);
            let retained = self.max - marker_chars;
            let head_chars = retained.div_ceil(2);
            let tail_chars = retained / 2;
            if let Some((end, _)) = self.text.char_indices().nth(head_chars) {
                self.text.truncate(end);
            }
            self.text.extend(MARKER.chars().take(marker_chars));
            self.text
                .extend(self.tail.iter().skip(self.tail.len() - tail_chars));
        }
        self.text
    }
}
