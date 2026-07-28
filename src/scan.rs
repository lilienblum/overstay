//! Finding cargo targets on disk: the `--include-untracked` flag shared by
//! `purge` and `ls`, plus the walk that discovers targets overstay never
//! recorded.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// A `target/` dir the walk accepted, and how strong the evidence was.
pub(crate) struct ScanHit {
    pub target: PathBuf,
    /// A sibling `Cargo.toml` *and* cargo's own markers vouch for this dir.
    /// Anything weaker is reported but never deleted unprompted.
    pub verified: bool,
}

/// What `--include-untracked` asked for.
#[derive(Debug, PartialEq)]
pub(crate) enum Scan {
    Off,
    /// A directory the user named. A bad path is a hard error — they meant
    /// that one.
    Explicit(PathBuf),
    /// No directory given: search the usual places.
    Defaults,
}

impl Scan {
    pub(crate) fn is_on(&self) -> bool {
        !matches!(self, Scan::Off)
    }

    pub(crate) fn roots(&self) -> Vec<PathBuf> {
        match self {
            Scan::Off => Vec::new(),
            Scan::Explicit(root) => vec![root.clone()],
            Scan::Defaults => default_roots(),
        }
    }
}

/// Where cargo targets pile up when nobody names a directory.
///
/// Home is the obvious one. The temp dirs matter just as much: agent tooling
/// (Claude Code, Codex) creates git worktrees under them, each with a full
/// `target/` — builds nobody thinks of as living on their disk, and the
/// single biggest thing a home-only scan misses. `/tmp` and `$TMPDIR` are
/// different directories on macOS, so both are candidates.
fn default_roots() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            candidates.push(PathBuf::from(home));
        }
    }
    candidates.push(PathBuf::from("/tmp"));
    candidates.push(std::env::temp_dir());
    usable_roots(candidates)
}

/// Canonicalizes, drops what is not a readable directory, and drops any root
/// already contained in an earlier one so an overlapping pair is walked once.
fn usable_roots(candidates: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for candidate in candidates {
        let Ok(canon) = candidate.canonicalize() else {
            continue;
        };
        if !canon.is_dir() || roots.iter().any(|root| canon.starts_with(root)) {
            continue;
        }
        roots.push(canon);
    }
    roots
}

/// Parses `[--include-untracked [dir]]`, shared so `ls` and `purge` accept —
/// and reject — exactly the same words. A parser only one command consults is
/// how a flag ends up silently ignored by the other.
pub(crate) fn parse_args(args: &[OsString]) -> Result<Scan, String> {
    let mut include_untracked = false;
    let mut root = None;
    let mut options = true;
    for arg in args {
        if options && arg == OsStr::new("--") {
            options = false;
        } else if options && arg == OsStr::new("--include-untracked") {
            include_untracked = true;
        } else if options && arg.to_string_lossy().starts_with('-') {
            return Err(format!("unknown option: {}", arg.to_string_lossy()));
        } else if root.is_some() {
            return Err(format!("unexpected argument: {}", arg.to_string_lossy()));
        } else {
            root = Some(PathBuf::from(arg));
        }
    }

    if root.is_some() && !include_untracked {
        return Err("a scan directory requires --include-untracked".to_string());
    }
    Ok(match (include_untracked, root) {
        (false, _) => Scan::Off,
        (true, Some(root)) => Scan::Explicit(root),
        (true, None) => Scan::Defaults,
    })
}

