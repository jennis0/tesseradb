//! `_tessera` — the declaration check, in the Python process.
//!
//! Two calls, and they are the two the Python SDK ran `tessera check` for: whether a deployment's
//! declaration checks, and the control-plane payloads it serialises to. What they call is what the
//! binary calls, so there is no second reading of the declaration to drift from the first.
//!
//! What a refusal carries is the point of the module. A subprocess hands back an exit code and a
//! page of text, so a client that wants to raise an error naming the block an author wrote has to
//! re-implement the rule in Python to know it first. Here the findings arrive as objects that name
//! their block and its name, and a refusal is [`DeclarationError`] carrying them. The page itself
//! is rendered by `tessera_build::check::page`, which is what the binary prints, so a caller that
//! has the module installed and one that shells out read the same bytes.

use std::collections::HashMap;
use std::path::Path;

use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;

create_exception!(
    _tessera,
    DeclarationError,
    PyException,
    "The declaration was not read, or was read and refused. `.findings` holds why."
);

/// The declaration a finding or a source is about, in its parts.
#[pyclass(frozen, skip_from_py_object, module = "_tessera")]
#[derive(Clone)]
pub struct Object {
    /// The block kind as the declaration spells it — `source`, `attribute`, `view`, `view group`,
    /// `layer` — or `deployment` and `declaration` for a file that could not be read at all.
    #[pyo3(get)]
    block: String,
    /// The name the block was declared under, or the path of a file that could not be read.
    #[pyo3(get)]
    name: String,
    /// The part of the block at fault, where it has several, else `None`.
    #[pyo3(get)]
    part: Option<String>,
}

#[pymethods]
impl Object {
    fn __str__(&self) -> String {
        let mut rendered = format!("{} '{}'", self.block, self.name);
        if let Some(part) = &self.part {
            rendered.push(' ');
            rendered.push_str(part);
        }
        rendered
    }

    fn __repr__(&self) -> String {
        format!("Object({})", self.__str__())
    }
}

impl Object {
    fn of(object: &tessera_build::check::Object) -> Object {
        Object {
            block: object.block.to_string(),
            name: object.name.clone(),
            part: object.part.clone(),
        }
    }

    /// A file that was never read, so there is no block to name — only the path asked for.
    fn file(block: &str, path: impl std::fmt::Display) -> Object {
        Object {
            block: block.to_string(),
            name: path.to_string(),
            part: None,
        }
    }
}

/// One thing wrong, or one thing worth an eye that refuses nothing.
#[pyclass(frozen, skip_from_py_object, module = "_tessera")]
#[derive(Clone)]
pub struct Finding {
    #[pyo3(get)]
    object: Object,
    #[pyo3(get)]
    detail: String,
}

#[pymethods]
impl Finding {
    fn __str__(&self) -> String {
        format!("{}: {}", self.object.__str__(), self.detail)
    }

    fn __repr__(&self) -> String {
        format!("Finding({})", self.__str__())
    }
}

impl Finding {
    fn of(finding: &tessera_build::check::Finding) -> Finding {
        Finding {
            object: Object::of(&finding.object),
            detail: finding.detail.clone(),
        }
    }
}

/// One source the check looked at. **The count of what was looked at is part of the answer**: a
/// check that examined nothing passes as loudly as one that examined everything.
#[pyclass(frozen, skip_from_py_object, module = "_tessera")]
#[derive(Clone)]
pub struct Source {
    #[pyo3(get)]
    object: Object,
    /// The path as the declaration resolved it, or `None` where the block names no file — which is
    /// legal, and normal for a deployment that writes through the service.
    #[pyo3(get)]
    path: Option<String>,
}

/// What a check found. `ok` is false exactly when `findings` is non-empty; warnings leave it true.
#[pyclass(frozen, module = "_tessera")]
pub struct CheckResult {
    #[pyo3(get)]
    ok: bool,
    #[pyo3(get)]
    findings: Vec<Finding>,
    #[pyo3(get)]
    warnings: Vec<Finding>,
    #[pyo3(get)]
    sources: Vec<Source>,
    /// The page `tessera check` prints, rendered by the renderer the binary renders with, so the
    /// two paths cannot disagree.
    #[pyo3(get)]
    page: String,
}

#[pymethods]
impl CheckResult {
    fn __repr__(&self) -> String {
        format!(
            "CheckResult(ok={}, {} finding(s), {} warning(s), {} source(s))",
            match self.ok {
                true => "True",
                false => "False",
            },
            self.findings.len(),
            self.warnings.len(),
            self.sources.len()
        )
    }
}

