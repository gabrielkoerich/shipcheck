use clap::CommandFactory;

use super::*;

fn parses(call: &[&str]) -> bool {
    Cli::try_parse_from(std::iter::once("shipcheck").chain(call.iter().copied())).is_ok()
}

#[test]
fn a_timeout_is_seconds_unless_it_names_a_unit() {
    assert_eq!(duration("90"), Ok(Duration::from_secs(90)));
    assert_eq!(duration("90s"), Ok(Duration::from_secs(90)));
    assert_eq!(duration("10m"), Ok(Duration::from_secs(600)));
    assert_eq!(duration("1h"), Ok(Duration::from_secs(3600)));
    assert!(duration("soon").is_err());
    assert!(duration("5d").is_err());
    // An async run is a full run, and never one the hooks started
    assert!(!parses(&["run", "--async", "--job", "test"]));
    assert!(!parses(&["run", "--async", "--background", "feature"]));
}

#[test]
fn the_command_line_is_consistent() {
    Cli::command().debug_assert();
}

/// Every way shipcheck is called by its own hooks, its own background runs, its tests and its
/// docs must keep parsing, since clap refuses a flag a command does not declare
#[test]
fn every_known_call_parses() {
    let sha = "0".repeat(40);
    let sha = sha.as_str();
    let calls: &[&[&str]] = &[
        // The hooks, setup.rs
        &["on-push", "refs/heads/feature", sha],
        &["on-push", "refs/heads/feature", sha, "refs/heads/feature"],
        &["on-commit"],
        // The end-to-end tests
        &["run", "--no-fetch"],
        &["run", "--no-fetch", "cheat"],
        &["status", "--no-fetch"],
        &["status", sha],
        &["status", "feature"],
        &["logs", "main"],
        &["post"],
        &["post", "--home-ok"],
        &["doctor"],
        &["clean", "--all"],
        // SKILL.md, the README and the site's docs
        &["run", "--staged"],
        &["run", "--sha", "HEAD"],
        &["run", "--json"],
        &["run", "--job", "test", "--job", "lint"],
        &["run", "--workflow", "ci.yml"],
        &["run", "--base", "shipcheck-base", "--no-fetch"],
        &["status", "--staged", "--json"],
        &["logs", "--staged"],
        &["history", "--json", "--limit", "5"],
        &["history", "--flaky"],
        &["queue", "--json"],
        &["run", "--no-heartbeat"],
        &["run", "--async"],
        &["run", "--detach", "--json"],
        &["wait"],
        &["wait", "--staged"],
        &["wait", "0123456789ab", "--timeout", "10m", "--json"],
        &["status", "0123456789ab"],
        &["commit", "-m", "subject"],
        &[
            "commit",
            "-m",
            "subject",
            "-m",
            "body",
            "--allow-failed",
            "--no-verify",
        ],
        &["commit", "-F", "message.txt"],
        &["init", "--yes"],
        &["install"],
        &["login"],
        &["logout", "--all"],
        &["token", "--name", "ci-box"],
        &["clean"],
        &["daemon", "--home-ok"],
        &["daemon", "--uninstall"],
    ];

    for call in calls {
        assert!(
            parses(call),
            "shipcheck {} no longer parses",
            call.join(" ")
        );
    }
}

#[test]
fn what_shipcheck_passes_itself_parses() {
    let sha = "0".repeat(40);

    for (post, moved) in [
        (Posting::Off, MovedHome::Refused),
        (Posting::On, MovedHome::Refused),
        (Posting::Off, MovedHome::Allowed),
        (Posting::On, MovedHome::Allowed),
    ] {
        let args = background::run_args(&sha, "origin/main", "pr:1", post, moved);
        let call: Vec<&str> = args.iter().map(String::as_str).collect();
        assert!(
            parses(&call),
            "background run: shipcheck {}",
            call.join(" ")
        );
    }

    // The hook lines setup writes, with the values git hands them
    for line in [setup::HOOK_LINE, setup::COMMIT_LINE] {
        let call: Vec<String> = line
            .split_whitespace()
            .skip(1)
            .map(|w| {
                w.replace("\"$local_ref\"", "refs/heads/feature")
                    .replace("\"$local_sha\"", &sha)
                    .replace("\"$remote_ref\"", "refs/heads/feature")
            })
            .collect();
        let call: Vec<&str> = call.iter().map(String::as_str).collect();
        assert!(parses(&call), "hook: {line}");
    }
}

#[test]
fn a_push_to_the_base_starts_no_run() {
    assert!(pushes_to_base("refs/heads/main", "origin/main"));
    assert!(pushes_to_base("refs/heads/trunk", "origin/trunk"));
    assert!(pushes_to_base("main", "main"));
    assert!(!pushes_to_base("refs/heads/feature", "origin/main"));
    assert!(!pushes_to_base("refs/heads/main-fix", "origin/main"));
    assert!(!pushes_to_base("refs/tags/main", "origin/main"));
}

#[test]
fn a_wrong_call_is_refused() {
    for call in [
        &["frob"][..],
        &["run", "--frob"],
        &["run", "--sha", "HEAD", "--staged"],
        &["commit"],
        &["history", "--limit", "many"],
    ] {
        assert!(
            !parses(call),
            "shipcheck {} should be refused",
            call.join(" ")
        );
    }
}
