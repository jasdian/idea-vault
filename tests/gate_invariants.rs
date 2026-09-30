//! scripts/check-invariants.sh under test (ADR-0041, docs/14-no-mistakes-gate.md): every catalog
//! rule has a seeded violation that flags exactly its own id, the catalog and the seed table are
//! the same set (so a rule cannot ship without a seed), and the committed tree is clean.
mod support;

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Output;

use support::gate::{
    allows_rs, catalog, clean_tree, floor, invariants, repo_root, write, CLAUDE_MD, COMPOSE, INTENT,
};
use tempfile::TempDir;

fn tree() -> TempDir {
    let dir = TempDir::new().unwrap();
    clean_tree(dir.path());
    dir
}

fn run(root: &Path, strict: bool) -> Output {
    let root = root.to_str().unwrap();
    if strict {
        invariants(&["--strict", "--root", root])
    } else {
        invariants(&["--root", root])
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `(severity, id)` of every finding line.
fn findings(out: &Output) -> Vec<(String, String)> {
    stdout(out)
        .lines()
        .filter_map(|l| {
            let (sev, rest) = l.split_once(' ')?;
            matches!(sev, "ERROR" | "WARN" | "INFO")
                .then(|| (sev.to_string(), rest.split(':').next().unwrap().to_string()))
        })
        .collect()
}

/// A catalog id and the function that plants one violation of it.
type Seed = (&'static str, fn(&Path));

/// One row per catalog id; each plants exactly one violation of that rule in a clean tree.
/// Forbidden tokens are spelled in pieces so this file never trips the rules it seeds.
const SEEDS: &[Seed] = &[
    ("ollama-url", |r| {
        write(
            r,
            "src/ai/url.rs",
            concat!("const U: &str = \"http://local", "host:11434\";\n"),
        )
    }),
    ("bind-addr", |r| {
        write(
            r,
            "src/web/bind.rs",
            concat!("const B: &str = \"127.0.0.1", ":3000\";\n"),
        )
    }),
    ("restart-no", |r| {
        write(
            r,
            "docker-compose.yml",
            &COMPOSE.replace("\"no\"", "unless-stopped"),
        )
    }),
    ("vault-bind-long", |r| {
        write(
            r,
            "docker-compose.yml",
            &format!("{COMPOSE}      - ./vault:/vault\n"),
        )
    }),
    ("no-docker-exec", |r| {
        write(
            r,
            "src/web/run.rs",
            concat!("fn f() { Command::new(\"doc", "ker\"); }\n"),
        )
    }),
    ("doc-links", |r| {
        write(
            r,
            "CLAUDE.md",
            &format!("{CLAUDE_MD}Also docs/adr/0009-gone.md.\n"),
        )
    }),
    ("ratchet", |r| {
        write(r, "src/raw.rs", concat!("fn f() { uns", "afe { } }\n"))
    }),
    ("d4-config", |r| {
        write(
            r,
            "src/ai/cfg.rs",
            concat!("use crate::con", "fig::Config;\n"),
        )
    }),
    ("d4-web-app", |r| {
        write(r, "src/web/up.rs", concat!("use crate::ap", "p::App;\n"))
    }),
    ("ratchet-slack", |r| {
        let n = floor("CLIPPY_ALLOW_FLOOR");
        assert!(n > 0, "the slack seed needs a clippy floor above 0");
        write(r, "src/allows.rs", &allows_rs(n - 1))
    }),
    ("ignore-ratchet", |r| {
        write(
            r,
            "tests/skipped.rs",
            concat!("#[test]\n#[ign", "ore]\nfn t() {}\n"),
        )
    }),
    ("doc-ranges", |r| {
        write(r, "CLAUDE.md", &CLAUDE_MD.replace("(D1–D1)", "(D1–D2)"))
    }),
    ("doc-range-gaps", |r| {
        write(r, "docs/adr/0003-y.md", "# ADR-0003\n");
        write(r, "CLAUDE.md", &CLAUDE_MD.replace("0001–0001", "0001–0003"))
    }),
    ("checklist-mirror", |r| {
        write(r, ".claude/rules/core.md", "- [CORE-8] something else\n")
    }),
    ("intent-archive", |r| {
        write(r, "docs/INTENT.md", &format!("{INTENT}\n{INTENT}"))
    }),
    ("tool-fence", |r| {
        write(
            r,
            "src/ai/tool_loop.rs",
            concat!(
                "fn f() {\n    json!({\"role\": \"to",
                "ol\", \"content\": result});\n}\n"
            ),
        )
    }),
    ("runs-not-truth", |r| {
        write(
            r,
            "src/index/scan.rs",
            concat!("const R: &str = \".ru", "ns\";\n"),
        )
    }),
    ("no-skip-permissions", |r| {
        write(
            r,
            "src/ai/foil.rs",
            concat!(
                "fn f() { cmd.arg(\"--dangerously-skip",
                "-permissions\"); }\n"
            ),
        )
    }),
];

#[test]
fn clean_tree_passes_with_every_id_ok() {
    let dir = tree();
    let out = run(dir.path(), true);
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(findings(&out).is_empty(), "{text}");
    for (id, _) in catalog() {
        assert!(text.contains(&format!("[ok] {id} — ")), "{id}\n{text}");
    }
}

#[test]
fn committed_tree_is_clean() {
    let out = invariants(&["--strict", "--root", repo_root().to_str().unwrap()]);
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(
        findings(&out).iter().all(|(sev, _)| sev == "INFO"),
        "{text}"
    );
}

#[test]
fn each_seed_flags_exactly_its_own_id() {
    let severities = catalog();
    for (id, seed) in SEEDS {
        let dir = tree();
        seed(dir.path());
        let sev = &severities
            .iter()
            .find(|(c, _)| c == id)
            .expect("a catalog id")
            .1;
        let plain = run(dir.path(), false);
        let strict = run(dir.path(), true);
        let got = findings(&plain);
        assert_eq!(
            got.len(),
            1,
            "{id}: exactly one finding\n{}",
            stdout(&plain)
        );
        assert_eq!(&got[0].1, id, "{id}\n{}", stdout(&plain));
        let (plain_exit, strict_exit, shown) = match sev.as_str() {
            "error" => (1, 1, "ERROR"),
            "warn" => (0, 1, "WARN"),
            "info" => (0, 0, "INFO"),
            other => panic!("{id}: unknown severity {other}"),
        };
        assert_eq!(got[0].0, shown, "{id}\n{}", stdout(&plain));
        assert_eq!(
            plain.status.code(),
            Some(plain_exit),
            "{id}\n{}",
            stdout(&plain)
        );
        assert_eq!(
            strict.status.code(),
            Some(strict_exit),
            "{id}\n{}",
            stdout(&strict)
        );
    }
}

#[test]
fn every_catalog_id_has_a_seed() {
    let listed: BTreeSet<String> = catalog().into_iter().map(|(id, _)| id).collect();
    let seeded: BTreeSet<String> = SEEDS.iter().map(|(id, _)| id.to_string()).collect();
    assert_eq!(seeded.len(), SEEDS.len(), "a seed id is duplicated");
    assert_eq!(listed, seeded);
}

#[test]
fn collect_all_reports_two_seeded_violations() {
    let dir = tree();
    for (id, seed) in SEEDS {
        if ["ollama-url", "d4-web-app"].contains(id) {
            seed(dir.path());
        }
    }
    let out = run(dir.path(), false);
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(1), "{text}");
    let ids: Vec<String> = findings(&out).into_iter().map(|(_, id)| id).collect();
    assert_eq!(ids, ["ollama-url", "d4-web-app"], "{text}");
    for (id, _) in catalog() {
        assert!(
            text.contains(&format!(" {id}")),
            "every rule still ran: {id}\n{text}"
        );
    }
}

#[test]
fn checklist_mirror_is_info_without_dot_claude() {
    let dir = tree();
    fs::remove_dir_all(dir.path().join(".claude")).unwrap();
    let out = run(dir.path(), true);
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    assert_eq!(
        findings(&out),
        [("INFO".to_string(), "checklist-mirror".to_string())]
    );
}

#[test]
fn unknown_flag_exits_2_listing_flags() {
    let out = invariants(&["--bogus"]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    for flag in ["--strict", "--root", "--list"] {
        assert!(err.contains(flag), "{flag}\n{err}");
    }
}

#[test]
fn list_with_other_flag_is_usage_error() {
    for args in [
        &["--list", "--strict"][..],
        &["--strict", "--list"],
        &["--list", "--root", "."],
    ] {
        let out = invariants(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(
            out.stdout.is_empty(),
            "{args:?}: no catalog on a usage error"
        );
    }
}
