//! The agent skills, and the marketplace manifest that publishes one of them to Claude Code and
//! Codex.

use std::path::{Path, PathBuf};

/// Every skill in `skills/` carries the frontmatter that agents require (a `name` equal to its
/// folder's, of lowercase letters, digits and hyphens, and a `description` of at most 1024
/// characters), and links only to files it brings along, since installers copy the skill's
/// folder alone. Every skill the marketplace lists is one of them.
#[test]
fn skills_are_complete() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let skills: Vec<PathBuf> = std::fs::read_dir(root.join("skills"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|dir| dir.join("SKILL.md").is_file())
        .collect();
    assert!(skills.len() >= 2, "{skills:?}");
    for dir in &skills {
        let text = std::fs::read_to_string(dir.join("SKILL.md"))
            .unwrap()
            .replace("\r\n", "\n");
        let (front, body) = text
            .strip_prefix("---\n")
            .and_then(|rest| rest.split_once("\n---\n"))
            .expect("SKILL.md starts with a YAML frontmatter block");
        let field = |key: &str| {
            front
                .lines()
                .find_map(|line| line.strip_prefix(key)?.strip_prefix(": "))
                .unwrap_or_default()
        };
        let name = field("name");
        assert_eq!(Some(name), dir.file_name().and_then(|n| n.to_str()));
        assert!(name.len() <= 64);
        assert!(
            name.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        );
        let description = field("description");
        assert!(!description.is_empty() && description.chars().count() <= 1024);
        for (target, _) in body
            .split("](")
            .skip(1)
            .filter_map(|rest| rest.split_once(')'))
        {
            if !target.contains("://") && !target.starts_with('#') {
                assert!(dir.join(target).is_file(), "{name}: broken link {target}");
            }
        }
    }

    let manifest = std::fs::read_to_string(root.join(".claude-plugin/marketplace.json")).unwrap();
    let manifest: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    for listed in manifest["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|plugin| plugin["skills"].as_array().unwrap())
    {
        let listed = dunce::canonicalize(root.join(listed.as_str().unwrap())).unwrap();
        assert!(
            skills
                .iter()
                .any(|dir| dunce::canonicalize(dir).unwrap() == listed),
            "{listed:?}"
        );
    }
}
