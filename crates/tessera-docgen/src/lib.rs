//! Reference pages rendered from the source they describe.
//!
//! A crate's tests read the crate's own source with `syn`, take each key's description from the
//! doc comment on the field that parses it, render a page, and compare it with the committed copy
//! ([`check_page`]). The site builds from the committed copy, so it needs no cargo.
//!
//! A field's doc comment is the key's description. Three kinds of paragraph are lifted out of it:
//!
//! - `Type: …` gives the TOML type where the Rust type cannot, as for a `toml::Value` read by hand
//!   or a field with a deserializer of its own.
//! - `Default: …` fills the Default column. A default that begins with a code span states a TOML
//!   value ([`Key::stated_value`]), and the generator's tests parse a file that writes it.
//! - `Required.` marks a key the parser refuses to go without.

use std::path::Path;

/// One Rust source file.
pub struct Source {
    file: syn::File,
}

/// One key of a table, as its field declares it.
pub struct Key {
    /// The name a TOML file writes: the field's own, or its `#[serde(rename)]`.
    pub name: String,
    /// The field's type with any `Option` removed.
    pub ty: syn::Type,
    pub optional: bool,
    /// `#[serde(default)]`: an absent key takes its type's own default.
    pub serde_default: bool,
    /// `#[serde(deserialize_with)]`: the Rust type does not say what the file writes.
    pub custom: bool,
    pub doc: Doc,
}

/// A doc comment, with its `Type:`, `Default:` and `Required.` paragraphs lifted out.
#[derive(Default)]
pub struct Doc {
    /// The remaining paragraphs as written, the lines of each joined by newlines.
    pub paragraphs: Vec<String>,
    pub ty: Option<String>,
    pub default: Option<String>,
    pub required: bool,
}

impl Source {
    pub fn read(path: &Path) -> Source {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let file = syn::parse_file(&text)
            .unwrap_or_else(|e| panic!("{} does not parse: {e}", path.display()));
        Source { file }
    }

    /// Whether the file declares a struct of this name at its top level.
    pub fn has_struct(&self, name: &str) -> bool {
        self.find(name).is_some()
    }

    /// The struct's own doc comment.
    pub fn struct_doc(&self, name: &str) -> Doc {
        Doc::of(&self.expect(name).attrs)
    }

    /// The struct's fields, in declaration order.
    pub fn keys(&self, name: &str) -> Vec<Key> {
        let syn::Fields::Named(fields) = &self.expect(name).fields else {
            panic!("struct {name} has no named fields");
        };
        fields.named.iter().map(Key::of).collect()
    }

    fn find(&self, name: &str) -> Option<&syn::ItemStruct> {
        self.file.items.iter().find_map(|item| match item {
            syn::Item::Struct(item) if item.ident == name => Some(item),
            _ => None,
        })
    }

    fn expect(&self, name: &str) -> &syn::ItemStruct {
        self.find(name)
            .unwrap_or_else(|| panic!("no struct {name} at the top level of the source"))
    }
}

impl Key {
    fn of(field: &syn::Field) -> Key {
        let ident = field.ident.as_ref().expect("a named field").to_string();
        let mut rename = None;
        let mut serde_default = false;
        let mut custom = false;
        for attr in field.attrs.iter().filter(|a| a.path().is_ident("serde")) {
            attr.parse_nested_meta(|meta| {
                let valued = meta.input.peek(syn::Token![=]);
                if meta.path.is_ident("rename") {
                    rename = Some(meta.value()?.parse::<syn::LitStr>()?.value());
                } else {
                    if meta.path.is_ident("default") {
                        serde_default = true;
                    }
                    if meta.path.is_ident("deserialize_with") || meta.path.is_ident("with") {
                        custom = true;
                    }
                    if valued {
                        meta.value()?.parse::<syn::Expr>()?;
                    }
                }
                Ok(())
            })
            .unwrap_or_else(|e| panic!("field {ident}: a serde attribute does not parse: {e}"));
        }
        let (ty, optional) = match generic(&field.ty, "Option").as_deref() {
            Some([inner]) => ((*inner).clone(), true),
            _ => (field.ty.clone(), false),
        };
        Key {
            name: rename.unwrap_or(ident),
            ty,
            optional,
            serde_default,
            custom,
            doc: Doc::of(&field.attrs),
        }
    }

    /// The TOML type: the `Type:` paragraph, or what the Rust type reads. `table` answers whether
    /// a type name is a table the page describes.
    pub fn type_column(&self, table: &dyn Fn(&str) -> bool) -> Result<String, String> {
        let derived = if self.custom {
            None
        } else {
            toml_type(&self.ty, table)
        };
        match (&self.doc.ty, derived) {
            (Some(stated), None) => Ok(stated.clone()),
            (None, Some(derived)) => Ok(derived),
            (Some(_), Some(derived)) => Err(format!(
                "`{}`: its type reads as {derived}, so its doc comment needs no `Type:` paragraph",
                self.name
            )),
            (None, None) => Err(format!(
                "`{}`: the Rust type does not say what the file writes; add a `Type:` paragraph \
                 to its doc comment",
                self.name
            )),
        }
    }

