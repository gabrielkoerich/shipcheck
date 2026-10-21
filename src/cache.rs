use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

/// Everything that decides a run's result, and nothing else
pub struct KeyInputs<'a> {
    pub tree: &'a str,
    pub config: &'a [u8],
    pub workflows: &'a BTreeMap<String, Vec<u8>>,
    /// `act` or `host`
    pub runner_kind: &'a str,
    /// The `[host]` checks by name, empty under act
    pub checks: &'a BTreeMap<String, String>,
    /// `act --version`, or for host each `versions` command with its output
    pub runner_version: &'a str,
    pub shipcheck_version: &'a str,
    pub platforms: &'a BTreeMap<String, String>,
}

pub fn key(inputs: &KeyInputs) -> String {
    let mut h = Sha256::new();
    h.update(format!("tree\0{}\n", inputs.tree));
    h.update(format!("config\0{}\n", hex(&Sha256::digest(inputs.config))));

    for (path, body) in inputs.workflows {
        h.update(format!(
            "workflow\0{path}\0{}\n",
            hex(&Sha256::digest(body))
        ));
    }

    h.update(format!("kind\0{}\n", inputs.runner_kind));

    for (name, command) in inputs.checks {
        h.update(format!(
            "check\0{name}\0{}\n",
            hex(&Sha256::digest(command))
        ));
    }

    h.update(format!(
        "runner\0{}\n",
        hex(&Sha256::digest(inputs.runner_version))
    ));
    h.update(format!("shipcheck\0{}\n", inputs.shipcheck_version));

    for (label, image) in inputs.platforms {
        h.update(format!("platform\0{label}\0{image}\n"));
    }

    hex(&h.finalize())
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    static NO_CHECKS: BTreeMap<String, String> = BTreeMap::new();

    fn inputs<'a>(
        tree: &'a str,
        workflows: &'a BTreeMap<String, Vec<u8>>,
        platforms: &'a BTreeMap<String, String>,
    ) -> KeyInputs<'a> {
        KeyInputs {
            tree,
            config: b"required = [\"ci.yml\"]\n",
            workflows,
            runner_kind: "act",
            checks: &NO_CHECKS,
            runner_version: "act version 0.2.89",
            shipcheck_version: "0.1.0",
            platforms,
        }
    }

    fn fixtures() -> (BTreeMap<String, Vec<u8>>, BTreeMap<String, String>) {
        let workflows = BTreeMap::from([(
            ".github/workflows/ci.yml".to_string(),
            b"on: workflow_dispatch\n".to_vec(),
        )]);
        let platforms =
            BTreeMap::from([("ubuntu-latest".to_string(), "img@sha256:abc".to_string())]);
        (workflows, platforms)
    }

    #[test]
    fn fixed_inputs_give_a_fixed_key() {
        let (workflows, platforms) = fixtures();
        let k = key(&inputs(
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
            &workflows,
            &platforms,
        ));
        assert_eq!(
            k,
            "165071eb660ec49135fc0d918c87ecc187c2223304cfd826ac50f1987bfd4b3d"
        );
    }

    #[test]
    fn every_input_changes_the_key() {
        let (workflows, platforms) = fixtures();
        let base = key(&inputs("t1", &workflows, &platforms));
        assert_ne!(base, key(&inputs("t2", &workflows, &platforms)));

        let mut other_wf = workflows.clone();
        other_wf.insert(".github/workflows/ci.yml".into(), b"on: push\n".to_vec());
        assert_ne!(base, key(&inputs("t1", &other_wf, &platforms)));

        let mut other_pf = platforms.clone();
        other_pf.insert("ubuntu-latest".into(), "img@sha256:def".into());
        assert_ne!(base, key(&inputs("t1", &workflows, &other_pf)));

        let mut other = inputs("t1", &workflows, &platforms);
        other.config = b"required = []\n";
        assert_ne!(base, key(&other));

        let mut other = inputs("t1", &workflows, &platforms);
        other.runner_version = "act version 0.2.90";
        assert_ne!(base, key(&other));

        let mut other = inputs("t1", &workflows, &platforms);
        other.shipcheck_version = "0.2.0";
        assert_ne!(base, key(&other));

        let mut other = inputs("t1", &workflows, &platforms);
        other.runner_kind = "host";
        assert_ne!(base, key(&other));

        let checks = BTreeMap::from([("test".to_string(), "npm test".to_string())]);
        let mut with_check = inputs("t1", &workflows, &platforms);
        with_check.checks = &checks;
        assert_ne!(base, key(&with_check));
        let renamed = BTreeMap::from([("tests".to_string(), "npm test".to_string())]);
        let mut other = inputs("t1", &workflows, &platforms);
        other.checks = &renamed;
        assert_ne!(key(&with_check), key(&other));
        let changed = BTreeMap::from([("test".to_string(), "true".to_string())]);
        other.checks = &changed;
        assert_ne!(key(&with_check), key(&other));
    }
}
