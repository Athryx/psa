//! Durable agent result artifacts and typed decoding that preserves provenance.

use crate::{
    AgentId, EventEnvelope, SessionId,
    events::{EventId, EventType},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use time::OffsetDateTime;

/// Default number of result-correction retries configured by the typed-result bundle.
pub const DEFAULT_RESULT_RETRIES: u32 = 3;

/// A durable agent-produced result, distinct from ordinary conversation messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentResult {
    /// A plain-text result; decoding treats it as a JSON string, not JSON source.
    Text(
        /// Result text.
        String,
    ),
    /// A structured JSON result.
    Json(
        /// Result value without an additional wrapper around the payload.
        serde_json::Value,
    ),
}

impl AgentResult {
    /// Deserialize the inner payload into `T`, without decoding the enum's wire tag.
    /// Text is supplied as a JSON string; JSON payloads retain their original type.
    /// Returns an error when the payload does not match `T`.
    pub fn decode<T: DeserializeOwned>(&self) -> serde_json::Result<T> {
        let value = match self {
            Self::Text(text) => serde_json::Value::String(text.clone()),
            Self::Json(value) => value.clone(),
        };
        serde_json::from_value(value)
    }
}

/// A result artifact with its durable event identity and session-view provenance.
/// The default payload type retains the result's text-or-JSON wire representation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultRecord<T = AgentResult> {
    /// Session in which this result is viewed.
    pub session_id: SessionId,
    /// Agent whose visible lineage contains this result.
    pub agent_id: AgentId,
    /// Result event's sequence number within that agent's lineage.
    pub event_id: EventId,
    /// Physical owner of the event, which may be a fork ancestor.
    pub source_agent_id: AgentId,
    /// Session-wide position of the result event.
    pub position: u64,
    /// Result event's timestamp, serialized in RFC 3339 format.
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    /// Raw or decoded result payload.
    pub value: T,
}

impl ResultRecord<AgentResult> {
    /// Decode the inner result payload into `T` while retaining all event metadata.
    /// Consumes this record; text payloads are strings rather than parsed JSON source.
    pub fn decode<T: DeserializeOwned>(self) -> serde_json::Result<ResultRecord<T>> {
        let value = self.value.decode()?;
        Ok(ResultRecord {
            session_id: self.session_id,
            agent_id: self.agent_id,
            event_id: self.event_id,
            source_agent_id: self.source_agent_id,
            position: self.position,
            timestamp: self.timestamp,
            value,
        })
    }
}

impl EventEnvelope {
    /// Extract a durable result while preserving its event and session-view metadata.
    /// Returns `None` for ordinary transcript events rather than synthesizing a result.
    pub fn into_result_record(self) -> Option<ResultRecord> {
        let EventType::Result(value) = self.event.kind else {
            return None;
        };
        Some(ResultRecord {
            session_id: self.session_id,
            agent_id: self.agent_id,
            event_id: self.event.id,
            source_agent_id: self.event.source_agent_id,
            position: self.position,
            timestamp: self.event.timestamp,
            value,
        })
    }

    /// Decode a durable result's inner payload into `T`, retaining all provenance.
    /// Returns `Ok(None)` for non-result events and an error for incompatible result
    /// payloads. Text results decode as strings, not as JSON source.
    pub fn decode_result<T: DeserializeOwned>(self) -> serde_json::Result<Option<ResultRecord<T>>> {
        self.into_result_record()
            .map(|record| record.decode::<T>())
            .transpose()
    }
}

/// Query for an agent's durable results, returned newest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ResultsQuery {
    /// Maximum number of results to return; defaults to one.
    pub limit: u32,
}

