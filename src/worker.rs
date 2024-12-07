use crate::models::worker::WorkerConfigBuilder;
use ::immortal::common;
use ::immortal::failure;
use ::immortal::immortal;
use ::immortal::register_workflow;
pub mod activities;
pub mod models;
pub mod workflows;
// use crate::activities::ActivityData;
use std::time::Duration;

use activities::test::ActivityData;
use anyhow::Result;

use models::worker::Worker;

#[tokio::main]
pub async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    // let server_options =
    //     sdk_client_options(Url::from_str(&env::var("TEMPORAL_URL").unwrap())?).build()?;

    println!("Connecting to Temporal...");
    // let client = server_options.connect("default", None).await?;

    println!("Connected to Temporal");

    let config = WorkerConfigBuilder::default()
        .namespace("default".to_string())
        .task_queue("test".to_string())
        .worker_build_id("rust-worker".to_string())
        .url(std::env::var("IMMORTAL_URL").unwrap())
        .build()?;

    let (mut worker, srx) = Worker::new(config).await?;

    worker.insert_app_data(ActivityData {
        data: "Hi".to_string(),
    });

    worker
        .register_activity("get_avs_request_id", activities::test::hs_tariff_sync)
        .await;

    // worker
    //     .register_wf("new_validate_repair_wf", workflows::test::main_wf, workflows::test::main_wf_schema())
    //     .await;
    register_workflow!(worker, "new_validate_repair_wf", workflows::test);

    // println!("{:#?}", serde_json::to_string_pretty(&workflows::test::main_wf_schema()));
    println!("Starting worker...");

    // worker.start_workflow("new_validate_repair_wf").await;

    let _ = worker.main_thread(srx).await;

    // worker.main_thread(srx).await;
    Ok(())
}
