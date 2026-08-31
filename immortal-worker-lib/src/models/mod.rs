pub mod activity;
pub mod call;
pub mod notification;
mod outbound;
pub use outbound::{OutboundStatsSnapshot, WorkerOutboundReceiver, WorkerOutboundSender};
pub mod worker;
pub mod workflow;

use schemars::Schema;
use serde::Serialize;

#[derive(Debug, Clone, Default, Serialize)]
pub struct WfSchema {
    pub args: Vec<Schema>,
    pub output: Schema,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ActivitySchema {
    pub args: Schema,
    pub output: Schema,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CallSchema {
    pub args: Schema,
    pub output: Schema,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct NotificationSchema {
    pub args: Schema,
}
