//! Tests of `pack_routing.rs`: which packs a goal calls for (move only: they were part of the inline module `tests` of `agent_tools.rs`; the path of every test is now `agent_tools::pack_routing_tests::<name>`).

use super::*;

/// Card R2-07: the two tools that read the screen are shown for a goal about the screen and for no other.
#[test]
fn a_goal_about_the_screen_brings_the_screen_tools_and_other_goals_do_not() {
    assert_eq!(pack_of("screen.ocr"), Some("screen"));
    assert_eq!(pack_of("screen.find_text"), Some("screen"));
    for goal in ["read the text on my screen", "what does the error on the screen say", "take a screenshot and tell me what it says", "find the word Save on screen"] {
        assert!(route(goal).contains(&"screen"), "{goal}: {:?}", route(goal));
    }
    for goal in ["save the file notes.txt to my disk", "what's the weather in Delhi", "open notepad and type hello", "list the open issues on my github repo"] {
        assert!(!route(goal).contains(&"screen"), "{goal}: {:?}", route(goal));
    }
}

#[test]
fn the_router_picks_at_most_two_packs_and_nothing_for_plain_desktop_goals() {
    assert_eq!(route("open notepad and type hello"), Vec::<&str>::new());
    assert_eq!(route("what's the weather in Delhi"), Vec::<&str>::new());
    assert_eq!(route("read the file notes.txt in my folder"), vec!["files"]);
    // A request to build or test code gets the files and terminal packs, and git when it names a repository.
    let r = route("git commit my changes then run the tests with cargo");
    assert_eq!(r, vec!["files", "terminal", "git"], "{r:?}");
    // The earlier rule is still there when the switch is off: the two packs the words pick.
    let old = route_with("git commit my changes then run the tests with cargo", false);
    assert!(old.len() == 2 && old.contains(&"git") && old.contains(&"terminal"), "{old:?}");
    assert_eq!(route("show the docker containers and kubernetes pods and the terraform plan").len(), 2);
    assert_eq!(route("check the logs for errors"), vec!["monitor"]);
    assert_eq!(route("list the open issues on my github repo"), vec!["forge", "git"]);
    assert_eq!(route("list the processes using memory"), vec!["system"]);
    assert!(route("summarise report.csv").contains(&"files"), "an extension pulls the files pack in");
    assert!(!route("dig into the catalogue").contains(&"git"), "words match whole words, not fragments");
}

/// Card R2-10: the research pack is shown for a goal that asks for research, and only with the switch on; with it off the words pick what they always picked.
#[test]
fn the_research_pack_takes_part_in_the_routing_only_with_the_switch_on() {
    assert_eq!(pack_of("research.run"), Some("research"));
    assert_eq!(route_by_words_with("do a deep research on solar panels", true), vec!["research"]);
    assert_eq!(route_by_words_with("do a deep research on solar panels", false), Vec::<&str>::new());
    assert_eq!(route_by_words_with("investigate the sources and fact-check the claim", true), vec!["research"]);
    assert_eq!(route_by_words_with("research the docker image", false), vec!["docker"]);
    let on = route_by_words_with("research the docker image", true);
    assert_eq!(on, vec!["docker", "research"], "the pack with more hits first, then the research pack");
    assert!(route_by_words_with("read the file notes.txt in my folder", true) == vec!["files"], "a goal that does not ask for research is routed as before");
}

#[test]
fn company_questions_route_to_the_company_graph_pack() {
    assert_eq!(pack_of("company.search_context"), Some("company"));
    assert_eq!(pack_of("company.explain_path"), Some("company"));
    assert_eq!(pack_of("company.propose_assignment"), Some("company"));
    assert_eq!(pack_of("company.assign_ticket"), Some("company"));
    assert_eq!(pack_of("company.record_outcome"), Some("company"));
    for goal in ["who owns the Identity project?", "why was this company decision made?", "which team is responsible for ticket INC-1042?"] {
        assert!(route(goal).contains(&"company"), "{goal}: {:?}", route(goal));
    }
    assert!(!route("what is the weather in Delhi").contains(&"company"));
}
