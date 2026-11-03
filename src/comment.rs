use crate::check::{self, Jobs};
use crate::github;
use crate::run::Entry;
use crate::service;

/// The first line of the App's PR comment, how a post finds it again
pub const MARKER: &str = "<!-- shipcheck:summary -->";
// GitHub refuses a comment over 65,536 characters, this leaves room for the header and a note
const BUDGET: usize = 64_000;

/// One job's part of the comment, as written between its markers
#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    pub name: String,
    pub failed: bool,
    pub text: String,
}

/**
 * Each job of the result as its check name, whether it failed, and its masked summary or None when
 * it wrote none. A job's summary is Markdown from the commit's own code, so it may not forge the
 * comment's markers
 **/
pub fn sections(
    sha: &str,
    entry: &Entry,
    secrets: &[String],
) -> Vec<(String, bool, Option<String>)> {
    let jobs: Jobs = entry
        .workflows
        .iter()
        .map(|w| (w.file.clone(), w.jobs.keys().cloned().collect()))
        .collect();
    let bodies = check::job_bodies(sha, &jobs, entry, None, false);

    check::job_names(&jobs)
        .into_iter()
        .zip(bodies)
        .map(|((file, job, name), body)| {
            let summary = entry
                .workflows
                .iter()
                .find(|w| w.file == file)
                .and_then(|w| w.summaries.get(&job))
                .filter(|s| !s.trim().is_empty())
                .map(|s| {
                    let mut s = s.replace("<!-- shipcheck:", "&lt;!-- shipcheck:");

                    for secret in secrets {
                        s = s.replace(secret.as_str(), "***");
                    }

                    s
                });
            (name, body["conclusion"] == "failure", summary)
        })
        .collect()
}

fn start(name: &str) -> String {
    format!("<!-- shipcheck:job:{name} -->")
}

fn end(name: &str) -> String {
    format!("<!-- shipcheck:end:{name} -->")
}

fn section(name: &str, failed: bool, sha: &str, summary: &str) -> Section {
    let state = if failed { "failed" } else { "passed" };
    let short = &sha[..12.min(sha.len())];
    Section {
        name: name.to_string(),
        failed,
        text: format!("### {name}: {state} on `{short}`\n\n{summary}\n"),
    }
}

/// The sections of a comment shipcheck wrote, None when a marker is missing, out of order or doubled
pub fn parse(body: &str) -> Option<Vec<Section>> {
    let mut rest = body.strip_prefix(MARKER)?;
    let mut found: Vec<Section> = Vec::new();
    // The header holds no marker, so a section that lost its start marker is broken, not header
    rest = &rest[rest.find("<!-- shipcheck:").unwrap_or(rest.len())..];

    while !rest.trim().is_empty() {
        let name = rest
            .strip_prefix("<!-- shipcheck:job:")?
            .split(" -->")
            .next()?;
        let inner = rest.strip_prefix(&start(name))?;
        let close = end(name);
        let at = inner.find(&close)?;
        let text = inner[..at].trim_matches('\n');

        if text.contains("<!-- shipcheck:") || found.iter().any(|s| s.name == name) {
            return None;
        }

        found.push(Section {
            name: name.to_string(),
            failed: text.starts_with(&format!("### {name}: failed")),
            text: format!("{text}\n"),
        });
        rest = inner[at + close.len()..].trim_start();
    }

    Some(found)
}

/**
 * The comment for a post of `sections`: a job with a summary gets its section, replaced whole, a
 * job that ran without one loses its old section, and jobs this result lacks keep theirs. A body
 * whose markers do not parse is rewritten from this result. None when no job wrote a summary, so a
 * post neither creates nor deletes anything
 **/
