use ::immortal::immortal;
use ::immortal::failure;
use ::immortal::common;
use serde::{Serialize, Deserialize};
use schemars::JsonSchema;
// use immortal::
pub mod activities;
pub mod models;
pub mod workflows;
// use crate::activities::ActivityData;
use std::time::Duration;

use activities::test::ActivityData;
use anyhow::Result;

use models::worker::Worker;

#[derive(JsonSchema, Serialize, Deserialize)]
struct FunctionSignature {
    input: String,  // Representing function input type
    output: String, // Representing function output type
}

macro_rules! function_signature_schema {
    ($func_name:ident($($arg_name:ident : $arg_type:ty),*) -> $ret_type:ty) => {
        #[derive(JsonSchema, Serialize, Deserialize)]
        struct FunctionSignature {
            inputs: Vec<String>, // Parameters of the function
            output: String,      // Return type of the function
        }

        fn $func_name($($arg_name: $arg_type),*) -> $ret_type {
            unimplemented!(); // Placeholder implementation
        }

        fn get_schema() -> RootSchema {
            let signature = FunctionSignature {
                inputs: vec![$(stringify!($arg_type).to_string()),*],
                output: stringify!($ret_type).to_string(),
            };
            schemars::schema_for!(FunctionSignature)
        }
    };
}


#[tokio::main]
pub async fn main() -> Result<()> {
    // let server_options =
    //     sdk_client_options(Url::from_str(&env::var("TEMPORAL_URL").unwrap())?).build()?;

    println!("Connecting to Temporal...");
    // let client = server_options.connect("default", None).await?;

    println!("Connected to Temporal");

    let (mut worker, srx) = Worker::new().await?;

    worker.insert_app_data(ActivityData {
        data: "Hi".to_string(),
    });

    worker
        .register_activity("get_avs_request_id", activities::test::hs_tariff_sync)
        .await;

    // worker
    //     .register_wf("new_validate_repair_wf", workflows::test::main)
    //     .await;

    println!("Starting worker...");


    // worker.start_workflow("new_validate_repair_wf").await;

    loop {
        let _ = worker.main_thread(&srx).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        println!("Main thread loop. Reconnecting...");
    }
}
