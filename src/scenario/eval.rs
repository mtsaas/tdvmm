//! Pure assertion / predicate evaluators: map a prepared expectation plus an
//! agent [`AgentLine`] to a `(passed, detail)` pair. No engine state and no I/O —
//! just the exec / containers / `until` predicates the engine calls to judge a
//! reply.

use std::collections::HashSet;

use tdvmm_proto::ContainerInfo;

use super::schema::{ContainersAssert, PreparedExpect, PreparedUntil};
use super::{truncate, AgentLine};

pub(super) fn eval_exec_assertion(expect: &PreparedExpect, reply: &AgentLine) -> (bool, String) {
    let exit = reply.exit.unwrap_or(-1);
    // Output matchers run against the TRIMMED stdout: command output (psql, curl,
    // ...) almost always carries a trailing newline, and Rust's `$` does not match
    // before one — so `^[0-9]+$` on "5\n" would surprise every author. Trimming
    // outer whitespace is the intuitive, documented convention.
    let stdout = reply.stdout.clone().unwrap_or_default();
    let out = stdout.trim();
    let mut ok = true;
    let mut notes: Vec<String> = Vec::new();

    if exit == expect.exit {
        notes.push(format!("exit={exit} ✓"));
    } else {
        ok = false;
        notes.push(format!("exit={exit} (want {}) ✗", expect.exit));
    }
    if let Some(re) = &expect.output_matches {
        if re.is_match(out) {
            notes.push(format!("output~=/{}/ ✓", re.as_str()));
        } else {
            ok = false;
            notes.push(format!(
                "output~=/{}/ ✗ (got {:?})",
                re.as_str(),
                truncate(out, 40)
            ));
        }
    }
    if let Some(sub) = &expect.output_contains {
        if out.contains(sub) {
            notes.push(format!("contains {sub:?} ✓"));
        } else {
            ok = false;
            notes.push(format!("contains {sub:?} ✗"));
        }
    }
    (ok, notes.join(", "))
}

pub(super) fn eval_containers_assertion(
    assert: ContainersAssert,
    reply: &AgentLine,
    expect_death: &HashSet<String>,
) -> (bool, String) {
    let empty = Vec::new();
    let list = reply.containers.as_ref().unwrap_or(&empty);
    match assert {
        ContainersAssert::AllRunning => {
            let bad: Vec<String> = list
                .iter()
                .filter(|c| c.state != "running")
                .map(|c| format!("{}={}", disp_name(c), c.state))
                .collect();
            if list.is_empty() {
                (false, "all_running ✗ (no containers)".into())
            } else if bad.is_empty() {
                (true, format!("all_running ✓ ({} containers)", list.len()))
            } else {
                (false, format!("all_running ✗ ({})", bad.join(", ")))
            }
        }
        ContainersAssert::NoneExitedNonzero => {
            // A container that exited nonzero is only a violation if its death was
            // NOT expected (TEST-1b): a deliberately killed/stopped service listed
            // in `expect_death` is exempt.
            let bad: Vec<String> = list
                .iter()
                .filter(|c| {
                    c.state == "exited" && c.exit_code != 0 && !expect_death.contains(&c.service)
                })
                .map(|c| format!("{}=exit{}", disp_name(c), c.exit_code))
                .collect();
            if bad.is_empty() {
                (true, format!("none_exited_nonzero ✓ ({} containers)", list.len()))
            } else {
                (false, format!("none_exited_nonzero ✗ ({})", bad.join(", ")))
            }
        }
    }
}

/// The first container that exited nonzero and is NOT in the expected-death set,
/// or `None` if every nonzero exit was expected. Backs the implicit end-of-run
/// census (TEST-1b expected-death policy).
pub(super) fn check_unexpected_deaths(
    list: &[ContainerInfo],
    expect_death: &HashSet<String>,
) -> Option<String> {
    list.iter()
        .find(|c| c.state == "exited" && c.exit_code != 0 && !expect_death.contains(&c.service))
        .map(|c| format!("{}=exit{}", disp_name(c), c.exit_code))
}

