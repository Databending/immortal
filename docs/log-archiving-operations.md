# Log archiving operations

Log archive production and consumption are disabled by default. The archiver is a separate service
module running inside the server binary; no second deployment is required.

## Build and run

Build the server binary:

```sh
cargo build --release --bin server
```

Provide AWS credentials and a bucket to the server and set:

```sh
IMMORTAL_LOG_ARCHIVER_ENABLED=true
IMMORTAL_LOG_ARCHIVE_ENABLED=true
```

Every server replica with the archiver enabled joins the same consumer group, so Redis assigns each
entry to one instance. The archiver can be enabled on all servers or only a selected subset. Keep
`IMMORTAL_LOG_ARCHIVE_SHARDS` fixed for archive schema `v1`; changing it requires a future staged
stream-family migration so the previous family can be drained safely.

## Required server settings

- `IMMORTAL_LOG_ARCHIVE_ENABLED=true`
- `IMMORTAL_LOG_ARCHIVER_ENABLED=true`
- `IMMORTAL_LOG_ARCHIVE_SHARDS=32`
- `IMMORTAL_LOG_ARCHIVE_MAX_RECORDS_PER_SHARD=100000`

The per-shard maximum is an emergency memory ceiling. Reaching it can trim a log before S3 receives
it. Alert well before the ceiling and scale or repair the archiver instead of treating trimming as
normal retention.

`IMMORTAL_LOG_ARCHIVE_ENABLED` controls production into the Redis archive handoff.
`IMMORTAL_LOG_ARCHIVER_ENABLED` independently controls the in-process S3 consumer. This allows the
consumer to be started and verified before production is enabled.

## Required archiver settings

- `IMMORTAL_LOG_S3_BUCKET`
- `IMMORTAL_LOG_S3_REGION` (defaults to `us-east-1`)
- `IMMORTAL_LOG_S3_PREFIX` (defaults to `raw/v1`)
- `IMMORTAL_LOG_ARCHIVE_SHARDS` (must match the server)
- `IMMORTAL_LOG_ARCHIVE_BATCH_RECORDS` (defaults to `256`)
- `IMMORTAL_LOG_ARCHIVE_BATCH_BYTES` (defaults to `8388608`)
- `IMMORTAL_LOG_ARCHIVE_CLAIM_IDLE_MS` (defaults to `60000`)
- `IMMORTAL_LOG_ARCHIVE_REQUEST_TIMEOUT_MS` (defaults to `30000`)

The in-process archiver uses the same `REDIS_HOST`, `REDIS_PORT`, `REDIS_USERNAME`, and
`REDIS_PASSWORD` configuration as the server.

## S3 policy

Grant the archiver only `s3:PutObject`, `s3:GetObject`, `s3:ListBucket`, and `s3:DeleteObject` on its
configured prefix. Enforce bucket-default encryption, block public access, enable access logging,
and configure lifecycle expiration to the product's retention requirement. Enable versioning only
if deletion policy accounts for non-current object versions.

## Alerts and rollout

Monitor `/api/logging/metrics` on each server and Redis consumer-group state. Alert on:

- archive stream length above 50% of the configured hard ceiling;
- oldest pending entry older than twice `IMMORTAL_LOG_ARCHIVE_CLAIM_IDLE_MS`;
- sustained growth in `persist_timed_out` or any log-drop counter;
- an archiver restart loop or repeated S3/Redis errors;
- durable deletion tombstones in `immortal:logs:archive:delete-pending:v1` older than the deletion SLA.

Roll out in this order: enable the in-process archiver, verify that its consumer groups are healthy,
configure alerts, then enable archive production. Roll back by disabling archive production first;
leave the in-process archiver enabled until all archive streams and pending entries are empty.

Archive uploads are content-addressed and idempotent. A crash after S3 upload but before Redis
acknowledgement may upload the same object again, but it uses the same key. Entries are deleted from
Redis only after a successful upload and acknowledgement. Workflow archive deletion uses a durable
Redis tombstone and is retried by every server until S3 deletion succeeds.