pub fn merge(
    old: Option<&str>,
    sha: &str,
    tree: &str,
    new: &[(String, bool, Option<String>)],
) -> Option<String> {
    if new.iter().all(|(_, _, s)| s.is_none()) {
        return None;
    }

    let mut all: Vec<Section> = old
        .and_then(parse)
        .unwrap_or_default()
        .into_iter()
        .filter(|s| !new.iter().any(|(n, _, _)| *n == s.name))
        .collect();
    all.extend(
        new.iter()
            .filter_map(|(n, failed, s)| Some(section(n, *failed, sha, s.as_deref()?))),
    );
    all.sort_by_key(|s| (!s.failed, s.name.clone()));

    let short = |s: &str| s[..12.min(s.len())].to_string();
    let mut body = format!(
        "{MARKER}\n## shipcheck reports\n\nWhat each job wrote to `$GITHUB_STEP_SUMMARY`. Latest post: commit `{}`, tree `{}`.\n",
        short(sha),
        short(tree)
    );

    for s in all {
        let text = if body.chars().count() + s.text.chars().count() > BUDGET {
            format!("### {}\n\nLeft out, the comment is full.\n", s.name)
        } else {
            s.text
        };
        body.push_str(&format!("\n{}\n{text}{}\n", start(&s.name), end(&s.name)));
    }

    Some(body)
}

/// Writes the comment on `sha`'s PRs with the App's own key, `prs` already checked to be into the base and not a fork's
pub fn post_with_app(
    app: &github::App,
    repo: &str,
    prs: Option<&[u64]>,
    base: &str,
    sha: &str,
    entry: &Entry,
    secrets: &[String],
) -> Result<Vec<String>, String> {
    let new = sections(sha, entry, secrets);

    if new.iter().all(|(_, _, s)| s.is_none()) {
        return Ok(Vec::new());
    }

    let token = app.comment_token(repo)?;
    let prs = match prs {
        Some(prs) => prs.to_vec(),
        None => github::prs_for(&token, repo, sha, base)?,
    };
    let mut urls = Vec::new();

    for pr in prs {
        let found = github::find_comment(&token, repo, pr, app.id(), MARKER)?;
        let old = found.as_ref().map(|(_, b)| b.as_str());

        if let Some(body) = merge(old, sha, &entry.tree, &new) {
            urls.push(github::put_comment(
                &token,
                repo,
                pr,
                found.map(|(id, _)| id),
                &body,
            )?);
        }
    }

    Ok(urls)
}

