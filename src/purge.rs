//! `cargo overstay purge`: delete store-known build targets, optionally
//! scanning the filesystem for cargo targets overstay has never seen.
//!
//! Deletion confidence is tiered. A dir is deleted outright only when
//! cargo's own droppings prove cargo built it (`CACHEDIR.TAG` mentioning
//! cargo, `.rustc_info.json`, or a compiled profile) — whether it came from
//! a store row or from the scan finding a `target/` that a sibling
//! `Cargo.toml` also vouches for. Weaker hits from either source (a stale
//! store path reused by something else, a fresh manifest with an unbuilt
//! target, cargo markers with no manifest beside them) are listed and gated
//! behind one batch confirmation. A `target/` nothing vouches for — someone's
//! JS build output, say — is never listed, never touched. Every deletion
//! first takes cargo's build lock, so running builds are skipped.
//!
//! The scan itself, and the `--include-untracked` parsing, live in
//! `crate::scan` so `ls` reports exactly what `purge` would act on.

use std::collections::HashSet;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

pub(crate) struct Summary {
    pub freed: u64,
    pub deleted: usize,
    pub locked: usize,
}

/// A candidate target with no cargo markers inside.
pub(crate) struct UnsureHit {
    pub target: PathBuf,
    pub size: u64,
}

pub fn run(args: &[OsString]) -> i32 {
    let style = crate::style::Style::stdout();
    let scan = match crate::scan::parse_args(args) {
        Ok(scan) => scan,
        Err(error) => {
            let error_style = crate::style::Style::stderr();
            eprintln!(
                "{} {error}\n{} {}",
                error_style.error("cargo-overstay:"),
                error_style.heading("Usage:"),
                error_style.command("cargo overstay purge [--include-untracked [dir]]")
            );
            return 2;
        }
    };
    if let crate::scan::Scan::Explicit(root) = &scan {
        if !root.is_dir() {
            let error_style = crate::style::Style::stderr();
            eprintln!(
                "{} {} is not a directory",
                error_style.error("cargo-overstay:"),
                error_style.path(root.display())
            );
            return 2;
        }
    }
    crate::shim::warn_if_inactive();
    let roots = scan.roots();
    if scan.is_on() {
        println!(
            "{} {}",
            style.muted("scanning"),
            style.muted(crate::scan::describe_roots(&roots))
        );
    }
    let store = crate::store::Store::open(&crate::paths::state_path());
    let summary = purge(&store, &roots, &mut confirm_on_stdin);
    let locked = if summary.locked > 0 {
        format!(
            " {}",
            style.warning(format!("({} skipped: build running)", summary.locked))
        )
    } else {
        String::new()
    };
    println!(
        "{} from {} target dir{}{}",
        style.success(format!("freed {}", crate::size::format_size(summary.freed))),
        style.strong(summary.deleted),
        if summary.deleted == 1 { "" } else { "s" },
        locked,
    );
    0
}

fn confirm_on_stdin(hits: &[UnsureHit]) -> bool {
    let style = crate::style::Style::stdout();
    let total: u64 = hits.iter().map(|h| h.size).sum();
    println!("\n{}", style.heading("Unverified targets"));
    println!(
        "{} target dir{} without cargo build markers:",
        style.warning(hits.len()),
        if hits.len() == 1 { "" } else { "s" }
    );
    for h in hits {
        println!(
            "  {}  {}",
            style.accent(format!("{:>10}", crate::size::format_size(h.size))),
            style.path(h.target.display())
        );
    }
    print!(
        "{} {} ",
        style.strong("Delete these too?"),
        style.muted(format!("({} total) [y/N]", crate::size::format_size(total)))
    );
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim(), "y" | "Y" | "yes")
}

pub(crate) fn purge(
    store: &crate::store::Store,
    scan_roots: &[PathBuf],
    confirm: &mut dyn FnMut(&[UnsureHit]) -> bool,
) -> Summary {
    let mut summary = Summary {
        freed: 0,
        deleted: 0,
        locked: 0,
    };

    // Phase 1: every target the store knows about. `seen` keeps an optional
    // scan from re-finding targets this phase handled (or skipped as locked).
    // Rows were recorded from real cargo runs, but the path may have been
    // reused since — a row whose dir no longer carries cargo markers is
    // demoted to the confirmation bucket rather than deleted outright.
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut unsure: Vec<UnsureHit> = Vec::new();
    for e in store.entries() {
        let target = PathBuf::from(&e.target_dir);
        if target.is_dir() {
            if let Ok(canon) = target.canonicalize() {
                seen.insert(canon);
            }
            if crate::scan::is_cargo_target(&target) {
                delete_target(&target, &mut summary);
            } else {
                unsure.push(UnsureHit {
                    size: crate::size::dir_size(&target),
                    target,
                });
            }
        }
    }

    // Phase 2 is opt-in because it discovers targets outside overstay's store.
    for hit in crate::scan::scan(scan_roots, &seen) {
        if hit.verified {
            delete_target(&hit.target, &mut summary);
        } else {
            unsure.push(UnsureHit {
                size: crate::size::dir_size(&hit.target),
                target: hit.target,
            });
        }
    }
    if !unsure.is_empty() && confirm(&unsure) {
        for hit in &unsure {
            delete_target(&hit.target, &mut summary);
        }
    }

    // Prune rows whose target is gone — deleted above or already missing.
    let prune: Vec<String> = store
        .entries()
        .into_iter()
        .filter(|e| !Path::new(&e.target_dir).exists())
        .map(|e| e.target_dir)
        .collect();
    let _ = store.remove_targets(&prune);
    summary
}