/// Names the directories being walked. With defaults now covering more than
/// home, a scan that reports nothing should say where it looked.
pub(crate) fn describe_roots(roots: &[PathBuf]) -> String {
    if roots.is_empty() {
        return "nothing (no readable scan root)".to_string();
    }
    roots
        .iter()
        .map(|root| root.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

const SKIP_DIRS: [&str; 2] = ["node_modules", "Library"];

/// Walks each root for `target/` dirs that something vouches for, skipping
/// any target already in `seen` and never reporting one twice across roots.
pub(crate) fn scan(roots: &[PathBuf], seen: &HashSet<PathBuf>) -> Vec<ScanHit> {
    let mut emitted = seen.clone();
    let mut hits = Vec::new();
    for root in roots {
        scan_root(root, &mut emitted, &mut hits);
    }
    hits
}

/// Hidden dirs and `SKIP_DIRS` are not entered, symlinks are never followed,
/// and a dir named `target` is never descended into — accepted or not.
///
/// Two independent signals vouch for a candidate: a sibling `Cargo.toml`, and
/// cargo's own markers inside. Both present means verified. Just one is still
/// reported, unverified, for a caller to gate behind confirmation — a manifest
/// with an unbuilt target, or markers with no manifest beside them (a worktree
/// whose manifest is nested deeper, or a `CARGO_TARGET_DIR` aimed here from
/// elsewhere). Neither signal means it is not reported at all: someone's JS
/// build output that happens to be named `target` stays invisible.
///
/// The walk also stays on the root's own filesystem. A mount underneath it is
/// someone else's storage — an OrbStack/Docker VM export, a file server, an
/// external disk — where walking is slow enough to stall on a timeout and
/// deleting a `target/` would reclaim space on a machine the user never named.
/// Because the baseline comes from the root, pointing the scan at a mount on
/// purpose still works.
fn scan_root(root: &Path, emitted: &mut HashSet<PathBuf>, hits: &mut Vec<ScanHit>) {
    let root_dev = std::fs::metadata(root).map(|m| m.dev()).ok();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let has_manifest = dir.join("Cargo.toml").is_file();
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            // file_type() does not follow symlinks, so a symlinked dir is
            // neither descended into nor deleted through.
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || SKIP_DIRS.contains(&name.as_ref()) {
                continue;
            }
            let path = entry.path();
            if !same_filesystem(root_dev, entry.metadata().map(|m| m.dev()).ok()) {
                continue;
            }
            if name != "target" {
                stack.push(path);
                continue;
            }
            let markers = is_cargo_target(&path);
            if !has_manifest && !markers {
                continue;
            }
            let canon = path.canonicalize().unwrap_or_else(|_| path.clone());
            if !emitted.insert(canon) {
                continue;
            }
            hits.push(ScanHit {
                verified: has_manifest && markers,
                target: path,
            });
        }
    }
}

/// Whether an entry sits on the same filesystem as the scan root. An entry
/// that cannot be stat'ed counts as foreign: a failing stat is usually the
/// unresponsive network mount the check exists to stay out of.
fn same_filesystem(root_dev: Option<u64>, entry_dev: Option<u64>) -> bool {
    match (root_dev, entry_dev) {
        (Some(root), Some(entry)) => root == entry,
        // No baseline (the root itself would not stat): do not start pruning.
        (None, _) => true,
        (Some(_), None) => false,
    }
}

