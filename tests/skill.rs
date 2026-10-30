//! The agent skill and the marketplace manifest that publishes it to Claude Code and Codex.

use std::path::Path;

/// Every skill the marketplace lists exists, carries the frontmatter that agents require (a
/// `name` equal to its folder's, of lowercase letters, digits and hyphens, and a `description`
/// of at most 1024 characters), and links only to files it brings along, since installers copy
/// the skill's folder alone.
#[test]
fn marketplace_skills_are_complete() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = std::fs::read_to_string(root.join(".claude-plugin/marketplace.json")).unwrap();
    let manifest: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    let skills: Vec<&str> = manifest["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|plugin| plugin["skills"].as_array().unwrap())
        .map(|skill| skill.as_str().unwrap())
        .collect();
    assert!(!skills.is_empty());
    for skill in skills {
        let dir = root.join(skill);
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
                assert!(dir.join(target).is_file(), "{skill}: broken link {target}");
            }
        }
    }
}
