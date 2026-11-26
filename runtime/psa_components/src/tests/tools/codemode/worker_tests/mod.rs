//! CodeMode tools run through the worker's tool executor.

use crate::{prompts, tools::Tool};
use llm_api::api::{self, Outcome};
use psa::api::{AgentState, events::*};
use psa_agents::{
    agent_context::AgentContext,
    agent_manager::{
        AgentManagerState,
        testing::{Fixture, append_tool_call, finish, load_tool_result},
    },
    cancellation::Cancellation,
    publication::status_event,
    test_support as support,
    tools::ExecutionMode,
};
use psa_store::EventBatchItem;
use std::{sync::Arc, time::Duration};

mod execution_tests;
mod monty_integration;
mod prior_results;
mod reset_codemode;
