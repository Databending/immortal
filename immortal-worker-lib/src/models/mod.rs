pub mod activity;
pub mod call;
pub mod notification;
pub mod worker;
pub mod workflow;

use schemars::schema::RootSchema;
use serde::Serialize;

#[derive(Debug, Clone, Default, Serialize)]
pub struct WfSchema {
    pub args: Vec<RootSchema>,
    pub output: RootSchema,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ActivitySchema {
    pub args: RootSchema,
    pub output: RootSchema,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CallSchema {
    pub args: RootSchema,
    pub output: RootSchema,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct NotificationSchema {
    pub args: RootSchema,
}