    /// The Default column: `required`, what the doc comment states, or the value serde fills.
    pub fn default_column(&self) -> Result<String, String> {
        if self.doc.required {
            return Ok("required".to_string());
        }
        if let Some(stated) = &self.doc.default {
            return Ok(stated.clone());
        }
        if !self.optional && !self.serde_default {
            return Ok("required".to_string());
        }
        if self.serde_default && !self.optional {
            if let Some(value) = zero_value(&self.ty) {
                return Ok(format!("`{value}`"));
            }
        }
        Err(format!(
            "`{}`: its doc comment states no default; add a `Default:` or a `Required.` paragraph",
            self.name
        ))
    }

    /// The description: every paragraph that is not lifted out, as one line.
    pub fn description(&self) -> Result<String, String> {
        let text = self.doc.paragraphs.join(" ");
        if text.trim().is_empty() {
            return Err(format!("`{}` has no doc comment to describe it", self.name));
        }
        Ok(text)
    }

    /// The name of the type this key's value is, and whether the file writes an array of them:
    /// `("RawServe", false)` for `Option<RawServe>`, `("LevelBlock", true)` for `Vec<LevelBlock>`.
    pub fn type_name(&self) -> Option<(String, bool)> {
        let (ty, array) = match generic(&self.ty, "Vec").as_deref() {
            Some([inner]) => (*inner, true),
            _ => (&self.ty, false),
        };
        let ty = match generic(ty, "Option").as_deref() {
            Some([inner]) => *inner,
            _ => ty,
        };
        let syn::Type::Path(path) = ty else {
            return None;
        };
        Some((path.path.segments.last()?.ident.to_string(), array))
    }

    /// The TOML value a `Default:` paragraph states, where it begins with a code span.
    pub fn stated_value(&self) -> Option<&str> {
        let rest = self.doc.default.as_deref()?.strip_prefix('`')?;
        rest.split_once('`').map(|(value, _)| value)
    }

    /// The row for this key in a table of [`TABLE_HEADER`]'s four columns.
    pub fn row(&self, table: &dyn Fn(&str) -> bool) -> Result<String, String> {
        Ok(format!(
            "| `{}` | {} | {} | {} |",
            self.name,
            cell(&self.type_column(table)?),
            cell(&self.default_column()?),
            cell(&self.description()?)
        ))
    }
}

impl Doc {
    fn of(attrs: &[syn::Attribute]) -> Doc {
        let mut lines = Vec::new();
        for attr in attrs.iter().filter(|a| a.path().is_ident("doc")) {
            if let syn::Meta::NameValue(syn::MetaNameValue {
                value:
                    syn::Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Str(text),
                        ..
                    }),
                ..
            }) = &attr.meta
            {
                // `split`, not `lines`: a blank `///` is an empty string, and a paragraph break.
                for line in text.value().split('\n') {
                    lines.push(line.strip_prefix(' ').unwrap_or(line).to_string());
                }
            }
        }
        let mut paragraphs: Vec<String> = Vec::new();
        let mut current: Vec<String> = Vec::new();
        let mut fenced = false;
        for line in lines {
            if line.trim_start().starts_with("```") {
                fenced = !fenced;
            }
            if line.trim().is_empty() && !fenced {
                if !current.is_empty() {
                    paragraphs.push(current.join("\n"));
                    current.clear();
                }
                continue;
            }
            current.push(line);
        }
        if !current.is_empty() {
            paragraphs.push(current.join("\n"));
        }

        let mut doc = Doc::default();
        for paragraph in paragraphs {
            let flat = paragraph.split_whitespace().collect::<Vec<_>>().join(" ");
            if let Some(ty) = flat.strip_prefix("Type: ") {
                doc.ty = Some(ty.strip_suffix('.').unwrap_or(ty).to_string());
            } else if let Some(default) = flat.strip_prefix("Default: ") {
                doc.default = Some(default.strip_suffix('.').unwrap_or(default).to_string());
            } else if flat == "Required." {
                doc.required = true;
            } else {
                doc.paragraphs.push(paragraph);
            }
        }
        doc
    }

    /// The paragraphs as markdown, for the text above a table. A paragraph's lines are joined, and
    /// a list item's with it, because a code span broken across two lines does not render; a
    /// fenced block is kept as written.
    pub fn markdown(&self) -> String {
        self.paragraphs
            .iter()
            .map(|paragraph| {
                if paragraph.starts_with("```") {
                    return paragraph.clone();
                }
                let mut lines: Vec<String> = Vec::new();
                for line in paragraph.lines() {
                    let item = line.starts_with("- ") || line.starts_with("* ");
                    match lines.last_mut() {
                        Some(last) if !item => {
                            last.push(' ');
                            last.push_str(line.trim());
                        }
                        _ => lines.push(line.to_string()),
                    }
                }
                lines.join("\n")
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

/// The first row of every table [`Key::row`] renders.
pub const TABLE_HEADER: &str = "| Key | Type | Default | Description |\n| --- | --- | --- | --- |";

/// What a TOML file writes for a field of Rust type `ty`, or `None` where the type does not say.
fn toml_type(ty: &syn::Type, table: &dyn Fn(&str) -> bool) -> Option<String> {
    match ty {
        syn::Type::Array(array) => {
            let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Int(len),
                ..
            }) = &array.len
            else {
                return None;
            };
            Some(format!(
                "array of {} {}",
                len.base10_digits(),
                plural(&toml_type(&array.elem, table)?)
            ))
        }
        syn::Type::Tuple(tuple) => {
            let mut kinds = tuple.elems.iter().map(|t| toml_type(t, table));
            let first = kinds.next()??;
            if !kinds.all(|kind| kind.as_deref() == Some(first.as_str())) {
                return None;
            }
            Some(format!("array of {} {}", tuple.elems.len(), plural(&first)))
        }
        syn::Type::Path(path) => {
            if let Some([inner]) = generic(ty, "Option").as_deref() {
                return toml_type(inner, table);
            }
            if let Some([inner]) = generic(ty, "Vec").as_deref() {
                return Some(format!("array of {}", plural(&toml_type(inner, table)?)));
            }
            if let Some([_, value]) = generic(ty, "BTreeMap").as_deref() {
                return Some(format!("table of {}", plural(&toml_type(value, table)?)));
            }
            let name = path.path.segments.last()?.ident.to_string();
            Some(
                match name.as_str() {
                    "String" => "string",
                    "PathBuf" => "string (a path)",
                    "bool" => "boolean",
                    "u8" | "u16" | "u32" | "u64" | "usize" | "i8" | "i16" | "i32" | "i64" => {
                        "integer"
                    }
                    "f32" | "f64" => "number",
                    other if table(other) => "table",
                    _ => return None,
                }
                .to_string(),
            )
        }
        _ => None,
    }
}

fn plural(kind: &str) -> String {
    if kind == "string (a path)" {
        return "strings (paths)".to_string();
    }
    for (one, many) in [("array of ", "arrays of "), ("table of ", "tables of ")] {
        if let Some(rest) = kind.strip_prefix(one) {
            return format!("{many}{rest}");
        }
    }
    format!("{kind}s")
}

/// The value serde gives an absent `#[serde(default)]` field of type `ty`.
fn zero_value(ty: &syn::Type) -> Option<&'static str> {
    if generic(ty, "Vec").is_some() {
        return Some("[]");
    }
    let syn::Type::Path(path) = ty else {
        return None;
    };
    match path.path.segments.last()?.ident.to_string().as_str() {
        "bool" => Some("false"),
        "u8" | "u16" | "u32" | "u64" | "usize" | "i8" | "i16" | "i32" | "i64" => Some("0"),
        _ => None,
    }
}

