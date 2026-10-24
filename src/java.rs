use std::fs;
use std::path::{Path, PathBuf};

use crate::workflow::Job;

/// A JDK this machine has
#[derive(Debug, Clone, PartialEq)]
pub struct Jdk {
    /// As its `release` file names it, with JDK 8's `1.8.0_242` read as `8.0.242`
    pub version: String,
    pub home: PathBuf,
    /// It has `native-image`, as a GraalVM has
    pub native: bool,
}

/// The JDKs this machine has outside the user's home, which a check's jail cannot read
#[cfg(not(test))]
pub fn installed() -> Vec<Jdk> {
    let mut homes: Vec<PathBuf> = Vec::new();

    // `java_home -V` lists each JDK on stderr, with its home last on the line
    if cfg!(target_os = "macos")
        && let Ok(out) = std::process::Command::new("/usr/libexec/java_home")
            .arg("-V")
            .output()
    {
        homes.extend(
            String::from_utf8_lossy(&out.stderr)
                .lines()
                .filter_map(|l| l.split_whitespace().last())
                .filter(|w| w.starts_with('/'))
                .map(PathBuf::from),
        );
    }

    homes.extend(std::env::var_os("JAVA_HOME").map(PathBuf::from));

    if let Ok(dir) = fs::read_dir("/usr/lib/jvm") {
        homes.extend(dir.flatten().map(|e| e.path()));
    }

    from_homes(&homes, &crate::run::user_dir())
}

// ponytail: a home with a space or a quote is left out, so its path can be written into a check as is
fn from_homes(homes: &[PathBuf], user: &Path) -> Vec<Jdk> {
    let mut found: Vec<Jdk> = Vec::new();

    for home in homes {
        let Ok(home) = home.canonicalize() else {
            continue;
        };
        let plain = home.to_str().is_some_and(|h| {
            h.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_./+".contains(c))
        });

        if !plain || home.starts_with(user) || found.iter().any(|j| j.home == home) {
            continue;
        }

        let Ok(release) = fs::read_to_string(home.join("release")) else {
            continue;
        };
        let version = release.lines().find_map(|l| {
            l.strip_prefix("JAVA_VERSION=")
                .map(|v| v.trim().trim_matches('"').to_string())
        });

        if let Some(version) = version.filter(|_| home.join("bin/java").exists()) {
            found.push(Jdk {
                version: normal(&version),
                native: home.join("bin/native-image").exists(),
                home,
            });
        }
    }

    found
}

/// `1.8.0_242` as `8.0.242`, the form `setup-java` takes
fn normal(version: &str) -> String {
    let v = version.replace('_', ".");

    match v.strip_prefix("1.") {
        Some(rest) if rest.contains('.') => rest.to_string(),
        _ => v,
    }
}

/// The numbers of a version, up to its first part that is not one, such as `-ea`
fn parts(version: &str) -> Vec<u32> {
    version
        .split('.')
        .map_while(|p| {
            let digits: String = p.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        })
        .collect()
}

/// One JDK a `setup-java` or `setup-graalvm` step installs
struct Need {
    version: String,
    native: bool,
}

fn needs(job: &Job) -> Result<Vec<Need>, String> {
    let mut out = Vec::new();

    for step in &job.steps {
        let Some(action) = step.uses.as_deref().and_then(|u| u.split('@').next()) else {
            continue;
        };
        let graal = match action {
            "actions/setup-java" => false,
            "graalvm/setup-graalvm" => true,
            _ => continue,
        };
        let set = |name: &str| {
            step.with
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_deref())
        };
        let native = graal || set("distribution") == Some(Some("graalvm"));
        let versions = match set("java-version") {
            Some(Some(v)) => v,
            Some(None) => return Err("needs a JDK from an expression".into()),
            None if set("java-version-file").is_some() => {
                return Err("needs the JDK its java-version-file names".into());
            }
            None => return Err("sets up a JDK without a version".into()),
        };

        for v in versions
            .split(['\n', ','])
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            let numeric = v.split('.').all(|p| p.parse::<u32>().is_ok());

            if !numeric {
                return Err(format!("needs JDK {v}, a version shipcheck does not read"));
            }

            out.push(Need {
                version: v.to_string(),
                native,
            });
        }
    }

    Ok(out)
}

/**
 * The JDK a job's check runs with: the newest match of the last version its setup steps install,
 * which is the one `setup-java` leaves in `JAVA_HOME`. None for a job that sets up no JDK, an error
 * naming a JDK this machine does not have. A version matches by its parts, so `21` takes `21.0.5`
 **/
