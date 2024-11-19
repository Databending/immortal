use crate::common::Payload;
use crate::immortal;
use futures::future::BoxFuture;
use futures::future::FutureExt;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::any::Any;
use std::fmt::Debug;
use std::pin::Pin;
use std::time::SystemTime;
use std::{collections::HashMap, future::Future, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use tracing::info_span;
use tracing::Instrument;

use super::activity::AppData;

#[derive(Default, Debug)]
pub struct NotificationOptions {
    /// Identifier to use for tracking the activity in Workflow history.
    /// The `activityId` can be accessed by the activity function.
    /// Does not need to be unique.
    ///
    /// If `None` use the context's sequence number
    pub call_id: Option<String>,
    /// Type of activity to schedule
    pub call_type: String,
    /// Input to the activity
    pub input: Payload,
    /// Task queue to schedule the activity in
    ///
    /// If `None`, use the same task queue as the parent workflow.
    pub task_queue: Option<String>,
}

// #[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
// pub enum ActivityCancellationType {
//     // Initiate a cancellation request and immediately report cancellation to the workflow.
//     #[default]
//     TryCancel,
//     // Wait for activity cancellation completion. Note that activity must heartbeat to receive a
//     // cancellation notification. This can block the cancellation for a long time if activity
//     // doesn't heartbeat or chooses to ignore the cancellation request.
//     WaitCancellationCompleted,
//     // Do not request cancellation of the activity and immediately report cancellation to the
//     // workflow
//     Abandon,
// }



#[derive(Debug, Deserialize, Serialize, Clone)]
/// Activity functions may return these values when exiting
pub enum NotificationExitValue {
    /// Completion requires an asynchronous callback
    // WillCompleteAsync,
    /// Finish with a result
    Normal,
}
//
// impl<T: Serialize> From<T> for NotificationExitValue {
//     fn from() -> Self {
//         Self::Normal
//     }
// }

type BoxNotificationFn = Arc<
    dyn Fn(NotificationContext, Payload) -> BoxFuture<'static, Result<(), anyhow::Error>>
        + Send
        + Sync,
>;

/// Container for user-defined activity functions
#[derive(Clone)]
pub struct NotificationFunction {
    pub notification_func: BoxNotificationFn,
}

impl NotificationFunction {
    /// Create a new ActivityFunction from a boxed function

    pub fn start_notification(
        &mut self,
        payload: Payload,
        app_data: Arc<AppData>,
        notification_type: String,
        notification_id: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), anyhow::Error>> + Send>> {
        let span = info_span!(
            "RunActivity",
            "otel.kind" = "server",
            "otel.name" = notification_type,
            "otel.kind" = "server",
            "notification_id" = notification_id,
        );
        let handle = (self.notification_func)(
            NotificationContext {
                app_data,
                cancellation_token: CancellationToken::new(),
                input: vec![],
                header_fields: HashMap::new(),
                info: NotificationInfo::default(),
            },
            payload,
        )
        .instrument(span);
        Box::pin(handle)
    }
}


/// Closures / functions which can be turned into activity functions implement this trait
pub trait IntoNotificationFunc<Args, Res> {
    /// Consume the closure or fn pointer and turned it into a boxed activity function
    fn into_notification_fn(self) -> BoxNotificationFn;
}

impl<A, Rf, F> IntoNotificationFunc<A, Rf> for F
where
    F: (Fn(NotificationContext, A) -> Rf) + Sync + Send + 'static,
    A: Serialize + DeserializeOwned,
    Rf: Future<Output = Result<(), anyhow::Error>> + Send + 'static,

{
    fn into_notification_fn(self) -> BoxNotificationFn {
        let wrapper = move |ctx: NotificationContext, input: Payload| {
            self(ctx, serde_json::from_slice(&input.data).unwrap())
                .map(|r| {
                    r.and_then(|r| {
                        Ok(r) 
                        // let exit_val: CallExitValue<O> = r.into();
                        // match exit_val {
                        //     // ActExitValue::WillCompleteAsync => Ok(ActExitValue::WillCompleteAsync),
                        //     CallExitValue::Normal(x) => match serde_json::to_value(x) {
                        //         Ok(v) => Ok(CallExitValue::Normal(v)),
                        //         Err(e) => Err(CallError::NonRetryable(e.into())),
                        //     },
                        // }
                    })
                })
                .boxed()
            // Some minor gymnastics are required to avoid needing to clone the function
            // match A::from_json_payload(&input) {
            //     Ok(deser) => ,
            //     Err(e) => async move { Err(ActivityError::NonRetryable(e.into())) }.boxed(),
            // }
        };
        Arc::new(wrapper)
    }
}

fn downcast_owned<T: Send + Sync + 'static>(boxed: Box<dyn Any + Send + Sync>) -> Option<T> {
    boxed.downcast().ok().map(|boxed| *boxed)
}

#[derive(Clone)]
pub struct NotificationContext {
    // worker: Arc<dyn Worker>,
    app_data: Arc<AppData>,
    cancellation_token: CancellationToken,
    input: Vec<Value>,
    // heartbeat_details: Vec<Value>,
    header_fields: HashMap<String, Value>,
    info: NotificationInfo,
}
//
// struct CallHeartbeat {
//     pub task_token: Vec<u8>,
//     pub details: Vec<Value>,
// }

