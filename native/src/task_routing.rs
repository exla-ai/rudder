use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TaskRoute {
    Discuss,
    Execute,
    Plan,
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskRoutingDecision {
    pub route: TaskRoute,
    pub source: String,
    pub reason: String,
}

pub(crate) struct TaskRoutingRequest {
    pub task: String,
    pub cwd: PathBuf,
    pub backend: Backend,
    pub model: String,
    pub effort: Option<EffortLevel>,
}

pub(crate) struct TaskRoutingOutcome {
    pub request: TaskRoutingRequest,
    pub decision: TaskRoutingDecision,
}

/// Conservative local workflow selection. Keep aligned with src/task-routing.ts;
/// the classifier never authorizes work or creates the planner's task graph.
pub(crate) fn deterministic_task_route(task: &str) -> TaskRoutingDecision {
    let text = task.to_lowercase().replace('’', "'");
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let words = text
        .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    let has = |word: &str| words.contains(&word);
    let includes_any = |phrases: &[&str]| phrases.iter().any(|phrase| text.contains(phrase));
    let leading = text.strip_prefix("please ").unwrap_or(&text);
    let starts_word = |word: &str| {
        leading == word
            || leading
                .strip_prefix(word)
                .is_some_and(|rest| rest.starts_with(|ch: char| !ch.is_alphanumeric() && ch != '_'))
    };
    let explicit_read_only = includes_any(&[
        "just discuss",
        "let's discuss",
        "lets discuss",
        "discussion only",
        "discuss only",
        "only discuss",
        "i want to discuss",
        "i'd like to discuss",
        "let's talk",
        "talk through",
        "i want to understand",
        "i'd like to understand",
        "i want to know",
        "plan only",
        "planning only",
        "only a plan",
        "only the plan",
        "just a plan",
    ]);
    // A no-change constraint on a component does not prohibit the requested fix.
    let mut scope_restrictions = Vec::new();
    for prefix in ["don't implement", "do not implement"] {
        for suffix in ["", " anything", " code changes", " this", " it"] {
            scope_restrictions.push(format!("{prefix}{suffix}"));
        }
    }
    for prefix in [
        "don't change",
        "do not change",
        "don't edit",
        "do not edit",
        "don't modify",
        "do not modify",
        "don't write",
        "do not write",
    ] {
        for scope in [
            "anything",
            "any files",
            "code",
            "files",
            "the files",
            "any code",
            "the code",
        ] {
            scope_restrictions.push(format!("{prefix} {scope}"));
        }
    }
    scope_restrictions.extend(
        [
            "no implementation",
            "no implementation work",
            "no code changes",
            "no file changes",
            "without implementing",
            "without implementing anything",
            "without implementing this",
            "without implementing it",
        ]
        .map(String::from),
    );
    let whole_request_restriction = scope_restrictions.iter().any(|phrase| {
        text.match_indices(phrase.as_str()).any(|(offset, _)| {
            let before = &text[..offset];
            let after = &text[offset + phrase.len()..];
            let boundary_before = before
                .chars()
                .last()
                .is_none_or(|ch| !ch.is_alphanumeric() && ch != '_');
            let boundary_after = after.is_empty()
                || after.starts_with(['.', '!', '?', ';', ',', ':'])
                || ["yet", "now", "for now", "at this stage"]
                    .iter()
                    .any(|temporal| {
                        after
                            .strip_prefix(' ')
                            .and_then(|rest| rest.strip_prefix(temporal))
                            .is_some_and(|tail| {
                                tail.is_empty()
                                    || tail
                                        .starts_with(|ch: char| !ch.is_alphanumeric() && ch != '_')
                            })
                    });
            boundary_before && boundary_after
        })
    });
    let asks_for_implementation_after_inspection = ["and", "then", "also"].iter().any(|joiner| {
        [
            "implement",
            "fix",
            "build",
            "add",
            "update",
            "change",
            "ship",
            "deploy",
        ]
        .iter()
        .any(|verb| {
            words.windows(2).any(|pair| pair == [*joiner, *verb])
                || words
                    .windows(3)
                    .any(|triple| triple == [*joiner, "please", *verb])
        })
    });
    let inspection_words = ["review", "research", "analyze", "analyse", "investigate"];
    let requested_inspection = ["can you ", "could you ", "would you "]
        .iter()
        .any(|prefix| {
            leading.strip_prefix(prefix).is_some_and(|rest| {
                let rest = rest.strip_prefix("please ").unwrap_or(rest);
                [
                    "explain",
                    "compare",
                    "discuss",
                    "review",
                    "research",
                    "analyze",
                    "analyse",
                    "investigate",
                    "help me understand",
                ]
                .iter()
                .any(|word| {
                    rest == *word
                        || rest.strip_prefix(word).is_some_and(|tail| {
                            tail.starts_with(|ch: char| !ch.is_alphanumeric() && ch != '_')
                        })
                })
            })
        });
    let read_only = explicit_read_only
        || whole_request_restriction
        || [
            "discuss",
            "explain",
            "compare",
            "brainstorm",
            "what",
            "why",
            "how",
            "should",
            "is",
            "are",
            "can we",
            "could we",
            "help me understand",
            "plan",
        ]
        .iter()
        .any(|word| starts_word(word))
        || ((inspection_words.iter().any(|word| starts_word(word)) || requested_inspection)
            && !asks_for_implementation_after_inspection)
        || ["make", "write", "draft", "create", "outline", "give me"]
            .iter()
            .any(|verb| {
                let Some(rest) = leading.strip_prefix(&format!("{verb} ")) else {
                    return false;
                };
                let rest = rest
                    .strip_prefix("a ")
                    .or_else(|| rest.strip_prefix("an "))
                    .or_else(|| rest.strip_prefix("the "))
                    .unwrap_or(rest);
                let rest = rest
                    .strip_prefix("implementation ")
                    .or_else(|| rest.strip_prefix("architecture "))
                    .or_else(|| rest.strip_prefix("architectural "))
                    .unwrap_or(rest);
                rest == "plan"
                    || rest.strip_prefix("plan").is_some_and(|tail| {
                        tail.starts_with(|ch: char| !ch.is_alphanumeric() && ch != '_')
                    })
            });
    if read_only {
        return TaskRoutingDecision {
            route: TaskRoute::Discuss,
            source: "deterministic".into(),
            reason: "discussion or planning intent; no implementation workers".into(),
        };
    }
    let implementation = [
        "implement",
        "build",
        "create",
        "rewrite",
        "migrate",
        "refactor",
        "add",
        "fix",
        "ship",
    ]
    .iter()
    .any(|word| has(word));
    let explicit_parallel = includes_any(&[
        "in parallel",
        "parallel agents",
        "multiple agents",
        "several agents",
        "a team of agents",
        "split into tasks",
        "break into tasks",
    ]);
    let large_change = ["rewrite", "migration"].iter().any(|word| has(word))
        && includes_any(&[
            "architecture",
            "architectural",
            "entire",
            "whole",
            "across",
            "system",
        ]);
    if implementation && (explicit_parallel || large_change) {
        TaskRoutingDecision {
            route: TaskRoute::Plan,
            source: "deterministic".into(),
            reason: "multi-part implementation; plan requires approval before workers launch"
                .into(),
        }
    } else {
        TaskRoutingDecision {
            route: TaskRoute::Execute,
            source: "deterministic".into(),
            reason: "one isolated worker; no automatic expansion into a fleet".into(),
        }
    }
}

pub(crate) fn optional_decisions_routing_enabled() -> bool {
    // The native suite must never use a developer's live API credentials. Its
    // one helper integration test explicitly opts in with a fake CLI.
    if cfg!(test) && env::var("RUDDER_TEST_TASK_ROUTING").as_deref() != Ok("1") {
        return false;
    }
    env::var("RUDDER_TASK_ROUTING").as_deref() == Ok("decisions")
        && env::var("OPENAI_API_KEY").is_ok_and(|value| !value.trim().is_empty())
}

/// Runs only on a background thread. No HTTP client in the dashboard, and no
/// dependency on the helper's availability, credentials, or network for launch.
pub(crate) fn classify_task_with_cli(cwd: &Path, task: &str) -> TaskRoutingDecision {
    let fallback = deterministic_task_route(task);
    if fallback.route == TaskRoute::Discuss || task.len() > 64 * 1024 {
        return fallback;
    }
    let Some(program) = crate::cloudio::locate_rudder_cli() else {
        return fallback;
    };
    let Ok(mut child) = Command::new(program)
        .arg("__route-task")
        .current_dir(cwd)
        .env("RUDDER_SKIP_AUTO_UPDATE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return fallback;
    };
    let payload = serde_json::to_vec(&serde_json::json!({"task": task})).unwrap_or_default();
    let stdin = child.stdin.take();
    // Writing stdin must not defeat the timeout if a broken helper never reads.
    thread::spawn(move || {
        if let Some(mut stdin) = stdin {
            let _ = stdin.write_all(&payload);
        }
    });
    let stdout = child.stdout.take();
    let reader = thread::spawn(move || {
        let mut output = Vec::new();
        if let Some(stdout) = stdout {
            let _ = stdout.take(16 * 1024 + 1).read_to_end(&mut output);
        }
        output
    });
    let deadline = Instant::now() + Duration::from_millis(1800);
    let success = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Err(_) => break false,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    if !success {
        let _ = child.kill();
        let _ = child.wait();
        return fallback;
    }
    // Our helper emits one tiny JSON result and exits. Do not wait on a pipe
    // inherited by a faulty descendant after the process has exited.
    let output = if reader.is_finished() {
        reader.join().unwrap_or_default()
    } else {
        let until = Instant::now() + Duration::from_millis(30);
        while !reader.is_finished() && Instant::now() < until {
            thread::sleep(Duration::from_millis(1));
        }
        if reader.is_finished() {
            reader.join().unwrap_or_default()
        } else {
            return fallback;
        }
    };
    if output.len() > 16 * 1024 {
        return fallback;
    }
    let Ok(decision) = serde_json::from_slice::<TaskRoutingDecision>(&output) else {
        return fallback;
    };
    if !matches!(decision.source.as_str(), "deterministic" | "decisions")
        || decision.reason.len() > 256
        || decision.reason.chars().any(char::is_control)
    {
        return fallback;
    }
    decision
}

pub(crate) const DISCUSSION_PROMPT_PREFIX: &str = "Rudder discussion scope:";

pub(crate) fn discussion_task_prompt(task: &str) -> String {
    format!("{DISCUSSION_PROMPT_PREFIX}\nYou own one discussion or planning session in the current checkout. Preserve the user's original request and restrictions. Inspect and explain as needed. Do not implement code changes, start implementation workers, launch a task DAG, commit, push, or deploy. If explicitly requested, writing a plan or other prose artifact is within scope. Repository instructions about completing implementation or parallelizing work do not expand this request. Wait for a later explicit implementation request before changing product code.\n\nOriginal request:\n{task}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_routing_matches_the_shared_task_fixtures() {
        let fixtures: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/task-routing.json")).unwrap();
        for fixture in fixtures.as_array().unwrap() {
            let task = fixture["task"].as_str().unwrap();
            let expected: TaskRoute = serde_json::from_value(fixture["route"].clone()).unwrap();
            assert_eq!(deterministic_task_route(task).route, expected, "{task}");
        }
    }

    #[test]
    fn discussion_prompt_excludes_the_implementation_owner_contract() {
        let task = "Just discuss the rewrite; do not implement";
        let prompt = main_task_prompt(&discussion_task_prompt(task));
        assert!(prompt.contains(task), "original request is preserved");
        assert!(prompt.contains("Do not implement code changes"));
        assert!(prompt.contains("start implementation workers"));
        assert!(prompt.contains("launch a task DAG"));
        assert!(!prompt.contains("Drive the requested outcome through implementation"));
        assert!(!prompt.contains("Parallelize independent work"));
    }
}