/// The type arguments of `ty` when it is `wrapper<…>`.
fn generic<'a>(ty: &'a syn::Type, wrapper: &str) -> Option<Vec<&'a syn::Type>> {
    let syn::Type::Path(path) = ty else {
        return None;
    };
    let last = path.path.segments.last()?;
    if last.ident != wrapper {
        return None;
    }
    let syn::PathArguments::AngleBracketed(args) = &last.arguments else {
        return None;
    };
    Some(
        args.args
            .iter()
            .filter_map(|arg| match arg {
                syn::GenericArgument::Type(ty) => Some(ty),
                _ => None,
            })
            .collect(),
    )
}

/// The keys serde lists in its refusal of an unknown field: "unknown field `x`, expected one of
/// `a`, `b`", "expected `a` or `b`" or "expected `a`".
pub fn accepted_keys(message: &str) -> Option<Vec<String>> {
    let (_, listed) = message.split_once("unknown field `")?;
    let (_, listed) = listed.split_once("expected ")?;
    let mut keys: Vec<String> = listed
        .split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect();
    keys.sort();
    Some(keys)
}

/// Text as one table cell: its lines joined, with `|`, `<` and `>` escaped outside code spans.
pub fn cell(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    outside_code(&line, |plain| escape_angles(plain).replace('|', "\\|"))
}

/// `text` with `escape` applied to the parts outside backtick code spans.
pub fn outside_code(text: &str, escape: impl Fn(&str) -> String) -> String {
    text.split('`')
        .enumerate()
        .map(|(i, part)| match i % 2 {
            0 => escape(part),
            _ => part.to_string(),
        })
        .collect::<Vec<_>>()
        .join("`")
}

pub fn escape_angles(text: &str) -> String {
    text.replace('<', "&lt;").replace('>', "&gt;")
}

/// Compare `rendered` with the committed page at `path`, or write it there when the environment
/// variable `variable` is set. `regenerate` is the command the failure tells a reader to run.
pub fn check_page(path: &Path, rendered: &str, variable: &str, regenerate: &str) {
    if std::env::var_os(variable).is_some() {
        std::fs::write(path, rendered)
            .unwrap_or_else(|e| panic!("cannot write {}: {e}", path.display()));
        return;
    }
    let committed = std::fs::read_to_string(path).unwrap_or_default();
    assert!(
        committed == rendered,
        "{} differs from the source it is generated from. Regenerate it with: {regenerate}",
        path.display()
    );
}
