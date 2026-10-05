//! A declared scalar column's type, and the value one of its rows carries.

/// A declared-scalar value carried alongside the fixed columns (`tessera_id`, `residual`).
/// The kinds below are the whole set.
///
/// Also the value a write-ahead log row carries. Postcard encodes a variant by its index, so
/// reordering these changes the log's bytes and is a `WAL_VERSION` bump.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ScalarValue {
    Bool(bool),
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    I8(i8),
    I16(i16),
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
    /// Microseconds since the Unix epoch, stored as an `i64`; the type exists so the unit is in
    /// the manifest.
    TimestampUs(i64),
    Utf8(String),
    /// No value at all: distinct from every in-band value, including the empty string.
    ///
    /// A category expresses absence in band as the reserved code 0; a string has no such spare
    /// value, since the empty string is one a corpus may legitimately hold.
    Null,
}

/// A number or timestamp field's value as its figures hold it: an integer, or a timestamp's
/// microseconds, exactly, and a float as itself.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Number {
    Int(i128),
    Float(f64),
}

impl Number {
    pub fn as_f64(self) -> f64 {
        match self {
            Number::Int(i) => i as f64,
            Number::Float(f) => f,
        }
    }
}

impl PartialOrd for Number {
    fn partial_cmp(&self, other: &Number) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (Number::Int(a), Number::Int(b)) => a.partial_cmp(b),
            (a, b) => a.as_f64().partial_cmp(&b.as_f64()),
        }
    }
}

impl ScalarValue {
    /// The value as a number, where it is one: every integer width, both floats and a timestamp.
    pub fn number(&self) -> Option<Number> {
        Some(match *self {
            ScalarValue::U8(x) => Number::Int(i128::from(x)),
            ScalarValue::U16(x) => Number::Int(i128::from(x)),
            ScalarValue::U32(x) => Number::Int(i128::from(x)),
            ScalarValue::U64(x) => Number::Int(i128::from(x)),
            ScalarValue::I8(x) => Number::Int(i128::from(x)),
            ScalarValue::I16(x) => Number::Int(i128::from(x)),
            ScalarValue::I32(x) => Number::Int(i128::from(x)),
            ScalarValue::I64(x) | ScalarValue::TimestampUs(x) => Number::Int(i128::from(x)),
            ScalarValue::F32(x) => Number::Float(f64::from(x)),
            ScalarValue::F64(x) => Number::Float(x),
            _ => return None,
        })
    }

    /// Whether two values are the same stored value: equal, with a float compared by its bits, so
    /// a NaN is the NaN it was and `-0.0` is not `0.0`.
    pub fn same_as(&self, other: &ScalarValue) -> bool {
        match (self, other) {
            (ScalarValue::F32(a), ScalarValue::F32(b)) => a.to_bits() == b.to_bits(),
            (ScalarValue::F64(a), ScalarValue::F64(b)) => a.to_bits() == b.to_bits(),
            _ => self == other,
        }
    }

    /// This value as a render column holds it: `columns.arrow`, non-nullable with nowhere to put
    /// [`ScalarValue::Null`]. The zero goes in the column, and a presence bitmap beside it
    /// records the substitution.
    pub fn or_render_placeholder(&self, ty: ScalarType) -> ScalarValue {
        if !matches!(self, ScalarValue::Null) {
            return self.clone();
        }
        match ty {
            ScalarType::Bool => ScalarValue::Bool(false),
            ScalarType::U8 => ScalarValue::U8(0),
            ScalarType::U16 => ScalarValue::U16(0),
            ScalarType::U32 => ScalarValue::U32(0),
            ScalarType::U64 => ScalarValue::U64(0),
            ScalarType::I8 => ScalarValue::I8(0),
            ScalarType::I16 => ScalarValue::I16(0),
            ScalarType::I32 => ScalarValue::I32(0),
            ScalarType::I64 => ScalarValue::I64(0),
            ScalarType::F32 => ScalarValue::F32(0.0),
            ScalarType::F64 => ScalarValue::F64(0.0),
            ScalarType::TimestampUs => ScalarValue::TimestampUs(0),
            // Unreachable in practice: `render` on strings is refused at schema parse.
            ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text => {
                ScalarValue::Utf8(String::new())
            }
        }
    }
}

/// The Arrow type of a declared scalar column, used to build `columns.arrow`'s schema.
///
/// The widths are narrow because a hot column is baked into every row and priced per bit; the
/// width is unalterable, since changing it rewrites the corpus. `Utf8` is the one variable-width
/// member; `Bool` packs to one bit per row in Arrow. `TimestampUs` stores as an `i64` and exists
/// for the declaration, not the storage, and is the only time type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarType {
    Bool,
    U8,
    U16,
    U32,
    U64,
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
    TimestampUs,
    Utf8,
    /// A short string with no vocabulary, matched exactly. Its value is a `ScalarValue::Utf8`;
    /// the type names the storage, a per-layer front-coded dictionary with a `u32` ordinal per
    /// present entity. There is no `ScalarValue::Keyword`, since an ordinal means nothing outside
    /// its layer.
    Keyword,
    /// Prose, matched by what it says rather than by its bytes. Its value is a
    /// `ScalarValue::Utf8`; the value lives in the record blob, and `index = true` adds a
    /// per-layer token dictionary over terms a named analyser produced.
    ///
    /// The analyser is part of the column's declaration, recorded in the manifest.
    Text,
}

