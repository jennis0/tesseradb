//! Ingest through `Engine::ingest` from rows shaped as the executor writes them.

use tessera_engine::{AcceptError, Engine, IngestRequest};
use tessera_lifecycle::{IngestRow, UnallocatedRow};
use tessera_types::EntityId;

pub trait IngestRows {
    /// Send `rows` as one batch into the view they all name, each carrying its label, its
    /// position and every value it holds, and answer the entity each row created or named.
    fn ingest_rows(
        &self,
        rows: Vec<UnallocatedRow>,
        batch_id: String,
        body_hash: [u8; 32],
    ) -> Result<Vec<EntityId>, AcceptError>;
}

impl IngestRows for Engine {
    fn ingest_rows(
        &self,
        rows: Vec<UnallocatedRow>,
        batch_id: String,
        body_hash: [u8; 32],
    ) -> Result<Vec<EntityId>, AcceptError> {
        let view = rows.first().map(|row| row.view.clone());
        let rows = rows
            .into_iter()
            .map(|row| IngestRow {
                tessera_id: None,
                labels: Some(row.descriptors),
                position: Some((row.x, row.y)),
                scalars: row.scalars,
                scoped: row.scoped,
                omitted: Vec::new(),
            })
            .collect();
        let receipt = self.ingest(IngestRequest {
            batch_id,
            body_hash,
            view,
            rows,
            artifacts: Default::default(),
            strict: false,
        })?;
        Ok(self
            .resolve_tessera_ids(&receipt.tessera_ids.iter().flatten().copied().collect::<Vec<_>>())
            .unwrap()
            .into_iter()
            .map(|entity| entity.expect("an accepted row names an item"))
            .collect())
    }
}
