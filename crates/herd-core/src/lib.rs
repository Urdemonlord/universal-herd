use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize)]
pub struct ProjectCandidate {
    pub name: String,
    pub path: PathBuf,
    pub kind: String,
    pub manifest: String,
}

const MARKERS: &[(&str, &str, &str)] = &[
    ("package.json", "Node.js", "package.json"),
    ("Cargo.toml", "Rust", "Cargo.toml"),
    ("composer.json", "PHP", "composer.json"),
    ("pyproject.toml", "Python", "pyproject.toml"),
    ("go.mod", "Go", "go.mod"),
    ("Gemfile", "Ruby", "Gemfile"),
];

/// Detect whether a single directory is a project by looking for a known
/// manifest file directly inside it. Returns `None` when no marker is
/// found. Used for linked paths, which are project directories themselves
/// rather than roots to scan.
pub fn detect_project(dir: &Path) -> Option<ProjectCandidate> {
    for (marker, kind, manifest) in MARKERS {
        if dir.join(marker).is_file() {
            let name = dir.file_name().and_then(|v| v.to_str()).unwrap_or("project").to_string();
            return Some(ProjectCandidate {
                name,
                path: dir.to_path_buf(),
                kind: (*kind).into(),
                manifest: (*manifest).into(),
            });
        }
    }
    None
}

pub fn scan_projects(root: &Path, max_depth: usize) -> std::io::Result<Vec<ProjectCandidate>> {
    let mut found = Vec::new();
    scan_dir(root, 0, max_depth.min(8), &mut found)?;
    found.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(found)
}

fn scan_dir(
    dir: &Path,
    depth: usize,
    max_depth: usize,
    out: &mut Vec<ProjectCandidate>,
) -> std::io::Result<()> {
    if depth > max_depth {
        return Ok(());
    }
    let entries = match fs::read_dir(dir) {
        Ok(v) => v,
        Err(e) if depth == 0 => return Err(e),
        Err(_) => return Ok(()),
    };
    let mut dirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.')
            || matches!(
                name.as_str(),
                "node_modules" | "target" | "vendor" | "dist" | "build" | ".git"
            )
        {
            continue;
        }
        if path.is_file() {
            if let Some((_, kind, manifest)) =
                MARKERS.iter().find(|(marker, _, _)| *marker == name.as_str())
            {
                let project_dir = path.parent().unwrap_or(dir);
                let project_name = project_dir
                    .file_name()
                    .and_then(|v| v.to_str())
                    .unwrap_or("project")
                    .to_string();
                if !out.iter().any(|p: &ProjectCandidate| p.path == project_dir) {
                    out.push(ProjectCandidate {
                        name: project_name,
                        path: project_dir.to_path_buf(),
                        kind: (*kind).into(),
                        manifest: (*manifest).into(),
                    });
                }
            }
        } else if path.is_dir() && depth < max_depth {
            dirs.push(path);
        }
    }
    for child in dirs {
        scan_dir(&child, depth + 1, max_depth, out)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_projects_and_skips_dependency_directories() {
        let root = std::env::temp_dir().join(format!("herd-scan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("web")).unwrap();
        fs::create_dir_all(root.join("web/node_modules/ignored")).unwrap();
        fs::create_dir_all(root.join("api")).unwrap();
        fs::write(root.join("web/package.json"), "{}").unwrap();
        fs::write(root.join("web/node_modules/ignored/package.json"), "{}").unwrap();
        fs::write(root.join("api/Cargo.toml"), "[package]").unwrap();

        let projects = scan_projects(&root, 3).unwrap();
        assert_eq!(projects.len(), 2);
        assert!(projects.iter().any(|p| p.kind == "Node.js"));
        assert!(projects.iter().any(|p| p.kind == "Rust"));
        assert!(!projects.iter().any(|p| p.path.to_string_lossy().contains("node_modules")));
        fs::remove_dir_all(root).unwrap();
    }
}
