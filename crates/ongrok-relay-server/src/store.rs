//! Durable redb repository for control-plane records.

use crate::state::TokenState;
use anyhow::{Context, Result};
use libongrok::{ControlEvent, NodeId, NodeMetric, NodeRecord, ServiceDefinition, ServiceId};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::{collections::BTreeMap, path::Path};

const SERVICES_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("services");
const NODES_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("nodes");
const METRICS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("metrics");
const TOKENS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("tokens");
const EVENTS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("events");
const TOKEN_STATE_KEY: &str = "state";
/// The dashboard needs recent history, not an unbounded time-series database.
const METRIC_RETENTION_MS: i64 = 3 * 24 * 60 * 60 * 1_000;

pub(crate) struct ServiceStore {
    db: Database,
}

impl ServiceStore {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let db = Database::create(path)
            .with_context(|| format!("failed to open database {}", path.display()))?;
        let write_txn = db
            .begin_write()
            .context("failed to begin database transaction")?;
        write_txn
            .open_table(SERVICES_TABLE)
            .context("failed to open services table")?;
        write_txn
            .open_table(NODES_TABLE)
            .context("failed to open nodes table")?;
        write_txn
            .open_table(METRICS_TABLE)
            .context("failed to open metrics table")?;
        write_txn
            .open_table(TOKENS_TABLE)
            .context("failed to open tokens table")?;
        write_txn
            .open_table(EVENTS_TABLE)
            .context("failed to open events table")?;
        write_txn
            .commit()
            .context("failed to initialize database")?;
        Ok(Self { db })
    }

    pub(crate) fn load_or_initialize_tokens(
        &self,
        initial_admin_hash: [u8; 32],
        initial_user_hash: [u8; 32],
    ) -> Result<TokenState> {
        let read_txn = self
            .db
            .begin_read()
            .context("failed to begin token read transaction")?;
        let table = read_txn
            .open_table(TOKENS_TABLE)
            .context("failed to open token table")?;
        if let Some(value) = table
            .get(TOKEN_STATE_KEY)
            .context("failed to read token state")?
        {
            return postcard::from_bytes(value.value()).context("failed to decode token state");
        }
        drop(table);
        drop(read_txn);
        let tokens = TokenState {
            admin_hash: Some(initial_admin_hash),
            user_hash: Some(initial_user_hash),
        };
        self.put_tokens(&tokens)?;
        Ok(tokens)
    }

    pub(crate) fn put_tokens(&self, tokens: &TokenState) -> Result<()> {
        let encoded = postcard::to_allocvec(tokens).context("failed to encode token state")?;
        let write_txn = self
            .db
            .begin_write()
            .context("failed to begin token write transaction")?;
        {
            let mut table = write_txn
                .open_table(TOKENS_TABLE)
                .context("failed to open token table")?;
            table
                .insert(TOKEN_STATE_KEY, encoded.as_slice())
                .context("failed to persist token state")?;
        }
        write_txn.commit().context("failed to commit token state")?;
        Ok(())
    }

    pub(crate) fn load_services(&self) -> Result<BTreeMap<ServiceId, ServiceDefinition>> {
        let txn = self
            .db
            .begin_read()
            .context("failed to begin database read")?;
        let table = txn
            .open_table(SERVICES_TABLE)
            .context("failed to open services table")?;
        let mut services = BTreeMap::new();
        for item in table.iter().context("failed to iterate services")? {
            let (_, value) = item.context("failed to read service record")?;
            let service: ServiceDefinition =
                postcard::from_bytes(value.value()).context("failed to decode service record")?;
            services.insert(service.service_id, service);
        }
        Ok(services)
    }

    pub(crate) fn put(&self, service: &ServiceDefinition) -> Result<()> {
        let encoded = postcard::to_allocvec(service).context("failed to encode service record")?;
        let key = service.service_id.0.to_string();
        let txn = self
            .db
            .begin_write()
            .context("failed to begin database write")?;
        {
            let mut table = txn
                .open_table(SERVICES_TABLE)
                .context("failed to open services table")?;
            table
                .insert(key.as_str(), encoded.as_slice())
                .context("failed to persist service")?;
        }
        txn.commit().context("failed to commit service")?;
        Ok(())
    }

    pub(crate) fn delete(&self, service_id: ServiceId) -> Result<()> {
        let key = service_id.0.to_string();
        let txn = self
            .db
            .begin_write()
            .context("failed to begin database write")?;
        {
            let mut table = txn
                .open_table(SERVICES_TABLE)
                .context("failed to open services table")?;
            table
                .remove(key.as_str())
                .context("failed to remove service")?;
        }
        txn.commit().context("failed to commit service removal")?;
        Ok(())
    }

    pub(crate) fn load_nodes(&self) -> Result<BTreeMap<NodeId, NodeRecord>> {
        let txn = self
            .db
            .begin_read()
            .context("failed to begin database read")?;
        let table = txn
            .open_table(NODES_TABLE)
            .context("failed to open nodes table")?;
        let mut nodes = BTreeMap::new();
        for item in table.iter().context("failed to iterate nodes")? {
            let (_, value) = item.context("failed to read node record")?;
            let node: NodeRecord =
                postcard::from_bytes(value.value()).context("failed to decode node record")?;
            nodes.insert(node.node_id, node);
        }
        Ok(nodes)
    }

    pub(crate) fn put_node(&self, node: &NodeRecord) -> Result<()> {
        let encoded = postcard::to_allocvec(node).context("failed to encode node record")?;
        let key = node.node_id.0.to_string();
        let txn = self
            .db
            .begin_write()
            .context("failed to begin database write")?;
        {
            let mut table = txn
                .open_table(NODES_TABLE)
                .context("failed to open nodes table")?;
            table
                .insert(key.as_str(), encoded.as_slice())
                .context("failed to persist node")?;
        }
        txn.commit().context("failed to commit node")?;
        Ok(())
    }

    pub(crate) fn put_metric(&self, metric: &NodeMetric) -> Result<()> {
        let encoded = postcard::to_allocvec(metric).context("failed to encode node metric")?;
        let key = format!(
            "{}:{:020}:{:020}",
            metric.node_id.0, metric.recorded_at_unix_ms, metric.snapshot.sequence
        );
        let txn = self
            .db
            .begin_write()
            .context("failed to begin database write")?;
        {
            let mut table = txn
                .open_table(METRICS_TABLE)
                .context("failed to open metrics table")?;
            table
                .insert(key.as_str(), encoded.as_slice())
                .context("failed to persist node metric")?;
            let cutoff = metric
                .recorded_at_unix_ms
                .saturating_sub(METRIC_RETENTION_MS);
            let expired = table
                .iter()
                .context("failed to iterate metrics for retention")?
                .filter_map(|item| match item {
                    Ok((key, value)) => postcard::from_bytes::<NodeMetric>(value.value())
                        .ok()
                        .filter(|stored| stored.recorded_at_unix_ms < cutoff)
                        .map(|_| key.value().to_owned()),
                    Err(_) => None,
                })
                .collect::<Vec<_>>();
            for key in expired {
                table
                    .remove(key.as_str())
                    .context("failed to prune expired node metric")?;
            }
        }
        txn.commit().context("failed to commit node metric")?;
        Ok(())
    }

    pub(crate) fn metrics_for_node(&self, node_id: NodeId) -> Result<Vec<NodeMetric>> {
        let prefix = format!("{}:", node_id.0);
        let txn = self
            .db
            .begin_read()
            .context("failed to begin database read")?;
        let table = txn
            .open_table(METRICS_TABLE)
            .context("failed to open metrics table")?;
        let mut metrics = Vec::new();
        for item in table.iter().context("failed to iterate metrics")? {
            let (key, value) = item.context("failed to read metric record")?;
            if key.value().starts_with(&prefix) {
                metrics.push(
                    postcard::from_bytes(value.value()).context("failed to decode node metric")?,
                );
            }
        }
        Ok(metrics)
    }

    pub(crate) fn put_event(&self, event: &ControlEvent) -> Result<()> {
        let encoded = postcard::to_allocvec(event).context("failed to encode control event")?;
        let key = format!(
            "{:020}:{}",
            event.occurred_at_unix_ms.max(0),
            event.event_id.0
        );
        let txn = self
            .db
            .begin_write()
            .context("failed to begin event write transaction")?;
        {
            let mut table = txn
                .open_table(EVENTS_TABLE)
                .context("failed to open events table")?;
            table
                .insert(key.as_str(), encoded.as_slice())
                .context("failed to persist control event")?;
        }
        txn.commit().context("failed to commit control event")?;
        Ok(())
    }

    pub(crate) fn recent_events(&self, limit: usize) -> Result<Vec<ControlEvent>> {
        let txn = self
            .db
            .begin_read()
            .context("failed to begin event read transaction")?;
        let table = txn
            .open_table(EVENTS_TABLE)
            .context("failed to open events table")?;
        let mut events = table
            .iter()
            .context("failed to iterate control events")?
            .map(|item| {
                let (_, value) = item.context("failed to read control event")?;
                postcard::from_bytes::<ControlEvent>(value.value())
                    .context("failed to decode control event")
            })
            .collect::<Result<Vec<_>>>()?;
        events.sort_by_key(|event| std::cmp::Reverse(event.occurred_at_unix_ms));
        events.truncate(limit);
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libongrok::{ControlEvent, EventId, EventKind, HeartbeatSnapshot, NodeMetric};

    fn metric(node_id: NodeId, recorded_at_unix_ms: i64, sequence: u64) -> NodeMetric {
        NodeMetric {
            node_id,
            recorded_at_unix_ms,
            rtt_ms: Some(12),
            snapshot: HeartbeatSnapshot {
                sequence,
                sent_at_unix_ms: recorded_at_unix_ms,
                ..HeartbeatSnapshot::default()
            },
        }
    }

    #[test]
    fn metric_retention_keeps_only_the_last_three_days() -> Result<()> {
        let path =
            std::env::temp_dir().join(format!("ongrok-metric-retention-{}.redb", NodeId::new().0));
        let store = ServiceStore::open(&path)?;
        let node_id = NodeId::new();
        let now = 1_800_000_000_000_i64;

        store.put_metric(&metric(node_id, now - METRIC_RETENTION_MS - 1, 1))?;
        store.put_metric(&metric(node_id, now - METRIC_RETENTION_MS + 1, 2))?;
        store.put_metric(&metric(node_id, now, 3))?;

        let metrics = store.metrics_for_node(node_id)?;
        assert_eq!(metrics.len(), 2);
        assert!(
            metrics
                .iter()
                .all(|item| item.recorded_at_unix_ms >= now - METRIC_RETENTION_MS)
        );
        assert!(!metrics.iter().any(|item| item.snapshot.sequence == 1));
        assert!(metrics.iter().any(|item| item.snapshot.sequence == 2));
        assert!(metrics.iter().any(|item| item.snapshot.sequence == 3));

        drop(store);
        let _ = std::fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn recent_events_are_newest_first_and_limited() -> Result<()> {
        let path = std::env::temp_dir().join(format!("ongrok-events-{}.redb", NodeId::new().0));
        let store = ServiceStore::open(&path)?;
        for (occurred_at_unix_ms, kind) in [
            (10, EventKind::NodeOnline),
            (30, EventKind::ServiceRegistered),
            (20, EventKind::NodeOffline),
        ] {
            store.put_event(&ControlEvent {
                event_id: EventId::new(),
                occurred_at_unix_ms,
                kind,
                node_id: None,
                service_id: None,
                token_kind: None,
            })?;
        }

        let events = store.recent_events(2)?;
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].occurred_at_unix_ms, 30);
        assert_eq!(events[1].occurred_at_unix_ms, 20);
        Ok(())
    }
}
