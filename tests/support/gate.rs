//! The smallest tree every invariant rule needs (ADR-0041, docs/14-no-mistakes-gate.md): shared by
//! tests/gate_invariants.rs, which seeds one violation per catalog id into it, and
//! tests/gate_script.rs, which runs the whole gate over it inside a scratch git repository.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The repository this test binary was built from.
pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// A ratchet floor's committed value, read from the real script so the clean tree tracks it.
pub fn floor(name: &str) -> usize {
    let script = fs::read_to_string(repo_root().join("scripts/check-invariants.sh")).unwrap();
    let prefix = format!("{name}=");
    script
        .lines()
        .find_map(|l| l.strip_prefix(&prefix))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or_else(|| panic!("{name} not found in scripts/check-invariants.sh"))
}

/// Write `text` to `root/rel`, creating parent directories.
pub fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// `n` reasoned clippy lint attributes, which the ratchet counts and allow-reason accepts.
pub fn allows_rs(n: usize) -> String {
    let attr = concat!(
        "#[expect(",
        "clippy::too_many_arguments, reason = \"seed\")]"
    );
    (0..n)
        .map(|i| format!("{attr}\nfn f{i}() {{}}\n"))
        .collect()
}

pub const MAIN_RS: &str =
    "fn main() { axum::serve(l, app).with_graceful_shutdown(shutdown_signal()); }\n";

pub const SCHEMA_RS: &str = "fn open() { conn.busy_timeout(BUSY_TIMEOUT)?; }\n";

pub const INTENT: &str =
    "# Intent — seed tree (ADR-0001)\n\n## Acceptance criteria\n\n- every rule holds\n";

pub const CLAUDE_MD: &str =
    "# CLAUDE.md\n\nA Mermaid diagram catalog (D1–D1), and ADRs 0001–0001. \
     See [ADR-0001](docs/adr/0001-x.md).\n";

pub const COMPOSE: &str = "services:\n  app:\n    restart: \"no\"\n    volumes:\n      - type: bind\n        source: ./vault\n        target: /vault\n";

/// Populate `root` with a tree on which `check-invariants.sh --strict` reports every id `[ok]`.
pub fn clean_tree(root: &Path) {
    write(
        root,
        "src/config.rs",
        "pub const OLLAMA: &str = \"http://localhost:11434\";\npub const BIND: &str = \"127.0.0.1:3000\";\n",
    );
    write(root, "src/ai/mod.rs", "pub fn ai() {}\n");
    write(root, "src/main.rs", MAIN_RS);
    write(root, "src/index/schema.rs", SCHEMA_RS);
    write(root, "src/web/mod.rs", "pub fn web() {}\n");
    write(
        root,
        "src/allows.rs",
        &allows_rs(floor("CLIPPY_ALLOW_FLOOR")),
    );
    write(root, "tests/it.rs", "#[test]\nfn it() {}\n");
    // Step 4 validates a scratch copy of the golden vault; the stub cargo only needs it to exist.
    write(
        root,
        "tests/fixtures/golden-vault/seed/idea.md",
        "---\ntitle: Seed\nslug: seed\nstate: draft\n\
created: 2026-07-07T10:00:00Z\nupdated: 2026-07-07T10:00:00Z\n---\n\nSeed.\n",
    );
    write(root, "docker-compose.yml", COMPOSE);
    write(root, "CLAUDE.md", CLAUDE_MD);
    write(
        root,
        "docs/08-diagrams.md",
        "| ID | Type | What | Home |\n|---|---|---|---|\n| **D1** | Flowchart | x | [ADR-0001](./adr/0001-x.md) |\n",
    );
    write(root, "docs/adr/0001-x.md", "# ADR-0001\n");
    write(root, "docs/INTENT.md", INTENT);
    write(
        root,
        "docs/14-no-mistakes-gate.md",
        "# 14\n\n## Checklist\n\n1. **alpha rule holds** [dev]\n2. **beta rule holds** [product]\n",
    );
    write(
        root,
        ".claude/skills/attack/SKILL.md",
        "6. alpha rule holds\n",
    );
    write(
        root,
        ".claude/rules/core.md",
        "- [CORE-8] alpha rule holds\n",
    );
}

/// Copy the repository's gate scripts into `root/scripts`.
pub fn copy_scripts(root: &Path) {
    for name in ["gate.sh", "check-invariants.sh", "skill-check.sh"] {
        let to = root.join("scripts").join(name);
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        fs::copy(repo_root().join("scripts").join(name), to).unwrap();
    }
}

/// Run the repository's check-invariants.sh with `args`.
pub fn invariants(args: &[&str]) -> Output {
    Command::new("bash")
        .arg(repo_root().join("scripts/check-invariants.sh"))
        .args(args)
        .output()
        .expect("bash runs")
}

/// The catalog as `(id, severity)` pairs, from `--list`.
pub fn catalog() -> Vec<(String, String)> {
    let out = invariants(&["--list"]);
    assert_eq!(out.status.code(), Some(0), "--list exits 0");
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| {
            let mut cells = l.split('|');
            let id = cells.next().unwrap().to_string();
            let sev = cells.next().expect("a severity cell").to_string();
            (id, sev)
        })
        .collect()
}