/// Writes the comment through the hosted service, on the PRs into `base` it finds for the commit
pub fn post_with_service(
    token: &str,
    repo: &str,
    base: &str,
    sha: &str,
    entry: &Entry,
    secrets: &[String],
) -> Result<Vec<String>, String> {
    let new = sections(sha, entry, secrets);

    if new.iter().all(|(_, _, s)| s.is_none()) {
        return Ok(Vec::new());
    }

    let mut urls = Vec::new();

    for (pr, pr_base, old) in service::find_comments(token, repo, sha)? {
        if pr_base != base {
            continue;
        }

        if let Some(body) = merge(old.as_deref(), sha, &entry.tree, &new) {
            urls.push(service::post_comment(token, repo, sha, pr, &body)?);
        }
    }

    Ok(urls)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::run::WorkflowResult;

    fn entry(jobs: &[(&str, &str, Option<&str>)]) -> Entry {
        Entry {
            key: "k".into(),
            sha: "a".repeat(40),
            tree: "b".repeat(40),
            base: "c".into(),
            passed: jobs.iter().all(|(_, s, _)| *s == "success"),
            workflows: vec![WorkflowResult {
                file: "ci.yml".into(),
                exit: Some(0),
                attempts: 1,
                log_sha256: "x".into(),
                jobs: jobs
                    .iter()
                    .map(|(j, s, _)| (j.to_string(), s.to_string()))
                    .collect(),
                passed: jobs.iter().all(|(_, s, _)| *s == "success"),
                shards: BTreeMap::new(),
                times: BTreeMap::new(),
                summaries: jobs
                    .iter()
                    .filter_map(|(j, _, s)| Some((j.to_string(), (*s)?.to_string())))
                    .collect(),
            }],
            finished_unix: 0,
            ci_changes: Vec::new(),
            skipped: None,
        }
    }

    fn post(old: Option<&str>, e: &Entry) -> Option<String> {
        merge(
            old,
            &e.sha,
            &e.tree,
            &sections(&e.sha, e, &["hunter2".into()]),
        )
    }

    #[test]
    fn failed_jobs_first_masked_and_marked() {
        let e = entry(&[
            ("lint", "success", Some("| a | b |\n|---|---|\n| 1 | 2 |")),
            (
                "test",
                "failure",
                Some("coverage 80% hunter2\n<!-- shipcheck:end:shipcheck/test -->"),
            ),
            ("docs", "success", None),
        ]);
        let body = post(None, &e).unwrap();
        assert!(body.starts_with(MARKER));
        assert!(body.contains("commit `aaaaaaaaaaaa`, tree `bbbbbbbbbbbb`"));
        assert!(
            body.find("### shipcheck/test: failed").unwrap()
                < body.find("### shipcheck/lint: passed").unwrap()
        );
        assert!(body.contains("coverage 80% ***"));
        assert!(!body.contains("hunter2"));
        assert!(!body.contains("shipcheck/docs"));
        assert_eq!(
            body.matches("<!-- shipcheck:end:shipcheck/test -->")
                .count(),
            1
        );
        let parsed = parse(&body).unwrap();
        assert_eq!(parsed.len(), 2);
        assert!(parsed[0].failed && !parsed[1].failed);
        // Parsing and writing again changes nothing
        assert_eq!(post(Some(&body), &e).unwrap(), body);
    }

    #[test]
    fn no_summary_posts_nothing() {
        assert_eq!(post(None, &entry(&[("test", "success", None)])), None);
        assert_eq!(
            post(Some("anything"), &entry(&[("test", "success", None)])),
            None
        );
    }

    #[test]
    fn two_steps_append_in_one_section() {
        // act and the host jail both hand over a job's steps joined in step order
        let body = post(None, &entry(&[("test", "success", Some("first\nsecond"))])).unwrap();
        assert!(
            body.contains(
                "on `aaaaaaaaaaaa`\n\nfirst\nsecond\n<!-- shipcheck:end:shipcheck/test -->"
            )
        );
    }

    #[test]
    fn a_later_post_changes_only_the_jobs_it_ran() {
        let first = post(
            None,
            &entry(&[
                ("lint", "success", Some("lint report")),
                ("test", "success", Some("old")),
            ]),
        )
        .unwrap();
        // One job ran again, as with `shipcheck run --job test`
        let mut again = entry(&[("test", "failure", Some("new"))]);
        again.sha = "d".repeat(40);
        let second = post(Some(&first), &again).unwrap();
        assert!(second.contains("lint report"));
        assert!(second.contains("### shipcheck/test: failed on `dddddddddddd`\n\nnew"));
        assert!(!second.contains("old"));
        assert_eq!(parse(&second).unwrap().len(), 2);
        // A job that ran again with no summary loses its section, the others stay
        let third = post(
            Some(&second),
            &entry(&[("test", "success", None), ("x", "success", Some("y"))]),
        )
        .unwrap();
        assert!(!third.contains("shipcheck/test"));
        assert!(third.contains("lint report"));
    }

    #[test]
    fn a_broken_body_is_rewritten() {
        let e = entry(&[("test", "success", Some("fresh"))]);
        let good = post(None, &entry(&[("lint", "success", Some("kept"))])).unwrap();
        assert!(post(Some(&good), &e).unwrap().contains("kept"));

        let broken = [
            good.replace("<!-- shipcheck:end:shipcheck/lint -->", ""),
            good.replace("<!-- shipcheck:job:shipcheck/lint -->", ""),
            good.replace(MARKER, "edited"),
            format!("{good}\ntext someone added"),
            format!("{good}\n{}", good.trim_start_matches(MARKER)),
        ];

        for old in broken {
            assert!(parse(&old).is_none(), "{old}");
            let body = post(Some(&old), &e).unwrap();
            assert!(!body.contains("kept"));
            assert_eq!(parse(&body).unwrap().len(), 1);
        }
    }

    #[test]
    fn stays_under_the_github_limit() {
        let big = "x".repeat(19_000);
        let jobs: Vec<(String, &str, Option<&str>)> = (0..6)
            .map(|i| (format!("j{i}"), "success", Some(big.as_str())))
            .collect();
        let jobs: Vec<(&str, &str, Option<&str>)> =
            jobs.iter().map(|(j, s, b)| (j.as_str(), *s, *b)).collect();
        let body = post(None, &entry(&jobs)).unwrap();
        assert!(body.chars().count() <= 65_536);
        assert!(body.contains("Left out, the comment is full."));
        assert_eq!(parse(&body).unwrap().len(), 6);
    }
}
