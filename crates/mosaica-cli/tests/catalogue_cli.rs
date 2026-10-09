//! The identity verbs against a served bundle whose control plane is a Unix socket: principals,
//! grants, keys, groups, providers, logging in and out, and minting, listing and ending sessions.
//! Each prints the server's JSON answer and exits 0, or exits 1 on a refusal.

mod common;

use std::io::Write;
use std::process::{Output, Stdio};

use serde_json::Value;
use tempfile::TempDir;

use common::{deployment_with_control, tessera, Ports, Server, OPERATOR_CREDENTIAL};

/// Runs `tessera` with `args`, the environment given, and `stdin` on its standard input.
fn run(args: &[&str], env: &[(&str, &str)], stdin: &str) -> Output {
    let mut child = tessera()
        .args(args)
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn answer(output: Output) -> Value {
    assert!(output.status.success(), "{output:?}");
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn the_catalogue_verbs_manage_principals_and_sessions() {
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("control.sock");
    let control = format!("unix:{}", socket.display());
    deployment_with_control(dir.path(), &Ports::chosen(), &control);
    let (_server, bound) = Server::announced(dir.path());
    assert_eq!(bound.control, control);
    let admin = [("TESSERA_CREDENTIAL", OPERATOR_CREDENTIAL)];
    let at = ["--control", control.as_str()];
    let admin_run = |args: &[&str], stdin: &str| {
        let mut all: Vec<&str> = args.to_vec();
        all.extend(at);
        run(&all, &admin, stdin)
    };

    let created = answer(admin_run(&["principal", "create", "ann", "--kind", "person"], ""));
    assert_eq!(created["sessions_ended"], 0);
    answer(admin_run(&["grant", "--principal", "ann", "--permission", "read"], ""));
    answer(admin_run(&["grant", "--principal", "ann", "--term", "0", "--term", "x"], ""));
    answer(admin_run(&["revoke-grant", "--principal", "ann", "--term", "x"], ""));
    let shown = answer(admin_run(&["principal", "show", "ann"], ""));
    assert_eq!(shown["terms"], serde_json::json!(["0"]));
    assert!(!admin_run(&["principal", "show", "nobody"], "").status.success());

    // Logging in by API key, read from stdin.
    let key = answer(admin_run(&["key", "create", "ann"], ""));
    let key = key["key"].as_str().unwrap().to_owned();
    let login = answer(run(
        &["login", "--server", &bound.viewer, "--api-key"],
        &[],
        &format!("{key}\n"),
    ));
    assert!(login["token"].is_string());
    let listed = answer(admin_run(&["session", "list", "--principal", "ann"], ""));
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 1);

    // Setting a password ends that session; a login by password, then a logout.
    let set = answer(admin_run(
        &["principal", "set-password", "ann"],
        "correct horse battery staple\n",
    ));
    assert_eq!(set["sessions_ended"], 1);
    let login = answer(run(
        &["login", "--server", &bound.viewer, "--principal", "ann"],
        &[],
        "correct horse battery staple\n",
    ));
    let token = login["token"].as_str().unwrap();
    let out = run(&["logout", "--server", &bound.viewer], &[("TESSERA_TOKEN", token)], "");
    assert!(out.status.success(), "{out:?}");
    let listed = answer(admin_run(&["session", "list", "--principal", "ann"], ""));
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 0);
    let refused = run(
        &["login", "--server", &bound.viewer, "--principal", "ann"],
        &[],
        "not the password at all\n",
    );
    assert!(!refused.status.success());

    // An integrator mints a session for ann, then revokes it.
    answer(admin_run(&["principal", "create", "portal", "--kind", "service"], ""));
    answer(admin_run(&["grant", "--principal", "portal", "--permission", "authorise-as"], ""));
    let portal_key = answer(admin_run(&["key", "create", "portal"], ""));
    let portal_key = portal_key["key"].as_str().unwrap().to_owned();
    let integrator = [("TESSERA_API_KEY", portal_key.as_str())];
    let minted = answer(run(
        &["session", "authorise", "--session", &bound.session, "--principal", "ann"],
        &integrator,
        "",
    ));
    let token_id = minted["token_id"].to_string();
    let out = run(
        &["session", "revoke", "--session", &bound.session, "--token-id", &token_id],
        &integrator,
        "",
    );
    assert!(out.status.success(), "{out:?}");
    let listed = answer(admin_run(&["session", "list", "--principal", "ann"], ""));
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 0);

    // The operator credential mints a session for the terms it names, which a key may not.
    let by_terms = [
        "session",
        "authorise",
        "--session",
        &bound.session,
        "--term",
        "0",
        "--term",
        "1",
    ];
    assert!(!run(&by_terms, &integrator, "").status.success());
    let operator = [("TESSERA_API_KEY", OPERATOR_CREDENTIAL)];
    let minted = answer(run(&by_terms, &operator, ""));
    let token = minted["token"].as_str().unwrap();
    let out = run(&["logout", "--server", &bound.viewer], &[("TESSERA_TOKEN", token)], "");
    assert!(out.status.success(), "{out:?}");

    // The operator credential mints a session of its own that reads every item, which a key may
    // not.
    let read_all = ["session", "authorise", "--session", &bound.session, "--read-all"];
    assert!(!run(&read_all, &integrator, "").status.success());
    assert!(answer(run(&read_all, &operator, ""))["token"].is_string());

    // Groups, a provider mapping to one, and revoking a grant.
    answer(admin_run(&["group", "create", "admins"], ""));
    answer(admin_run(&["group", "add-member", "admins", "ann"], ""));
    answer(admin_run(
        &[
            "provider",
            "put",
            "corp",
            "--issuer",
            "https://login.example.org",
            "--audience",
            "tessera",
            "--jwks-url",
            "https://login.example.org/keys",
            "--claim-rule",
            "groups[*]",
            "{value}",
            "--role-mapping",
            "groups[*]",
            "tessera-admins",
            "admins",
        ],
        "",
    ));
    let providers = answer(admin_run(&["provider", "list"], ""));
    assert_eq!(
        providers["providers"][0]["role_mappings"][0]["group"],
        "admins"
    );
    answer(admin_run(&["revoke-grant", "--principal", "ann", "--term", "0"], ""));
    let shown = answer(admin_run(&["principal", "show", "ann"], ""));
    assert_eq!(shown["terms"], serde_json::json!([]));
    assert_eq!(shown["groups"], serde_json::json!(["admins"]));

    // Without the credential, every control verb is refused.
    let out = run(&["principal", "list", "--control", &control], &[], "");
    assert!(!out.status.success());
}
