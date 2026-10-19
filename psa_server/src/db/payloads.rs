use super::{
    DbError,
    schema::{decode_integer, encode_integer},
};
use crate::{
    agents::{AgentStatus, events::*},
    tools::ToolId,
};
use llm_api::{
    api::{self, Outcome, Usage},
    providers::{Effort, Model, ModelConfig, ProviderType},
};
use serde::{Serialize, de::DeserializeOwned};
use sqlx::SqliteConnection;
use std::{collections::HashMap, time::Duration};

pub(super) fn event_kind(event: &EventType) -> &'static str {
    match event {
        EventType::StatusChange(_) => "status_change",
        EventType::Model(_) => "model",
        EventType::NewContext => "new_context",
        EventType::Message(_) => "message",
        EventType::Thinking(_) => "thinking",
        EventType::ProviderItem(_) => "provider_item",
        EventType::ToolDefinition(_) => "tool_definition",
        EventType::ToolCall(_) => "tool_call",
        EventType::ToolResult(_) => "tool_result",
        EventType::Compaction(_) => "compaction",
        EventType::Log(_) => "log",
        // Stable on-disk tags from migration 0001; the payload tables were renamed.
        EventType::InferenceResultStart(_) => "inference_start",
        EventType::InferenceResultEnd(_) => "inference_end",
    }
}

fn serialize_json(value: &impl Serialize) -> Result<String, DbError> {
    Ok(serde_json::to_string(value)?)
}

fn parse_json<T: DeserializeOwned>(value: &str) -> Result<T, DbError> {
    Ok(serde_json::from_str(value)?)
}

fn invalid(value: &str) -> DbError {
    DbError::Corrupt(format!("unknown stored discriminator: {value}"))
}

fn parse_model(provider: &str, model: String, effort: &str) -> Result<ModelConfig, DbError> {
    Ok(ModelConfig {
        provider: match provider {
            "openai_codex" => ProviderType::OpenAiCodex,
            "openrouter" => ProviderType::Openrouter,
            other => return Err(invalid(other)),
        },
        model: Model::new(model).map_err(|error| DbError::Corrupt(error.to_string()))?,
        effort: match effort {
            "low" => Effort::Low,
            "medium" => Effort::Medium,
            "high" => Effort::High,
            "xhigh" => Effort::Xhigh,
            "max" => Effort::Max,
            other => return Err(invalid(other)),
        },
    })
}

fn encode_native(item: &api::HistoryItem) -> Result<String, DbError> {
    if !matches!(item.kind, api::HistoryItemKind::ProviderItem(_)) {
        return Err(DbError::InvalidInput(
            "native replay requires a provider item".into(),
        ));
    }
    serialize_json(item)
}

fn decode_native(value: &str) -> Result<api::HistoryItem, DbError> {
    let item: api::HistoryItem = parse_json(value)?;
    if !matches!(item.kind, api::HistoryItemKind::ProviderItem(_)) {
        return Err(DbError::Corrupt(
            "native replay contains a non-provider item".into(),
        ));
    }
    Ok(item)
}

struct StoredUsage {
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    total_tokens: Option<i64>,
    cached_tokens: Option<i64>,
    reasoning_tokens: Option<i64>,
    usage_details: String,
}

impl StoredUsage {
    fn encode(usage: &Usage) -> Result<Self, DbError> {
        Ok(Self {
            input_tokens: usage.input_tokens.map(encode_integer).transpose()?,
            output_tokens: usage.output_tokens.map(encode_integer).transpose()?,
            total_tokens: usage.total_tokens.map(encode_integer).transpose()?,
            cached_tokens: usage.cached_input_tokens.map(encode_integer).transpose()?,
            reasoning_tokens: usage
                .reasoning_output_tokens
                .map(encode_integer)
                .transpose()?,
            usage_details: serialize_json(&usage.provider_details)?,
        })
    }

    fn decode(self) -> Result<Usage, DbError> {
        Ok(Usage {
            input_tokens: self.input_tokens.map(decode_integer).transpose()?,
            output_tokens: self.output_tokens.map(decode_integer).transpose()?,
            total_tokens: self.total_tokens.map(decode_integer).transpose()?,
            cached_input_tokens: self.cached_tokens.map(decode_integer).transpose()?,
            reasoning_output_tokens: self.reasoning_tokens.map(decode_integer).transpose()?,
            provider_details: parse_json(&self.usage_details)?,
        })
    }
}

