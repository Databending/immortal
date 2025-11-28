--!df flags=allow-undeclared-keys

-- KEYS[1] = workflow index key
-- KEYS[2] = workflow hash key prefix

-- ARGV[1] = offset           -- OFFSET APPLIES FROM THE END (0 = newest)
-- ARGV[2] = limit            -- Max number of workflows to return
-- ARGV[3] = status filter    -- e.g. "Running" or "" for none
-- ARGV[4] = num task_queues
-- ARGV[5..] = task_queue values
-- Then worker id count + values

local offset = tonumber(ARGV[1]) or 0
local limit  = tonumber(ARGV[2]) or 10
local status_filter = ARGV[3]
if status_filter == "" then status_filter = nil end

local num_task_queues = tonumber(ARGV[4]) or 0
local task_queues = {}
local idx = 5
for i = 1, num_task_queues do
    task_queues[i] = ARGV[idx]
    idx = idx + 1
end

local num_worker_ids = tonumber(ARGV[idx]) or 0
idx = idx + 1
local worker_ids = {}
for i = 1, num_worker_ids do
    worker_ids[i] = ARGV[idx]
    idx = idx + 1
end

local function contains(tbl, value)
    for i = 1, #tbl do
        if tbl[i] == value then
            return true
        end
    end
    return false
end

local result = {}
local index_len = redis.call("LLEN", KEYS[1])

if index_len == 0 then
    return result
end

-- NEW: start from newest entry (right side of list)
-- Example: LLEN = 100 → last index = 99
-- offset=0 → start_at = 99
local start_at = index_len - 1 - offset

-- iterate backwards until limit reached or out of bounds
for i = start_at, 0, -1 do
    if limit > 0 and #result >= limit then
        break
    end

    local wf_id = redis.call("LINDEX", KEYS[1], i)
    if not wf_id then
        break
    end

    local wf_key = KEYS[2] .. wf_id

    local wf_status     = redis.call("HGET", wf_key, "status")
    local wf_task_queue = redis.call("HGET", wf_key, "task_queue")
    local wf_worker_id  = redis.call("HGET", wf_key, "worker_id")

    if wf_status ~= false then
        local ok = true

        -- status filter
        if status_filter ~= nil and wf_status ~= status_filter then
            ok = false
        end

        -- task queue filter
        if ok and num_task_queues > 0 then
            if (wf_task_queue == false) or (not contains(task_queues, wf_task_queue)) then
                ok = false
            end
        end

        -- worker id filter
        if ok and num_worker_ids > 0 then
            if (wf_worker_id == false) or (not contains(worker_ids, wf_worker_id)) then
                ok = false
            end
        end

        if ok then
            table.insert(result, wf_id)
        end
    end
end

return result

