//! `reports/disclosure.json` — every disclosure decision a declaration makes, in one diffable
//! document.
//!
//! **What it is for is the diff.** The controls this records are individually small and
//! collectively the whole of who may see what: a layer's gate, how much of a membership a viewer
//! must already hold, whether a vocabulary's values are published as authored, where each
//! attribute's values live. Reading them out of a config means reading a document written to be
//! authored rather than reviewed — six blocks, sub-blocks, sugar that expands into layers nobody
//! typed. Reading them out of `MANIFEST.json` means reading them mixed into segment digests and
//! file sizes. So a reviewer's real question — *which disclosure decision moved between these two
//! builds?* — has no cheap answer, and this file is that answer.
//!
//! **Everything here is stable across two builds of one declaration**: no timestamp, no path, no
//! machine-dependent value, no map whose iteration order is a hash. Views, attributes and layers
//! keep declaration order, which is load-bearing on all three (a reordered attribute list reorders
//! the stored scalar tail, a reordered layer list reorders registration); vocabularies are sorted
//! by name, theirs being a map with no declared order to keep.
//!
//! **It is derived from the declaration and from nothing the build computes**, which is why it is
//! written beside the build rather than inside it, and why `tessera check` can emit exactly the
//! same document without opening a data file. `reports/containment.json` is the other way round —
//! it is a *result*, needing every artifact published — and the two sit in one directory because
//! both are notices for an operator rather than anything a reader loads.

use std::path::Path;

use serde::Serialize;
use serde_json::json;
use tessera_types::layer::{ExistenceCriterion, MemberDefault, MembershipSource, SuppliedRequirement};

use crate::config::{Config, ValueSet};
use crate::error::Result;

/// Every disclosure decision one declaration makes.
#[derive(Debug, Clone, Serialize)]
pub struct Disclosure {
    pub views: Vec<ViewDisclosure>,
    pub vocabularies: Vec<VocabularyDisclosure>,
    pub attributes: Vec<AttributeDisclosure>,
    pub layers: Vec<LayerDisclosure>,
}

