use std::fs;
use std::path::PathBuf;

use crate::workflow::Job;

/// A Ruby a version manager installed
#[derive(Debug, Clone, PartialEq)]
pub struct Ruby {
    /// `ruby`, `jruby` or `truffleruby`
    pub engine: String,
    pub version: String,
    /// The directory with `bin/ruby`, which a check puts first on its `PATH`
    pub root: PathBuf,
}

/// The Rubies mise, rbenv, asdf and chruby installed, in the directories a check's jail can read
#[cfg(not(test))]
pub fn installed() -> Vec<Ruby> {
    let user = crate::run::user_dir();
    let var = |name: &str, default: &str| {
        std::env::var_os(name)
            .map(PathBuf::from)
            .unwrap_or_else(|| user.join(default))
    };
    from_dirs(&[
        var("MISE_DATA_DIR", ".local/share/mise").join("installs/ruby"),
        var("RBENV_ROOT", ".rbenv").join("versions"),
        var("ASDF_DATA_DIR", ".asdf").join("installs/ruby"),
        user.join(".rubies"),
        PathBuf::from("/opt/rubies"),
    ])
}

fn from_dirs(dirs: &[PathBuf]) -> Vec<Ruby> {
    let mut found: Vec<Ruby> = Vec::new();

    for entry in dirs
        .iter()
        .filter_map(|d| fs::read_dir(d).ok())
        .flatten()
        .flatten()
    {
        let Ok(root) = entry.path().canonicalize() else {
            continue;
        };
        let plain = root.to_str().is_some_and(|r| {
            r.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_./+".contains(c))
        });

        if !plain || !root.join("bin/ruby").is_file() || found.iter().any(|r| r.root == root) {
            continue;
        }

        let (engine, version) = split(&entry.file_name().to_string_lossy());
        found.push(Ruby {
            engine,
            version,
            root,
        });
    }

    found
}

/// `jruby-9.4.5.0` as its engine and version, a plain `3.4.1` or `ruby-3.4.1` as CRuby's
fn split(name: &str) -> (String, String) {
    let name = name.trim();

    for engine in ["jruby", "truffleruby", "ruby"] {
        if name == engine {
            return (engine.into(), String::new());
        }

        if let Some(v) = name.strip_prefix(engine).and_then(|v| v.strip_prefix('-')) {
            return (engine.into(), v.into());
        }
    }

    ("ruby".into(), name.into())
}

fn parts(version: &str) -> Vec<u64> {
    version.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

/// Whether `version` is `wanted` or one of its patch releases, so `3.4` takes `3.4.1` but not `3.41`
fn takes(wanted: &str, version: &str) -> bool {
    wanted.is_empty() || version == wanted || version.starts_with(&format!("{wanted}."))
}

/// The Ruby a job's check runs with, and a note when it differs from the workflow's
pub struct Pick<'a> {
    pub ruby: Option<&'a Ruby>,
    pub note: Option<String>,
}

/**
 * The Ruby `ruby/setup-ruby` sets up for a job: its `ruby-version`, or `.ruby-version` when that is
 * unset or `default`. A version manager's install that matches is pinned. Else a CRuby on the
 * `PATH` stands in, with a note when its version differs, and an error keeps the job on GitHub
 * when this machine has no such Ruby. `ruby` is the latest stable, which any CRuby stands in for
 **/
