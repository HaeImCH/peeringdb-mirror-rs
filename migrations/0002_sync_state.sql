-- Per-resource incremental sync watermark (unix seconds). The worker writes it
-- only after a resource syncs completely, so an interrupted full sync is
-- retried instead of resuming from a partial MAX(updated).
CREATE TABLE IF NOT EXISTS sync_state (
    resource TEXT PRIMARY KEY,
    since INTEGER NOT NULL
);

-- Existing deployments: carry over the watermark the worker derived before
-- this table existed, so they keep syncing incrementally. Delete a resource's
-- row to force a full re-sync of it.
INSERT OR IGNORE INTO sync_state (resource, since)
SELECT resource, CAST(strftime('%s', MAX(updated)) AS INTEGER)
FROM objects
GROUP BY resource
HAVING strftime('%s', MAX(updated)) IS NOT NULL;
