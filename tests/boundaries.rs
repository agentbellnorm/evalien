//! Folders are modules. They share types through `domain` and never import
//! each other. Top-level files are composition: the entry points and what
//! they share.

use std::{collections::BTreeSet, fs, path::Path};

/// Every `crate::<module>` a file refers to.
fn crate_modules(source: &str) -> BTreeSet<String> {
    source
        .match_indices("crate::")
        .map(|(at, prefix)| {
            let rest = &source[at + prefix.len()..];
            rest.chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect::<String>()
        })
        .filter(|name| !name.is_empty())
        .collect()
}

fn rust_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(rust_files(&path));
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
    files
}

#[test]
fn folder_modules_depend_only_on_the_domain() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations = Vec::new();
    let mut modules = 0;
    for entry in fs::read_dir(&src).unwrap() {
        let dir = entry.unwrap().path();
        let Some(module) = dir.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
            continue;
        };
        if !dir.is_dir() || module == "bin" {
            continue;
        }
        modules += 1;
        for file in rust_files(&dir) {
            let source = fs::read_to_string(&file).unwrap();
            for used in crate_modules(&source) {
                let allowed = used == module || (used == "domain" && module != "domain");
                if !allowed {
                    violations.push(format!(
                        "{} uses crate::{used}",
                        file.strip_prefix(&src).unwrap().display()
                    ));
                }
            }
        }
    }
    assert!(modules >= 7, "expected the folder modules under src/");
    assert!(
        violations.is_empty(),
        "module boundary violations:\n{}",
        violations.join("\n")
    );
}