impl Default for ResultsQuery {
    fn default() -> Self {
        Self { limit: 1 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct Answer {
        count: u32,
    }

    fn record(value: AgentResult) -> ResultRecord {
        ResultRecord {
            session_id: SessionId(3),
            agent_id: AgentId(5),
            event_id: EventId(7),
            source_agent_id: AgentId(2),
            position: 11,
            timestamp: OffsetDateTime::from_unix_timestamp(123).unwrap(),
            value,
        }
    }

    fn envelope(kind: EventType) -> EventEnvelope {
        EventEnvelope {
            session_id: SessionId(3),
            agent_id: AgentId(5),
            position: 11,
            event: crate::events::Event {
                id: EventId(7),
                source_agent_id: AgentId(2),
                timestamp: OffsetDateTime::from_unix_timestamp(123).unwrap(),
                kind,
            },
        }
    }

    #[test]
    fn event_result_conversions_preserve_all_provenance() {
        let result = AgentResult::Json(json!({"count": 4}));
        let event = envelope(EventType::Result(result.clone()));
        let expected = record(result);
        assert_eq!(event.clone().into_result_record().unwrap(), expected);
        assert_eq!(
            event.decode_result::<Answer>().unwrap().unwrap(),
            expected.decode::<Answer>().unwrap(),
        );
    }

    #[test]
    fn ordinary_event_conversions_return_no_result() {
        let event = envelope(EventType::NewContext);
        assert!(event.clone().into_result_record().is_none());
        assert!(event.decode_result::<Answer>().unwrap().is_none());
    }

    #[test]
    fn event_result_decoding_reports_incompatible_payloads() {
        let event = envelope(EventType::Result(AgentResult::Json(
            json!({"count": "wrong type"}),
        )));
        assert!(event.decode_result::<Answer>().is_err());
    }

    #[test]
    fn result_payloads_roundtrip_and_decode_without_wire_tags() {
        let text = AgentResult::Text("{\"count\":4}".into());
        assert_eq!(
            serde_json::to_value(&text).unwrap(),
            json!({"Text": "{\"count\":4}"})
        );
        assert_eq!(text.decode::<String>().unwrap(), "{\"count\":4}");
        assert!(text.decode::<Answer>().is_err());

        let result = AgentResult::Json(json!({"count": 4}));
        assert_eq!(result.decode::<Answer>().unwrap(), Answer { count: 4 });
        assert!(result.decode::<String>().is_err());
        let wire = json!({"Json": {"count": 4}});
        assert_eq!(serde_json::to_value(&result).unwrap(), wire);
        assert_eq!(serde_json::from_value::<AgentResult>(wire).unwrap(), result);
        assert_eq!(
            AgentResult::Json(json!("answer"))
                .decode::<String>()
                .unwrap(),
            "answer"
        );
    }

    #[test]
    fn typed_result_records_preserve_all_provenance() {
        let raw = record(AgentResult::Json(json!({"count": 4})));
        let decoded = raw.clone().decode::<Answer>().unwrap();
        assert_eq!(decoded.session_id, raw.session_id);
        assert_eq!(decoded.agent_id, raw.agent_id);
        assert_eq!(decoded.event_id, raw.event_id);
        assert_eq!(decoded.source_agent_id, raw.source_agent_id);
        assert_eq!(decoded.position, raw.position);
        assert_eq!(decoded.timestamp, raw.timestamp);
        assert_eq!(decoded.value, Answer { count: 4 });
        assert_eq!(
            record(AgentResult::Text("done".into()))
                .decode::<String>()
                .unwrap()
                .value,
            "done"
        );
        assert!(raw.decode::<Vec<String>>().is_err());
    }

    #[test]
    fn raw_result_record_wire_format_retains_tags_and_rfc3339_time() {
        let record = record(AgentResult::Text("done".into()));
        let wire = json!({
            "session_id": 3,
            "agent_id": 5,
            "event_id": 7,
            "source_agent_id": 2,
            "position": 11,
            "timestamp": "1970-01-01T00:02:03Z",
            "value": {"Text": "done"},
        });
        assert_eq!(serde_json::to_value(&record).unwrap(), wire);
        assert_eq!(
            serde_json::from_value::<ResultRecord>(wire).unwrap(),
            record
        );
    }

    #[test]
    fn result_event_and_route_preserve_durable_artifact_identity() {
        let event = crate::events::EventType::Result(AgentResult::Text("done".into()));
        let wire = json!({"Result": {"Text": "done"}});
        assert_eq!(serde_json::to_value(&event).unwrap(), wire);
        let decoded: crate::events::EventType = serde_json::from_value(wire).unwrap();
        let crate::events::EventType::Result(value) = decoded else {
            panic!("expected a result event");
        };
        assert_eq!(value, AgentResult::Text("done".into()));
        assert_eq!(
            crate::routes::agent_path(crate::routes::AGENT_RESULTS, SessionId(3), AgentId(5)),
            "/sessions/3/agents/5/results"
        );
    }

    #[test]
    fn result_event_filter_defaults_to_false_and_roundtrips() {
        let defaults: crate::EventsQuery = serde_json::from_value(json!({})).unwrap();
        assert!(!defaults.results_only);
        assert!(!crate::EventsQuery::default().results_only);
        let query: crate::EventsQuery =
            serde_json::from_value(json!({"results_only": true})).unwrap();
        assert!(query.results_only);
        assert_eq!(serde_json::to_value(query).unwrap()["results_only"], true);
        assert!(
            serde_json::from_value::<crate::EventsQuery>(json!({"results_only": null})).is_err()
        );
    }

    #[test]
    fn client_helpers_construct_raw_and_typed_result_futures_without_io() {
        fn returns<T>(_: impl std::future::Future<Output = Result<T, crate::ClientError>>) {}
        let client = crate::Client::new("http://example.invalid").unwrap();
        let session = SessionId(3);
        let agent = AgentId(5);
        returns::<Vec<ResultRecord>>(client.results(session, agent, 5));
        returns::<Option<ResultRecord>>(client.last_result_raw(session, agent));
        returns::<Option<ResultRecord<Answer>>>(client.last_result::<Answer>(session, agent));
        returns::<Vec<ResultRecord<Answer>>>(client.last_results::<Answer>(session, agent, 5));
        returns::<Option<ResultRecord<String>>>(client.last_result::<String>(session, agent));
    }

    #[test]
    fn result_query_defaults_to_one_and_rejects_unknown_fields() {
        assert_eq!(
            serde_json::from_value::<ResultsQuery>(json!({})).unwrap(),
            ResultsQuery::default()
        );
        assert_eq!(
            serde_json::from_value::<ResultsQuery>(json!({"limit": 5}))
                .unwrap()
                .limit,
            5
        );
        assert!(serde_json::from_value::<ResultsQuery>(json!({"unknown": 1})).is_err());
    }
}
