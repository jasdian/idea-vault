//! scripts/check-invariants.sh under test (ADR-0041, docs/14-no-mistakes-gate.md): every detection
//! arm of every catalog rule has a seeded violation that flags exactly its own id, the catalog and
//! the seed table's ids are the same set (so a rule cannot ship without a seed), and the versioned
//! tree is clean whatever the unversioned `.claude/` holds.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; HTC-6 binds shipping code"
)]
mod support;

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

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

/// A catalog id, the detection arm it exercises, and the function that plants one violation of it.
type Seed = (&'static str, &'static str, fn(&Path));

/// At least one row per catalog id and one per detection arm of a multi-arm rule; each plants
/// exactly one violation of that rule in a clean tree, so deleting an arm goes red.
/// Forbidden tokens are spelled in pieces so this file never trips the rules it seeds.
const SEEDS: &[Seed] = &[
    ("ollama-url", "src literal", |r| {
        write(
            r,
            "src/ai/url.rs",
            concat!("const U: &str = \"http://local", "host:11434\";\n"),
        )
    }),
    ("bind-addr", "src literal", |r| {
        write(
            r,
            "src/web/bind.rs",
            concat!("const B: &str = \"127.0.0.1", ":3000\";\n"),
        )
    }),
    ("restart-no", "compose directive", |r| {
        write(
            r,
            "docker-compose.yml",
            &COMPOSE.replace("\"no\"", "unless-stopped"),
        )
    }),
    ("vault-bind-long", "short bind", |r| {
        write(
            r,
            "docker-compose.yml",
            &format!("{COMPOSE}      - ./vault:/vault\n"),
        )
    }),
    ("no-docker-exec", "Command::new", |r| {
        write(
            r,
            "src/web/run.rs",
            concat!("fn f() { Command::new(\"doc", "ker\"); }\n"),
        )
    }),
    ("doc-links", "CLAUDE.md ADR path", |r| {
        write(
            r,
            "CLAUDE.md",
            &format!("{CLAUDE_MD}Also docs/adr/0009-gone.md.\n"),
        )
    }),
    ("ratchet", "unsafe over floor", |r| {
        write(r, "src/raw.rs", concat!("fn f() { uns", "afe { } }\n"))
    }),
    ("d4-config", "use path", |r| {
        write(
            r,
            "src/ai/cfg.rs",
            concat!("use crate::con", "fig::Config;\n"),
        )
    }),
    ("d4-web-app", "use path", |r| {
        write(r, "src/web/up.rs", concat!("use crate::ap", "p::App;\n"))
    }),
    ("ratchet-slack", "clippy allows under floor", |r| {
        let n = floor("CLIPPY_ALLOW_FLOOR");
        assert!(n > 0, "the slack seed needs a clippy floor above 0");
        write(r, "src/allows.rs", &allows_rs(n - 1))
    }),
    ("ignore-ratchet", "ignore over floor", |r| {
        write(
            r,
            "tests/skipped.rs",
            concat!("#[test]\n#[ign", "ore]\nfn t() {}\n"),
        )
    }),
    ("doc-ranges", "D range stale", |r| {
        write(r, "CLAUDE.md", &CLAUDE_MD.replace("(D1–D1)", "(D1–D2)"))
    }),
    ("doc-range-gaps", "ADR gap", |r| {
        write(r, "docs/adr/0003-y.md", "# ADR-0003\n");
        write(r, "CLAUDE.md", &CLAUDE_MD.replace("0001–0001", "0001–0003"))
    }),
    ("checklist-mirror", "mirror lacks a phrase", |r| {
        write(r, ".claude/rules/core.md", "- [CORE-8] something else\n")
    }),
    ("intent-archive", "two blocks", |r| {
        write(r, "docs/INTENT.md", &format!("{INTENT}\n{INTENT}"))
    }),
    ("tool-fence", "unfenced tool message", |r| {
        write(
            r,
            "src/ai/tool_loop.rs",
            concat!(
                "fn f() {\n    json!({\"role\": \"to",
                "ol\", \"content\": result});\n}\n"
            ),
        )
    }),
    ("runs-not-truth", "journal path in index", |r| {
        write(
            r,
            "src/index/scan.rs",
            concat!("const R: &str = \".ru", "ns\";\n"),
        )
    }),
    ("no-skip-permissions", "flag in src", |r| {
        write(
            r,
            "src/ai/foil.rs",
            concat!(
                "fn f() { cmd.arg(\"--dangerously-skip",
                "-permissions\"); }\n"
            ),
        )
    }),
    ("doc-links", "08-diagrams relative link", |r| {
        let text = fs::read_to_string(r.join("docs/08-diagrams.md")).unwrap();
        write(
            r,
            "docs/08-diagrams.md",
            &format!("{text}\nSee [gone](./adr/0009-gone.md).\n"),
        )
    }),
    ("ratchet", "clippy allows over floor", |r| {
        write(
            r,
            "src/allows.rs",
            &allows_rs(floor("CLIPPY_ALLOW_FLOOR") + 1),
        )
    }),
    ("doc-ranges", "ADR range stale", |r| {
        write(r, "CLAUDE.md", &CLAUDE_MD.replace("0001–0001", "0001–0002"))
    }),
    ("doc-ranges", "no D range sentence", |r| {
        write(r, "CLAUDE.md", &CLAUDE_MD.replace(" (D1–D1)", ""))
    }),
    ("doc-ranges", "no ADR range sentence", |r| {
        write(
            r,
            "CLAUDE.md",
            &CLAUDE_MD.replace(", and ADRs 0001–0001", ""),
        )
    }),
    ("doc-ranges", "CLAUDE.md missing", |r| {
        fs::remove_file(r.join("CLAUDE.md")).unwrap()
    }),
    ("checklist-mirror", "numbering gap", |r| {
        write(
            r,
            "docs/14-no-mistakes-gate.md",
            "# 14\n\n## Checklist\n\n1. **alpha rule holds** [dev]\n3. **beta rule holds** [product]\n",
        )
    }),
    ("checklist-mirror", "malformed checklist line", |r| {
        write(
            r,
            "docs/14-no-mistakes-gate.md",
            "# 14\n\n## Checklist\n\n1. **alpha rule holds** [dev]\n2. beta rule holds\n",
        )
    }),
    ("discard-truth-write", "store write", |r| {
        write(
            r,
            "src/web/turn.rs",
            concat!("fn f() { let _ = store::wri", "te_idea(&v, &i); }\n"),
        )
    }),
    ("graceful-shutdown", "serve without it", |r| {
        write(r, "src/main.rs", "fn main() { axum::serve(l, app); }\n")
    }),
    ("graceful-shutdown", "main.rs missing", |r| {
        fs::remove_file(r.join("src/main.rs")).unwrap()
    }),
    ("sql-literal", "literal on the format! line", |r| {
        write(
            r,
            "src/index/q.rs",
            concat!(
                "fn f() { conn.execute(&for",
                "mat!(\"DELETE FROM ideas WHERE slug = '{s}'\"), []); }\n"
            ),
        )
    }),
    ("sql-literal", "literal on the next line", |r| {
        write(
            r,
            "src/memory/q.rs",
            concat!(
                "fn f() {\n    let q = for",
                "mat!(\n        \"SELECT * FROM t WHERE x = {x}\"\n    );\n}\n"
            ),
        )
    }),
    ("anyhow-edge", "library module", |r| {
        write(
            r,
            "src/vault/x.rs",
            concat!("fn f() -> any", "how::Result<()> { Ok(()) }\n"),
        )
    }),
    ("no-deep-super", "super::super", |r| {
        write(r, "src/web/deep.rs", concat!("use super::su", "per::x;\n"))
    }),
    ("no-deep-super", "#[path]", |r| {
        write(
            r,
            "src/web/pathed.rs",
            concat!("#[pa", "th = \"elsewhere.rs\"]\nmod m;\n"),
        )
    }),
    ("busy-timeout", "no busy_timeout call", |r| {
        write(r, "src/index/schema.rs", "fn open() {}\n")
    }),
    ("busy-timeout", "schema.rs missing", |r| {
        fs::remove_file(r.join("src/index/schema.rs")).unwrap()
    }),
    ("allow-reason", "allow instead of expect", |r| {
        let rs = allows_rs(floor("CLIPPY_ALLOW_FLOOR") - 1);
        let bad = concat!("#[all", "ow(clippy::too_many_arguments)]\nfn g() {}\n");
        write(r, "src/allows.rs", &format!("{rs}{bad}"))
    }),
    ("allow-reason", "multi-line allow", |r| {
        let rs = allows_rs(floor("CLIPPY_ALLOW_FLOOR") - 1);
        let bad = concat!(
            "#[all",
            "ow(\n    clippy::too_many_arguments\n)]\nfn g() {}\n"
        );
        write(r, "src/allows.rs", &format!("{rs}{bad}"))
    }),
    ("allow-reason", "expect without reason", |r| {
        let rs = allows_rs(floor("CLIPPY_ALLOW_FLOOR") - 1);
        let bad = concat!("#[exp", "ect(clippy::too_many_arguments)]\nfn g() {}\n");
        write(r, "src/allows.rs", &format!("{rs}{bad}"))
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

/// The versioned tree (tracked plus untracked-but-not-ignored files) copied into a tempdir, so the
/// verdict never depends on gitignored state such as `.claude/`.
fn versioned_tree() -> TempDir {
    let root = repo_root();
    let out = Command::new("git")
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .current_dir(&root)
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git ls-files: {out:?}");
    let dir = TempDir::new().unwrap();
    for rel in out.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        let rel = std::str::from_utf8(rel).unwrap();
        let from = root.join(rel);
        // A tracked file deleted in the working tree is not part of the tree being checked.
        if from.is_file() {
            let to = dir.path().join(rel);
            fs::create_dir_all(to.parent().unwrap()).unwrap();
            fs::copy(&from, &to).unwrap();
        }
    }
    dir
}

#[test]
fn committed_tree_is_clean() {
    let tree = versioned_tree();
    assert!(
        !tree.path().join(".claude").exists(),
        ".claude/ is unversioned"
    );
    let out = invariants(&["--strict", "--root", tree.path().to_str().unwrap()]);
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
    for (id, arm, seed) in SEEDS {
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
            "{id} ({arm}): exactly one finding\n{}",
            stdout(&plain)
        );
        assert_eq!(&got[0].1, id, "{id} ({arm})\n{}", stdout(&plain));
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
    let seeded: BTreeSet<String> = SEEDS.iter().map(|(id, _, _)| id.to_string()).collect();
    let arms: BTreeSet<(&str, &str)> = SEEDS.iter().map(|(id, arm, _)| (*id, *arm)).collect();
    assert_eq!(arms.len(), SEEDS.len(), "a seed (id, arm) is duplicated");
    assert_eq!(listed, seeded);
}

#[test]
fn collect_all_reports_two_seeded_violations() {
    let dir = tree();
    for (id, _, seed) in SEEDS {
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
fn checklist_mirror_is_info_for_an_absent_mirror_file() {
    // A worktree whose .claude/ holds only a campaign workspace has neither mirror file.
    let dir = tree();
    fs::remove_dir_all(dir.path().join(".claude/skills")).unwrap();
    fs::remove_file(dir.path().join(".claude/rules/core.md")).unwrap();
    write(dir.path(), ".claude/attack-workspace/plan.md", "# plan\n");
    let out = run(dir.path(), true);
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    let info = ("INFO".to_string(), "checklist-mirror".to_string());
    assert_eq!(findings(&out), [info.clone(), info], "{}", stdout(&out));
}

#[test]
fn checklist_mirror_errors_on_a_drifted_mirror_beside_an_absent_one() {
    let dir = tree();
    fs::remove_file(dir.path().join(".claude/rules/core.md")).unwrap();
    write(
        dir.path(),
        ".claude/skills/attack/SKILL.md",
        "6. something else\n",
    );
    let out = run(dir.path(), false);
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    let got = findings(&out);
    assert!(
        got.contains(&("ERROR".to_string(), "checklist-mirror".to_string()))
            && got.contains(&("INFO".to_string(), "checklist-mirror".to_string()))
            && got.len() == 2,
        "{}",
        stdout(&out)
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