pub(super) fn eval_until(
    until: &PreparedUntil,
    reply: &AgentLine,
    expect_death: &HashSet<String>,
) -> (bool, String) {
    // A probe whose command could not run (ok:false) is simply "not ready yet".
    let ok = reply.ok == Some(true);
    match until {
        PreparedUntil::ExitZero => {
            let e = reply.exit.unwrap_or(-1);
            (ok && e == 0, format!("exit_zero (ok={ok} exit={e})"))
        }
        PreparedUntil::ExitNonzero => {
            let e = reply.exit.unwrap_or(-1);
            (ok && e != 0, format!("exit_nonzero (ok={ok} exit={e})"))
        }
        PreparedUntil::OutputMatches(re) => {
            let s = reply.stdout.clone().unwrap_or_default();
            (ok && re.is_match(s.trim()), format!("output_matches /{}/", re.as_str()))
        }
        PreparedUntil::OutputContains(sub) => {
            let s = reply.stdout.clone().unwrap_or_default();
            (ok && s.trim().contains(sub), format!("output_contains {sub:?}"))
        }
        PreparedUntil::AllRunning => {
            let (r, d) = eval_containers_assertion(ContainersAssert::AllRunning, reply, expect_death);
            (ok && r, d)
        }
        PreparedUntil::NoneExitedNonzero => {
            let (r, d) =
                eval_containers_assertion(ContainersAssert::NoneExitedNonzero, reply, expect_death);
            (ok && r, d)
        }
    }
}

fn disp_name(c: &ContainerInfo) -> String {
    if !c.service.is_empty() {
        c.service.clone()
    } else {
        c.name.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;

    #[test]
    fn exec_assertion_exit_and_regex() {
        let expect = PreparedExpect {
            exit: 0,
            output_matches: Some(Regex::new("^5$").unwrap()),
            output_contains: None,
        };
        let good = AgentLine {
            id: Some(1),
            ok: Some(true),
            exit: Some(0),
            stdout: Some("5\n".into()),
            ..Default::default()
        };
        let (p, _) = eval_exec_assertion(&expect, &good);
        assert!(p);
        let bad = AgentLine {
            id: Some(1),
            ok: Some(true),
            exit: Some(0),
            stdout: Some("6\n".into()),
            ..Default::default()
        };
        let (p, d) = eval_exec_assertion(&expect, &bad);
        assert!(!p, "{d}");
    }

    #[test]
    fn containers_all_running() {
        let reply = AgentLine {
            id: Some(1),
            ok: Some(true),
            containers: Some(vec![
                ContainerInfo { name: "a".into(), service: "postgres".into(), state: "running".into(), exit_code: 0, health: String::new() },
                ContainerInfo { name: "b".into(), service: "service".into(), state: "running".into(), exit_code: 0, health: String::new() },
            ]),
            ..Default::default()
        };
        let none = HashSet::new();
        assert!(eval_containers_assertion(ContainersAssert::AllRunning, &reply, &none).0);
        let reply2 = AgentLine {
            id: Some(1),
            ok: Some(true),
            containers: Some(vec![
                ContainerInfo { name: "b".into(), service: "service".into(), state: "exited".into(), exit_code: 1, health: String::new() },
            ]),
            ..Default::default()
        };
        assert!(!eval_containers_assertion(ContainersAssert::AllRunning, &reply2, &none).0);
        assert!(!eval_containers_assertion(ContainersAssert::NoneExitedNonzero, &reply2, &none).0);
        // But if `service`'s death is EXPECTED, none_exited_nonzero passes.
        let expect: HashSet<String> = ["service".to_string()].into_iter().collect();
        assert!(eval_containers_assertion(ContainersAssert::NoneExitedNonzero, &reply2, &expect).0);
    }
    #[test]
    fn check_unexpected_deaths_honors_expect_death() {
        let list = vec![
            ContainerInfo { name: "p".into(), service: "postgres".into(), state: "exited".into(), exit_code: 137, health: String::new() },
            ContainerInfo { name: "s".into(), service: "service".into(), state: "running".into(), exit_code: 0, health: String::new() },
        ];
        let none = HashSet::new();
        assert!(check_unexpected_deaths(&list, &none).is_some(), "unexpected death must be flagged");
        let expect: HashSet<String> = ["postgres".to_string()].into_iter().collect();
        assert!(check_unexpected_deaths(&list, &expect).is_none(), "expected death must be exempt");
    }
}
