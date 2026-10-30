use psa_api::{Agent, AgentId};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone)]
pub struct AgentTreeRow {
    pub agent: Agent,
    pub depth: usize,
    /// Hidden ancestor retained to explain visible descendants.
    pub context_only: bool,
}
/// Subscribe before fetching metadata; live updates win over an in-flight list response.
#[derive(Debug, Clone, Default)]
pub struct AgentIndex {
    pub agents: HashMap<AgentId, Agent>,
    generation: u64,
    bootstrapping: bool,
    live_updates: HashMap<AgentId, Agent>,
}
impl AgentIndex {
    pub fn begin_bootstrap(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.bootstrapping = true;
        self.live_updates.clear();
        self.generation
    }
    pub fn apply_live(&mut self, generation: u64, agent: Agent) {
        if generation != self.generation {
            return;
        }
        if self.bootstrapping {
            self.live_updates.insert(agent.id, agent.clone());
        }
        self.agents.insert(agent.id, agent);
    }
    /// Keep the cache on request failure; only call this with a successful response.
    pub fn finish_bootstrap(&mut self, generation: u64, agents: Vec<Agent>) -> bool {
        if generation != self.generation || !self.bootstrapping {
            return false;
        }
        self.agents = agents.into_iter().map(|a| (a.id, a)).collect();
        self.agents.extend(self.live_updates.drain());
        self.bootstrapping = false;
        true
    }
}

/// Ancestry uses subagent parents only. Missing parents and cycles remain renderable.
pub fn agent_tree(agents: &[Agent], show_hidden: bool) -> Vec<AgentTreeRow> {
    let by_id: HashMap<_, _> = agents.iter().map(|a| (a.id, a)).collect();
    let mut included: HashSet<_> = agents
        .iter()
        .filter(|a| show_hidden || !a.hidden)
        .map(|a| a.id)
        .collect();
    for a in agents.iter().filter(|a| !a.hidden || show_hidden) {
        let mut current = a.subagent_parent_id;
        let mut seen = HashSet::new();
        while let Some(id) = current {
            if !seen.insert(id) {
                break;
            }
            let Some(parent) = by_id.get(&id) else {
                break;
            };
            included.insert(id);
            current = parent.subagent_parent_id;
        }
    }
    let mut sorted: Vec<_> = agents.iter().filter(|a| included.contains(&a.id)).collect();
    sorted.sort_by_key(|a| a.id.0);
    let mut out = Vec::new();
    let mut visited = HashSet::new();
    fn visit(
        a: &Agent,
        depth: usize,
        sorted: &[&Agent],
        visited: &mut HashSet<AgentId>,
        out: &mut Vec<AgentTreeRow>,
        show_hidden: bool,
    ) {
        if !visited.insert(a.id) {
            return;
        }
        out.push(AgentTreeRow {
            agent: a.clone(),
            depth,
            context_only: a.hidden && !show_hidden,
        });
        for child in sorted.iter().filter(|c| c.subagent_parent_id == Some(a.id)) {
            visit(child, depth + 1, sorted, visited, out, show_hidden);
        }
    }
    for a in sorted
        .iter()
        .filter(|a| a.subagent_parent_id.is_none_or(|p| !included.contains(&p)))
    {
        visit(a, 0, &sorted, &mut visited, &mut out, show_hidden);
    }
    for a in &sorted {
        visit(a, 0, &sorted, &mut visited, &mut out, show_hidden);
    }
    out
}
/// Last viewed visible agent, then visible interactive root, then visible root.
pub fn select_session_agent(agents: &[Agent], last_viewed: Option<AgentId>) -> Option<AgentId> {
    if let Some(id) = last_viewed
        && agents.iter().any(|a| a.id == id && !a.hidden)
    {
        return Some(id);
    }
    let visible_roots: Vec<_> = agents
        .iter()
        .filter(|a| !a.hidden && a.subagent_parent_id.is_none())
        .collect();
    visible_roots
        .iter()
        .filter(|a| a.interactive)
        .min_by_key(|a| a.id.0)
        .or_else(|| visible_roots.iter().min_by_key(|a| a.id.0))
        .map(|a| a.id)
}
