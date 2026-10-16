//! Ready-made flows. They are also the planner's starting points (Phase 2): it picks the closest
//! template and adapts it, which is far more reliable with a small local model than drawing a graph
//! from nothing.

use super::model::*;
use serde::Serialize;
use serde_json::json;

#[derive(Debug, Clone, Serialize)]
pub struct TemplateInfo {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
}

pub fn list() -> Vec<TemplateInfo> {
    vec![
        TemplateInfo { id: "blank", name: "Blank flow", description: "A trigger and nothing else." },
        TemplateInfo { id: "social_post", name: "AI social post", description: "Research a topic, write a post, make an image, ask you, publish, then offer to save to Excel." },
        TemplateInfo {
            id: "memory_dream",
            name: "Tidy my memory",
            description: "Looks through what you asked Aqua to remember for the same thing said twice and for dates that say tomorrow, and puts suggestions in a list for you to look at. It changes nothing by itself: schedule it for the night (`0 3 * * *`).",
        },
        TemplateInfo { id: "comment_watch", name: "Watch comments on my post", description: "Read the comments on a LinkedIn post, draft replies to the new ones, and tell you how many wait for your approval. Nothing is posted until you approve. Add a schedule to repeat it." },
    ]
}

pub fn build(id: &str) -> Option<FlowDef> {
    match id {
        "blank" => Some(blank()),
        "social_post" => Some(social_post()),
        "comment_watch" => Some(comment_watch()),
        "memory_dream" => Some(memory_dream()),
        _ => None,
    }
}

pub fn blank() -> FlowDef {
    FlowDef {
        name: "New flow".into(),
        nodes: vec![Node::new("start", NodeKind::Trigger, "Start").with_config(json!({ "trigger_type": "manual" })).at(80.0, 40.0)],
        ..Default::default()
    }
}

fn var(name: &str, label: &str, required: bool, default: Option<&str>) -> VariableDef {
    VariableDef { name: name.into(), label: label.into(), kind: VarKind::Text, required, default: default.map(|d| json!(d)), choices: vec![] }
}

/// The dream of the memory (card R6-36): one tool call that puts suggestions in the list the person reviews, then a line in the app saying how many. The tool changes no fact and
/// asks nothing, so a schedule may run it with nobody there. It is a template and not a flow made for the person: a job in the night is theirs to switch on.
pub fn memory_dream() -> FlowDef {
    use NodeKind::*;
    let col = 80.0;
    let nodes = vec![
        Node::new("start", Trigger, "Start").with_config(json!({ "trigger_type": "manual" })).at(col, 40.0),
        Node::new("tidy", Tool, "Look for things to tidy").with_config(json!({ "tool": "memory.consolidate", "arguments": { "mode": "suggest" } })).at(col, 150.0),
        Node::new("tell", Notification, "Tell you").with_config(json!({ "channel": "in_app", "message": "{{nodes.tidy.result}} They are in Settings, Memory." })).at(col, 260.0),
    ];
    let edges = vec![Edge::new("start", "tidy", "out"), Edge::new("tidy", "tell", "out")];
    FlowDef {
        name: "Tidy my memory".into(),
        description: "Suggests merging what says the same and writing real dates for \"tomorrow\"; you decide on each.".into(),
        nodes,
        edges,
        variables: vec![],
        settings: FlowSettings::default(),
    }
}

