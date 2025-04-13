use anyhow::{anyhow, Error};
use async_stream::stream;
use schemars::schema::RootSchema;
use schemars::schema_for;
use serde_json::{json, Value};
use std::fmt::{Debug, Write};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::broadcast::Receiver;
use tokio::sync::{
    broadcast,
    mpsc::{self, UnboundedReceiver, UnboundedSender},
    Mutex,
};
use tonic::transport::Channel;
use tracing::field::{Field, Visit};
use tracing::level_filters::LevelFilter;
use tracing::span::{Attributes, Id};
use tracing::{Event, Subscriber};
use tracing_subscriber::Registry;
use tracing_subscriber::{layer::Context, layer::SubscriberExt, Layer};
use uuid::Uuid;

use crate::common::{Payload, Payloads};
use crate::failure::failure::FailureInfo;
use crate::failure::Failure;
use crate::immortal::{
    self, call_result_version, workflow_result_v1, workflow_result_version, CallResultV1,
    CallResultVersion, Failure as ImmortalFailure, RegisteredActivity, RegisteredCall,
    RegisteredNotification, RegisteredWorkflow, RequestStartActivityOptionsV1,
    RequestStartActivityOptionsVersion, Success as ImmortalSuccess, WorkflowResultV1,
    WorkflowResultVersion,
};
use crate::immortal::{
    activity_result_v1, activity_result_version, immortal_client::ImmortalClient,
    immortal_server_action_v1, immortal_server_action_version, immortal_worker_action_v1::Action,
    immortal_worker_action_version, request_start_activity_options_version, ActivityResultV1,
    ActivityResultVersion, ImmortalServerActionV1, ImmortalServerActionVersion, Log,
    RegisterImmortalWorkerV1, StartWorkflowOptionsV1,
};
use tokio::task::JoinHandle;

use super::call::{CallError, CallExitValue, CallFunction, IntoCallFunc};
use super::notification::{IntoNotificationFunc, NotificationFunction};
use super::serverless;
use super::{
    activity::{
        ActExitValue, ActivityError, ActivityFunction, ActivityOptions, AppData, IntoActivityFunc,
    },
    workflow::{WfExitValue, WorkflowFunction},
};
use super::{ActivitySchema, CallSchema, NotificationSchema, WfSchema};

pub struct RunningWorkflow {
    pub workflow_id: String,
    pub join_handle: JoinHandle<()>,
}

//
// pub struct PreviousWorkflow {
//     pub workflow_id: String,
//     pub run_id: String,
//     pub start: NaiveDateTime,
//     pub end: NaiveDateTime,
// }

pub struct RunningActivity {
    pub activity_id: String,
    pub run_id: String,
    pub join_handle: JoinHandle<()>,
}

pub struct RunningCall {
    pub call_id: String,
    pub call_run_id: String,
    pub join_handle: JoinHandle<()>,
}

// #[derive(Debug)]
pub struct Worker {
    pub task_queue: String,
    pub client: ImmortalClient<Channel>,
    pub config: WorkerConfig,
    pub server_channel: tokio::sync::broadcast::Sender<ImmortalServerActionV1>,
    pub workflow_sender: UnboundedSender<(String, Result<WfExitValue<Value>, Error>)>,
    pub activity_sender: UnboundedSender<(
        String,
        String,
        String,
        Result<ActExitValue<Value>, ActivityError>,
    )>,

    pub call_sender: UnboundedSender<(String, String, Result<CallExitValue<Value>, CallError>)>,
    // pub client: Arc<dyn WorkerClient>,
    pub workery_key: String,
    pub registered_workflows: Arc<Mutex<HashMap<String, (WorkflowFunction, WfSchema)>>>,
    pub running_workflows: Arc<Mutex<HashMap<String, RunningWorkflow>>>,
    pub running_activities: Arc<Mutex<HashMap<String, RunningActivity>>>,
    pub running_calls: Arc<Mutex<HashMap<String, RunningCall>>>,
    // active_workflows: WorkflowHalf,
    pub registered_activities: Arc<Mutex<HashMap<String, (ActivityFunction, ActivitySchema)>>>,
    pub registered_calls: Arc<Mutex<HashMap<String, (CallFunction, CallSchema)>>>,
    pub registered_notifications:
        Arc<Mutex<HashMap<String, (NotificationFunction, NotificationSchema)>>>,
    pub app_data: Option<AppData>,
    pub workflow_capacity: i32,
    pub activity_capacity: i32,
}

struct ChannelLayer {
    sender: broadcast::Sender<ImmortalServerActionV1>, // Define the type of data to be sent through the channel
}

// impl Into<immortal::Event> for Event<'_> {
//     fn into(self) -> immortal::Event {
//         let metadata = self.metadata().
//         immortal::Event {
//             metadata: Some(immortal::Metadata { name: self.metadata().name(), target: self.metadata().target(), level: self.metadata().level(), module_path: self.metadata().module_path(), location: self.metadata().line(), fields: self.metadata().fields(), callsite: self.metadata().callsite(), kind: self.metadata().  })
//             level: self.metadata.level.into(),
//             target: self.metadata.target.into(),
//             fields: self.fields.into_iter().map(|x| x.into()).collect(),
//         }
//     }
// }

// impl Into<prost_wkt_types::Duration> for Duration {
//     fn into(self) -> immortal::Duration {
//         immortal::Duration {
//             seconds: self.as_secs() as i64,
//             nanos: self.subsec_nanos() as i32,
//         }
//     }
// }
//
// impl From<immortal::Duration> for Duration {
//     fn from(d: immortal::Duration) -> Self {
//         Duration::new(d.seconds as u64, d.nanos as u32)
//     }
// }

#[derive(Debug)]
struct SpanData {
    workflow_id: String,
    activity_id: Option<String>,
    activity_run_id: Option<String>,
    // message: String,
}
#[derive(Default)]
struct StrVisitor {
    activity_id: Option<String>,
    activity_run_id: Option<String>,
    workflow_id: Option<String>,
    // message: Option<String>,
}

#[derive(Default, Debug)]
struct EventVisitor {
    message: Option<String>,
}

//struct MessageLog {}

