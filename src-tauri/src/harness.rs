//! The template of a new harness folder: instructions, skills and task notes
//! for the agents, separate from the code.

use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::Path;

use crate::agent::AgentKind;

/// Writes the template files for `agents` into `dir`. A file that exists
/// stays as it is. Returns the files that it wrote, relative to `dir`.
pub fn write_template(dir: &Path, agents: &[AgentKind]) -> io::Result<Vec<&'static str>> {
    let has = |kind| agents.contains(&kind);
    let mut files = vec![
        ("AGENTS.md", AGENTS_MD.to_owned()),
        (
            "tasks/README.md",
            "# Task notes\n\nOne Markdown file per task: the goal, the decisions and the open points.\n".to_owned(),
        ),
    ];
    if has(AgentKind::Claude) {
        files.push(("CLAUDE.md", "@AGENTS.md\n".to_owned()));
        files.push((".claude/skills/README.md", SKILLS.to_owned()));
    }
    if has(AgentKind::Gemini) {
        files.push(("GEMINI.md", "@./AGENTS.md\n".to_owned()));
    }
    if [AgentKind::Codex, AgentKind::Pi, AgentKind::Gemini].into_iter().any(has) {
        files.push((".agents/skills/README.md", SKILLS.to_owned()));
    }
    let mut written = Vec::new();
    for (name, text) in files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        // `create_new` also refuses a link at the path, so it never writes through one.
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                file.write_all(text.as_bytes())?;
                written.push(name);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Ok(written)
}

const SKILLS: &str = "# Skills\n\nPut one folder per skill here, with a SKILL.md in each.\n";

/// The guest folder of the code depends on the VM members at each start
/// (ADR 0007), so the text names the var that each agent gets
/// ([`crate::agent::launch::CODE_ENV`]), not a path.
const AGENTS_MD: &str = "# Harness\n\n\
    This folder holds the instructions, skills and task notes of the agents. The code is not here.\n\n\
    - The code is in `$ZWINGER_CODE`. Read and change the code there.\n\
    - Read the instructions of the code (AGENTS.md or CLAUDE.md in `$ZWINGER_CODE`) too.\n\
    - Work in small steps. Run the tests of the code before you finish.\n\
    - Write the notes of each task in `tasks/` of this folder, one file per task.\n";

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> std::path::PathBuf {
        crate::test_dir::root().join(format!("zwinger-harness-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn writes_only_the_files_of_the_selected_agents() {
        let dir = dir();
        let written = write_template(&dir, &[AgentKind::Claude]).unwrap();
        assert_eq!(written, ["AGENTS.md", "tasks/README.md", "CLAUDE.md", ".claude/skills/README.md"]);
        assert!(!dir.join("GEMINI.md").exists());
        assert!(!dir.join(".agents").exists());
        assert_eq!(fs::read_to_string(dir.join("CLAUDE.md")).unwrap(), "@AGENTS.md\n");
        assert!(fs::read_to_string(dir.join("AGENTS.md")).unwrap().contains("The code is in `$ZWINGER_CODE`."));

        let dir = self::dir();
        let written = write_template(&dir, &[AgentKind::Codex, AgentKind::Gemini]).unwrap();
        assert_eq!(written, ["AGENTS.md", "tasks/README.md", "GEMINI.md", ".agents/skills/README.md"]);
        assert!(!dir.join("CLAUDE.md").exists());
        assert!(!dir.join(".claude").exists());
    }

    #[test]
    fn never_overwrites_a_file() {
        let dir = dir();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("AGENTS.md"), "mine").unwrap();
        std::os::unix::fs::symlink(dir.join("elsewhere.md"), dir.join("CLAUDE.md")).unwrap();
        let written = write_template(&dir, &[AgentKind::Claude]).unwrap();
        assert_eq!(written, ["tasks/README.md", ".claude/skills/README.md"]);
        assert_eq!(fs::read_to_string(dir.join("AGENTS.md")).unwrap(), "mine");
        assert!(!dir.join("elsewhere.md").exists());

        assert!(write_template(&dir, &[AgentKind::Claude]).unwrap().is_empty());
    }
}
