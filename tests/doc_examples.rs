//! Worked examples in the docs are held to the code (CORE-5, ADR-0041): the skill-file example in
//! docs/06-concepts/skills.md must parse to the same frontmatter as the built-in it shows, and the
//! frontmatter field table must list exactly the values the enums accept.
mod support;

use std::collections::BTreeSet;
use std::path::Path;

use idea_vault::domain::frontmatter::parse_skill;
use idea_vault::domain::skill::{OutputContract, SkillRole, SkillStage};

fn read(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

const SKILLS_DOC: &str = "docs/06-concepts/skills.md";

#[test]
fn skill_doc_example_frontmatter_equals_builtin() {
    let doc = read(SKILLS_DOC);
    let start = doc.find("```markdown\n").expect("the skill-file example") + "```markdown\n".len();
    let example = &doc[start..start + doc[start..].find("\n```").expect("the fence end")];
    let (shown, _) = parse_skill(example).expect("the doc example parses as a skill file");
    let builtin = read(&format!("src/concepts/skills/{}.md", shown.name));
    let (real, _) = parse_skill(&builtin).expect("the built-in parses");
    assert_eq!(
        shown, real,
        "{SKILLS_DOC} shows a skill that differs from its built-in"
    );
}

/// The backticked values in the `| \`<field>\` | … | <values> |` row of the field table.
fn table_values(doc: &str, field: &str) -> BTreeSet<String> {
    let row = doc
        .lines()
        .find(|l| l.starts_with(&format!("| `{field}` |")))
        .unwrap_or_else(|| panic!("no `{field}` row in {SKILLS_DOC}"));
    let values = row.rsplit('|').nth(1).expect("a values column");
    values
        .split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

fn names<T: serde::Serialize>(all: &[T]) -> BTreeSet<String> {
    all.iter()
        .map(
            |v| match serde_json::to_value(v).expect("a unit variant serializes") {
                serde_json::Value::String(s) => s,
                other => panic!("not a unit variant: {other}"),
            },
        )
        .collect()
}

#[test]
fn skill_field_table_matches_enums() {
    let doc = read(SKILLS_DOC);
    assert_eq!(table_values(&doc, "stage"), names(&SkillStage::ALL));
    assert_eq!(table_values(&doc, "role"), names(&SkillRole::ALL));
    assert_eq!(table_values(&doc, "contract"), names(&OutputContract::ALL));
}