/// Research a topic, write a post, make an image, ask for approval, publish, save to Excel.
/// The platform, topic and tone are inputs, so the same flow serves LinkedIn, Instagram and the rest.
pub fn social_post() -> FlowDef {
    use NodeKind::*;
    let col = 80.0;
    let mut nodes = vec![
        Node::new("start", Trigger, "Start").with_config(json!({ "trigger_type": "manual" })).at(col, 40.0),
        Node::new("research", Research, "Research the topic").with_config(json!({ "topic": "{{vars.topic}}", "depth": "standard", "max_sources": 5 })).at(col, 150.0),
        Node::new("write_post", TextGeneration, "Write the post")
            .with_config(json!({ "prompt": "Write an engaging {{vars.platform}} post about {{vars.topic}}. Use only facts from this research: {{nodes.research.summary}}", "tone": "professional", "max_words": 200 }))
            .at(col, 260.0),
        Node::new("image_prompt", TextGeneration, "Write the image prompt")
            .with_config(json!({ "prompt": "Write one short prompt for a professional image to go with this post: {{nodes.write_post.text}}", "max_words": 40 }))
            .at(col, 370.0),
        Node::new("make_image", ImageGeneration, "Generate the image").with_config(json!({ "prompt": "{{nodes.image_prompt.text}}", "provider": "diffusion", "size": "1200x627" })).at(col, 480.0),
        Node::new("open_site", Website, "Open the platform").with_config(json!({ "url": "https://www.linkedin.com", "browser": "managed", "session": "linkedin-main" })).at(col, 590.0),
        Node::new("logged_in", Condition, "Signed in?").with_config(json!({ "expression": "{{nodes.open_site.logged_in}}" })).at(col, 700.0),
        Node::new("ask_login", UserInput, "Ask you to sign in")
            .with_config(json!({ "question": "Please sign in in the Aqua browser window, then answer yes.", "answer_kind": "yes_no" }))
            .at(col + 300.0, 700.0),
        Node::new("preview", Approval, "Approve the post")
            .with_config(json!({ "title": "Ready to publish", "preview": "{{nodes.write_post.text}}", "allow_edit": true }))
            .at(col, 820.0),
        {
            let mut n = Node::new("publish", SocialMedia, "Publish")
                .with_config(json!({ "platform": "linkedin", "action": "publish", "text": "{{nodes.write_post.text}}", "media": "{{nodes.make_image.image}}", "hashtags": "#AI #Automation", "mode": "auto" }))
                .at(col, 930.0);
            n.credential = Some("linkedin/main".into());
            n.retry = RetryPolicy { retries: 1, delay_secs: 30, backoff: Backoff::Exponential };
            n.on_error = OnError::AskUser;
            n
        },
        Node::new("ask_excel", UserInput, "Save to Excel?").with_config(json!({ "question": "Save this post to Excel?", "answer_kind": "yes_no" })).at(col, 1040.0),
        Node::new("want_excel", Condition, "Save it?").with_config(json!({ "expression": "{{nodes.ask_excel.answer}}" })).at(col, 1150.0),
        Node::new("save_row", Excel, "Save to Excel")
            .with_config(json!({
                "operation": "append_row", "path": "posts.xlsx", "sheet": "Posts",
                "columns": { "Platform": "{{vars.platform}}", "Post": "{{nodes.write_post.text}}", "Status": "{{nodes.publish.status}}", "URL": "{{nodes.publish.post_url}}", "Date": "{{now.date}}" }
            }))
            .at(col + 300.0, 1150.0),
        Node::new("ask_schedule", UserInput, "Schedule another?").with_config(json!({ "question": "Schedule another post?", "answer_kind": "yes_no" })).at(col, 1260.0),
    ];
    for n in &mut nodes {
        n.inputs = Vec::new();
    }
    let edges = vec![
        Edge::new("start", "research", "out"),
        Edge::new("research", "write_post", "out"),
        Edge::new("write_post", "image_prompt", "out"),
        Edge::new("image_prompt", "make_image", "out"),
        Edge::new("make_image", "open_site", "out"),
        Edge::new("open_site", "logged_in", "out"),
        Edge::new("logged_in", "preview", "true"),
        Edge::new("logged_in", "ask_login", "false"),
        Edge::new("ask_login", "preview", "out"),
        Edge::new("preview", "publish", "approved"),
        Edge::new("publish", "ask_excel", "out"),
        Edge::new("ask_excel", "want_excel", "out"),
        Edge::new("want_excel", "save_row", "true"),
        Edge::new("want_excel", "ask_schedule", "false"),
        Edge::new("save_row", "ask_schedule", "out"),
    ];
    let mut platform = var("platform", "Platform", true, Some("linkedin"));
    platform.kind = VarKind::Choice;
    platform.choices = super::registry::platforms().iter().map(|s| s.to_string()).collect();
    FlowDef {
        name: "AI social post".into(),
        description: "Research, write, illustrate, approve, publish, save.".into(),
        nodes,
        edges,
        variables: vec![var("topic", "Topic", true, None), platform],
        settings: FlowSettings::default(),
    }
}

/// Read the comments on one post, draft replies to the new ones, say how many are waiting. It only reads and drafts,
/// so it needs no approval and is safe to run on a schedule; replies are posted one at a time from the inbox.
pub fn comment_watch() -> FlowDef {
    use NodeKind::*;
    let col = 80.0;
    let nodes = vec![
        Node::new("start", Trigger, "Start").with_config(json!({ "trigger_type": "manual" })).at(col, 40.0),
        Node::new("read", SocialMedia, "Read the comments")
            .with_config(json!({ "platform": "linkedin", "action": "read_comments", "post_url": "{{vars.post_url}}", "my_name": "{{vars.my_name}}" }))
            .at(col, 150.0),
        Node::new("drafts", CommentReplies, "Draft replies to new comments")
            .with_config(json!({ "comments": "{{nodes.read.comments}}", "post_text": "{{vars.post_text}}", "post_url": "{{vars.post_url}}", "my_name": "{{vars.my_name}}", "facts": "{{vars.facts}}", "tone": "friendly and concise" }))
            .at(col, 260.0),
        Node::new("any", Condition, "Anything new?").with_config(json!({ "expression": "{{nodes.drafts.in_inbox}}" })).at(col, 370.0),
        Node::new("tell", Notification, "Tell you")
            .with_config(json!({ "channel": "in_app", "message": "{{nodes.drafts.in_inbox}} new reply draft(s) are waiting for your approval in AI Flows." }))
            .at(col, 480.0),
        Node::new("quiet", SetVariable, "Nothing new").with_config(json!({ "name": "new_drafts", "value": "0" })).at(col + 300.0, 480.0),
    ];
    let edges = vec![
        Edge::new("start", "read", "out"),
        Edge::new("read", "drafts", "out"),
        Edge::new("drafts", "any", "out"),
        Edge::new("any", "tell", "true"),
        Edge::new("any", "quiet", "false"),
    ];
    let mut post_text = var("post_text", "What the post says (the model may quote only this)", true, None);
    post_text.kind = VarKind::LongText;
    let mut facts = var("facts", "Facts about you the replies may use", false, Some(""));
    facts.kind = VarKind::LongText;
    FlowDef {
        name: "Watch comments on my post".into(),
        description: "Read comments, draft replies to the new ones, tell you; you approve each reply.".into(),
        nodes,
        edges,
        variables: vec![var("post_url", "Post address", true, None), var("my_name", "Your name on LinkedIn", true, None), post_text, facts],
        settings: FlowSettings::default(),
    }
}
