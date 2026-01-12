use redis::{AsyncCommands, Script, aio::MultiplexedConnection};

static CLAIM_DUE_TIMERS_LUA: &str = r#"
local zkey = KEYS[1]
local now  = tonumber(ARGV[1])
local lim  = tonumber(ARGV[2])

local due = redis.call('ZRANGEBYSCORE', zkey, '-inf', now, 'LIMIT', 0, lim)
if #due == 0 then
  return due
end

for i=1,#due do
  redis.call('ZREM', zkey, due[i])
end

return due
"#;

async fn claim_due_timers(
    con: &mut MultiplexedConnection,
    now_ms: i64,
    limit: i64,
) -> redis::RedisResult<Vec<String>> {
    let script = Script::new(CLAIM_DUE_TIMERS_LUA);
    script
        .key("immortal:timers")
        .arg(now_ms)
        .arg(limit)
        .invoke_async(con)
        .await
}


async fn schedule_timer(
    con: &mut redis::aio::MultiplexedConnection,
    workflow_id: &str,
    timer_id: &str,
    fire_at_ms: i64,
) -> redis::RedisResult<()> {
    let member = format!("{workflow_id}:{timer_id}");
    let _: () = con.zadd("immortal:timers", member, fire_at_ms).await?;
    Ok(())
}