// ponytail: `distribution` other than graalvm is not matched, a vendor check if a build needs one
pub fn pick<'a>(job: &Job, jdks: &'a [Jdk]) -> Result<Option<&'a Jdk>, String> {
    let mut last = None;

    for need in needs(job)? {
        let want = parts(&need.version);
        let found = jdks
            .iter()
            .filter(|j| !need.native || j.native)
            .filter(|j| parts(&j.version).starts_with(&want))
            .max_by_key(|j| parts(&j.version));

        match found {
            Some(j) => last = Some(j),
            None if need.native => {
                return Err(format!(
                    "needs GraalVM {} with native-image, which this machine does not have",
                    need.version
                ));
            }
            None => {
                return Err(format!(
                    "needs JDK {}, which this machine does not have",
                    need.version
                ));
            }
        }
    }

    Ok(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jdk(dir: &Path, name: &str, version: &str, native: bool) -> PathBuf {
        let home = dir.join(name);
        fs::create_dir_all(home.join("bin")).unwrap();
        fs::write(home.join("bin/java"), "").unwrap();

        if native {
            fs::write(home.join("bin/native-image"), "").unwrap();
        }

        fs::write(
            home.join("release"),
            format!("IMPLEMENTOR=\"x\"\nJAVA_VERSION=\"{version}\"\n"),
        )
        .unwrap();
        home
    }

    fn job(steps: &str) -> Job {
        let ci = format!(
            "on: push\njobs:\n  t:\n    runs-on: ubuntu-latest\n    steps:\n{steps}      - run: ./gradlew check\n"
        );
        crate::workflow::job_runs(&ci).remove(0)
    }

    #[test]
    fn reads_jdk_homes_and_leaves_out_the_users() {
        let dir = tempfile::tempdir().unwrap();
        let user = tempfile::tempdir().unwrap();
        let homes = [
            jdk(dir.path(), "8", "1.8.0_242", false),
            jdk(dir.path(), "graal", "25.0.1", true),
            jdk(user.path(), "mine", "21.0.5", false),
            dir.path().join("missing"),
        ];
        let found = from_homes(&homes, &user.path().canonicalize().unwrap());
        let got: Vec<(&str, bool)> = found
            .iter()
            .map(|j| (j.version.as_str(), j.native))
            .collect();
        assert_eq!(got, [("8.0.242", false), ("25.0.1", true)]);
    }

    #[test]
    fn picks_the_last_jdk_and_names_a_missing_one() {
        let dir = tempfile::tempdir().unwrap();
        let jdks = [
            Jdk {
                version: "11.0.2".into(),
                home: dir.path().join("11"),
                native: false,
            },
            Jdk {
                version: "21.0.1".into(),
                home: dir.path().join("21.0.1"),
                native: false,
            },
            Jdk {
                version: "21.0.5".into(),
                home: dir.path().join("21.0.5"),
                native: false,
            },
            Jdk {
                version: "25".into(),
                home: dir.path().join("graal"),
                native: true,
            },
        ];
        let two = job(
            "      - uses: actions/setup-java@v5\n        with:\n          distribution: temurin\n          java-version: |\n            11\n            21\n",
        );
        assert_eq!(
            pick(&two, &jdks).unwrap().unwrap().home,
            dir.path().join("21.0.5")
        );

        let graal = job(
            "      - uses: actions/setup-java@v5\n        with:\n          java-version: 21\n      - uses: graalvm/setup-graalvm@v1\n        with:\n          java-version: 25\n",
        );
        assert_eq!(
            pick(&graal, &jdks).unwrap().unwrap().home,
            dir.path().join("graal")
        );
        assert_eq!(
            pick(&graal, &jdks[..3]).unwrap_err(),
            "needs GraalVM 25 with native-image, which this machine does not have"
        );

        for (steps, why) in [
            (
                "      - uses: actions/setup-java@v5\n        with:\n          java-version: 17\n",
                "needs JDK 17, which this machine does not have",
            ),
            (
                "      - uses: actions/setup-java@v5\n        with:\n          java-version: 25-ea\n",
                "needs JDK 25-ea, a version shipcheck does not read",
            ),
            (
                "      - uses: actions/setup-java@v5\n        with:\n          java-version: ${{ matrix.java }}\n",
                "needs a JDK from an expression",
            ),
            (
                "      - uses: actions/setup-java@v5\n        with:\n          java-version-file: .java-version\n",
                "needs the JDK its java-version-file names",
            ),
        ] {
            assert_eq!(pick(&job(steps), &jdks).unwrap_err(), why);
        }

        assert_eq!(pick(&job(""), &jdks), Ok(None));
    }
}