impl Visit for StrVisitor {
    fn record_debug(&mut self, _field: &Field, _value: &dyn Debug) {}
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "workflow_id" {
            self.workflow_id = Some(value.to_string());
        } else if field.name() == "activity_id" {
            self.activity_id = Some(value.to_string());
        } else if field.name() == "activity_run_id" {
            self.activity_run_id = Some(value.to_string());
        }
    }
}

impl Visit for EventVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        if field.name() == "message" {
            self.message = Some(format!("{:?}", value));
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        }
    }
}

impl<S> Layer<S> for ChannelLayer
where
    S: Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let fields: Vec<_> = attrs.fields().iter().map(|f| f.name()).collect();
        if fields.contains(&"workflow_id") {
            let values = attrs.values();
            let mut visitor = StrVisitor::default();
            values.record(&mut visitor);
            ctx.span(id).unwrap().extensions_mut().insert(SpanData {
                workflow_id: visitor.workflow_id.unwrap(),
                activity_id: visitor.activity_id,
                activity_run_id: visitor.activity_run_id,
                // message: visitor.message.unwrap(),
            });
        }

        // match visitor.value {
        //     StrVisitorFound::Found(x) => {
        //         println!("Found: {:?}", x);
        //     }
        //     StrVisitorFound::NotFound => {
        //         println!("Not Found");
        //     }
        // }

        // let span = ctx.span(id).unwrap();
        // if value_in_valueset(attrs.values(), "myfield", "myvalue") {
        // ctx.span(id).unwrap().extensions_mut().insert(CustomLayerEnabled);
        // }
    }
    fn on_event(&self, event: &Event, ctx: Context<S>) {
        // event.
        // Format the event as a string
        let mut event_str = String::new();

        let _ = writeln!(&mut event_str, "{:?}", event);
        let mut visitor = EventVisitor::default();
        event.record(&mut visitor);
        println!("Event: {:?}", event);
        if let Some(scope) = ctx.event_scope(&event) {
            for span in scope.from_root() {
                if let Some(data) = span.extensions().get::<SpanData>() {
                    let _ = self.sender.send(ImmortalServerActionV1 {
                        action: Some(immortal_server_action_v1::Action::LogEvent(Log {
                            activity_run_id: data.activity_run_id.clone(),
                            message: visitor.message.unwrap_or("".to_string()),
                            metadata: Some(
                                serde_json::to_vec(&json!({
                                    "target": event.metadata().target().to_string(),
                                    "module_path": event.metadata().module_path().unwrap_or_default(),
                                    "line": event.metadata().line().unwrap_or_default(),
                                    "file": event.metadata().file().unwrap_or_default(),
                                })).unwrap_or_default(),
                            ),
                            level: match event.metadata().level() {
                                &tracing::Level::ERROR => immortal::Level::Error,
                                &tracing::Level::WARN => immortal::Level::Warn,
                                &tracing::Level::INFO => immortal::Level::Info,
                                &tracing::Level::DEBUG => immortal::Level::Debug,
                                &tracing::Level::TRACE => immortal::Level::Trace,
                            }.into(),
                            when: chrono::Utc::now().to_utc().timestamp(),
                            workflow_id: data.workflow_id.clone(),
                            activity_id: data.activity_id.clone(),
                        })),
                    });
                    return;
                }
            }
        }

        // Send the formatted event to the channel
    }
}

impl Worker {
    pub async fn new(
        config: WorkerConfig,
    ) -> anyhow::Result<(Self, Receiver<ImmortalServerActionV1>)> {
        let (tx, rx) = mpsc::unbounded_channel();
        let (atx, arx) = mpsc::unbounded_channel();
        let (ctx, crx) = mpsc::unbounded_channel();

        let (stx, srx) = broadcast::channel(100);
        let stx2 = stx.clone();
        // let make_writer = move || ChannelWriter(stx2.clone())
        let channel_layer = ChannelLayer {
            sender: stx2.clone(),
        };
        let subscriber = Registry::default()
            .with(channel_layer)
            .with(tracing_subscriber::fmt::Layer::default())
            .with(LevelFilter::INFO);
        tracing::subscriber::set_global_default(subscriber)
            .expect("Failed to set global subscriber");
        let client = ImmortalClient::connect(config.url.clone()).await?;
        let mut worker = Worker {
            server_channel: stx,
            client,
            config: config.clone(),
            workflow_sender: tx,
            activity_sender: atx,
            call_sender: ctx,
            task_queue: config.task_queue.clone(),
            workery_key: config.worker_build_id.clone(),
            running_workflows: Arc::new(Mutex::new(HashMap::new())),
            running_activities: Arc::new(Mutex::new(HashMap::new())),
            running_calls: Arc::new(Mutex::new(HashMap::new())),
            registered_workflows: Arc::new(Mutex::new(HashMap::new())),
            registered_activities: Arc::new(Mutex::new(HashMap::new())),
            registered_calls: Arc::new(Mutex::new(HashMap::new())),
            registered_notifications: Arc::new(Mutex::new(HashMap::new())),
            workflow_capacity: config.workflow_capacity,
            activity_capacity: config.activity_capacity,
            // previous_workflows: Arc::new(Mutex::new(HashMap::new())),
            app_data: Some(AppData::default()),
        };
        worker.workflow_thread(rx);
        worker.activity_thread(arx);
        worker.call_thread(crx);
        Ok((worker, srx))
    }

    pub async fn register_wf(
        &mut self,
        workflow_type: impl Into<String>,
        wf_function: impl Into<WorkflowFunction>,
        schema: (Vec<RootSchema>, RootSchema),
    ) {
        let mut registered_workflows = self.registered_workflows.lock().await;
        registered_workflows.insert(
            workflow_type.into(),
            (
                wf_function.into(),
                WfSchema {
                    args: schema.0,
                    output: schema.1,
                },
            ),
        );
        // .workflow_fns
        // .get_mut()
        // .insert(workflow_type.into(), wf_function.into());
    }

    // pub async fn register_wf(
    //     &mut self,
    //     workflow_type: impl Into<String>,
    //     wf_function: impl Into<WorkflowFunction>,
    // ) {
    //     let mut registered_workflows = self.registered_workflows.lock().await;
    //     registered_workflows.insert(workflow_type.into(), wf_function.into());
    // .workflow_fns
    // .get_mut()
    // .insert(workflow_type.into(), wf_function.into());
    // }
    pub async fn register_activity<A: schemars::JsonSchema, R, O: schemars::JsonSchema>(
        &mut self,
        activity_type: impl Into<String>,
        act_function: impl IntoActivityFunc<A, R, O>,
    ) {
        let arg_schema = schema_for!(A);
        let out_schema = schema_for!(O);
        let mut registered_activities = self.registered_activities.lock().await;
        registered_activities.insert(
            activity_type.into(),
            (
                ActivityFunction {
                    act_func: act_function.into_activity_fn(),
                },
                ActivitySchema {
                    args: arg_schema,
                    output: out_schema,
                },
            ),
        );
    }