impl ScalarType {
    /// The spelling `MANIFEST.declared_scalars[].arrow_type` uses.
    ///
    /// These are the short forms a schema author types: `u8`, `u16`, `u32` and so on.
    pub fn arrow_type_name(self) -> &'static str {
        match self {
            ScalarType::Bool => "bool",
            ScalarType::U8 => "u8",
            ScalarType::U16 => "u16",
            ScalarType::U32 => "u32",
            ScalarType::U64 => "u64",
            ScalarType::I8 => "i8",
            ScalarType::I16 => "i16",
            ScalarType::I32 => "i32",
            ScalarType::I64 => "i64",
            ScalarType::F32 => "f32",
            ScalarType::F64 => "f64",
            ScalarType::TimestampUs => "timestamp_us",
            ScalarType::Utf8 => "utf8",
            ScalarType::Keyword => "keyword",
            ScalarType::Text => "text",
        }
    }

    /// The inverse of [`ScalarType::arrow_type_name`]; `None` for a spelling this build cannot
    /// write, treated as fail-closed rather than as an absent column.
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "bool" => ScalarType::Bool,
            "u8" => ScalarType::U8,
            "u16" => ScalarType::U16,
            "u32" => ScalarType::U32,
            "u64" => ScalarType::U64,
            "i8" => ScalarType::I8,
            "i16" => ScalarType::I16,
            "i32" => ScalarType::I32,
            "i64" => ScalarType::I64,
            "f32" => ScalarType::F32,
            "f64" => ScalarType::F64,
            "timestamp_us" => ScalarType::TimestampUs,
            "utf8" => ScalarType::Utf8,
            "keyword" => ScalarType::Keyword,
            "text" => ScalarType::Text,
            _ => return None,
        })
    }

    /// Bits, not bytes, this column adds to every row; `None` for the two string types, since
    /// neither is ever in a row.
    ///
    /// [`ScalarType::Bool`] costs one bit; a byte-denominated figure would round it to 0 or 1.
    pub fn row_bits(self) -> Option<u64> {
        Some(match self {
            ScalarType::Bool => 1,
            ScalarType::U8 | ScalarType::I8 => 8,
            ScalarType::U16 | ScalarType::I16 => 16,
            ScalarType::U32 | ScalarType::I32 | ScalarType::F32 => 32,
            ScalarType::U64 | ScalarType::I64 | ScalarType::F64 | ScalarType::TimestampUs => 64,
            // Neither is ever in a row: `render` is refused on both at the declaration, and their
            // storage is entity-space (a keyword's ordinal) or the record blob (a text value).
            ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text => return None,
        })
    }

    /// Whether a value of this type can be a category code: `u8`, `u16` or `u32` only, since a
    /// wider code space is not a vocabulary and `utf8` cannot index one at all.
    pub fn is_category_width(self) -> bool {
        matches!(self, ScalarType::U8 | ScalarType::U16 | ScalarType::U32)
    }

    /// The values an integer declaration holds, `None` for any other type. A timestamp holds an
    /// `i64`'s.
    pub fn integer_range(self) -> Option<(i128, i128)> {
        Some(match self {
            ScalarType::U8 => (0, u8::MAX.into()),
            ScalarType::U16 => (0, u16::MAX.into()),
            ScalarType::U32 => (0, u32::MAX.into()),
            ScalarType::U64 => (0, u64::MAX.into()),
            ScalarType::I8 => (i8::MIN.into(), i8::MAX.into()),
            ScalarType::I16 => (i16::MIN.into(), i16::MAX.into()),
            ScalarType::I32 => (i32::MIN.into(), i32::MAX.into()),
            ScalarType::I64 | ScalarType::TimestampUs => (i64::MIN.into(), i64::MAX.into()),
            ScalarType::Bool
            | ScalarType::F32
            | ScalarType::F64
            | ScalarType::Utf8
            | ScalarType::Keyword
            | ScalarType::Text => return None,
        })
    }

    /// The largest code this width can carry. Code `0` is the reserved absent sentinel, so the
    /// usable count is one less than this.
    pub fn max_code(self) -> Option<u32> {
        Some(match self {
            ScalarType::U8 => u8::MAX as u32,
            ScalarType::U16 => u16::MAX as u32,
            ScalarType::U32 => u32::MAX,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_float_is_the_same_value_only_bit_for_bit() {
        assert!(ScalarValue::F64(f64::NAN).same_as(&ScalarValue::F64(f64::NAN)));
        assert!(!ScalarValue::F64(-0.0).same_as(&ScalarValue::F64(0.0)));
        assert!(ScalarValue::F32(f32::NAN).same_as(&ScalarValue::F32(f32::NAN)));
        assert!(!ScalarValue::F32(-0.0).same_as(&ScalarValue::F32(0.0)));
        assert!(ScalarValue::F64(1.5).same_as(&ScalarValue::F64(1.5)));
        assert!(!ScalarValue::F64(1.5).same_as(&ScalarValue::F32(1.5)));
        assert!(ScalarValue::Utf8("a".into()).same_as(&ScalarValue::Utf8("a".into())));
    }

    #[test]
    fn an_integer_type_holds_exactly_its_rust_types_values() {
        assert_eq!(ScalarType::U8.integer_range(), Some((0, 255)));
        assert_eq!(ScalarType::I8.integer_range(), Some((-128, 127)));
        assert_eq!(
            ScalarType::U64.integer_range(),
            Some((0, u64::MAX.into()))
        );
        assert_eq!(
            ScalarType::TimestampUs.integer_range(),
            ScalarType::I64.integer_range()
        );
        assert_eq!(ScalarType::F32.integer_range(), None);
        assert_eq!(ScalarType::Keyword.integer_range(), None);
    }
}