/// A view's own decision: where a point's access terms come from, and what a point carrying none
/// gets. **The path is deliberately absent** — where the rows are is acquisition, varies by
/// machine, and would put a diff-defeating absolute path in a document whose whole value is the
/// diff (`configuration.md` §2).
#[derive(Debug, Clone, Serialize)]
pub struct ViewDisclosure {
    pub name: String,
    /// `field`, `relation`, or `default_only` — which of the three shapes `point_visibility`
    /// declares.
    pub labels_from: &'static str,
    /// What a point carrying no terms of its own is given. Never `inherited`: a point carrying no
    /// terms sits in no principal's mask.
    pub default: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct VocabularyDisclosure {
    pub name: String,
    /// `public` — published as authored — or `derived`, filtered per principal to the values a
    /// viewer can already see a row carrying.
    pub visibility: &'static str,
    /// `closed` or `open`: is an unknown key at ingest refused, or minted?
    pub value_set: &'static str,
    /// How many values the declaration itself carries. A count rather than the keys: the keys are
    /// in the manifest, and an authored value set is the one part of this whose diff is the
    /// vocabulary's own history rather than a control moving.
    pub declared_values: usize,
    /// Retired codes, never reassigned.
    pub reserved: Vec<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AttributeDisclosure {
    pub name: String,
    /// The source column, where the declaration moved it off the served name.
    pub field: String,
    #[serde(rename = "type")]
    pub ty: &'static str,
    /// Which of the three homes this column occupies (`records-and-search.md` §3–§5): `hot`, a
    /// fixed-width slot in every row; `index`, an entity-space search structure; `hot+index`,
    /// both; `blob`, neither — record-resident, for drill-down alone.
    pub placement: &'static str,
    /// The value set this column draws on, for a category.
    pub vocabulary: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LayerDisclosure {
    pub name: String,
    /// The layer whose `[layer.labels]` block wrote this one, where the sugar did. Absent for a
    /// layer someone typed.
    pub expanded_from: Option<String>,
    pub views: Vec<String>,
    /// The access label a viewer must hold to know this layer exists — `public` where the
    /// declaration said so.
    pub visibility: String,
    pub artifact_visibility: ArtifactVisibilityDisclosure,
    /// `all`, `any`, `{ "count": n }`, `{ "fraction": p }` or `none` — the caller's own spelling,
    /// so a reviewer diffs the words that were written rather than the form they compiled to.
    pub require_member_visibility: serde_json::Value,
    /// The layers this one's edges point into. **A disclosure decision, and not obviously one**:
    /// an artifact here is served only where the artifact it attaches to is served
    /// ([decision 0089](../../../docs/decisions/0089-a-dependency-edge-carries-deletion-and-visibility.md)),
    /// so adding an edge narrows this layer and removing one widens it — neither visible in this
    /// layer's own gate.
    pub depends_on: Vec<String>,
    /// `enumerated`, `spatial`, or the value column an attribute membership is a predicate over.
    pub membership: String,
    pub content: ContentDisclosure,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArtifactVisibilityDisclosure {
    /// Present iff artifacts on this layer carry access labels of their own — what the register
    /// watches (C27).
    pub field: Option<String>,
    /// What an artifact carrying none gets: `inherited`, or a label.
    pub default: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContentDisclosure {
    /// Recomputed per viewer from `membership ∩ M_auth`; contained by construction, so these carry
    /// no requirement of their own.
    pub computed: Vec<String>,
    pub supplied: Vec<SuppliedDisclosure>,
    /// Whether supplied content is dropped when one of its generating set is deleted.
    pub withdraw_on_member_deletion: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SuppliedDisclosure {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
    /// `all` — generated from the documents it names, so read only by a viewer who can read all of
    /// them — or `inherited`, true whether or not any of them exists. The register watches this
    /// (C28).
    pub require_member_visibility: &'static str,
}

impl Disclosure {
    /// Read every disclosure decision out of a parsed declaration.
    pub fn of(config: &Config) -> Disclosure {
        let views = config
            .views
            .iter()
            .map(|view| ViewDisclosure {
                name: view.name.clone(),
                labels_from: match (
                    &view.point_visibility.field,
                    &view.point_visibility.source,
                ) {
                    (_, Some(_)) => "relation",
                    (Some(_), None) => "field",
                    (None, None) => "default_only",
                },
                default: view.point_visibility.default.clone(),
            })
            .collect();

        // Sorted, because `Schema::vocabularies` is a hash map: iteration order there is not a
        // fact about the declaration and would make every build's diff noise.
        let mut vocabulary_names: Vec<&String> = config.schema.vocabularies.keys().collect();
        vocabulary_names.sort_unstable();
        let vocabularies = vocabulary_names
            .into_iter()
            .map(|name| {
                let vocabulary = &config.schema.vocabularies[name];
                VocabularyDisclosure {
                    name: vocabulary.name.clone(),
                    visibility: match vocabulary.visibility {
                        crate::config::Listing::Public => "public",
                        crate::config::Listing::PerViewer => "derived",
                    },
                    value_set: match vocabulary.value_set {
                        ValueSet::Closed => "closed",
                        ValueSet::Open => "open",
                    },
                    declared_values: vocabulary.codes.len(),
                    reserved: vocabulary.reserved.clone(),
                }
            })
            .collect();

        let attributes = config
            .schema
            .attributes
            .iter()
            .map(|attribute| AttributeDisclosure {
                name: attribute.name.clone(),
                field: attribute.column().to_string(),
                ty: attribute.ty.arrow_type_name(),
                placement: match (attribute.render, attribute.index) {
                    (true, true) => "hot+index",
                    (true, false) => "hot",
                    (false, true) => "index",
                    (false, false) => "blob",
                },
                vocabulary: attribute.vocabulary.clone(),
            })
            .collect();

        let layers = config
            .layers
            .iter()
            .map(|layer| LayerDisclosure {
                name: layer.name.clone(),
                expanded_from: config.label_layers.get(&layer.name).cloned(),
                views: layer.views.clone(),
                // `None` here is the config's `visibility = "public"`, spelled as an absence in the
                // declaration type. Written back out as the word, which is what a reviewer wrote.
                visibility: layer
                    .visibility
                    .clone()
                    .unwrap_or_else(|| "public".to_string()),
                artifact_visibility: ArtifactVisibilityDisclosure {
                    field: layer.artifact_visibility.field.clone(),
                    default: match &layer.artifact_visibility.default {
                        MemberDefault::Inherited => "inherited".to_string(),
                        MemberDefault::Label(label) => label.clone(),
                    },
                },
                require_member_visibility: criterion(layer.require_member_visibility),
                depends_on: layer.depends_on.clone(),
                membership: match &layer.membership {
                    MembershipSource::Enumerated => "enumerated".to_string(),
                    MembershipSource::Spatial => "spatial".to_string(),
                    MembershipSource::Attribute(field) => format!("attribute:{field}"),
                },
                content: ContentDisclosure {
                    computed: layer.content.computed.clone(),
                    supplied: layer
                        .content
                        .supplied
                        .iter()
                        .map(|supplied| SuppliedDisclosure {
                            name: supplied.name.clone(),
                            ty: supplied.ty.clone(),
                            require_member_visibility: match supplied.require_member_visibility {
                                SuppliedRequirement::All => "all",
                                SuppliedRequirement::Inherited => "inherited",
                            },
                        })
                        .collect(),
                    withdraw_on_member_deletion: layer.content.withdraw_on_member_deletion,
                },
            })
            .collect();

        Disclosure {
            views,
            vocabularies,
            attributes,
            layers,
        }
    }

    /// Whether this declaration makes any disclosure decision at all beyond a view's own default.
    ///
    /// A declaration with no layer, no vocabulary and no attribute is bare geometry: the only
    /// control it carries is the label every point takes, which the view's own compiled form
    /// already records. Writing a report for it would create `reports/` in every bundle ever
    /// built — the directory an operator polls for the fold's notices — to say nothing, which is
    /// the argument `write_containment_report` makes for its own emptiness.
    fn decides_anything(&self) -> bool {
        !(self.layers.is_empty() && self.vocabularies.is_empty() && self.attributes.is_empty())
    }
}

/// One `require_member_visibility`, in the words the config spells it with.
///
/// `all` and `any` are the two the declaration compiles to a criterion — `Fraction(1.0)` and
/// `Count(1)` — and writing the criterion back out would make a reviewer translate. The point of
/// the file is that they do not have to.
fn criterion(criterion: Option<ExistenceCriterion>) -> serde_json::Value {
    match criterion {
        None => json!("none"),
        Some(ExistenceCriterion::Count(1)) => json!("any"),
        Some(ExistenceCriterion::Count(n)) => json!({ "count": n }),
        Some(ExistenceCriterion::Fraction(1.0)) => json!("all"),
        Some(ExistenceCriterion::Fraction(p)) => json!({ "fraction": p }),
    }
}

/// Write `reports/disclosure.json` into a bundle root, or nothing at all where the declaration
/// decides nothing ([`Disclosure::decides_anything`]).
pub fn write_disclosure_report(root: &Path, disclosure: &Disclosure) -> Result<()> {
    if !disclosure.decides_anything() {
        return Ok(());
    }
    let dir = root.join("reports");
    std::fs::create_dir_all(&dir).map_err(|e| crate::error::BuildError::io(&dir, e))?;
    crate::write_json(&dir.join("disclosure.json"), disclosure)
}
