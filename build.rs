fn main() {
    tonic_build::configure()
        .type_attribute(".", "#[derive(serde::Deserialize)]")
        .enum_attribute(".", "#[serde(tag = \"type\", content = \"spec\")]")
        .extern_path(".google.protobuf.Any", "::prost_wkt_types::Any")
        .extern_path(".google.protobuf.Timestamp", "::prost_wkt_types::Timestamp")
        .extern_path(".google.protobuf.Duration", "::prost_wkt_types::Duration")
        .extern_path(".google.protobuf.Value", "::prost_wkt_types::Value")
        .build_server(true)
        .compile(
            &[

                "proto/common/message.proto",
                "proto/immortal.proto",
            ],
            &["proto/", "proto/common", "proto/enums/", "proto/failure/"],
        )
        .unwrap_or_else(|e| panic!("Failed to compile protos {:?}", e));
}
