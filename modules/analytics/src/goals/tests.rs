//! Unit tests of the goal matching, validation and funnel arithmetic.
//!
//! These are the parts that cannot be wrong in production and cannot be checked by looking at a
//! screen: a pattern that matches one page too many, a step that can be reached out of order, or
//! a funnel whose counts are not monotone would all render as a plausible chart.

use std::collections::{BTreeSet, HashMap};

use super::*;

/// A single-step goal description.
fn changes(kind: &str, matches: GoalMatch) -> GoalChanges {
    GoalChanges {
        name: "Signup".to_owned(),
        kind: kind.to_owned(),
        matches,
        enabled: true,
        steps: Vec::new(),
    }
}

/// A download fact for one file.
fn download<'a>(file: &'a str, path: &'a str) -> Fact<'a> {
    Fact {
        kind: "download",
        path: Some(path),
        name: None,
        file: Some(file),
        value: Some(12.0),
    }
}

#[test]
fn a_page_pattern_is_the_page_itself_or_a_glob() {
    assert!(path_matches("/pricing", "/pricing"));
    assert!(path_matches("/pricing", "/pricing?utm_source=x"));
    assert!(!path_matches("/pricing", "/pricing-2"));
    assert!(!path_matches("/pricing", "/prices"));

    assert!(path_matches("/docs/*", "/docs/getting-started"));
    assert!(path_matches("/docs/*", "/docs/"));
    assert!(!path_matches("/docs/*", "/blog/getting-started"));
    assert!(path_matches("/*/checkout", "/shop/checkout"));
    assert!(glob_matches("a*c", "abbbc"));
    assert!(glob_matches("*", "/anything"));
    assert!(!glob_matches("a*c", "abbb"));
}

#[test]
fn validation_refuses_a_step_that_matches_nothing() {
    let missing = |kind: &str| validate(&changes(kind, GoalMatch::default())).unwrap_err();
    assert!(matches!(
        missing("pageview"),
        AnalyticsError::InvalidGoal(_)
    ));
    assert!(matches!(missing("event"), AnalyticsError::InvalidGoal(_)));
    assert!(matches!(
        missing("download"),
        AnalyticsError::InvalidGoal(_)
    ));
    assert!(matches!(
        missing("form_submit"),
        AnalyticsError::InvalidGoal(_)
    ));

    // A download matches on its file alone; a form on its name alone.
    assert!(
        validate(&changes(
            "download",
            GoalMatch {
                file: Some("/files/guide.pdf".to_owned()),
                ..GoalMatch::default()
            }
        ))
        .is_ok()
    );
    assert!(
        validate(&changes(
            "form_submit",
            GoalMatch {
                name: Some("contact".to_owned()),
                ..GoalMatch::default()
            }
        ))
        .is_ok()
    );
}

#[test]
fn validation_refuses_an_unknown_kind_a_blank_name_and_a_six_step_funnel() {
    assert!(matches!(
        validate(&changes("cart", GoalMatch::default())),
        Err(AnalyticsError::InvalidGoal(_))
    ));

    let mut blank = changes(
        "pageview",
        GoalMatch {
            path: Some("/pricing".to_owned()),
            ..GoalMatch::default()
        },
    );
    blank.name = "   ".to_owned();
    assert!(matches!(
        validate(&blank),
        Err(AnalyticsError::InvalidGoal(_))
    ));

    let step = StepChanges {
        kind: "pageview".to_owned(),
        matches: GoalMatch {
            path: Some("/pricing".to_owned()),
            ..GoalMatch::default()
        },
    };
    let mut long = blank.clone();
    long.name = "Funnel".to_owned();
    long.steps = vec![step; MAX_STEPS + 1];
    assert!(matches!(
        validate(&long),
        Err(AnalyticsError::InvalidGoal(_))
    ));
}

#[test]
fn a_goal_without_steps_is_one_step_and_one_with_steps_mirrors_its_last() {
    let single = validate(&changes(
        "pageview",
        GoalMatch {
            path: Some("  /pricing  ".to_owned()),
            ..GoalMatch::default()
        },
    ))
    .expect("a pageview goal with a path is valid");
    assert_eq!(single.steps.len(), 1);
    assert_eq!(single.steps[0].matches.path.as_deref(), Some("/pricing"));

    let funnel = validate(&GoalChanges {
        name: "Trial".to_owned(),
        kind: "event".to_owned(),
        matches: GoalMatch {
            name: Some("trial_started".to_owned()),
            ..GoalMatch::default()
        },
        enabled: true,
        steps: vec![
            StepChanges {
                kind: "pageview".to_owned(),
                matches: GoalMatch {
                    path: Some("/pricing".to_owned()),
                    ..GoalMatch::default()
                },
            },
            StepChanges {
                kind: "download".to_owned(),
                matches: GoalMatch {
                    file: Some("/files/guide.pdf".to_owned()),
                    ..GoalMatch::default()
                },
            },
            StepChanges {
                kind: "event".to_owned(),
                matches: GoalMatch {
                    name: Some("trial_started".to_owned()),
                    ..GoalMatch::default()
                },
            },
        ],
    })
    .expect("a three-step funnel is valid");
    assert_eq!(funnel.steps.len(), 3);
    assert_eq!(last_step(&funnel).kind, "event");
}

