use super::step::{Step, StepId};
use std::collections::HashMap;

/// OperationPlan holds the execution DAG for a GraphQL operation.
#[derive(Debug, Default, Clone)]
pub struct OperationPlan {
    steps: HashMap<StepId, Step>,
    next_id: usize,
    /// Root query fields mapped to their corresponding StepId.
    /// e.g. "forums" -> StepId(1), "user" -> StepId(2)
    root_field_steps: Vec<(String, StepId)>,
}

impl OperationPlan {
    pub fn new() -> Self {
        Self {
            steps: HashMap::new(),
            next_id: 1,
            root_field_steps: Vec::new(),
        }
    }

    /// Add a step to the plan and return its unique StepId.
    pub fn add_step(&mut self, step: Step) -> StepId {
        let id = StepId(self.next_id);
        self.next_id += 1;
        self.steps.insert(id, step);
        id
    }

    /// Register a root field output mapping.
    pub fn add_root_field(&mut self, field_name: impl Into<String>, step_id: StepId) {
        self.root_field_steps.push((field_name.into(), step_id));
    }

    pub fn get_step(&self, id: StepId) -> Option<&Step> {
        self.steps.get(&id)
    }

    pub fn get_step_mut(&mut self, id: StepId) -> Option<&mut Step> {
        self.steps.get_mut(&id)
    }

    pub fn root_fields(&self) -> &[(String, StepId)] {
        &self.root_field_steps
    }

    pub fn step_count(&self) -> usize {
        self.steps.len()
    }
}
