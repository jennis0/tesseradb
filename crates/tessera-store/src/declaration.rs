//! What an attribute column and a vocabulary may declare. `tessera build` and the running service
//! both check a declaration here, so it means the same thing at either.

use std::collections::BTreeSet;

use tessera_spatial::tiler::ScalarType;

pub const DECLARABLE_TYPES: &str = "bool, u8, u16, u32, u64, i8, i16, i32, i64, f32, f64, \
                                    timestamp_us, keyword, text and category";

/// Columns every segment carries.
const FIXED_COLUMNS: [&str; 2] = ["tessera_id", "residual"];
/// Columns of an ingest batch that are not attributes.
const INGEST_COLUMNS: [&str; 5] = ["external_id", "x", "y", "access", "node_id"];
/// Keys of a filter expression and the frames' highlight column. A filter names a column directly,
/// so a column under one of these would make a request ambiguous.
pub const REQUEST_NAMES: [&str; 6] = [
    "all_of",
    "any_of",
    "none_of",
    "region",
    "member_of",
    "highlighted",
];

/// An attribute as declared.
#[derive(Debug, Clone, Copy)]
pub struct AttributeSpec<'a> {
    pub name: &'a str,
    /// A storage type's name, or `category`.
    pub ty: &'a str,
    pub vocabulary: Option<&'a str>,
    pub analyser: Option<&'a str>,
    pub index: bool,
    pub render: bool,
    /// Whether the column is scoped to a view group rather than to the entity.
    pub group_scoped: bool,
}

/// What a checked attribute stores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub ty: ScalarType,
    pub vocabulary: Option<String>,
    /// The analyser's recorded identity, for a `text` column.
    pub analyser: Option<String>,
}

/// Check an attribute. `vocabulary_width` answers the width of a declared vocabulary, or `None`
/// where no vocabulary has that name.
pub fn check_attribute(
    spec: &AttributeSpec,
    vocabulary_width: impl Fn(&str) -> Option<ScalarType>,
) -> Result<Column, String> {
    let name = spec.name;
    check_column_name(name)?;

    if spec.ty == "category" {
        let vocabulary = spec.vocabulary.ok_or_else(|| {
            format!("attribute '{name}': a category needs `vocabulary`, naming a declared one")
        })?;
        let width = vocabulary_width(vocabulary)
            .ok_or_else(|| format!("attribute '{name}': no vocabulary named '{vocabulary}'"))?;
        if spec.analyser.is_some() {
            return Err(format!(
                "attribute '{name}': `analyser` applies to `text` only"
            ));
        }
        return Ok(Column {
            ty: width,
            vocabulary: Some(vocabulary.to_string()),
            analyser: None,
        });
    }

    // `utf8` is a wire type, not a declarable one.
    let ty = ScalarType::parse(spec.ty)
        .filter(|ty| *ty != ScalarType::Utf8)
        .ok_or_else(|| {
            format!(
                "attribute '{name}': unknown type '{}'. The types are {DECLARABLE_TYPES}",
                spec.ty
            )
        })?;
    if spec.vocabulary.is_some() {
        return Err(format!(
            "attribute '{name}': `vocabulary` applies to `type = \"category\"` only"
        ));
    }
    let analyser = match (ty, spec.analyser) {
        (ScalarType::Text, analyser) => {
            let analyser = analyser.unwrap_or(tessera_analyse::UNICODE);
            let identity = tessera_analyse::identity_of(analyser).ok_or_else(|| {
                format!(
                    "attribute '{name}': no analyser named '{analyser}'. Available: {}",
                    tessera_analyse::ANALYSER_NAMES.join(", ")
                )
            })?;
            Some(identity)
        }
        (_, Some(_)) => {
            return Err(format!(
                "attribute '{name}': `analyser` applies to `text` only"
            ));
        }
        (_, None) => None,
    };
    // A rendered value is a fixed-width slot in every row, which a string is not.
    if spec.render && matches!(ty, ScalarType::Text | ScalarType::Keyword) {
        return Err(format!(
            "attribute '{name}': `render` does not apply to `{}`; use a category to colour by a \
             string, or `index = true` to filter by it",
            spec.ty
        ));
    }
    // A group-scoped column has no slot in the record blob, so the token index is the only place
    // a scoped text column's values can live.
    if spec.group_scoped && ty == ScalarType::Text && !spec.index {
        return Err(format!(
            "attribute '{name}': a group-scoped `text` column needs `index = true`"
        ));
    }
    Ok(Column {
        ty,
        vocabulary: None,
        analyser,
    })
}

/// A column name is a URL path segment (`/v1/categories/{column}`) and a key in a filter
/// expression, and must not collide with a column the system writes itself.
pub fn check_column_name(name: &str) -> Result<(), String> {
    check_identifier("attribute", name)?;
    if FIXED_COLUMNS.contains(&name) || INGEST_COLUMNS.contains(&name) || name == "record" {
        return Err(format!("attribute '{name}': that name is reserved"));
    }
    if REQUEST_NAMES.contains(&name) {
        return Err(format!(
            "attribute '{name}': that name is a key of a filter request. Reserved: {}",
            REQUEST_NAMES.join(", ")
        ));
    }
    Ok(())
}

