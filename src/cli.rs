//! The `cargo overstay <command>` dispatch: `purge` and `ls`.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::Path;

pub(crate) fn run_cli(args: &[OsString]) -> i32 {
    match args.first().and_then(|a| a.to_str()) {
        Some("purge") => crate::purge::run(&args[1..]),
        Some("ls") => ls(&args[1..]),
        // Both spellings: `--version` is the usual flag, `version` matches
        // cargo's own subcommand. An install that silently went stale is only
        // diagnosable if asking is easy.
        Some("--version" | "-V" | "version") => {
            println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
            0
        }
        _ => {
            print_usage();
            2
        }
    }
}

fn print_usage() {
    let style = crate::style::Style::stderr();
    eprintln!("{}", style.heading("Usage"));
    eprintln!(
        "  {} {}",
        style.command("cargo overstay"),
        style.muted("<command>")
    );
    eprintln!();
    eprintln!("{}", style.heading("Commands"));
    eprintln!(
        "  {} delete tracked build targets",
        style.command(format!("{:<36}", "purge"))
    );
    eprintln!(
        "  {} also scan dir (default: home + temp)",
        style.command(format!("{:<36}", "purge --include-untracked [dir]"))
    );
    eprintln!(
        "  {} list tracked projects with sizes",
        style.command(format!("{:<36}", "ls"))
    );
    eprintln!(
        "  {} also scan dir (default: home + temp)",
        style.command(format!("{:<36}", "ls --include-untracked [dir]"))
    );
    eprintln!(
        "  {} print the version and exit",
        style.command(format!("{:<36}", "--version"))
    );
}