pub fn pick<'a>(
    job: &Job,
    file: Option<&str>,
    on_path: Option<&str>,
    rubies: &'a [Ruby],
) -> Result<Pick<'a>, String> {
    let none = Pick {
        ruby: None,
        note: None,
    };
    let Some(step) = job.steps.iter().find(|s| {
        s.uses
            .as_deref()
            .and_then(|u| u.split('@').next())
            .is_some_and(|a| a == "ruby/setup-ruby")
    }) else {
        return Ok(none);
    };
    let wanted = match step.with.iter().find(|(k, _)| k == "ruby-version") {
        Some((_, None)) => return Err("needs a Ruby from an expression".into()),
        Some((_, Some(v))) if v.trim() != "default" => v.trim(),
        _ => match file {
            Some(f) => f.trim(),
            None => return Ok(none),
        },
    };
    let (engine, version) = split(wanted);

    if engine == "ruby" && version.is_empty() {
        return Ok(none);
    }

    let cruby = engine == "ruby" && version.chars().all(|c| c.is_ascii_digit() || c == '.');

    if cruby && on_path.is_some_and(|p| takes(&version, p)) {
        return Ok(none);
    }

    let managed = rubies
        .iter()
        .filter(|r| r.engine == engine && takes(&version, &r.version))
        .max_by_key(|r| parts(&r.version));

    if managed.is_some() {
        return Ok(Pick {
            ruby: managed,
            note: None,
        });
    }

    let name = match engine.as_str() {
        "ruby" => format!("Ruby {version}"),
        _ => format!("{engine} {version}"),
    };
    let name = name.trim_end();

    match on_path {
        Some(here) if cruby => Ok(Pick {
            ruby: None,
            note: Some(format!(
                "runs on Ruby {here}, which differs from the {name} its workflow sets up"
            )),
        }),
        _ => Err(format!("needs {name}, which this machine does not have")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn job(with: &str) -> Job {
        let ci = format!(
            "on: push\njobs:\n  test:\n    steps:\n      - uses: ruby/setup-ruby@v1\n{with}      - run: bundle exec rake\n"
        );
        crate::workflow::job_runs(&ci).remove(0)
    }

    fn version(v: &str) -> Job {
        job(&format!("        with:\n          ruby-version: {v}\n"))
    }

    #[test]
    fn finds_the_rubies_version_managers_installed() {
        let dir = tempfile::tempdir().unwrap();

        for name in ["3.4.1", "jruby-9.4.5.0", "ruby-2.7.8", "broken"] {
            let bin = dir.path().join(name).join("bin");
            fs::create_dir_all(&bin).unwrap();

            if name != "broken" {
                fs::write(bin.join("ruby"), "").unwrap();
            }
        }

        let mut got: Vec<(String, String)> = from_dirs(&[dir.path().to_path_buf()])
            .into_iter()
            .map(|r| (r.engine, r.version))
            .collect();
        got.sort();
        assert_eq!(
            got,
            [
                ("jruby".into(), "9.4.5.0".into()),
                ("ruby".into(), "2.7.8".into()),
                ("ruby".into(), "3.4.1".into()),
            ]
        );
    }

    #[test]
    fn picks_the_workflows_ruby_and_warns_when_only_another_runs() {
        let ruby = |engine: &str, version: &str| Ruby {
            engine: engine.into(),
            version: version.into(),
            root: Path::new("/r").join(version),
        };
        let rubies = [
            ruby("ruby", "2.7.1"),
            ruby("ruby", "2.7.8"),
            ruby("jruby", "9.4.5.0"),
        ];
        let pin = |job: &Job, file: Option<&str>, on_path: Option<&str>| {
            pick(job, file, on_path, &rubies).map(|p| (p.ruby.map(|r| r.version.clone()), p.note))
        };
        let here = Some("4.0.9");

        // The newest matching install, and none when the PATH's matches
        assert_eq!(
            pin(&version("2.7"), None, here),
            Ok((Some("2.7.8".into()), None))
        );
        assert_eq!(pin(&version("'4.0'"), None, here), Ok((None, None)));
        assert_eq!(pin(&version("ruby"), None, here), Ok((None, None)));
        assert_eq!(
            pin(&version("jruby"), None, here),
            Ok((Some("9.4.5.0".into()), None))
        );
        assert_eq!(
            pin(&job(""), Some("ruby-2.7.1\n"), here),
            Ok((Some("2.7.1".into()), None))
        );
        assert_eq!(pin(&job(""), None, here), Ok((None, None)));

        // `3.4` is not `3.41`, and a CRuby that differs runs with a note
        assert_eq!(
            pick(&version("3.4"), None, Some("3.41.0"), &rubies).map(|p| p.note),
            Ok(Some(
                "runs on Ruby 3.41.0, which differs from the Ruby 3.4 its workflow sets up".into()
            ))
        );

        // No such Ruby at all keeps the job on GitHub
        assert_eq!(
            pick(&version("truffleruby"), None, here, &rubies).map(|_| ()),
            Err("needs truffleruby, which this machine does not have".into())
        );
        assert_eq!(
            pick(&version("3.3"), None, None, &[]).map(|_| ()),
            Err("needs Ruby 3.3, which this machine does not have".into())
        );
        assert_eq!(
            pick(&version("head"), None, here, &rubies).map(|_| ()),
            Err("needs Ruby head, which this machine does not have".into())
        );
        assert_eq!(
            pick(&version("${{ matrix.ruby }}"), None, here, &rubies).map(|_| ()),
            Err("needs a Ruby from an expression".into())
        );
    }
}