#[test]
fn a_fact_only_matches_its_own_kind() {
    let page = Fact {
        kind: "pageview",
        path: Some("/pricing"),
        name: None,
        file: None,
        value: None,
    };
    let page_goal = GoalMatch {
        path: Some("/pricing".to_owned()),
        ..GoalMatch::default()
    };
    assert!(fact_matches("pageview", &page_goal, &page));
    assert!(!fact_matches(
        "pageview",
        &page_goal,
        &download("/x.pdf", "/pricing")
    ));

    // An event goal needs its name; an optional path narrows it.
    let event = Fact {
        kind: "event",
        path: Some("/pricing"),
        name: Some("signup"),
        file: None,
        value: Some(9.0),
    };
    assert!(fact_matches(
        "event",
        &GoalMatch {
            name: Some("signup".to_owned()),
            ..GoalMatch::default()
        },
        &event
    ));
    assert!(!fact_matches(
        "event",
        &GoalMatch {
            name: Some("signup".to_owned()),
            path: Some("/blog/*".to_owned()),
            ..GoalMatch::default()
        },
        &event
    ));
    assert!(!fact_matches(
        "event",
        &GoalMatch {
            name: Some("other".to_owned()),
            ..GoalMatch::default()
        },
        &event
    ));
}

#[test]
fn a_download_matches_on_the_file_with_the_page_as_an_optional_narrowing() {
    let fact = download("/files/guide.pdf", "/pricing");

    assert!(fact_matches(
        "download",
        &GoalMatch {
            file: Some("/files/*.pdf".to_owned()),
            ..GoalMatch::default()
        },
        &fact
    ));
    assert!(fact_matches(
        "download",
        &GoalMatch {
            file: Some("/files/*.pdf".to_owned()),
            path: Some("/pricing".to_owned()),
            ..GoalMatch::default()
        },
        &fact
    ));
    assert!(!fact_matches(
        "download",
        &GoalMatch {
            file: Some("/files/*.zip".to_owned()),
            ..GoalMatch::default()
        },
        &fact
    ));
    assert!(!fact_matches(
        "download",
        &GoalMatch {
            file: Some("/files/*.pdf".to_owned()),
            path: Some("/docs".to_owned()),
            ..GoalMatch::default()
        },
        &fact
    ));
}

#[test]
fn a_form_goal_matches_on_the_form_name() {
    let fact = Fact {
        kind: "form_submit",
        path: Some("/contact"),
        name: Some("contact"),
        file: None,
        value: None,
    };
    assert!(fact_matches(
        "form_submit",
        &GoalMatch {
            name: Some("contact".to_owned()),
            ..GoalMatch::default()
        },
        &fact
    ));
    assert!(!fact_matches(
        "form_submit",
        &GoalMatch {
            name: Some("newsletter".to_owned()),
            ..GoalMatch::default()
        },
        &fact
    ));
    // A form goal may match on the page alone, for forms the tracker cannot name.
    assert!(fact_matches(
        "form_submit",
        &GoalMatch {
            path: Some("/contact".to_owned()),
            ..GoalMatch::default()
        },
        &fact
    ));
}

#[test]
fn progress_advances_to_the_earliest_missing_step_only() {
    let mut hits = BTreeSet::new();
    assert_eq!(eligible_step(&hits, 3), Some(1));

    hits.insert(1);
    assert_eq!(eligible_step(&hits, 3), Some(2));

    hits.insert(2);
    hits.insert(3);
    assert_eq!(eligible_step(&hits, 3), None);

    // A hole in the middle is the next step, whatever else was reached.
    let mut holed = BTreeSet::new();
    holed.insert(2);
    assert_eq!(eligible_step(&holed, 3), Some(1));
}

#[test]
fn the_funnel_counts_are_cumulative_and_monotone() {
    // Five visitors stopped at step 1, three at step 2, two completed.
    let reached = reached_from_furthest(&[5, 3, 2]);
    assert_eq!(reached, vec![10, 5, 2]);

    let mut drops = Vec::new();
    for index in 0..reached.len() {
        let before = if index == 0 {
            reached[0]
        } else {
            reached[index - 1]
        };
        drops.push(before - reached[index]);
    }
    assert_eq!(drops, vec![0, 5, 3]);
    assert!(reached.windows(2).all(|pair| pair[0] >= pair[1]));
}

#[test]
fn a_furthest_position_beyond_the_funnel_counts_at_its_end() {
    let mut by_position = HashMap::new();
    by_position.insert(1, 2i64);
    by_position.insert(3, 1i64);

    // The goal shrank to two steps: the visitor who was at step 3 reached the end.
    let vector = furthest_vector(&by_position, 2);
    assert_eq!(vector, vec![2, 1]);
    assert_eq!(reached_from_furthest(&vector), vec![3, 1]);
}

#[test]
fn a_rate_without_a_denominator_is_not_measured() {
    assert_eq!(ratio(0, 0), None);
    assert_eq!(ratio(1, 0), None);
    assert_eq!(ratio(1, 4), Some(0.25));
    assert_eq!(ratio(9, 4), Some(2.25));
}