fn ls(args: &[OsString]) -> i32 {
    let style = crate::style::Style::stdout();
    let error_style = crate::style::Style::stderr();
    let scan = match crate::scan::parse_args(args) {
        Ok(scan) => scan,
        Err(error) => {
            eprintln!(
                "{} {error}\n{} {}",
                error_style.error("cargo-overstay:"),
                error_style.heading("Usage:"),
                error_style.command("cargo overstay ls [--include-untracked [dir]]")
            );
            return 2;
        }
    };
    let policy = match crate::config::load_policy() {
        Ok(policy) => policy,
        Err(error) => {
            eprintln!("{} {error}", error_style.error("cargo-overstay:"));
            return 2;
        }
    };
    if let crate::scan::Scan::Explicit(root) = &scan {
        if !root.is_dir() {
            eprintln!(
                "{} {} is not a directory",
                error_style.error("cargo-overstay:"),
                error_style.path(root.display())
            );
            return 2;
        }
    }
    crate::shim::warn_if_inactive();

    let store = crate::store::Store::open(&crate::paths::state_path());
    let now = crate::size::now_unix();
    let mut rows = store.entries();
    rows.sort_by_key(|e| std::cmp::Reverse(e.last_used));

    let mut tracked_total = 0u64;
    let mut missing = 0usize;
    if rows.is_empty() {
        println!(
            "{}",
            style.muted("No tracked projects yet — build something through cargo first.")
        );
    } else {
        println!();
    }
    for e in &rows {
        let target = Path::new(&e.target_dir);
        // A recorded target that is gone measures as 0 B, which reads as an
        // empty build dir rather than a stale row. Name the real condition.
        let size_cell = if target.is_dir() {
            let size = crate::size::dir_size(target);
            tracked_total += size;
            let cell = format!("{:>10}", crate::size::format_size(size));
            if size > policy.max_project_size {
                style.warning(cell)
            } else {
                style.accent(cell)
            }
        } else {
            missing += 1;
            style.muted(format!("{:>10}", "missing"))
        };
        let mut line = format!(
            "{}  {}  {}",
            size_cell,
            style.muted(format!("{:>8}", format_age(now - e.last_used))),
            style.path(&e.path)
        );
        if target != Path::new(&e.path).join("target") {
            line.push_str(&format!(
                "  {} {}{}",
                style.muted("(target:"),
                style.path(&e.target_dir),
                style.muted(")")
            ));
        }
        println!("{line}");
    }

    let mut untracked_total = 0u64;
    if scan.is_on() {
        // Canonical paths of tracked targets, so a target that is both
        // recorded and inside a scan root is listed once, above.
        let seen: HashSet<_> = rows
            .iter()
            .filter_map(|e| Path::new(&e.target_dir).canonicalize().ok())
            .collect();
        let roots = scan.roots();
        let mut hits: Vec<_> = crate::scan::scan(&roots, &seen)
            .into_iter()
            .map(|hit| (crate::size::dir_size(&hit.target), hit))
            .collect();
        hits.sort_by_key(|(size, _)| std::cmp::Reverse(*size));

        println!();
        println!(
            "{} {}",
            style.heading("Untracked targets"),
            style.muted(format!("(scanned {})", crate::scan::describe_roots(&roots)))
        );
        if hits.is_empty() {
            println!("{}", style.muted("  none found"));
        }
        for (size, hit) in &hits {
            untracked_total += size;
            let cell = format!("{:>10}", crate::size::format_size(*size));
            let cell = if *size > policy.max_project_size {
                style.warning(cell)
            } else {
                style.accent(cell)
            };
            // Unverified hits are what `purge` would ask about rather than
            // delete outright; flag them here so the two agree.
            let note = if hit.verified {
                String::new()
            } else {
                format!("  {}", style.muted("(unverified)"))
            };
            println!("{}  {}{}", cell, style.path(hit.target.display()), note);
        }
    }

    println!();
    let total_cell = format!("{:>10}", crate::size::format_size(tracked_total));
    let total_cell = if tracked_total > policy.max_total_cache {
        style.error(total_cell)
    } else {
        style.success(total_cell)
    };
    println!(
        "{}  {} {}",
        total_cell,
        style.strong(if scan.is_on() { "tracked" } else { "total" }),
        style.muted(format!(
            "(budget {})",
            crate::size::format_size(policy.max_total_cache)
        ))
    );
    if scan.is_on() {
        println!(
            "{}  {} {}",
            style.warning(format!("{:>10}", crate::size::format_size(untracked_total))),
            style.strong("untracked"),
            style.muted("(not counted against the budget)")
        );
    }
    if missing > 0 {
        println!(
            "{}",
            style.muted(format!(
                "{missing} tracked target{} no longer on disk — `cargo overstay purge` prunes the row{}.",
                if missing == 1 { "" } else { "s" },
                if missing == 1 { "" } else { "s" },
            ))
        );
    }
    0
}

fn format_age(secs: i64) -> String {
    let secs = secs.max(0);
    match secs {
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_verbs_exit_2() {
        assert_eq!(run_cli(&["bogus".into()]), 2);
        assert_eq!(run_cli(&[]), 2);
    }

    #[test]
    fn version_is_reported_under_every_spelling() {
        assert_eq!(run_cli(&["--version".into()]), 0);
        assert_eq!(run_cli(&["-V".into()]), 0);
        assert_eq!(run_cli(&["version".into()]), 0);
    }

    #[test]
    fn ls_rejects_unknown_options_instead_of_swallowing_them() {
        // `ls` used to take no args at all, so any flag — including the
        // `--include-untracked` that `purge` honors — was silently dropped
        // and the scan just never happened.
        assert_eq!(run_cli(&["ls".into(), "--bogus".into()]), 2);
        assert_eq!(run_cli(&["ls".into(), "/some/dir".into()]), 2);
    }

    #[test]
    fn ages_format_coarsely() {
        assert_eq!(format_age(90), "1m ago");
        assert_eq!(format_age(7200), "2h ago");
        assert_eq!(format_age(3 * 86_400 + 5), "3d ago");
        assert_eq!(format_age(-5), "0m ago"); // clock skew
    }
}