/// Proof cargo wrote this dir: its cache-directory tag, its rustc probe
/// cache, or a compiled profile.
pub(crate) fn is_cargo_target(target: &Path) -> bool {
    if std::fs::read_to_string(target.join("CACHEDIR.TAG")).is_ok_and(|tag| tag.contains("cargo")) {
        return true;
    }
    if target.join(".rustc_info.json").is_file() {
        return true;
    }
    !crate::trim::profile_dirs(target).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("overstay_scan_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    fn scan_one(root: &Path) -> Vec<ScanHit> {
        scan(&[root.to_path_buf()], &HashSet::new())
    }

    #[test]
    fn parse_args_defaults_to_tracked_only() {
        assert_eq!(parse_args(&[]).unwrap(), Scan::Off);
    }

    #[test]
    fn bare_include_untracked_uses_the_default_roots() {
        let args = [OsString::from("--include-untracked")];
        assert_eq!(parse_args(&args).unwrap(), Scan::Defaults);
    }

    #[test]
    fn parse_args_accepts_include_untracked_with_root() {
        let args = [
            OsString::from("--include-untracked"),
            OsString::from("/work"),
        ];
        assert_eq!(
            parse_args(&args).unwrap(),
            Scan::Explicit(PathBuf::from("/work"))
        );
    }

    #[test]
    fn parse_args_rejects_scan_root_without_opt_in() {
        let error = parse_args(&[OsString::from("/work")]).unwrap_err();
        assert_eq!(error, "a scan directory requires --include-untracked");
    }

    #[test]
    fn parse_args_rejects_unknown_options() {
        let error = parse_args(&[OsString::from("--bogus")]).unwrap_err();
        assert_eq!(error, "unknown option: --bogus");
    }

    #[test]
    fn default_roots_cover_home_and_a_temp_dir() {
        // Agent worktrees land in temp, not under home, so a home-only
        // default silently misses whole builds.
        let roots = default_roots();
        let home = PathBuf::from(std::env::var("HOME").unwrap())
            .canonicalize()
            .unwrap();
        assert!(roots.contains(&home));
        assert!(roots.iter().any(|root| root != &home));
    }

    #[test]
    fn usable_roots_drops_missing_and_nested_candidates() {
        let base = temp("roots");
        let nested = base.join("inner");
        fs::create_dir_all(&nested).unwrap();

        let roots = usable_roots(vec![base.clone(), nested, base.join("gone")]);

        assert_eq!(roots, vec![base.clone()]);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn an_entry_on_another_filesystem_is_skipped() {
        // The OrbStack case: an NFS export of a Linux VM mounted under home.
        assert!(!same_filesystem(Some(1), Some(2)));
        assert!(same_filesystem(Some(1), Some(1)));
        // An entry that will not stat is treated as foreign...
        assert!(!same_filesystem(Some(1), None));
        // ...but an unstattable root leaves the walk unrestricted.
        assert!(same_filesystem(None, Some(2)));
    }

    #[test]
    fn manifest_and_markers_together_are_verified() {
        let base = temp("verified");
        let project = base.join("proj");
        fs::create_dir_all(project.join("target")).unwrap();
        fs::write(project.join("Cargo.toml"), "[package]\n").unwrap();
        fs::write(project.join("target/.rustc_info.json"), "{}").unwrap();

        let hits = scan_one(&base);

        assert_eq!(hits.len(), 1);
        assert!(hits[0].verified);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn markers_without_a_sibling_manifest_are_reported_unverified() {
        // The shape that made a 52.8 GiB worktree target invisible: cargo
        // plainly wrote it, but no `Cargo.toml` sits beside it.
        let base = temp("no_manifest");
        let target = base.join("worktree/target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join(".rustc_info.json"), "{}").unwrap();

        let hits = scan_one(&base);

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].target, target);
        assert!(!hits[0].verified);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn manifest_without_markers_is_reported_unverified() {
        let base = temp("no_markers");
        let project = base.join("proj");
        fs::create_dir_all(project.join("target")).unwrap();
        fs::write(project.join("Cargo.toml"), "[package]\n").unwrap();

        let hits = scan_one(&base);

        assert_eq!(hits.len(), 1);
        assert!(!hits[0].verified);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn target_dirs_with_neither_signal_stay_invisible() {
        let base = temp("neither");
        let junk = base.join("frontend/target");
        fs::create_dir_all(&junk).unwrap();
        fs::write(junk.join("bundle.js"), vec![0u8; 1024]).unwrap();

        assert!(scan_one(&base).is_empty());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn seen_targets_are_not_reported_again() {
        let base = temp("seen");
        let project = base.join("proj");
        fs::create_dir_all(project.join("target")).unwrap();
        fs::write(project.join("Cargo.toml"), "[package]\n").unwrap();
        fs::write(project.join("target/.rustc_info.json"), "{}").unwrap();
        let canon = project.join("target").canonicalize().unwrap();

        let hits = scan(std::slice::from_ref(&base), &HashSet::from([canon]));

        assert!(hits.is_empty());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn overlapping_roots_report_a_target_once() {
        let base = temp("overlap");
        let project = base.join("proj");
        fs::create_dir_all(project.join("target")).unwrap();
        fs::write(project.join("Cargo.toml"), "[package]\n").unwrap();
        fs::write(project.join("target/.rustc_info.json"), "{}").unwrap();

        let hits = scan(&[base.clone(), project.clone()], &HashSet::new());

        assert_eq!(hits.len(), 1);
        fs::remove_dir_all(&base).unwrap();
    }
}