#[derive(Clone, Default)]
pub struct NotificationInfo {
    pub task_token: Vec<u8>,
    pub namespace: String,
    pub notification_id: String,
    pub notification_type: String,
    pub task_queue: String,
    // pub heartbeat_timeout: Option<Duration>,
    // /// Time activity was scheduled by a workflow
    // pub scheduled_time: Option<SystemTime>,
    // /// Time of activity start
    // pub started_time: Option<SystemTime>,
    // /// Time of activity timeout
    // pub deadline: Option<SystemTime>,
    // /// Attempt starts from 1, and increase by 1 for every retry, if retry policy is specified.
    // pub attempt: u32,
    // /// Time this attempt at the activity was scheduled
    // pub current_attempt_scheduled_time: Option<SystemTime>,
    // pub retry_policy: immortal::RetryPolicy,
    // pub is_local: bool,
}
//
// #[derive(Clone, Default)]
// pub struct WorkflowExecution {
//     pub workflow_id: String,
//     pub run_id: String,
// }

#[derive(Clone)]
struct Start {
    // The namespace the workflow lives in
    pub namespace: String,
    // The workflow's type name or function identifier
    pub notification_type: String,
    // The activity's ID
    pub notification_id: String,
    // The activity's type name or function identifier
    pub header_fields: HashMap<String, Value>,
    // Arguments to the activity
    pub input: Vec<Value>,
    // The last details that were recorded by a heartbeat when this task was generated
    // When the task was *first* scheduled
    // When this current attempt at the task was scheduled
    // When this attempt was started, which is to say when core received it by polling.

    // Timeout from the first schedule time to completion
    // Timeout from starting an attempt to reporting its result
    // This is an actual retry policy the service uses. It can be different from the one provided
    // (or not) during activity scheduling as the service can override the provided one in case its
    // values are not specified or exceed configured system limits.

    // Set to true if this is a local activity. Note that heartbeating does not apply to local
    // activities.
}

impl NotificationContext {
    /// Construct new Activity Context, returning the context and the first argument to the activity
    /// (which may be a default [Payload]).
    pub(crate) fn new(
        // worker: Arc<dyn Worker>,
        app_data: Arc<AppData>,
        cancellation_token: CancellationToken,
        task_queue: String,
        task_token: Vec<u8>,
        task: Start,
    ) -> (Self, Value) {
        let Start {

            header_fields,
            mut input,
            namespace,
            notification_type,
            notification_id,

        } = task;

        let first_arg = input.pop().unwrap_or_default();

        (
            NotificationContext {
                // worker,
                app_data,
                cancellation_token,
                input,
                header_fields,
                info: NotificationInfo {
                    task_token,
                    task_queue,
                    notification_id,
                    notification_type,
                    namespace,
                },
            },
            first_arg,
        )
    }

    /// Returns a future the completes if and when the activity this was called inside has been
    /// cancelled
    // pub async fn cancelled(&self) {
    //     self.cancellation_token.clone().cancelled().await
    // }
    //
    // /// Returns true if this activity has already been cancelled
    // pub fn is_cancelled(&self) -> bool {
    //     self.cancellation_token.is_cancelled()
    // }

    /// Retrieve extra parameters to the Activity. The first input is always popped and passed to
    /// the Activity function for the currently executing activity. However, if more parameters are
    /// passed, perhaps from another language's SDK, explicit access is available from extra_inputs
    pub fn extra_inputs(&mut self) -> &mut [Value] {
        &mut self.input
    }

    /// Extract heartbeat details from last failed attempt. This is used in combination with retry policy.
    // pub fn get_heartbeat_details(&self) -> &[Value] {
    //     &self.heartbeat_details
    // }

    /// RecordHeartbeat sends heartbeat for the currently executing activity
    // pub fn record_heartbeat(&self, details: Vec<Value>) {
        // self.worker.record_activity_heartbeat(ActivityHeartbeat {
        //     task_token: self.info.task_token.clone(),
        //     details,
        // })
    // }

    /// Get activity info of the executing activity
    pub fn get_info(&self) -> &NotificationInfo {
        &self.info
    }

    /// Get headers attached to this activity
    pub fn headers(&self) -> &HashMap<String, Value> {
        &self.header_fields
    }

    /// Get custom Application Data
    pub fn app_data<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.app_data.get::<T>()
    }
}

// Deadline calculation.  This is a port of
// /// Helper function lifted from prost_types::Timestamp implementation to prevent double cloning in
// /// error construction
// fn maybe_convert_timestamp(timestamp: &SystemTime) -> Option<SystemTime> {
//     let mut timestamp = *timestamp;
//     timestamp.normalize();
//
//     let system_time = if timestamp.seconds >= 0 {
//         std::time::UNIX_EPOCH.checked_add(StdDuration::from_secs(timestamp.seconds as u64))
//     } else {
//         std::time::UNIX_EPOCH.checked_sub(StdDuration::from_secs((-timestamp.seconds) as u64))
//     };
//
//     system_time.and_then(|system_time| {
//         system_time.checked_add(StdDuration::from_nanos(timestamp.nanos as u64))
//     })
// }
//