    pub async fn register_call<A: schemars::JsonSchema, R, O: schemars::JsonSchema>(
        &mut self,
        call_type: impl Into<String>,
        call_function: impl IntoCallFunc<A, R, O>,
    ) {
        let arg_schema = schema_for!(A);
        let out_schema = schema_for!(O);
        let mut registered_calls = self.registered_calls.lock().await;
        registered_calls.insert(
            call_type.into(),
            (
                CallFunction {
                    call_func: call_function.into_call_fn(),
                },
                CallSchema {
                    args: arg_schema,
                    output: out_schema,
                },
            ),
        );
    }

    pub async fn register_notification<A: schemars::JsonSchema, R>(
        &mut self,
        notification_type: impl Into<String>,
        notification_function: impl IntoNotificationFunc<A, R>,
    ) {
        let arg_schema = schema_for!(A);
        let mut registered_notifications = self.registered_notifications.lock().await;
        registered_notifications.insert(
            notification_type.into(),
            (
                NotificationFunction {
                    notification_func: notification_function.into_notification_fn(),
                },
                NotificationSchema { args: arg_schema },
            ),
        );
    }

    /// Insert Custom App Context for Workflows and Activities
    pub fn insert_app_data<T: Send + Sync + 'static>(&mut self, data: T) {
        self.app_data.as_mut().map(|a| a.insert(data));
    }

    async fn main_thread2(
        &mut self,
        mut rx: Receiver<ImmortalServerActionV1>,
        register_immortal_worker: RegisterImmortalWorkerV1,
        safe_app_data: &Arc<AppData>,
    ) -> anyhow::Result<()> {
        let mut stream = self
            .client
            .register_worker(stream! {
                yield ImmortalServerActionVersion {
                    version: Some(immortal_server_action_version::Version::V1(ImmortalServerActionV1 {
                        action: Some(immortal_server_action_v1::Action::RegisterWorker(register_immortal_worker.clone()))
                    }
                    ))
                };
                while let y = rx.recv().await {
                    match y {
                        Ok(y) => {
                            yield ImmortalServerActionVersion {
                                version: Some(immortal_server_action_version::Version::V1(y))
                            }
                        },
                        Err(e) => println!("Error: {:?}", e)

                    }

                }
                println!("worker Stream ended");
            })
            .await?
            .into_inner();

        // info!("Worker registered");
        // on failure this needs to reconnect
        while let Some(feature) = stream.message().await? {
            // println!("NOTE = {:?}", feature);
            match feature.version {
                Some(immortal_worker_action_version::Version::V1(x)) => match x.action {
                    Some(Action::KillWorkflow(workflow_id)) => {
                        self.kill_workflow(&workflow_id).await;
                    }
                    Some(Action::KillActivity(activity_id)) => {
                        self.kill_activity(&activity_id).await;
                    }
                    Some(Action::KillCall(call_id)) => {
                        self.kill_call(&call_id).await;
                    }
                    Some(Action::StartWorkflow(workflow)) => {
                        self.start_workflow_v1(&workflow).await;
                    }
                    Some(Action::StartActivity(activity)) => {
                        self.start_activity(
                            &activity.workflow_id,
                            &activity.activity_type,
                            &activity.activity_id,
                            &activity.activity_run_id,
                            activity
                                .activity_input
                                .unwrap_or(Payload::new(&None::<String>)),
                            safe_app_data,
                        )
                        .await;
                    }
                    Some(Action::StartCall(call)) => {
                        self.start_call(
                            &call.call_type,
                            &call.call_id,
                            &call.call_run_id,
                            call.call_input.unwrap_or(Payload::new(&None::<String>)),
                            safe_app_data,
                        )
                        .await;
                    }
                    Some(Action::Notify(notification)) => {
                        self.start_notification(
                            &notification.notification_id,
                            &notification.notification_type,
                            notification
                                .notification_input
                                .unwrap_or(Payload::new(&None::<String>)),
                            safe_app_data,
                        )
                        .await;
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        Ok(())
    }

    async fn main_thread3(
        &mut self,
        rx: &broadcast::Receiver<ImmortalServerActionV1>,
    ) -> anyhow::Result<()> {
        // let (tx, mut rx) = broadcast::channel(100);
        let rx = rx.resubscribe();

        let register_immortal_worker = RegisterImmortalWorkerV1 {
            workflow_capacity: self.workflow_capacity,
            activity_capacity: self.activity_capacity,
            task_queue: self.task_queue.clone(),
            worker_id: self.workery_key.clone(),
            worker_type: self.task_queue.clone(),
            registered_notifications: self
                .registered_notifications
                .lock()
                .await
                .iter()
                .map(|x| RegisteredNotification {
                    notification_type: x.0.clone(),
                    args: serde_json::to_vec(&x.1 .1.args.clone()).unwrap(),
                })
                .collect(),
            registered_calls: self
                .registered_calls
                .lock()
                .await
                .iter()
                .map(|x| RegisteredCall {
                    call_type: x.0.clone(),
                    args: serde_json::to_vec(&x.1 .1.args.clone()).unwrap(),
                    output: serde_json::to_vec(&x.1 .1.output.clone()).unwrap(),
                })
                .collect(),
            registered_activities: self
                .registered_activities
                .lock()
                .await
                .iter()
                .map(|x| RegisteredActivity {
                    activity_type: x.0.clone(),
                    args: serde_json::to_vec(&x.1 .1.args.clone()).unwrap(),
                    output: serde_json::to_vec(&x.1 .1.output.clone()).unwrap(),
                })
                .collect(),
            registered_workflows: self
                .registered_workflows
                .lock()
                .await
                .iter()
                .map(|x| RegisteredWorkflow {
                    workflow_type: x.0.clone(),
                    args: serde_json::to_vec(&x.1 .1.args.clone()).unwrap(),
                    output: serde_json::to_vec(&x.1 .1.output.clone()).unwrap(),
                })
                .collect(),
        };

        let safe_app_data = Arc::new(
            self.app_data
                .take()
                .ok_or_else(|| anyhow!("app_data should exist on run"))
                .unwrap(),
        );
        match self
            .main_thread2(rx, register_immortal_worker, &safe_app_data)
            .await
        {
            Ok(_) => {}
            Err(_) => {}
        }
        // let temp_stream = BroadcastStream::new(rx)
        //     .filter_map(|item| async move {
        //         // ignore receive errors
        //         item.ok()
        //     })
        //     .map(Ok);

        self.app_data = Some(
            Arc::try_unwrap(safe_app_data)
                .map_err(|_| anyhow!("some references of AppData exist on worker shutdown"))?,
        );
        Ok(())
    }

    pub async fn main_thread(
        mut self,
        rx: broadcast::Receiver<ImmortalServerActionV1>,
    ) -> anyhow::Result<()> {
        let serverless_mode = std::env::var("SERVERLESS_MODE").unwrap_or_default();
        if serverless_mode == "true" {
            println!("Starting serverless mode");
            let safe_app_data = Arc::new(
                self.app_data
                    .take()
                    .ok_or_else(|| anyhow!("app_data should exist on run"))
                    .unwrap(),
            );
            let _ = serverless::main(self, safe_app_data).await;
        } else {
            loop {
                let _ = self.main_thread3(&rx).await;
                tokio::time::sleep(Duration::from_secs(1)).await;
                println!("Main thread loop. Reconnecting...");
            }
        }

        Ok(())
    }

    pub async fn start_workflow_v1(&mut self, workflow_options: &StartWorkflowOptionsV1) {
        let registered_workflows = Arc::clone(&self.registered_workflows);
        let workflow_type = workflow_options.workflow_type.to_string();
        let sender = self.workflow_sender.clone();
        let client = self.client.clone();
        // let args = serde_json::from_slice(&workflow_options.input).unwrap();
        let workflow_id = workflow_options.workflow_id.clone();
        let wf_handle;
        {
            let mut wf2 = registered_workflows.lock().await;
            if let Some(wf) = wf2.get_mut(&workflow_type) {
                wf_handle = wf.0.start_workflow(
                    client,
                    workflow_options
                        .input
                        .clone()
                        .unwrap_or(Payloads::default()),
                    workflow_type.clone(),
                    workflow_id.clone(),
                    self.config.namespace.clone(),
                    self.config.task_queue.clone(),
                );
            } else {
                if let Err(e) = sender.send((
                    workflow_id.to_string(),
                    Err(anyhow!("Workflow doesn't exist")),
                )) {
                    eprintln!("Error sending to sender: {}", e.to_string())
                }

                return;
            }
        }
        let handle;
        {
            let workflow_id = workflow_id.clone();
            handle = tokio::spawn(async move {
                let res = tokio::spawn(wf_handle);
                match res.await {
                    Ok(res) => {
                        if let Err(e) = sender.send((workflow_id.to_string(), res)) {
                            eprintln!("Error sending to sender: {}", e.to_string())
                        }
                    }
                    Err(e) => {
                        if let Err(e) = sender.send((workflow_id.to_string(), Err(e.into()))) {
                            eprintln!("Error sending to sender: {}", e.to_string())
                        }
                    }
                }
            });
            //handle = tokio::spawn(async move {
            //    let res = wf_handle.await;
            //    match sender.send((workflow_id.clone(), res)) {
            //        Ok(_) => {}
            //        Err(e) => println!("ERROR: {:?}", e),
            //    }
            //});
        }

        let running_workflow = RunningWorkflow {
            workflow_id: workflow_id.clone(),
            join_handle: handle,
        };
        self.running_workflows
            .lock()
            .await
            .insert(workflow_id.to_string(), running_workflow);
    }

    pub fn workflow_thread(
        &mut self,
        mut rx: UnboundedReceiver<(String, Result<WfExitValue<Value>, Error>)>,
    ) {
        let mut client = self.client.clone();
        let running_workflows_arc = Arc::clone(&self.running_workflows);
        let worker_key = self.workery_key.clone();
        tokio::spawn(async move {
            while let Some(result) = rx.recv().await {
                match result.1 {
                    Ok(res) => {
                        if let Err(e) = client
                            .completed_workflow(WorkflowResultVersion {
                                version: Some(workflow_result_version::Version::V1(
                                    WorkflowResultV1 {
                                        worker_id: worker_key.clone(),
                                        workflow_id: result.0.clone(),
                                        status: Some(workflow_result_v1::Status::Completed(
                                            ImmortalSuccess {
                                                result: Some(Payload::new(&res)),
                                            },
                                        )),
                                    },
                                )),
                            })
                            .await
                        {
                            eprintln!("Error sending completed workflow: {}", e.to_string())
                        }
                    }
                    Err(e) => {
                        if let Err(e) = client
                            .completed_workflow(WorkflowResultVersion {
                                version: Some(workflow_result_version::Version::V1(
                                    WorkflowResultV1 {
                                        worker_id: worker_key.clone(),
                                        workflow_id: result.0.clone(),
                                        status: Some(workflow_result_v1::Status::Failed(
                                            ImmortalFailure {
                                                failure: Some(Failure {
                                                    message: e.to_string(),
                                                    source: "worker".to_string(),
                                                    stack_trace: e.backtrace().to_string(),
                                                    encoded_attributes: None,
                                                    cause: None,
                                                    failure_info: None,
                                                }),
                                            },
                                        )),
                                    },
                                )),
                            })
                            .await
                        {
                            eprintln!("Error sending completed workflow: {}", e.to_string())
                        }
                    }
                }
                let mut running_workflows = running_workflows_arc.lock().await;
                // let running_workflow = running_workflows.get("test").unwrap();
                // running_workflow.join_handle.abort();
                running_workflows.remove(&result.0);
            }
        });
    }

    pub fn activity_thread(
        &mut self,
        mut rx: UnboundedReceiver<(
            String,
            String,
            String,
            Result<ActExitValue<Value>, ActivityError>,
        )>,
    ) {
        let running_activities_arc = Arc::clone(&self.running_activities);

        let mut client = self.client.clone();
        tokio::spawn(async move {
            while let Some(result) = rx.recv().await {
                let res = match result.3 {
                    // Err(e) => ActivityExecutionResult::fail(Failure::application_failure(
                    //     format!("Activity function panicked: {}", panic_formatter(e)),
                    //     true,
                    // )),
                    Ok(ActExitValue::Normal(p)) => ActivityResultV1::ok(
                        result.0.clone(),
                        result.1.clone(),
                        result.2.clone(),
                        Some(Payload::new(&p)),
                    ),

                    Err(err) => match err {
                        ActivityError::Retryable {
                            source,
                            explicit_delay,
                        } => ActivityResultV1::fail(
                            result.0.clone(),
                            result.1.clone(),
                            result.2.clone(),
                            {
                                println!("source: {:?}", source);
                                let mut f = Failure::application_failure_from_error(source, false);
                                if let Some(d) = explicit_delay {
                                    if let Some(FailureInfo::ApplicationFailureInfo(fi)) =
                                        f.failure_info.as_mut()
                                    {
                                        fi.next_retry_delay = d.try_into().ok();
                                    }
                                }
                                f
                            },
                        ),
                        ActivityError::Cancelled { details } => {
                            ActivityResultV1::cancel_from_details(
                                result.0.clone(),
                                result.1.clone(),
                                result.2.clone(),
                                match details {
                                    Some(d) => {
                                        if let Ok(x) = serde_json::to_vec(&d) {
                                            Some(vec![x])
                                        } else {
                                            None
                                        }
                                    }
                                    None => None,
                                },
                            )
                        }
                        ActivityError::NonRetryable(nre) => ActivityResultV1::fail(
                            result.0.clone(),
                            result.1.clone(),
                            result.2.clone(),
                            Failure::application_failure_from_error(nre, true),
                        ),
                    },
                };
                if let Err(e) = client
                    .completed_activity(ActivityResultVersion {
                        version: Some(activity_result_version::Version::V1(res)),
                    })
                    .await
                {
                    eprintln!("Error completing activity: {}", e);
                }
                let mut running_activities = running_activities_arc.lock().await;
                // let running_activity = running_activities.get("test").unwrap();
                // running_activity.join_handle.abort();
                running_activities.remove(&result.0);
            }
        });
    }

    pub fn call_thread(
        &mut self,
        mut rx: UnboundedReceiver<(String, String, Result<CallExitValue<Value>, CallError>)>,
    ) {
        let running_calls_arc = Arc::clone(&self.running_calls);

        let mut client = self.client.clone();
        tokio::spawn(async move {
            while let Some(result) = rx.recv().await {
                let res = match result.2 {
                    // Err(e) => ActivityExecutionResult::fail(Failure::application_failure(
                    //     format!("Activity function panicked: {}", panic_formatter(e)),
                    //     true,
                    // )),
                    Ok(CallExitValue::Normal(p)) => {
                        CallResultV1::ok(result.0.clone(), result.1.clone(), Some(Payload::new(&p)))
                    }

                    Err(err) => match err {
                        CallError::Retryable {
                            source,
                            explicit_delay,
                        } => CallResultV1::fail(result.0.clone(), result.1.clone(), {
                            println!("source: {:?}", source);
                            let mut f = Failure::application_failure_from_error(source, false);
                            if let Some(d) = explicit_delay {
                                if let Some(FailureInfo::ApplicationFailureInfo(fi)) =
                                    f.failure_info.as_mut()
                                {
                                    fi.next_retry_delay = d.try_into().ok();
                                }
                            }
                            f
                        }),
                        CallError::Cancelled { details } => CallResultV1::cancel_from_details(
                            result.0.clone(),
                            result.1.clone(),
                            match details {
                                Some(d) => {
                                    if let Ok(x) = serde_json::to_vec(&d) {
                                        Some(vec![x])
                                    } else {
                                        None
                                    }
                                }
                                None => None,
                            },
                        ),
                        CallError::NonRetryable(nre) => CallResultV1::fail(
                            result.0.clone(),
                            result.1.clone(),
                            Failure::application_failure_from_error(nre, true),
                        ),
                    },
                };
                if let Err(e) = client
                    .completed_call(CallResultVersion {
                        version: Some(call_result_version::Version::V1(res)),
                    })
                    .await
                {
                    eprintln!("error completing call: {}", e.to_string());
                }

                let mut running_activities = running_calls_arc.lock().await;
                // let running_activity = running_activities.get("test").unwrap();
                // running_activity.join_handle.abort();
                running_activities.remove(&result.0);
            }
        });
    }

    pub async fn activity(
        &mut self,
        workflow_id: String,
        activity_options: ActivityOptions,
    ) -> anyhow::Result<Value> {
        let mut temp = RequestStartActivityOptionsV1 {
            activity_id: activity_options
                .activity_id
                .unwrap_or_else(|| Uuid::new_v4().to_string()),
            activity_type: activity_options.activity_type,
            activity_input: Some(activity_options.input),
            schedule_to_close_timeout: activity_options
                .schedule_to_close_timeout
                .map(|x| x.try_into().unwrap()),
            schedule_to_start_timeout: activity_options
                .schedule_to_start_timeout
                .map(|x| x.try_into().unwrap()),
            start_to_close_timeout: activity_options
                .start_to_close_timeout
                .map(|x| x.try_into().unwrap()),
            heartbeat_timeout: activity_options
                .heartbeat_timeout
                .map(|x| x.try_into().unwrap()),
            retry_policy: activity_options.retry_policy.map(|x| x.try_into().unwrap()),
            workflow_id,
            task_queue: activity_options
                .task_queue
                .unwrap_or(self.task_queue.clone()),
            ..Default::default()
        };
        temp.set_cancellation_type(activity_options.cancellation_type);
        match self
            .client
            .start_activity(RequestStartActivityOptionsVersion {
                version: Some(request_start_activity_options_version::Version::V1(temp)),
            })
            .await
        {
            Ok(activity_result) => {
                println!("received Activity completed: {:?}", activity_result);
                match activity_result.into_inner().version {
                    Some(activity_result_version::Version::V1(x)) => match x.status {
                        Some(activity_result_v1::Status::Failed(x)) => Err(anyhow!("{:#?}", x)),
                        Some(activity_result_v1::Status::Cancelled(x)) => Err(anyhow!("{:#?}", x)),
                        Some(activity_result_v1::Status::Completed(y)) => {
                            println!("Activity result: {:?}", y.result);
                            Ok(serde_json::from_slice(
                                &y.result.ok_or(anyhow!("No payload"))?.data,
                            )?)
                        }

                        None => Err(anyhow!("Activity failed")),
                    },
                    // Some(x) => Ok(serde_json::from_str(&x)?),
                    None => Err(anyhow!("Activity failed")),
                }
            }
            Err(e) => Err(anyhow!("Activity failed: {:?}", e)),
        }
    }

    pub async fn run_serverless_activity(
        &mut self,
        workflow_id: &str,
        activity_type: &str,
        activity_id: &str,
        activity_run_id: &str,
        payload: Payload,
        safe_app_data: &Arc<AppData>,
    ) -> anyhow::Result<ActivityResultV1> {
        let registered_activities = Arc::clone(&self.registered_activities);
        let running_activities_arc = Arc::clone(&self.running_activities);
        let activity_type = activity_type.to_string();

        // let sender = self.activity_sender.clone();

        let (atx, mut arx) = mpsc::unbounded_channel();
        let mut act2 = registered_activities.lock().await;
        let act = act2.get_mut(&activity_type).unwrap();

        let aid = activity_id.to_string();
        let aid_run = activity_run_id.to_string();
        let wid = workflow_id.to_string();

        let act_handle = act.0.start_activity(
            payload,
            safe_app_data.clone(),
            activity_type,
            workflow_id.to_string(),
            activity_id.to_string(),
            activity_run_id.to_string(),
        );
        let handle = tokio::spawn(async move {
            let res = act_handle.await;

            if let Err(e) = atx.send((wid.to_string(), aid, aid_run, res)) {
                eprintln!("Error atx send: {}", e.to_string())
            }
        });
        let running_workflow = RunningActivity {
            activity_id: activity_id.to_string(),
            run_id: activity_run_id.to_string(),
            join_handle: handle,
        };
        self.running_activities
            .lock()
            .await
            .insert(activity_id.to_string(), running_workflow);
        loop {
            if let Some(result) = arx.recv().await {
                let res = match result.3 {
                    // Err(e) => ActivityExecutionResult::fail(Failure::application_failure(
                    //     format!("Activity function panicked: {}", panic_formatter(e)),
                    //     true,
                    // )),
                    Ok(ActExitValue::Normal(p)) => ActivityResultV1::ok(
                        result.0.clone(),
                        result.1.clone(),
                        result.2.clone(),
                        Some(Payload::new(&p)),
                    ),

                    Err(err) => match err {
                        ActivityError::Retryable {
                            source,
                            explicit_delay,
                        } => ActivityResultV1::fail(
                            result.0.clone(),
                            result.1.clone(),
                            result.2.clone(),
                            {
                                println!("source: {:?}", source);
                                let mut f = Failure::application_failure_from_error(source, false);
                                if let Some(d) = explicit_delay {
                                    if let Some(FailureInfo::ApplicationFailureInfo(fi)) =
                                        f.failure_info.as_mut()
                                    {
                                        fi.next_retry_delay = d.try_into().ok();
                                    }
                                }
                                f
                            },
                        ),
                        ActivityError::Cancelled { details } => {
                            ActivityResultV1::cancel_from_details(
                                result.0.clone(),
                                result.1.clone(),
                                result.2.clone(),
                                match details {
                                    Some(d) => Some(vec![serde_json::to_vec(&d).unwrap()]),
                                    None => None,
                                },
                            )
                        }
                        ActivityError::NonRetryable(nre) => ActivityResultV1::fail(
                            result.0.clone(),
                            result.1.clone(),
                            result.2.clone(),
                            Failure::application_failure_from_error(nre, true),
                        ),
                    },
                };

                let mut running_activities = running_activities_arc.lock().await;
                // let running_activity = running_activities.get("test").unwrap();
                // running_activity.join_handle.abort();
                running_activities.remove(&result.0);
                return Ok(res);
            }
        }
        // self.app_data = Some(
        //     Arc::try_unwrap(safe_app_data)
        //         .map_err(|_| anyhow!("some references of AppData exist on worker shutdown"))
        //         .unwrap(),
        // );
    }

    pub async fn start_activity(
        &mut self,
        workflow_id: &str,
        activity_type: &str,
        activity_id: &str,
        activity_run_id: &str,
        payload: Payload,
        safe_app_data: &Arc<AppData>,
    ) {
        let registered_activities = Arc::clone(&self.registered_activities);
        let activity_type = activity_type.to_string();

        let sender = self.activity_sender.clone();
        let mut act2 = registered_activities.lock().await;
        let act = act2.get_mut(&activity_type).unwrap();

        let aid = activity_id.to_string();
        let aid_run = activity_run_id.to_string();
        let wid = workflow_id.to_string();

        let act_handle = act.0.start_activity(
            payload,
            safe_app_data.clone(),
            activity_type,
            workflow_id.to_string(),
            activity_id.to_string(),
            activity_run_id.to_string(),
        );
        let handle = tokio::spawn(async move {
            let res = tokio::spawn(act_handle);
            match res.await {
                Ok(res) => {
                    if let Err(e) = sender.send((wid.to_string(), aid, aid_run, res)) {
                        eprintln!("{:#?}", e);
                    }
                }
                Err(e) => {
                    if let Err(e) = sender.send((
                        wid.to_string(),
                        aid,
                        aid_run,
                        Err(ActivityError::NonRetryable(e.into())),
                    )) {
                        eprintln!("{:#?}", e);
                    }
                }
            }
        });
        let running_workflow = RunningActivity {
            activity_id: activity_id.to_string(),
            run_id: activity_run_id.to_string(),
            join_handle: handle,
        };
        self.running_activities
            .lock()
            .await
            .insert(activity_id.to_string(), running_workflow);
        // self.app_data = Some(
        //     Arc::try_unwrap(safe_app_data)
        //         .map_err(|_| anyhow!("some references of AppData exist on worker shutdown"))
        //         .unwrap(),
        // );
    }

    pub async fn kill_workflow(&mut self, workflow_id: &str) {
        let mut running_workflows = self.running_workflows.lock().await;
        if let Some(running_workflow) = running_workflows.get(workflow_id) {
            let _ = running_workflow.join_handle.abort();
            running_workflows.remove(workflow_id);
        }
    }

    pub async fn kill_activity(&mut self, activity_id: &str) {
        let mut running_activities = self.running_activities.lock().await;
        if let Some(running_activity) = running_activities.get(activity_id) {
            let _ = running_activity.join_handle.abort();
            running_activities.remove(activity_id);
        }
    }

    pub async fn kill_call(&mut self, call_id: &str) {
        let mut running_calls = self.running_calls.lock().await;
        if let Some(running_call) = running_calls.get(call_id) {
            let _ = running_call.join_handle.abort();
            running_calls.remove(call_id);
        }
    }

    pub async fn start_call(
        &mut self,
        call_type: &str,
        call_id: &str,
        call_run_id: &str,
        payload: Payload,
        safe_app_data: &Arc<AppData>,
    ) {
        let registered_calls = Arc::clone(&self.registered_calls);
        let call_type = call_type.to_string();

        let sender = self.call_sender.clone();
        let mut act2 = registered_calls.lock().await;
        let act = act2.get_mut(&call_type).unwrap();

        let cid = call_id.to_string();
        let crid = call_run_id.to_string();

        let act_handle = act.0.start_call(
            payload,
            safe_app_data.clone(),
            call_type.to_string(),
            call_id.to_string(),
        );

        let handle = tokio::spawn(async move {
            let res = tokio::spawn(act_handle);
            match res.await {
                Ok(res) => {
                    if let Err(e) = sender.send((cid.to_string(), crid.to_string(), res)) {
                        eprintln!("error sending to sender: {}", e.to_string())
                    }
                }
                Err(e) => {
                    if let Err(e) = sender.send((cid, crid, Err(CallError::NonRetryable(e.into()))))
                    {
                        eprintln!("error sending to sender: {}", e.to_string())
                    }
                }
            }
        });
        let running_workflow = RunningCall {
            call_id: call_id.to_string(),
            call_run_id: call_run_id.to_string(),
            join_handle: handle,
        };
        self.running_calls
            .lock()
            .await
            .insert(call_id.to_string(), running_workflow);
        // self.app_data = Some(
        //     Arc::try_unwrap(safe_app_data)
        //         .map_err(|_| anyhow!("some references of AppData exist on worker shutdown"))
        //         .unwrap(),
        // );
    }

    pub async fn start_notification(
        &mut self,
        notification_id: &str,
        notification_type: &str,
        payload: Payload,
        safe_app_data: &Arc<AppData>,
    ) {
        let registered_notifications = Arc::clone(&self.registered_notifications);
        let notification_type = notification_type.to_string();

        let mut act2 = registered_notifications.lock().await;
        let act = act2.get_mut(&notification_type).unwrap();

        // let nid = notification_id.to_string();

        let act_handle = act.0.start_notification(
            payload,
            safe_app_data.clone(),
            notification_type,
            notification_id.to_string(),
        );
        tokio::spawn(async move {
            let res = tokio::spawn(act_handle);
            match res.await {
                Err(e) => {
                    println!("ERROR IN NOTIFICATION: {}", e.to_string());
                }

                _ => {}
            }
        });
    }
}

/// Defines per-worker configuration options
#[derive(Clone, derive_builder::Builder)]
#[builder(setter(into))]
#[non_exhaustive]
pub struct WorkerConfig {
    /// The Temporal service namespace this worker is bound to
    pub namespace: String,
    /// What task queue will this worker poll from? This task queue name will be used for both
    /// workflow and activity polling.
    pub task_queue: String,
    /// A string that should be unique to the set of code this worker uses. IE: All the workflow,
    /// activity, interceptor, and data converter code.
    pub worker_build_id: String,
    pub url: String,

    // custom
    #[builder(default = "1")]
    pub workflow_capacity: i32,

    #[builder(default = "1")]
    pub activity_capacity: i32,
    /// A human-readable string that can identify this worker. Using something like sdk version
    /// and host name is a good default. If set, overrides the identity set (if any) on the client
    /// used by this worker.
    #[builder(default)]
    pub client_identity_override: Option<String>,
    /// If set nonzero, workflows will be cached and sticky task queues will be used, meaning that
    /// history updates are applied incrementally to suspended instances of workflow execution.
    /// Workflows are evicted according to a least-recently-used policy one the cache maximum is
    /// reached. Workflows may also be explicitly evicted at any time, or as a result of errors
    /// or failures.
    #[builder(default = "0")]
    pub max_cached_workflows: usize,
    /// Set a [WorkerTuner] for this worker. Either this or at least one of the `max_outstanding_*`
    /// fields must be set.
    // #[builder(setter(into = false, strip_option), default)]
    // pub tuner: Option<Arc<dyn WorkerTuner + Send + Sync>>,
    /// Maximum number of concurrent poll workflow task requests we will perform at a time on this
    /// worker's task queue. See also [WorkerConfig::nonsticky_to_sticky_poll_ratio]. Must be at
    /// least 1.
    // #[builder(default = "MAX_CONCURRENT_WFT_POLLS_DEFAULT")]
    // pub max_concurrent_wft_polls: usize,
    /// [WorkerConfig::max_concurrent_wft_polls] * this number = the number of max pollers that will
    /// be allowed for the nonsticky queue when sticky tasks are enabled. If both defaults are used,
    /// the sticky queue will allow 4 max pollers while the nonsticky queue will allow one. The
    /// minimum for either poller is 1, so if `max_concurrent_wft_polls` is 1 and sticky queues are
    /// enabled, there will be 2 concurrent polls.
    #[builder(default = "0.2")]
    pub nonsticky_to_sticky_poll_ratio: f32,
    /// Maximum number of concurrent poll activity task requests we will perform at a time on this
    /// worker's task queue
    #[builder(default = "5")]
    pub max_concurrent_at_polls: usize,
    /// If set to true this worker will only handle workflow tasks and local activities, it will not
    /// poll for activity tasks.
    #[builder(default = "false")]
    pub no_remote_activities: bool,
    /// How long a workflow task is allowed to sit on the sticky queue before it is timed out
    /// and moved to the non-sticky queue where it may be picked up by any worker.
    #[builder(default = "Duration::from_secs(10)")]
    pub sticky_queue_schedule_to_start_timeout: Duration,

    /// Longest interval for throttling activity heartbeats
    #[builder(default = "Duration::from_secs(60)")]
    pub max_heartbeat_throttle_interval: Duration,

    /// Default interval for throttling activity heartbeats in case
    /// `ActivityOptions.heartbeat_timeout` is unset.
    /// When the timeout *is* set in the `ActivityOptions`, throttling is set to
    /// `heartbeat_timeout * 0.8`.
    #[builder(default = "Duration::from_secs(30)")]
    pub default_heartbeat_throttle_interval: Duration,

    /// Sets the maximum number of activities per second the task queue will dispatch, controlled
    /// server-side. Note that this only takes effect upon an activity poll request. If multiple
    /// workers on the same queue have different values set, they will thrash with the last poller
    /// winning.
    #[builder(default)]
    pub max_task_queue_activities_per_second: Option<f64>,

    /// Limits the number of activities per second that this worker will process. The worker will
    /// not poll for new activities if by doing so it might receive and execute an activity which
    /// would cause it to exceed this limit. Negative, zero, or NaN values will cause building
    /// the options to fail.
    #[builder(default)]
    pub max_worker_activities_per_second: Option<f64>,

    /// # UNDER DEVELOPMENT
    /// If set to true this worker will opt-in to the whole-worker versioning feature.
    /// `worker_build_id` will be used as the version.
    /// todo: link to feature docs
    #[builder(default = "false")]
    pub use_worker_versioning: bool,

    /// If set false (default), shutdown will not finish until all pending evictions have been
    /// issued and replied to. If set true shutdown will be considered complete when the only
    /// remaining work is pending evictions.
    ///
    /// This flag is useful during tests to avoid needing to deal with lots of uninteresting
    /// evictions during shutdown. Alternatively, if a lang implementation finds it easy to clean
    /// up during shutdown, setting this true saves some back-and-forth.
    #[builder(default = "false")]
    pub ignore_evicts_on_shutdown: bool,

    /// Maximum number of next page (or initial) history event listing requests we'll make
    /// concurrently. I don't this it's worth exposing this to users until we encounter a reason.
    #[builder(default = "5")]
    pub fetching_concurrency: usize,

    /// If set, core will issue cancels for all outstanding activities after shutdown has been
    /// initiated and this amount of time has elapsed.
    #[builder(default)]
    pub graceful_shutdown_period: Option<Duration>,

    /// The amount of time core will wait before timing out activities using its own local timers
    /// after one of them elapses. This is to avoid racing with server's own tracking of the
    /// timeout.
    #[builder(default = "Duration::from_secs(5)")]
    pub local_timeout_buffer_for_activities: Duration,

    /// Any error types listed here will cause any workflow being processed by this worker to fail,
    /// rather than simply failing the workflow task.
    // #[builder(default)]
    // pub workflow_failure_errors: HashSet<WorkflowErrorType>,

    /// Like [WorkerConfig::workflow_failure_errors], but specific to certain workflow types (the
    /// map key).
    // #[builder(default)]
    // pub workflow_types_to_failure_errors: HashMap<String, HashSet<WorkflowErrorType>>,

    /// The maximum allowed number of workflow tasks that will ever be given to this worker at one
    /// time. Note that one workflow task may require multiple activations - so the WFT counts as
    /// "outstanding" until all activations it requires have been completed.
    ///
    /// Mutually exclusive with `tuner`
    #[builder(setter(into, strip_option), default)]
    pub max_outstanding_workflow_tasks: Option<usize>,
    /// The maximum number of activity tasks that will ever be given to this worker concurrently
    ///
    /// Mutually exclusive with `tuner`
    #[builder(setter(into, strip_option), default)]
    pub max_outstanding_activities: Option<usize>,
    /// The maximum number of local activity tasks that will ever be given to this worker
    /// concurrently
    ///
    /// Mutually exclusive with `tuner`
    #[builder(setter(into, strip_option), default)]
    pub max_outstanding_local_activities: Option<usize>,
}

pub(crate) trait WorkerClient: Sync + Send {
    // async fn poll_workflow_task(
    //     &self,
    //     task_queue: TaskQueue,
    // ) -> Result<PollWorkflowTaskQueueResponse>;
    // async fn poll_activity_task(
    //     &self,
    //     task_queue: String,
    //     max_tasks_per_sec: Option<f64>,
    // ) -> Result<PollActivityTaskQueueResponse>;
    // async fn complete_workflow_task(
    //     &self,
    //     request: WorkflowTaskCompletion,
    // ) -> Result<RespondWorkflowTaskCompletedResponse>;
    // async fn complete_activity_task(
    //     &self,
    //     task_token: TaskToken,
    //     result: Option<Payloads>,
    // ) -> Result<RespondActivityTaskCompletedResponse>;
    // async fn record_activity_heartbeat(
    //     &self,
    //     task_token: TaskToken,
    //     details: Option<Payloads>,
    // ) -> Result<RecordActivityTaskHeartbeatResponse>;
    // async fn cancel_activity_task(
    //     &self,
    //     task_token: TaskToken,
    //     details: Option<Payloads>,
    // ) -> Result<RespondActivityTaskCanceledResponse>;
    // async fn fail_activity_task(
    //     &self,
    //     task_token: TaskToken,
    //     failure: Option<Failure>,
    // ) -> Result<RespondActivityTaskFailedResponse>;
    // async fn fail_workflow_task(
    //     &self,
    //     task_token: TaskToken,
    //     cause: WorkflowTaskFailedCause,
    //     failure: Option<Failure>,
    // ) -> Result<RespondWorkflowTaskFailedResponse>;
    // async fn get_workflow_execution_history(
    //     &self,
    //     workflow_id: String,
    //     run_id: Option<String>,
    //     page_token: Vec<u8>,
    // ) -> Result<GetWorkflowExecutionHistoryResponse>;
    // async fn respond_legacy_query(
    //     &self,
    //     task_token: TaskToken,
    //     query_result: QueryResult,
    // ) -> Result<RespondQueryTaskCompletedResponse>;
    // async fn describe_namespace(&self) -> Result<DescribeNamespaceResponse>;
    //
    // fn replace_client(&self, new_client: RetryClient<Client>);
    // fn capabilities(&self) -> Option<Capabilities>;
    // fn workers(&self) -> Arc<SlotManager>;
    // fn is_mock(&self) -> bool;
}
