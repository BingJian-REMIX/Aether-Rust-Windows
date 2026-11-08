use std::collections::HashSet;
use std::path::Path;

/// Rejects events whose path contains an excluded directory name at any depth.
pub struct PathFilter {
    root: String,
    excluded: HashSet<String>,
    fold_case: bool,
}

fn normalize(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_string()
}

impl PathFilter {
    pub fn new(root: &Path, excluded: &[String]) -> Self {
        let fold_case = cfg!(windows);
        let set = excluded
            .iter()
            .map(|name| if fold_case { name.to_lowercase() } else { name.clone() })
            .collect();
        Self {
            root: normalize(root),
            excluded: set,
            fold_case,
        }
    }

    pub fn root(&self) -> &str {
        &self.root
    }

    pub fn is_excluded(&self, path: &Path) -> bool {
        let full = normalize(path);
        let relative = match full.strip_prefix(&self.root) {
            Some(rest) => rest.trim_start_matches('/'),
            None => {
                // An absolute path outside the watched tree is never ours to watch.
                // Relative paths can still come from notify and are checked by segment.
                if path.is_absolute() {
                    return true;
                }
                full.as_str()
            }
        };
        if relative.is_empty() {
            return true;
        }
        if relative == ".." || relative.starts_with("../") {
            return true;
        }
        relative.split('/').any(|segment| {
            if segment.is_empty() {
                return false;
            }
            let key = if self.fold_case {
                segment.to_lowercase()
            } else {
                segment.to_string()
            };
            self.excluded.contains(&key)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn filter() -> PathFilter {
        let excludes = [".git".to_string(), "bin".to_string(), "obj".to_string()];
        PathFilter::new(Path::new("D:/work/repo"), &excludes)
    }

    #[test]
    fn excludes_git_bin_obj_at_any_depth() {
        let f = filter();
        for path in [
            "D:/work/repo/.git/config",
            "D:/work/repo/.git/objects/ab/cd",
            "D:/work/repo/src/bin/Debug/app.dll",
            "D:/work/repo/src/obj/Release/app.o",
            "D:/work/repo/bin/x",
            "D:/work/repo/obj/y",
        ] {
            assert!(f.is_excluded(&PathBuf::from(path)), "should exclude {path}");
        }
    }

    #[test]
    fn allows_normal_paths() {
        let f = filter();
        for path in [
            "D:/work/repo/src/main.rs",
            "D:/work/repo/Cargo.toml",
            "D:/work/repo/Dockerfile",
            "D:/work/repo/objserve/keep.txt",
        ] {
            assert!(!f.is_excluded(&PathBuf::from(path)), "should allow {path}");
        }
    }

    #[test]
    fn rejects_root_itself_and_paths_outside_root() {
        let f = filter();
        assert!(f.is_excluded(Path::new("D:/work/repo")));
        assert!(f.is_excluded(Path::new("D:/work/other/file.txt")));
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_are_case_insensitive() {
        let f = filter();
        assert!(f.is_excluded(Path::new(r"D:\work\repo\.GIT\config")));
        assert!(f.is_excluded(Path::new(r"D:\work\repo\src\BIN\x.dll")));
    }
}
