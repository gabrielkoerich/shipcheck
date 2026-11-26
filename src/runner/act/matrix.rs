use crate::workflow::key;
use std::collections::BTreeMap;
use yaml_rust2::Yaml;
use yaml_rust2::yaml::Hash;

pub(super) fn matches(combo: &Hash, filter: &Hash) -> bool {
    filter.iter().all(|(k, v)| combo.get(k) == Some(v))
}

/// The combinations of a static matrix by GitHub's rules, or None when part of it is an expression
pub(super) fn legs(matrix: &Hash) -> Option<Vec<Hash>> {
    let (include, exclude) = (key("include"), key("exclude"));
    let mut combos = vec![Hash::new()];
    let mut axes = 0;

    for (k, v) in matrix
        .iter()
        .filter(|(k, _)| **k != include && **k != exclude)
    {
        let Yaml::Array(values) = v else {
            return None;
        };
        axes += 1;
        combos = combos
            .into_iter()
            .flat_map(|c| {
                values.iter().map(move |val| {
                    let mut c = c.clone();
                    c.insert(k.clone(), val.clone());
                    c
                })
            })
            .collect();
    }

    if axes == 0 {
        combos.clear();
    }

    match matrix.get(&exclude) {
        Some(Yaml::Array(filters)) => {
            let filters: Option<Vec<&Hash>> = filters.iter().map(|f| f.as_hash()).collect();
            combos.retain(|c| {
                !filters
                    .as_ref()
                    .is_some_and(|fs| fs.iter().any(|f| matches(c, f)))
            });
        }
        Some(_) => {
            return None;
        }
        None => {}
    }

    let originals = combos.clone();

    match matrix.get(&include) {
        Some(Yaml::Array(entries)) => {
            for entry in entries {
                let entry = entry.as_hash()?;
                let mut added = false;

                // An entry extends every original combination whose own values it does not overwrite
                for (combo, original) in combos.iter_mut().zip(&originals) {
                    if entry
                        .iter()
                        .all(|(k, v)| original.get(k).is_none_or(|o| o == v))
                    {
                        combo.extend(entry.iter().map(|(k, v)| (k.clone(), v.clone())));
                        added = true;
                    }
                }

                if !added {
                    combos.push(entry.clone());
                }
            }
        }
        Some(_) => {
            return None;
        }
        None => {}
    }

    Some(combos)
}

pub(super) fn matrix_condition(value: &str, leg: &Hash) -> Option<bool> {
    let expr = value.trim().strip_prefix("${{")?.strip_suffix("}}")?.trim();
    let operand = |text: &str| -> Option<String> {
        let text = text.trim();

        if let Some(quoted) = text.strip_prefix('\'').and_then(|t| t.strip_suffix('\'')) {
            return Some(quoted.replace("''", "'"));
        }

        match leg.get(&key(text.strip_prefix("matrix.")?))? {
            Yaml::String(s) => Some(s.clone()),
            Yaml::Integer(n) => Some(n.to_string()),
            Yaml::Real(r) => Some(r.clone()),
            Yaml::Boolean(b) => Some(b.to_string()),
            _ => None,
        }
    };

    match expr {
        "true" => return Some(true),
        "false" => return Some(false),
        _ => {}
    }

    for (name, test) in [
        ("endsWith", {
            let ends: fn(&str, &str) -> bool = |a, b| a.ends_with(b);
            ends
        }),
        ("startsWith", |a, b| a.starts_with(b)),
        ("contains", |a, b| a.contains(b)),
    ] {
        if let Some(args) = expr
            .strip_prefix(name)
            .and_then(|r| r.trim().strip_prefix('('))
            .and_then(|r| r.strip_suffix(')'))
        {
            let (a, b) = args.split_once(',')?;
            return Some(test(&operand(a)?, &operand(b)?));
        }
    }

    if let Some((a, b)) = expr.split_once("!=") {
        return Some(operand(a)? != operand(b)?);
    }

    let (a, b) = expr.split_once("==")?;
    Some(operand(a)? == operand(b)?)
}

pub(super) fn resolve_matrix(value: &Yaml, leg: &Hash) -> Option<Yaml> {
    let inner = value
        .as_str()?
        .trim()
        .strip_prefix("${{")?
        .strip_suffix("}}")?
        .trim();

    let operands: Vec<&str> = inner.split("||").map(str::trim).collect();

    for (n, operand) in operands.iter().enumerate() {
        let last = n + 1 == operands.len();
        let value = match operand
            .strip_prefix('\'')
            .and_then(|t| t.strip_suffix('\''))
        {
            Some(quoted) if quoted.contains('\'') => return None,
            Some(quoted) => Some(key(quoted)),
            None => {
                let name = operand.strip_prefix("matrix.")?;

                if !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
                {
                    return None;
                }

                leg.get(&key(name)).cloned()
            }
        };
        let falsy = match &value {
            None | Some(Yaml::Null | Yaml::Boolean(false) | Yaml::Integer(0)) => true,
            Some(Yaml::String(s)) => s.is_empty(),
            _ => false,
        };

        // As in GitHub, `a || b` is `a` unless it is falsy, and the last operand whatever it is
        if last || !falsy {
            return value;
        }
    }

    None
}

pub(super) fn expand_matrices(jobs: Hash) -> Hash {
    let mut out = Hash::new();
    let mut renamed: BTreeMap<String, Vec<Yaml>> = BTreeMap::new();

    for (id, job) in jobs {
        let legs = job["strategy"]["matrix"]
            .as_hash()
            .and_then(legs)
            .filter(|l| !l.is_empty());
        let (Some(legs), Some(name)) = (legs, id.as_str().map(str::to_string)) else {
            out.insert(id, job);
            continue;
        };

        for (n, leg) in legs.into_iter().enumerate() {
            let mut leg_job = job.clone();
            let Yaml::Hash(h) = &mut leg_job else {
                continue;
            };

            // shipcheck reads `continue-on-error` itself to judge the workflow, so it must be literal too
            for field in ["runs-on", "continue-on-error", "container"] {
                if let Some(resolved) = h.get(&key(field)).and_then(|r| resolve_matrix(r, &leg)) {
                    h.insert(key(field), resolved);
                }
            }

            if let Some(allowed) = h
                .get(&key("continue-on-error"))
                .and_then(|v| v.as_str())
                .and_then(|v| matrix_condition(v, &leg))
            {
                h.insert(key("continue-on-error"), Yaml::Boolean(allowed));
            }

            if let Some(Yaml::Hash(strategy)) = h.get_mut(&key("strategy")) {
                strategy.insert(
                    key("matrix"),
                    Yaml::Hash(Hash::from_iter([(
                        key("include"),
                        Yaml::Array(vec![Yaml::Hash(leg)]),
                    )])),
                );
            }

            let leg_id = key(&format!("{name}__{}", n + 1));
            renamed
                .entry(name.clone())
                .or_default()
                .push(leg_id.clone());
            out.insert(leg_id, leg_job);
        }
    }

    for job in out.values_mut() {
        let Yaml::Hash(job) = job else {
            continue;
        };
        let rename = |n: &Yaml| match n.as_str().and_then(|s| renamed.get(s)) {
            Some(ids) => ids.clone(),
            None => vec![n.clone()],
        };
        let needs = match job.get(&key("needs")) {
            Some(Yaml::Array(list)) => Some(Yaml::Array(list.iter().flat_map(rename).collect())),
            Some(single @ Yaml::String(_)) => Some(Yaml::Array(rename(single))),
            _ => None,
        };

        if let Some(needs) = needs {
            job.insert(key("needs"), needs);
        }
    }

    out
}
