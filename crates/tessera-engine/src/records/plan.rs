//! Which columns a request names, and where each is read from.

use tessera_spatial::tiler::ScalarType;

use super::{RecordsOrder, RecordsRefused};
use crate::error::{EngineError, Result};
use crate::filter::{Family, FieldHomes, PIN};
use crate::viewport::{EngineMeta, LeafColumn};

/// One named field, resolved.
pub(crate) struct Named {
    /// The spelling the request used, which is the column's name in every page.
    pub(crate) name: String,
    pub(crate) ty: ScalarType,
    pub(crate) vocabulary: Option<String>,
    pub(crate) home: Home,
}

/// Where a named field's value is read from, one home per field.
pub(crate) enum Home {
    /// The row tail of the requested view, by the declared name.
    Rendered,
    /// An entity-space value column, under its internal key: a declared column's own name, or a
    /// group-scoped family's resolved column.
    ValueColumn(String),
    /// The record store, under the field's declaration position.
    Record(u16),
}

/// A column the system supplies rather than the schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SystemField {
    Position,
    ExternalId,
    Labels,
}

impl SystemField {
    fn parse(name: &str) -> Option<SystemField> {
        match name {
            "position" => Some(SystemField::Position),
            "external_id" => Some(SystemField::ExternalId),
            "labels" => Some(SystemField::Labels),
            _ => None,
        }
    }
}

/// Every column a request's pages carry after `tessera_id`, in order.
pub(crate) struct FieldPlan {
    pub(crate) named: Vec<Named>,
    pub(crate) system: Vec<SystemField>,
}

impl FieldPlan {
    /// Resolve the request's `fields` and `system_fields` under `view`. A group-scoped field
    /// resolves as a filter leaf on it does, through the same resolution, with every family
    /// admitted since a field need not be filterable to be read.
    pub(crate) fn resolve(
        meta: &EngineMeta,
        view: &str,
        visible: &crate::gate::VisibleViews,
        fields: &[String],
        system_fields: &[String],
    ) -> Result<FieldPlan> {
        let refused = |why| Err(EngineError::RecordsRefused(why));
        let mut named: Vec<Named> = Vec::with_capacity(fields.len());
        for spelling in fields {
            if named.iter().any(|n| &n.name == spelling) {
                return refused(RecordsRefused::RepeatedField(spelling.clone()));
            }
            named.push(resolve_field(meta, view, visible, spelling)?);
        }
        let mut system: Vec<SystemField> = Vec::with_capacity(system_fields.len());
        for spelling in system_fields {
            let Some(field) = SystemField::parse(spelling) else {
                return refused(RecordsRefused::UnknownSystemField(spelling.clone()));
            };
            if system.contains(&field) {
                return refused(RecordsRefused::RepeatedField(spelling.clone()));
            }
            system.push(field);
        }
        Ok(FieldPlan { named, system })
    }

    /// The order a request that names none and carries no cursor is served in: stored when any
    /// named field is read only from the record store, which holds rows in that order.
    pub(crate) fn preferred_order(&self) -> RecordsOrder {
        match self.named.iter().any(|n| matches!(n.home, Home::Record(_))) {
            true => RecordsOrder::Stored,
            false => RecordsOrder::Map,
        }
    }
}

fn resolve_field(
    meta: &EngineMeta,
    view: &str,
    visible: &crate::gate::VisibleViews,
    spelling: &str,
) -> Result<Named> {
    let refused = |why| Err(EngineError::RecordsRefused(why));
    let (name, pin) = match spelling.split_once(PIN) {
        Some((name, pin)) => (name, Some(pin)),
        None => (spelling, None),
    };
    if let Some(index) = meta.declared_scalars.iter().position(|d| d.name == name) {
        if pin.is_some() {
            return refused(RecordsRefused::PinOnUnscoped(name.to_string()));
        }
        let declared = &meta.declared_scalars[index];
        let homes: FieldHomes = meta.homes[index];
        let home = if homes.rendered {
            Home::Rendered
        } else if homes.value_column {
            Home::ValueColumn(declared.name.clone())
        } else {
            Home::Record(u16::try_from(index).map_err(|_| {
                EngineError::Malformed(format!(
                    "field '{name}' is declared at position {index}, past what a record row can \
                     tag"
                ))
            })?)
        };
        return Ok(Named {
            name: spelling.to_string(),
            ty: declared.arrow_type,
            vocabulary: declared.vocabulary.clone(),
            home,
        });
    }
    match meta.resolve_scoped_column(name, pin, view, visible, |_| true) {
        LeafColumn::Resolved {
            family: Family::Text,
            ..
        } => refused(RecordsRefused::ScopedText(spelling.to_string())),
        LeafColumn::Resolved { column, .. } => {
            let family = meta
                .scoped_scalars
                .iter()
                .find(|f| f.name == name)
                .expect("a resolved scoped column names a declared family");
            Ok(Named {
                name: spelling.to_string(),
                ty: family.arrow_type,
                vocabulary: family.vocabulary.clone(),
                home: Home::ValueColumn(column),
            })
        }
        LeafColumn::Unpinned { group } => refused(RecordsRefused::Unpinned {
            field: name.to_string(),
            group,
        }),
        LeafColumn::UnknownPin { group, pin } => Err(EngineError::UnknownView(format!(
            "{group}{}{pin}",
            tessera_store::GROUP_SEPARATOR
        ))),
        LeafColumn::PinOnUnscoped { column } => refused(RecordsRefused::PinOnUnscoped(column)),
        LeafColumn::Unknown => refused(RecordsRefused::UnknownField(spelling.to_string())),
    }
}