/// Check a vocabulary's name, code width and retired codes, and answer the width.
pub fn check_vocabulary(name: &str, width: &str, reserved: &[u32]) -> Result<ScalarType, String> {
    check_identifier("vocabulary", name)?;
    let width = ScalarType::parse(width)
        .filter(|ty| ty.is_category_width())
        .ok_or_else(|| {
            format!("vocabulary '{name}': `width = \"{width}\"` is not `u8`, `u16` or `u32`")
        })?;
    let max = max_code(width);
    for &code in reserved {
        // Code 0 means "no value" in every category column.
        if code == 0 || code > max {
            return Err(format!(
                "vocabulary '{name}': reserved code {code} is outside 1..={max}"
            ));
        }
    }
    Ok(width)
}

/// Check the keys of the values declared together: none empty, none twice.
pub fn check_value_keys<'a>(
    vocabulary: &str,
    keys: impl IntoIterator<Item = &'a str>,
) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for key in keys {
        if key.is_empty() {
            return Err(format!(
                "vocabulary '{vocabulary}': a value with an empty key"
            ));
        }
        if !seen.insert(key) {
            return Err(format!(
                "vocabulary '{vocabulary}': value '{key}' is given twice"
            ));
        }
    }
    Ok(())
}

/// The highest code a category width holds.
pub fn max_code(width: ScalarType) -> u32 {
    width.max_code().unwrap_or(u32::MAX)
}

fn check_identifier(kind: &str, name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err(format!("{kind} with an empty name"));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(format!(
            "{kind} '{name}': a name is limited to ASCII letters, digits, `_` and `-`"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec<'a>(name: &'a str, ty: &'a str) -> AttributeSpec<'a> {
        AttributeSpec {
            name,
            ty,
            vocabulary: None,
            analyser: None,
            index: false,
            render: false,
            group_scoped: false,
        }
    }

    fn widths(name: &str) -> Option<ScalarType> {
        (name == "dept").then_some(ScalarType::U8)
    }

    #[test]
    fn a_category_stores_its_vocabularys_width() {
        let column = check_attribute(
            &AttributeSpec {
                vocabulary: Some("dept"),
                ..spec("team", "category")
            },
            widths,
        )
        .unwrap();
        assert_eq!(column.ty, ScalarType::U8);
        assert_eq!(column.vocabulary.as_deref(), Some("dept"));
    }

    #[test]
    fn a_text_column_records_its_analyser_and_defaults_to_unicode() {
        let column = check_attribute(&spec("body", "text"), widths).unwrap();
        let unicode = tessera_analyse::analyser(tessera_analyse::UNICODE).unwrap();
        assert_eq!(column.analyser, Some(unicode.identity()));
    }

    #[test]
    fn every_other_declarable_type_is_accepted_as_itself() {
        for ty in [
            "bool",
            "u8",
            "u16",
            "u32",
            "u64",
            "i8",
            "i16",
            "i32",
            "i64",
            "f32",
            "f64",
            "timestamp_us",
            "keyword",
        ] {
            let column = check_attribute(&spec("c", ty), widths).unwrap();
            assert_eq!(column.ty, ScalarType::parse(ty).unwrap());
        }
    }

    #[test]
    fn declarations_that_are_refused() {
        let refused: Vec<(&str, AttributeSpec)> = vec![
            ("an empty name", spec("", "u8")),
            ("a name that is not a path segment", spec("a/b", "u8")),
            ("a fixed column's name", spec("tessera_id", "u8")),
            ("an ingest column's name", spec("access", "u8")),
            ("the record blob's name", spec("record", "u8")),
            ("a filter key", spec("any_of", "u8")),
            ("an unknown type", spec("c", "string")),
            ("the wire-only string type", spec("c", "utf8")),
            ("a category without a vocabulary", spec("c", "category")),
            (
                "a category over an undeclared vocabulary",
                AttributeSpec {
                    vocabulary: Some("nothing"),
                    ..spec("c", "category")
                },
            ),
            (
                "a vocabulary on a plain type",
                AttributeSpec {
                    vocabulary: Some("dept"),
                    ..spec("c", "u8")
                },
            ),
            (
                "an analyser on a plain type",
                AttributeSpec {
                    analyser: Some("unicode"),
                    ..spec("c", "keyword")
                },
            ),
            (
                "an unknown analyser",
                AttributeSpec {
                    analyser: Some("klingon"),
                    ..spec("c", "text")
                },
            ),
            (
                "rendered text",
                AttributeSpec {
                    render: true,
                    ..spec("c", "text")
                },
            ),
            (
                "a rendered keyword",
                AttributeSpec {
                    render: true,
                    ..spec("c", "keyword")
                },
            ),
            (
                "group-scoped text without an index",
                AttributeSpec {
                    group_scoped: true,
                    ..spec("c", "text")
                },
            ),
        ];
        for (what, declared) in refused {
            assert!(
                check_attribute(&declared, widths).is_err(),
                "{what} was accepted"
            );
        }
    }

    #[test]
    fn a_vocabulary_answers_its_width_and_bounds_its_reserved_codes() {
        assert_eq!(check_vocabulary("dept", "u8", &[1, 255]), Ok(ScalarType::U8));
        assert!(check_vocabulary("dept", "u8", &[0]).is_err());
        assert!(check_vocabulary("dept", "u8", &[256]).is_err());
        assert!(check_vocabulary("dept", "u64", &[]).is_err());
        assert!(check_vocabulary("a b", "u8", &[]).is_err());
        assert!(check_vocabulary("", "u8", &[]).is_err());
    }

    #[test]
    fn value_keys_are_non_empty_and_distinct() {
        assert!(check_value_keys("dept", ["a", "b"]).is_ok());
        assert!(check_value_keys("dept", ["a", ""]).is_err());
        assert!(check_value_keys("dept", ["a", "a"]).is_err());
    }
}
