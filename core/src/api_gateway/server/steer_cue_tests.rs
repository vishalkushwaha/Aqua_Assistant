use super::*;
use crate::flags::with_overrides;

/// Card OA10-03 (UX-11): a typed sentence that goes on from the task the person waits for is said to that task, instead of waiting its turn as a request of its own. Only with
/// `AQUA_STEER_CUES` on, only while a task is going, a line for after the task only for a task that was started detached (nothing would start one after another), and never an answer
/// to a question.
#[test]
fn a_sentence_that_goes_on_from_the_task_is_said_to_it_and_everything_else_is_a_request_as_it_was() {
    let id = "run_oa10_03_cue_plain";
    let text = "also add tests for the parser";
    // The setting is off: a request, as always.
    with_overrides(&[("AQUA_STEER_CUES", None)], || assert_eq!(steer_line("Hey Aqua", text, Some(id)), None));
    with_overrides(&[("AQUA_STEER_CUES", Some("on"))], || {
        // No task is going: a request.
        assert_eq!(steer_line("Hey Aqua", text, None), None);
        // A task is going: the sentence is added to it.
        assert_eq!(steer_line("Hey Aqua", text, Some(id)).as_deref(), Some("Okay, I added that to what I am doing."));
        assert_eq!(crate::steer::waiting_for(id).0, vec![text.to_string()]);
        // A line for after the task is for a task that was started detached; this one was not, so it is a request.
        assert_eq!(steer_line("Hey Aqua", "then run clippy", Some(id)), None);
        // An answer to a question, a stop and everything that is not a continuation are requests (a stop is handled before this is asked).
        for said in ["yes", "no", "approve it", "stop", "what time is it", "open notepad"] {
            assert_eq!(steer_line("Hey Aqua", said, Some(id)), None, "{said}");
        }
        assert_eq!(crate::steer::waiting_for(id).0.len(), 1, "nothing else was kept");
        let _ = crate::steer::take_run_end(id);
    });
}

/// Review fix (steering by cue reached a script's run): while a run that a script started with the API token held the turn, the person's typed "also ..." went into it and the script
/// could read it at `GET /api/v1/runs/{id}/queue`. Before the fix `steer_line` handed the sentence to whichever run was given, so the first two assertions below (`None` and nothing
/// kept) would have been `Some("Okay, I added that to what I am doing.")` and a kept line. A run the person's own app started, and one nobody marked, are steered as before.
#[test]
fn a_sentence_is_never_said_to_a_run_that_a_script_started_and_takes_its_turn_as_a_request_of_its_own() {
    let text = "also add tests for the parser";
    with_overrides(&[("AQUA_STEER_CUES", Some("on"))], || {
        // A run that is going on and was begun for the API token (what the agent routes do for it).
        let scripts = crate::agent_runs::with_origin(false, || crate::agent_runs::scope::RunScope::begin(None));
        assert_eq!(crate::agent_runs::scope::origin_of_run(scripts.id()), "api");
        assert_eq!(steer_line("Hey Aqua", text, Some(scripts.id())), None, "a request of its own, which waits its turn");
        assert_eq!(steer_line("Hey Aqua", "then run clippy", Some(scripts.id())), None);
        assert_eq!(crate::steer::waiting_for(scripts.id()), (Vec::new(), Vec::new()), "nothing was kept for the script to read");
        // The same sentence, for the run of the person's own app: steered.
        let persons = crate::agent_runs::with_origin(true, || crate::agent_runs::scope::RunScope::begin(None));
        assert_eq!(crate::agent_runs::scope::origin_of_run(persons.id()), "ui");
        assert_eq!(steer_line("Hey Aqua", text, Some(persons.id())).as_deref(), Some("Okay, I added that to what I am doing."));
        assert_eq!(crate::steer::waiting_for(persons.id()).0, vec![text.to_string()]);
        // A run nobody marked (the voice orb, a workflow) is the person's task as it was.
        let unmarked = crate::agent_runs::scope::RunScope::begin(None);
        assert_eq!(crate::agent_runs::scope::origin_of_run(unmarked.id()), "");
        assert_eq!(steer_line("Hey Aqua", text, Some(unmarked.id())).as_deref(), Some("Okay, I added that to what I am doing."));
        let (scripts_id, persons_id, unmarked_id) = (scripts.id().to_string(), persons.id().to_string(), unmarked.id().to_string());
        drop(unmarked);
        drop(persons);
        drop(scripts);
        assert_eq!(crate::agent_runs::scope::origin_of_run(&scripts_id), "", "a run that is over is not remembered");
        for id in [&scripts_id, &persons_id, &unmarked_id] {
            let _ = crate::steer::take_run_end(id);
        }
    });
    // A detached run that a script started and that still waits for its turn is the registry's to describe: the same.
    let registered = super::super::run_host::register("api", "a script's long task", None, "", None).expect("a place in the line");
    with_overrides(&[("AQUA_STEER_CUES", Some("on"))], || {
        assert_eq!(steer_line("Hey Aqua", "Hey Aqua, then run clippy", Some(registered.id.as_str())), None);
        assert_eq!(steer_line("Hey Aqua", text, Some(registered.id.as_str())), None);
    });
    assert_eq!(crate::steer::waiting_for(&registered.id), (Vec::new(), Vec::new()));
    let _ = super::super::run_host::interrupt(&registered.id);
    super::super::run_host::forget_for_test(&registered.id);
}

#[test]
fn a_line_for_after_the_task_is_kept_for_a_task_that_was_started_detached() {
    let registered = super::super::run_host::register("ui", "a long task", None, "", None).expect("a place in the line");
    let id = registered.id.as_str();
    with_overrides(&[("AQUA_STEER_CUES", Some("on"))], || {
        assert_eq!(steer_line("Hey Aqua", "Hey Aqua, then run clippy", Some(id)).as_deref(), Some("Okay, I will do that when this is done."));
        assert_eq!(crate::steer::waiting_for(id).1, vec!["run clippy".to_string()], "the cue is not part of what is started later");
    });
    let _ = super::super::run_host::interrupt(id);
    assert_eq!(crate::steer::waiting_for(id), (Vec::new(), Vec::new()), "a run that is ended lets go of what was kept for after it");
    super::super::run_host::forget_for_test(id);
}
