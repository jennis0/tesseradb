//! `docs/reference/cli.md`, rendered from the command definitions in `main.rs`.
//!
//! The page is committed so the documentation site builds without cargo. The test below renders
//! it again and fails when the committed copy differs. To regenerate the page after changing a
//! command or its help text, run:
//!
//! ```text
//! MOSAICA_WRITE_CLI_REFERENCE=1 cargo test -p mosaica-cli cli_reference
//! ```

use std::fmt::Write;
use std::path::PathBuf;

use clap::{Arg, ArgAction, Command, CommandFactory};
use mosaica_docgen::{cell, escape_angles, outside_code};

const REGENERATE: &str = "MOSAICA_WRITE_CLI_REFERENCE=1 cargo test -p mosaica-cli cli_reference";

const INTRO: &str = "\
`mosaica <subcommand> --help` prints the text on this page. `mosaica --version` prints the commit \
the binary was built from. It prints `unknown` when `MOSAICA_BUILD_COMMIT` was unset at build \
time and git could not name the commit, as in a build outside a git checkout.

`build`, `check`, `health` and `serve` read the deployment file `mosaica.toml` from the working \
directory, or from the nearest directory above it that has one. `--deployment` names a different \
file.
";

fn page_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/reference/cli.md")
}

/// The whole page.
pub(crate) fn render() -> String {
    let mut cli = crate::Cli::command();
    cli.build();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "<!-- Generated from crates/mosaica-cli/src/main.rs. Edit the help text there, then run: \
         {REGENERATE} -->\n"
    );
    out.push_str("# CLI\n\n");
    out.push_str(INTRO);
    let names: Vec<String> = cli
        .get_subcommands()
        .filter(|sub| !sub.is_hide_set() && sub.get_name() != "help")
        .map(|sub| sub.get_name().to_string())
        .collect();
    for name in names {
        let sub = cli
            .find_subcommand_mut(&name)
            .expect("a name read from this command's own subcommands");
        section(&mut out, sub, "mosaica");
    }
    out
}

/// The section for `sub`, then one for each of its own subcommands, each headed with the whole
/// command line that reaches it.
fn section(out: &mut String, sub: &mut Command, parent: &str) {
    let path = format!("{parent} {}", sub.get_name());
    let usage = sub.render_usage().to_string();
    let usage = usage.trim().trim_start_matches("Usage:").trim();
    let about = sub
        .get_long_about()
        .or(sub.get_about())
        .map(|text| outside_code(&sentence(&text.to_string()), escape_angles))
        .unwrap_or_default();
    let _ = writeln!(out, "\n## `{path}`\n");
    let _ = writeln!(out, "```text\n{usage}\n```\n");
    let _ = writeln!(out, "{about}\n");

    let args: Vec<&Arg> = sub
        .get_arguments()
        .filter(|arg| !arg.is_hide_set())
        .filter(|arg| {
            !matches!(
                arg.get_action(),
                ArgAction::Help | ArgAction::HelpShort | ArgAction::HelpLong | ArgAction::Version
            )
        })
        .collect();
    let nested: Vec<String> = sub
        .get_subcommands()
        .filter(|s| !s.is_hide_set() && s.get_name() != "help")
        .map(|s| s.get_name().to_string())
        .collect();
    if !args.is_empty() {
        arguments(out, &args);
    }
    for name in nested {
        let child = sub
            .find_subcommand_mut(&name)
            .expect("a name read from this command's own subcommands");
        section(out, child, &path);
    }
}

fn arguments(out: &mut String, args: &[&Arg]) {
    out.push_str("| Argument | Value | Default | Description |\n");
    out.push_str("| --- | --- | --- | --- |\n");
    for &arg in args {
        let value = value_name(arg);
        let name = match arg.get_long() {
            Some(long) => format!("`--{long}`"),
            None => format!("`{value}`"),
        };
        let value = match (arg.get_long(), arg.get_action().takes_values()) {
            (Some(_), true) => format!("`{value}`"),
            _ => String::new(),
        };
        let help = arg
            .get_long_help()
            .or(arg.get_help())
            .map(|text| cell(&sentence(&text.to_string())))
            .unwrap_or_default();
        // A switch's default of `false` says nothing its description does not. A value computed
        // at run time has no clap default, and its help states it after "Default:".
        let default = match arg.get_action() {
            ArgAction::SetTrue => String::new(),
            _ if arg.get_default_values().is_empty() && help.contains("Default:") => {
                "derived".to_string()
            }
            _ => arg
                .get_default_values()
                .iter()
                .map(|v| format!("`{}`", v.to_string_lossy()))
                .collect::<Vec<_>>()
                .join(", "),
        };
        let _ = writeln!(out, "| {name} | {value} | {default} | {help} |");
    }
}

fn value_name(arg: &Arg) -> String {
    match arg.get_value_names() {
        Some(names) => names
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(" "),
        None => arg.get_id().as_str().to_uppercase(),
    }
}

/// Help text ending in a full stop. Clap drops the full stop from a help text of one paragraph,
/// which reads well at a terminal and badly in running prose.
fn sentence(text: &str) -> String {
    let text = text.trim();
    match text.chars().last() {
        Some('.') | Some(':') | None => text.to_string(),
        Some(_) => format!("{text}."),
    }
}

#[test]
fn cli_reference_matches_the_commands() {
    mosaica_docgen::check_page(
        &page_path(),
        &render(),
        "MOSAICA_WRITE_CLI_REFERENCE",
        REGENERATE,
    );
}