/// rm -rf one target behind cargo's build lock; prints what happened.
fn delete_target(target: &Path, summary: &mut Summary) {
    let style = crate::style::Style::stdout();
    let Some(_locks) = crate::trim::lock_target(target) else {
        println!(
            "{}  {}  {}",
            style.warning(format!("{:>10}", "-")),
            style.path(target.display()),
            style.warning("(skipped: build running)")
        );
        summary.locked += 1;
        return;
    };
    let before = crate::size::dir_size(target);
    let _ = std::fs::remove_dir_all(target);
    let freed = before.saturating_sub(crate::size::dir_size(target));
    println!(
        "{}  {}",
        style.success(format!("{:>10}", crate::size::format_size(freed))),
        style.path(target.display())
    );
    summary.freed += freed;
    summary.deleted += 1;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use std::fs;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("overstay_purge_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A project whose target carries cargo markers (the "sure" tier).
    fn cargo_project(project: &Path) -> PathBuf {
        fs::create_dir_all(project).unwrap();
        fs::write(project.join("Cargo.toml"), "[package]\n").unwrap();
        let target = project.join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join(".rustc_info.json"), "{}").unwrap();
        fs::write(target.join("blob.bin"), vec![0u8; 4096]).unwrap();
        target
    }

    fn no_confirm(_: &[UnsureHit]) -> bool {
        panic!("confirmation must not be requested");
    }

    #[test]
    fn include_untracked_purges_tracked_and_scanned_targets() {
        let base = temp("known_sure");
        // Known project lives OUTSIDE the scan root; scanned one inside.
        let known = cargo_project(&base.join("known"));
        let root = base.join("scanroot");
        let found = cargo_project(&root.join("proj"));

        let store = Store::open(&base.join("state"));
        store
            .touch(
                &base.join("known").to_string_lossy(),
                &known.to_string_lossy(),
                1_000,
            )
            .unwrap();

        let summary = purge(&store, std::slice::from_ref(&root), &mut no_confirm);
        assert!(!known.exists());
        assert!(!found.exists());
        assert_eq!(summary.deleted, 2);
        assert!(summary.freed >= 8192);
        assert!(store.entries().is_empty());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn tracked_only_purge_leaves_untracked_target_untouched() {
        let base = temp("tracked_only");
        let tracked = cargo_project(&base.join("tracked"));
        let untracked = cargo_project(&base.join("untracked"));
        let store = Store::open(&base.join("state"));
        store
            .touch(
                &base.join("tracked").to_string_lossy(),
                &tracked.to_string_lossy(),
                1_000,
            )
            .unwrap();

        let summary = purge(&store, &[], &mut no_confirm);

        assert_eq!(
            (tracked.exists(), untracked.exists(), summary.deleted),
            (false, true, 1)
        );
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn unsure_targets_are_gated_on_confirmation() {
        let base = temp("unsure");
        let project = base.join("proj");
        fs::create_dir_all(project.join("target")).unwrap();
        fs::write(project.join("Cargo.toml"), "[package]\n").unwrap();
        fs::write(project.join("target/stuff.bin"), vec![0u8; 1024]).unwrap();
        let store = Store::open(&base.join("state"));

        // Declined -> survives.
        let mut asked = 0;
        let summary = purge(&store, std::slice::from_ref(&base), &mut |hits| {
            asked += 1;
            assert_eq!(hits.len(), 1);
            assert!(hits[0].size >= 1024);
            false
        });
        assert_eq!(asked, 1);
        assert_eq!(summary.deleted, 0);
        assert!(project.join("target/stuff.bin").exists());

        // Accepted -> deleted.
        let summary = purge(&store, std::slice::from_ref(&base), &mut |_| true);
        assert_eq!(summary.deleted, 1);
        assert!(!project.join("target").exists());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn manifestless_target_dirs_are_invisible() {
        let base = temp("nomanifest");
        let junk = base.join("frontend/target");
        fs::create_dir_all(&junk).unwrap();
        fs::write(junk.join("bundle.js"), vec![0u8; 1024]).unwrap();
        let store = Store::open(&base.join("state"));

        let summary = purge(&store, std::slice::from_ref(&base), &mut no_confirm);
        assert_eq!(summary.deleted, 0);
        assert!(junk.join("bundle.js").exists());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn hidden_dirs_and_symlinks_are_not_entered() {
        let base = temp("hidden");
        let hidden = cargo_project(&base.join(".stash/proj"));
        let outside = cargo_project(&base.join("outside-root"));
        let root = base.join("root");
        fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(base.join("outside-root"), root.join("link")).unwrap();
        let store = Store::open(&base.join("state"));

        let summary = purge(&store, &[base.join(".stash")], &mut no_confirm);
        // Scanning an explicit root works even if the root itself is hidden…
        assert_eq!(summary.deleted, 1);
        assert!(!hidden.exists());
        // …but a scan never crosses symlinks.
        let summary = purge(&store, std::slice::from_ref(&root), &mut no_confirm);
        assert_eq!(summary.deleted, 0);
        assert!(outside.exists());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn locked_targets_survive_and_keep_their_row() {
        let base = temp("locked");
        let target = cargo_project(&base.join("busy"));
        fs::create_dir_all(target.join("debug/.fingerprint")).unwrap();
        fs::write(target.join("debug/.cargo-lock"), b"").unwrap();
        let store = Store::open(&base.join("state"));
        store
            .touch(
                &base.join("busy").to_string_lossy(),
                &target.to_string_lossy(),
                1_000,
            )
            .unwrap();

        let build = fs::File::open(target.join("debug/.cargo-lock")).unwrap();
        assert!(crate::trim::flock_exclusive_nb(&build));
        let summary = purge(&store, &[], &mut no_confirm);
        assert_eq!(summary.locked, 1);
        assert_eq!(summary.deleted, 0);
        assert!(target.exists());
        assert_eq!(store.entries().len(), 1);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn store_rows_without_markers_are_gated_on_confirmation() {
        let base = temp("stale_row");
        // A recorded path that no longer looks cargo-built: no markers inside.
        let target = base.join("proj/target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("data.bin"), vec![0u8; 2048]).unwrap();
        let store = Store::open(&base.join("state"));
        store
            .touch(
                &base.join("proj").to_string_lossy(),
                &target.to_string_lossy(),
                1_000,
            )
            .unwrap();
        // Declined -> survives, row kept.
        let mut asked = 0;
        let summary = purge(&store, &[], &mut |hits| {
            asked += 1;
            assert_eq!(hits.len(), 1);
            assert!(hits[0].size >= 2048);
            false
        });
        assert_eq!(asked, 1);
        assert_eq!(summary.deleted, 0);
        assert!(target.join("data.bin").exists());
        assert_eq!(store.entries().len(), 1);

        // Accepted -> deleted, row pruned.
        let summary = purge(&store, &[], &mut |_| true);
        assert_eq!(summary.deleted, 1);
        assert!(!target.exists());
        assert!(store.entries().is_empty());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn scan_does_not_recount_known_targets() {
        let base = temp("recount");
        let root = base.join("root");
        let target = cargo_project(&root.join("proj"));
        let store = Store::open(&base.join("state"));
        store
            .touch(
                &root.join("proj").to_string_lossy(),
                &target.to_string_lossy(),
                1_000,
            )
            .unwrap();

        // Known target sits inside the scan root: phase 1 deletes it, the
        // scan must not report it again.
        let summary = purge(&store, std::slice::from_ref(&root), &mut no_confirm);
        assert_eq!(summary.deleted, 1);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn manifestless_target_with_cargo_markers_is_gated_not_ignored() {
        // Real shape from a git worktree: cargo plainly built here, but no
        // `Cargo.toml` sits beside the target. Previously invisible to the
        // scan, so its space could never be reclaimed.
        let base = temp("markers_no_manifest");
        let target = base.join("worktree/target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join(".rustc_info.json"), "{}").unwrap();
        fs::write(target.join("blob.bin"), vec![0u8; 4096]).unwrap();
        let store = Store::open(&base.join("state"));

        // Declined -> survives, never deleted on markers alone.
        let summary = purge(&store, std::slice::from_ref(&base), &mut |hits| {
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].target, target);
            false
        });
        assert_eq!(summary.deleted, 0);
        assert!(target.exists());

        // Accepted -> deleted.
        let summary = purge(&store, std::slice::from_ref(&base), &mut |_| true);
        assert_eq!(summary.deleted, 1);
        assert!(!target.exists());
        fs::remove_dir_all(&base).unwrap();
    }
}
