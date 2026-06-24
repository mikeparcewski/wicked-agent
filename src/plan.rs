//! PLAN — deterministic decomposition of a free-text problem into ordered work units.
//!
//! Ported from the prototype's `planUnits` (`session.mjs`): split on newlines / sentence
//! terminators / semicolons, trim, drop blanks; fall back to a single unit. The plan is
//! wicked-agent's; ordering and gating belong to orchestration. DETERMINISTIC: the same problem
//! always yields the same ordered units (no randomness, no model).

use crate::WorkUnit;

/// Decompose `problem` into ordered [`WorkUnit`]s owned by `session_id`.
///
/// Unit ids are `<session_id>:u<ord>` (1-based) so they are stable and collision-free across
/// sessions. Splitting mirrors the Node prototype: `\n+`, a sentence terminator followed by
/// whitespace, or `;`.
pub fn plan_units(problem: &str, session_id: &str) -> Vec<WorkUnit> {
    let pieces: Vec<String> = split_problem(problem);
    let descriptions: Vec<String> = if pieces.is_empty() {
        let trimmed = problem.trim();
        vec![if trimmed.is_empty() { "unit".to_string() } else { trimmed.to_string() }]
    } else {
        pieces
    };

    descriptions
        .into_iter()
        .enumerate()
        .map(|(i, description)| {
            let ord = (i + 1) as u32;
            WorkUnit::pending(format!("{session_id}:u{ord}"), session_id, ord, description)
        })
        .collect()
}

/// Split a problem into trimmed, non-empty pieces on newlines, sentence terminators (`.`/`!`/`?`
/// followed by whitespace), or semicolons. A hand-rolled scanner (no regex dep): it walks the
/// string and cuts at any of those boundaries.
fn split_problem(problem: &str) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = problem.chars().collect();

    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\n' => {
                push_trimmed(&mut pieces, &mut current);
                // Collapse a run of newlines.
                while i + 1 < chars.len() && chars[i + 1] == '\n' {
                    i += 1;
                }
            }
            ';' => {
                push_trimmed(&mut pieces, &mut current);
                // Skip trailing whitespace after the semicolon.
                while i + 1 < chars.len() && chars[i + 1].is_whitespace() {
                    i += 1;
                }
            }
            '.' | '!' | '?' => {
                current.push(c);
                // Cut only when the terminator is followed by whitespace (so "3.5" stays whole).
                if i + 1 < chars.len() && chars[i + 1].is_whitespace() {
                    push_trimmed(&mut pieces, &mut current);
                    while i + 1 < chars.len() && chars[i + 1].is_whitespace() {
                        i += 1;
                    }
                }
            }
            _ => current.push(c),
        }
        i += 1;
    }
    push_trimmed(&mut pieces, &mut current);
    pieces
}

fn push_trimmed(pieces: &mut Vec<String>, current: &mut String) {
    let trimmed = current.trim();
    if !trimmed.is_empty() {
        pieces.push(trimmed.to_string());
    }
    current.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UnitStatus;

    #[test]
    fn splits_on_newlines_and_terminators_and_semicolons() {
        let units = plan_units("First task.\nSecond task; third task", "s1");
        assert_eq!(units.len(), 3);
        assert_eq!(units[0].description, "First task.");
        assert_eq!(units[1].description, "Second task");
        assert_eq!(units[2].description, "third task");
        // Ids are stable, 1-based, session-scoped.
        assert_eq!(units[0].id, "s1:u1");
        assert_eq!(units[2].id, "s1:u3");
        // ord matches.
        assert_eq!(units[0].ord, 1);
        assert_eq!(units[2].ord, 3);
        assert!(units.iter().all(|u| u.status == UnitStatus::Pending));
    }

    #[test]
    fn deterministic_same_input_same_units() {
        let a = plan_units("Do X; do Y", "s");
        let b = plan_units("Do X; do Y", "s");
        assert_eq!(a, b, "planning is deterministic");
    }

    #[test]
    fn empty_problem_falls_back_to_one_unit() {
        let units = plan_units("   ", "s");
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].description, "unit");
    }

    #[test]
    fn decimal_point_does_not_split() {
        // "3.5" must NOT split (terminator not followed by whitespace).
        let units = plan_units("Upgrade to version 3.5 now", "s");
        assert_eq!(units.len(), 1, "a decimal point mid-token is not a boundary");
    }
}
