--!df flags=allow-undeclared-keys

-- KEYS[1] = workflow index list key
-- KEYS[2] = workflow hash key prefix (e.g. "immortal:history:workflow:")

-- ARGV[1] = offset              -- paging offset over FILTERED results
-- ARGV[2] = limit               -- max number of results to return
-- ARGV[3] = status filter       -- "Running" | "Failed" | "Completed" | "" for none
-- ARGV[4] = num_task_queues
-- ARGV[5 .. 4 + N_tq] = task_queue values
-- ARGV[5 + N_tq]     = num_worker_ids
-- ARGV[6 + N_tq .. ] = worker_id values
-- ARGV[7 + N_tq]     = num_worker_instance_ids
-- ARGV[8 + N_tq .. ] = worker_instance_id values

local offset = tonumber(ARGV[1]) or 0
local limit  = tonumber(ARGV[2]) or 10
if limit <= 0 then
    return {}
end

local status_filter = ARGV[3]
if status_filter == "" then
    status_filter = nil
end

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

local num_worker_instance_ids = tonumber(ARGV[idx]) or 0
idx = idx + 1
local worker_instance_ids = {}
for i = 1, num_worker_instance_ids do
    worker_instance_ids[i] = ARGV[idx]
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

local index_len = redis.call("LLEN", KEYS[1])
if index_len == 0 then
    return {}
end

-- We need at most offset + limit matching items
local needed = offset + limit

local collected = {}   -- filtered workflow IDs, newest → oldest
local seen = {}        -- set of IDs to dedup (first occurrence = newest)

-- iterate from newest (index 0) to oldest (index_len - 1)
for i = 0, index_len - 1 do
    if #collected >= needed then
        break
    end

    local wf_id = redis.call("LINDEX", KEYS[1], i)
    if not wf_id then
        break
    end

    -- Dedup: if we've already seen this wf_id, skip older entries
    if not seen[wf_id] then
        local wf_key = KEYS[2] .. wf_id

        local wf_status     = redis.call("HGET", wf_key, "status")
        local wf_task_queue = redis.call("HGET", wf_key, "task_queue")
        local wf_worker_id  = redis.call("HGET", wf_key, "worker_id")
        local wf_worker_instance_id  = redis.call("HGET", wf_key, "worker_instance_id")

        -- if hash exists at all
        if wf_status ~= false or wf_task_queue ~= false or wf_worker_id ~= false then
            local ok = true

            -- status filter
            if status_filter ~= nil then
                if wf_status ~= status_filter then
                    ok = false
                end
            end

            -- task_queue filter
            if ok and num_task_queues > 0 then
                if (wf_task_queue == false) or (not contains(task_queues, wf_task_queue)) then
                    ok = false
                end
            end

            -- worker_id filter
            if ok and num_worker_ids > 0 then
                if (wf_worker_id == false) or (not contains(worker_ids, wf_worker_id)) then
                    ok = false
                end
            end

            -- worker_id filter
            if ok and num_worker_instance_ids > 0 then
                if (wf_worker_instance_id == false) or (not contains(worker_instance_ids, wf_worker_instance_id)) then
                    ok = false
                end
            end

            if ok then
                seen[wf_id] = true
                table.insert(collected, wf_id)  -- keep natural order: newest → oldest
            end
        end
    end
end

-- Now apply pagination over the *filtered* list
local result = {}
local start_idx = offset + 1     -- Lua is 1-based
local end_idx   = offset + limit -- inclusive

if start_idx > #collected then
    return {}
end

if end_idx > #collected then
    end_idx = #collected
end

for i = start_idx, end_idx do
    table.insert(result, collected[i])
end

return result

