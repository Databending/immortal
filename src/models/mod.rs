use schemars::schema::RootSchema;
use serde::Serialize;

pub mod activity;
pub mod call;
pub mod client;
pub mod history;
pub mod worker;

pub mod serverless;
pub mod notification;
// mod worker;
pub mod workflow;

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
//
// #[derive(Debug, Clone, Default)]
// pub struct RetryPolicy {
//     /// Interval of the first retry. If retryBackoffCoefficient is 1.0 then it is used for all retries.
//     pub initial_interval: Option<Duration>,
//     /// Coefficient used to calculate the next retry interval.
//     /// The next retry interval is previous interval multiplied by the coefficient.
//     /// Must be 1 or larger.
//     pub backoff_coefficient: f64,
//     /// Maximum interval between retries. Exponential backoff leads to interval increase.
//     /// This value is the cap of the increase. Default is 100x of the initial interval.
//     pub maximum_interval: Option<Duration>,
//     /// Maximum number of attempts. When exceeded the retries stop even if not expired yet.
//     /// 1 disables retries. 0 means unlimited (up to the timeouts)
//     pub maximum_attempts: i32,
//     /// Non-Retryable errors types. Will stop retrying if the error type matches this list. Note that
//     /// this is not a substring match, the error *type* (not message) must match exactly.
//     pub non_retryable_error_types: Vec<String>,
// }