pub(super) async fn insert_payload(
    conn: &mut SqliteConnection,
    pk: i64,
    event: &EventType,
) -> Result<(), DbError> {
    match event {
        EventType::StatusChange(value) => {
            let status = match value.new_status {
                AgentStatus::Idle => "idle",
                AgentStatus::Running => "running",
                AgentStatus::Interrupted => "interrupted",
            };
            sqlx::query!(
                "INSERT INTO event_status_change (event_pk,status,reason) VALUES (?,?,?)",
                pk,
                status,
                value.reason
            )
            .execute(&mut *conn)
            .await?;
        }
        EventType::Model(value) => {
            Model::new(value.model.api_name.clone())
                .map_err(|error| DbError::InvalidInput(error.to_string()))?;
            let provider = match value.provider {
                ProviderType::OpenAiCodex => "openai_codex",
                ProviderType::Openrouter => "openrouter",
            };
            let effort = value.effort.as_str();
            sqlx::query!(
                "INSERT INTO event_model (event_pk,provider,model,effort) VALUES (?,?,?,?)",
                pk,
                provider,
                value.model.api_name,
                effort
            )
            .execute(&mut *conn)
            .await?;
        }
        EventType::NewContext => {
            sqlx::query!("INSERT INTO event_new_context (event_pk) VALUES (?)", pk)
                .execute(&mut *conn)
                .await?;
        }
        EventType::Message(value) => {
            let message_type = match value.message_type {
                MessageType::Human => "human",
                MessageType::Agent => "agent",
                MessageType::Harness => "harness",
                MessageType::System => "system",
            };
            let delivery = value.delivery.map(|d| match d {
                MessageDelivery::Queue => "queue",
                MessageDelivery::Steer => "steer",
            });
            let metadata = serialize_json(&value.metadata)?;
            let content = value
                .replay_content
                .as_ref()
                .map(serialize_json)
                .transpose()?;

            sqlx::query!(
                "INSERT INTO event_message (event_pk,message_type,delivery,contents,metadata,replay_content) VALUES (?,?,?,?,?,?)",
                pk,
                message_type,
                delivery,
                value.contents,
                metadata,
                content
            )
            .execute(&mut *conn)
            .await?;
        }
        EventType::Thinking(value) => {
            let reasoning = serialize_json(&value.contents)?;
            let metadata = serialize_json(&value.metadata)?;
            sqlx::query!(
                "INSERT INTO event_thinking (event_pk,reasoning,metadata) VALUES (?,?,?)",
                pk,
                reasoning,
                metadata
            )
            .execute(&mut *conn)
            .await?;
        }
        EventType::ProviderItem(value) => {
            let item = encode_native(value)?;
            sqlx::query!(
                "INSERT INTO event_provider_item (event_pk,item) VALUES (?,?)",
                pk,
                item
            )
            .execute(&mut *conn)
            .await?;
        }
        EventType::ToolDefinition(value) => {
            let tool_id = match value.id {
                ToolId::Noop => "noop",
            };
            let def = &value.definition;
            let parameters = serialize_json(&def.parameters)?;

            sqlx::query!(
                "INSERT INTO event_tool_definition (event_pk,tool_id,name,description,parameters,strict) VALUES (?,?,?,?,?,?)",
                pk,
                tool_id,
                def.name,
                def.description,
                parameters,
                def.strict
            )
            .execute(&mut *conn)
            .await?;
        }
        EventType::ToolCall(value) => {
            let metadata = serialize_json(&value.metadata)?;
            sqlx::query!(
                "INSERT INTO event_tool_call (event_pk,call_id,tool_name,arguments,metadata) VALUES (?,?,?,?,?)",
                pk,
                value.call_id,
                value.tool_name,
                value.arguments,
                metadata
            )
            .execute(&mut *conn)
            .await?;
        }
        EventType::ToolResult(value) => {
            let metadata = serialize_json(&value.metadata)?;
            let content = serialize_json(&value.content)?;

            sqlx::query!(
                "INSERT INTO event_tool_result (event_pk,call_id,content,error,metadata) VALUES (?,?,?,?,?)",
                pk,
                value.call_id,
                content,
                value.error,
                metadata
            )
            .execute(&mut *conn)
            .await?;
        }
        EventType::Compaction(value) => {
            let (summary_type, summary) = match &value.summary {
                CompactionSummary::Text(s) => ("text", s.clone()),
                CompactionSummary::ProviderItem(s) => ("provider_item", encode_native(s)?),
            };
            let usage = StoredUsage::encode(&value.usage)?;

            sqlx::query!(
                "INSERT INTO event_compaction (event_pk,summary_type,summary,input_tokens,output_tokens,total_tokens,cached_tokens,reasoning_tokens,usage_details) VALUES (?,?,?,?,?,?,?,?,?)",
                pk,
                summary_type,
                summary,
                usage.input_tokens,
                usage.output_tokens,
                usage.total_tokens,
                usage.cached_tokens,
                usage.reasoning_tokens,
                usage.usage_details
            )
            .execute(&mut *conn)
            .await?;
        }
        EventType::Log(value) => {
            sqlx::query!(
                "INSERT INTO event_log (event_pk,error,message) VALUES (?,?,?)",
                pk,
                value.error,
                value.message
            )
            .execute(&mut *conn)
            .await?;
        }
        EventType::InferenceResultStart(value) => {
            let id = encode_integer(value.id)?;
            Model::new(value.model_config.model.api_name.clone())
                .map_err(|error| DbError::InvalidInput(error.to_string()))?;
            let provider = match value.model_config.provider {
                ProviderType::OpenAiCodex => "openai_codex",
                ProviderType::Openrouter => "openrouter",
            };
            let effort = value.model_config.effort.as_str();
            let max_output_tokens = value
                .request_config
                .max_output_tokens
                .map(encode_integer)
                .transpose()?;
            let tool_choice = value
                .request_config
                .tool_choice
                .as_ref()
                .map(serialize_json)
                .transpose()?;

            sqlx::query!(
                "INSERT INTO event_inference_result_start (event_pk,inference_id,model_provider,model_name,model_effort,max_output_tokens,tool_choice,parallel_tool_calls) VALUES (?,?,?,?,?,?,?,?)",
                pk,
                id,
                provider,
                value.model_config.model.api_name,
                effort,
                max_output_tokens,
                tool_choice,
                value.request_config.parallel_tool_calls
            )
            .execute(&mut *conn)
            .await?;
        }
        EventType::InferenceResultEnd(value) => {
            let id = encode_integer(value.id)?;
            let (outcome, reason, failure) = match &value.outcome {
                Outcome::Completed { reason } => ("completed", Some(serialize_json(reason)?), None),
                Outcome::Incomplete { reason } => {
                    ("incomplete", Some(serialize_json(reason)?), None)
                }
                Outcome::Failed(error) => ("failed", None, Some(serialize_json(error)?)),
                Outcome::Cancelled => ("cancelled", None, None),
                Outcome::InProgress => {
                    return Err(DbError::InvalidInput(
                        "result end must have a terminal outcome".into(),
                    ));
                }
            };
            let duration = i64::try_from(value.request_time.as_micros())
                .map_err(|_| DbError::InvalidInput("duration exceeds SQLite range".into()))?;
            let metadata = serialize_json(&value.provider_metadata)?;
            let usage = StoredUsage::encode(&value.usage)?;

            sqlx::query!(
                "INSERT INTO event_inference_result_end (event_pk,inference_id,outcome,stop_reason,failure,request_time_us,response_id,provider_metadata,input_tokens,output_tokens,total_tokens,cached_tokens,reasoning_tokens,usage_details) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                pk,
                id,
                outcome,
                reason,
                failure,
                duration,
                value.response_id,
                metadata,
                usage.input_tokens,
                usage.output_tokens,
                usage.total_tokens,
                usage.cached_tokens,
                usage.reasoning_tokens,
                usage.usage_details
            )
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

pub(super) async fn load_payloads(
    conn: &mut SqliteConnection,
    agent_id: i64,
    lower: i64,
    upper: i64,
) -> Result<HashMap<i64, EventType>, DbError> {
    let mut payloads = HashMap::new();
    macro_rules! load {
        ($query:literal, |$r:ident| $event:expr) => {
            for $r in sqlx::query!($query, agent_id, lower, upper)
                .fetch_all(&mut *conn)
                .await?
            {
                if payloads.insert($r.event_pk, $event).is_some() {
                    return Err(DbError::Corrupt("multiple payloads for one event".into()));
                }
            }
        };
    }
    macro_rules! usage {
        ($r:ident) => {
            StoredUsage {
                input_tokens: $r.input_tokens,
                output_tokens: $r.output_tokens,
                total_tokens: $r.total_tokens,
                cached_tokens: $r.cached_tokens,
                reasoning_tokens: $r.reasoning_tokens,
                usage_details: $r.usage_details,
            }
            .decode()?
        };
    }
    load!(
        "SELECT p.event_pk, p.status, p.reason FROM event_status_change p JOIN events e ON e.event_pk=p.event_pk WHERE e.agent_id=? AND e.event_seq BETWEEN ? AND ?",
        |r| EventType::StatusChange(StatusChange {
            new_status: match r.status.as_str() {
                "idle" => AgentStatus::Idle,
                "running" => AgentStatus::Running,
                "interrupted" => AgentStatus::Interrupted,
                s => return Err(invalid(s)),
            },
            reason: r.reason
        })
    );
    load!(
        "SELECT p.event_pk, p.provider, p.model, p.effort FROM event_model p JOIN events e ON e.event_pk=p.event_pk WHERE e.agent_id=? AND e.event_seq BETWEEN ? AND ?",
        |r| EventType::Model(parse_model(&r.provider, r.model, &r.effort)?)
    );
    load!(
        "SELECT p.event_pk FROM event_new_context p JOIN events e ON e.event_pk=p.event_pk WHERE e.agent_id=? AND e.event_seq BETWEEN ? AND ?",
        |r| EventType::NewContext
    );
    load!(
        "SELECT p.event_pk, p.message_type, p.delivery, p.contents, p.metadata, p.replay_content FROM event_message p JOIN events e ON e.event_pk=p.event_pk WHERE e.agent_id=? AND e.event_seq BETWEEN ? AND ?",
        |r| EventType::Message(Message {
            message_type: match r.message_type.as_str() {
                "human" => MessageType::Human,
                "agent" => MessageType::Agent,
                "harness" => MessageType::Harness,
                "system" => MessageType::System,
                s => return Err(invalid(s)),
            },
            delivery: match r.delivery.as_deref() {
                None => None,
                Some("queue") => Some(MessageDelivery::Queue),
                Some("steer") => Some(MessageDelivery::Steer),
                Some(s) => return Err(invalid(s)),
            },
            contents: r.contents,
            metadata: parse_json(&r.metadata)?,
            replay_content: r.replay_content.as_deref().map(parse_json).transpose()?
        })
    );
    load!(
        "SELECT p.event_pk, p.reasoning, p.metadata FROM event_thinking p JOIN events e ON e.event_pk=p.event_pk WHERE e.agent_id=? AND e.event_seq BETWEEN ? AND ?",
        |r| EventType::Thinking(Thinking {
            contents: parse_json(&r.reasoning)?,
            metadata: parse_json(&r.metadata)?
        })
    );
    load!(
        "SELECT p.event_pk, p.item FROM event_provider_item p JOIN events e ON e.event_pk=p.event_pk WHERE e.agent_id=? AND e.event_seq BETWEEN ? AND ?",
        |r| EventType::ProviderItem(decode_native(&r.item)?)
    );
    load!(
        "SELECT p.event_pk, p.tool_id, p.name, p.description, p.parameters, p.strict FROM event_tool_definition p JOIN events e ON e.event_pk=p.event_pk WHERE e.agent_id=? AND e.event_seq BETWEEN ? AND ?",
        |r| EventType::ToolDefinition(ToolDefinition {
            id: match r.tool_id.as_str() {
                "noop" => ToolId::Noop,
                s => return Err(invalid(s)),
            },
            definition: api::ToolDefinition {
                name: r.name,
                description: r.description,
                parameters: parse_json(&r.parameters)?,
                strict: r.strict.map(|v| v != 0)
            }
        })
    );
    load!(
        "SELECT p.event_pk, p.call_id, p.tool_name, p.arguments, p.metadata FROM event_tool_call p JOIN events e ON e.event_pk=p.event_pk WHERE e.agent_id=? AND e.event_seq BETWEEN ? AND ?",
        |r| EventType::ToolCall(ToolCall {
            call_id: r.call_id,
            tool_name: r.tool_name,
            arguments: r.arguments,
            metadata: parse_json(&r.metadata)?
        })
    );
    load!(
        "SELECT p.event_pk, p.call_id, p.content, p.error, p.metadata FROM event_tool_result p JOIN events e ON e.event_pk=p.event_pk WHERE e.agent_id=? AND e.event_seq BETWEEN ? AND ?",
        |r| EventType::ToolResult(ToolResult {
            call_id: r.call_id,
            content: parse_json(&r.content)?,
            error: r.error != 0,
            metadata: parse_json(&r.metadata)?
        })
    );
    load!(
        "SELECT p.event_pk, p.summary_type, p.summary, p.input_tokens, p.output_tokens, p.total_tokens, p.cached_tokens, p.reasoning_tokens, p.usage_details FROM event_compaction p JOIN events e ON e.event_pk=p.event_pk WHERE e.agent_id=? AND e.event_seq BETWEEN ? AND ?",
        |r| EventType::Compaction(Compaction {
            summary: match r.summary_type.as_str() {
                "text" => CompactionSummary::Text(r.summary),
                "provider_item" => CompactionSummary::ProviderItem(decode_native(&r.summary)?),
                s => return Err(invalid(s)),
            },
            usage: usage!(r)
        })
    );
    load!(
        "SELECT p.event_pk, p.error, p.message FROM event_log p JOIN events e ON e.event_pk=p.event_pk WHERE e.agent_id=? AND e.event_seq BETWEEN ? AND ?",
        |r| EventType::Log(Log {
            error: r.error != 0,
            message: r.message
        })
    );
    load!(
        "SELECT p.event_pk, p.inference_id, p.model_provider, p.model_name, p.model_effort, p.max_output_tokens, p.tool_choice, p.parallel_tool_calls FROM event_inference_result_start p JOIN events e ON e.event_pk=p.event_pk WHERE e.agent_id=? AND e.event_seq BETWEEN ? AND ?",
        |r| EventType::InferenceResultStart(InferenceResultStart {
            id: decode_integer(r.inference_id)?,
            model_config: parse_model(
                r.model_provider.as_deref().ok_or_else(|| DbError::Corrupt(
                    "legacy result start has no model snapshot".into()
                ))?,
                r.model_name
                    .ok_or_else(|| DbError::Corrupt("result start has no model name".into()))?,
                r.model_effort
                    .as_deref()
                    .ok_or_else(|| DbError::Corrupt("result start has no model effort".into()))?
            )?,
            request_config: api::RequestConfig {
                max_output_tokens: r.max_output_tokens.map(decode_integer).transpose()?,
                tool_choice: r.tool_choice.as_deref().map(parse_json).transpose()?,
                parallel_tool_calls: r.parallel_tool_calls.map(|v| v != 0)
            }
        })
    );
    load!(
        "SELECT p.event_pk, p.inference_id, p.outcome, p.stop_reason, p.failure, p.request_time_us, p.response_id, p.provider_metadata, p.input_tokens, p.output_tokens, p.total_tokens, p.cached_tokens, p.reasoning_tokens, p.usage_details FROM event_inference_result_end p JOIN events e ON e.event_pk=p.event_pk WHERE e.agent_id=? AND e.event_seq BETWEEN ? AND ?",
        |r| EventType::InferenceResultEnd(InferenceResultEnd {
            id: decode_integer(r.inference_id)?,
            outcome: match r.outcome.as_str() {
                "completed" => Outcome::Completed {
                    reason: parse_json(
                        r.stop_reason
                            .as_deref()
                            .ok_or_else(|| invalid("missing stop reason"))?
                    )?
                },
                "incomplete" => Outcome::Incomplete {
                    reason: parse_json(
                        r.stop_reason
                            .as_deref()
                            .ok_or_else(|| invalid("missing stop reason"))?
                    )?
                },
                "failed" => Outcome::Failed(parse_json(
                    r.failure
                        .as_deref()
                        .ok_or_else(|| invalid("missing failure"))?
                )?),
                "cancelled" => Outcome::Cancelled,
                s => return Err(invalid(s)),
            },
            request_time: Duration::from_micros(decode_integer(r.request_time_us)?),
            response_id: r.response_id,
            provider_metadata: parse_json(&r.provider_metadata)?,
            usage: usage!(r)
        })
    );
    Ok(payloads)
}
