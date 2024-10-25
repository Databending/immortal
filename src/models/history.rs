use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct History(pub Vec<WorkflowHistory>);

impl History {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    pub fn add_workflow(&mut self, workflow: WorkflowHistory) {
        if self.0.iter().any(|w| w.workflow_id == workflow.workflow_id) {
            return;
        }
        self.0.push(workflow);
    }
    pub fn get_workflow(&self, workflow_id: &str) -> Option<&WorkflowHistory> {
        self.0.iter().find(|w| w.workflow_id == workflow_id)
    }
    pub fn get_workflow_mut(&mut self, workflow_id: &str) -> Option<&mut WorkflowHistory> {
        self.0.iter_mut().find(|w| w.workflow_id == workflow_id)
    }
    pub fn get_activity(&self, activity_id: &str) -> Option<&ActivityHistory> {
        self.0
            .iter()
            .flat_map(|w| w.runs.iter())
            .flat_map(|r| r.activities.iter())
            .find(|a| a.activity_id == activity_id)
    }
    pub fn get_activity_mut(&mut self, activity_id: &str) -> Option<&mut ActivityHistory> {
        self.0
            .iter_mut()
            .flat_map(|w| w.runs.iter_mut())
            .flat_map(|r| r.activities.iter_mut())
            .find(|a| a.activity_id == activity_id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowHistory {
    pub runs: Vec<WorkflowRun>,
    pub args: Vec<Value>,
    pub workflow_id: String,
    // pub status: Status,
}

impl WorkflowHistory {
    pub fn new(workflow_id: String, args: Vec<Value>) -> Self {
        Self {
            runs: Vec::new(),
            args,
            workflow_id,
        }
    }

    pub fn add_run(&mut self, run: WorkflowRun) {
        self.runs.push(run);
    }
    pub fn get_run(&self, run_id: &str) -> Option<&WorkflowRun> {
        self.runs.iter().find(|r| r.run_id == run_id)
    }
    pub fn get_run_mut(&mut self, run_id: &str) -> Option<&mut WorkflowRun> {
        self.runs.iter_mut().find(|r| r.run_id == run_id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowRun {
    pub run_id: String,
    pub status: Status,
    pub activities: Vec<ActivityHistory>,
    pub start_time: NaiveDateTime,
    pub end_time: Option<NaiveDateTime>,
}

impl WorkflowRun {
    pub fn new(run_id: String) -> Self {
        Self {
            run_id,
            status: Status::Running,
            activities: Vec::new(),
            start_time: chrono::Utc::now().naive_utc(),
            end_time: None,
        }
    }

    pub fn add_activity(&mut self, activity: ActivityHistory) {
        self.activities.push(activity);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Status {
    Running,
    Completed(Value),
    Failed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityHistory {
    pub activity_id: String,
    // pub status: Status,
    // pub result: Option<Value>,
    pub runs: Vec<ActivityRun>,
}

impl ActivityHistory {

    pub fn new(activity_id: String) -> Self {
        Self {
            activity_id,
            // status: Status::Running,
            runs: Vec::new(),
        }
    }

    pub fn add_run(&mut self, run: ActivityRun) {
        if self.runs.iter().any(|r| r.run_id == run.run_id) {
            return;
        }
        self.runs.push(run);
    }
    pub fn get_run(&self, run_id: &str) -> Option<&ActivityRun> {
        self.runs.iter().find(|r| r.run_id == run_id)
    }
    pub fn get_run_mut(&mut self, run_id: &str) -> Option<&mut ActivityRun> {
        self.runs.iter_mut().find(|r| r.run_id == run_id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityRun {
    pub run_id: String,
    pub status: Status,
    pub start_time: NaiveDateTime,
    pub end_time: Option<NaiveDateTime>,
}

impl ActivityRun {
    pub fn new(run_id: String) -> Self {
        Self {
            run_id,
            status: Status::Running,
            start_time: chrono::Utc::now().naive_utc(),
            end_time: None,
        }
    }
}
