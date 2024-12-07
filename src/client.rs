use ::immortal::common::Payloads;
use std::str::FromStr;
use immortal::{
    client_start_workflow_options_version, ClientStartWorkflowOptionsV1,
    ClientStartWorkflowOptionsVersion,
};
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;

use immortal::immortal_client::ImmortalClient;

pub mod immortal {
    tonic::include_proto!("immortal");
}
use ::immortal::common;
use ::immortal::failure;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneratePreview {
    pub file_storage_id: Uuid,
    pub file_mimetype: String,
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = ImmortalClient::connect("http://localhost:10000").await?;
    client
        .start_workflow(ClientStartWorkflowOptionsVersion {
            version: Some(client_start_workflow_options_version::Version::V1(
                ClientStartWorkflowOptionsV1 {
                    workflow_type: "generate_preview_wf".to_string(),
                    task_queue: "attachamizer".to_string(),
                    input: Some(Payloads::new(vec![&GeneratePreview {
                        file_storage_id: Uuid::from_str("a402841b-dbf9-4a47-a237-560c5f072fa7")?,
                        file_mimetype: "video/mp4".to_string(),
                    }])),
                    ..Default::default()
                },
            )),
        })
        .await
        .unwrap();
    // client
    //     .start_workflow(ClientStartWorkflowOptionsVersion {
    //         version: Some(client_start_workflow_options_version::Version::V1(
    //             ClientStartWorkflowOptionsV1 {
    //                 task_queue: "test".to_string(),
    //                 workflow_type: "new_validate_repair_wf".to_string(),
    //                 input: Some(Payloads::new(vec![&Uuid::new_v4()])),
    //                 workflow_version: "v1".to_string(),
    //                 ..Default::default()
    //             },
    //         )),
    //     })
    //     .await?;

    Ok(())
}