/// `DeclarationError`, carrying the findings as `.findings`.
fn refuse(py: Python<'_>, findings: Vec<Finding>) -> PyErr {
    let message = findings
        .iter()
        .map(Finding::__str__)
        .collect::<Vec<_>>()
        .join("\n");
    let error = DeclarationError::new_err(message);
    match error.value(py).setattr("findings", findings) {
        Ok(()) => error,
        Err(attaching) => attaching,
    }
}

/// The deployment file at `deployment_path`, and the declaration it names, read as `tessera check`
/// reads them: a block that names no file is declared and empty rather than refused.
fn declaration(deployment_path: &str) -> Result<(Object, tessera_build::config::Config), Finding> {
    let path = Path::new(deployment_path);
    let from = path.parent().unwrap_or(Path::new("."));
    let (_, deployment) =
        tessera_server::config::open(Some(path), from).map_err(|detail| Finding {
            object: Object::file("deployment", path.display()),
            detail,
        })?;
    let named = Object::file("declaration", deployment.schema_path.display());
    let config = tessera_build::config::Config::parse_with(
        &deployment.schema_path,
        &HashMap::new(),
        tessera_build::config::Strictness::Declared,
    )
    .map_err(|e| Finding {
        object: named.clone(),
        detail: e.to_string(),
    })?;
    Ok((named, config))
}

/// Check the declaration the deployment file at `deployment_path` names.
///
/// Nothing is read but the deployment file, the declaration and the Parquet schemas the
/// declaration names, so a clean check is not a clean build: it cannot see a value against a
/// closed vocabulary, a member id that resolves to nothing, or where the data sits inside a view's
/// extent.
///
/// Raises `DeclarationError` where the deployment file or the declaration could not be read at
/// all. A declaration that was read and found wanting comes back as a result with `ok` false.
#[pyfunction]
fn check(py: Python<'_>, deployment_path: &str) -> PyResult<CheckResult> {
    let (_, config) = declaration(deployment_path).map_err(|finding| refuse(py, vec![finding]))?;
    Ok(report(&config, &tessera_build::check::check(&config)))
}

/// The control-plane payloads the declaration serialises to, as JSON text.
///
/// One object with a key per block kind, each body addressed by a path segment — the shape
/// `/control` takes. It is text rather than a dict because the declaration minus its acquisition
/// keys *is* the payload: handing back what serde wrote keeps one serialiser, where a second pass
/// through Python objects would be a second encoding of the same numbers, free to round a width or
/// an id differently from the one the control plane will read.
///
/// Raises `DeclarationError` where the declaration could not be read, or was read and did not
/// check — no payload is emitted for a declaration nothing would accept.
#[pyfunction]
fn payloads(py: Python<'_>, deployment_path: &str) -> PyResult<String> {
    let (named, config) =
        declaration(deployment_path).map_err(|finding| refuse(py, vec![finding]))?;
    let checked = tessera_build::check::check(&config);
    if !checked.is_clean() {
        return Err(refuse(py, checked.findings.iter().map(Finding::of).collect()));
    }
    serde_json::to_string_pretty(&tessera_build::config::control_payloads(&config)).map_err(|e| {
        refuse(
            py,
            vec![Finding {
                object: named,
                detail: format!("serialising the declaration payloads: {e}"),
            }],
        )
    })
}

fn report(
    config: &tessera_build::config::Config,
    checked: &tessera_build::check::CheckReport,
) -> CheckResult {
    CheckResult {
        page: tessera_build::check::page(config, checked),
        ok: checked.is_clean(),
        findings: checked.findings.iter().map(Finding::of).collect(),
        warnings: checked.warnings.iter().map(Finding::of).collect(),
        sources: checked
            .sources
            .iter()
            .map(|source| Source {
                object: Object::of(&source.object),
                path: source.path.clone(),
            })
            .collect(),
    }
}

#[pymodule]
fn _tessera(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Object>()?;
    m.add_class::<Finding>()?;
    m.add_class::<Source>()?;
    m.add_class::<CheckResult>()?;
    m.add("DeclarationError", m.py().get_type::<DeclarationError>())?;
    m.add_function(wrap_pyfunction!(check, m)?)?;
    m.add_function(wrap_pyfunction!(payloads, m)?)?;
    Ok(())
}
