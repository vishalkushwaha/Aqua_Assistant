//! `ScriptedApprover`: the person who answers the tool host's questions, as a script.
//!
//! It answers every approval the same way and remembers what it was asked, so a test can say "the host asked once, about this call" or
//! "the host asked nobody". (A test that needs different answers for different questions makes its own `Approver`: this one is the
//! common case.)

use aqua_tool_host::{ApprovalOutcome, ApprovalRequest, ApprovalScope, Approver, ProviderFuture};
use std::sync::Mutex;

pub struct ScriptedApprover {
    pub outcome: ApprovalOutcome,
    /// Every question it was asked, in order.
    pub asked: Mutex<Vec<ApprovalRequest>>,
}

impl ScriptedApprover {
    pub fn answering(outcome: ApprovalOutcome) -> Self {
        Self { outcome, asked: Mutex::new(Vec::new()) }
    }

    /// Says no to everything.
    pub fn denying() -> Self {
        Self::answering(ApprovalOutcome::Denied)
    }

    /// Says yes, once, to everything.
    pub fn approving_once() -> Self {
        Self::answering(ApprovalOutcome::Approved(ApprovalScope::Once))
    }

    /// Yes or no, as a test's flag has it.
    pub fn approving(approve: bool) -> Self {
        if approve {
            Self::approving_once()
        } else {
            Self::denying()
        }
    }

    pub fn asked(&self) -> Vec<ApprovalRequest> {
        self.asked.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn asked_count(&self) -> usize {
        self.asked.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

impl Approver for ScriptedApprover {
    fn request(&self, req: ApprovalRequest) -> ProviderFuture<'_, ApprovalOutcome> {
        self.asked.lock().unwrap_or_else(|e| e.into_inner()).push(req);
        let outcome = self.outcome;
        Box::pin(async move { outcome })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question(tool: &str) -> ApprovalRequest {
        ApprovalRequest {
            id: "a1".into(),
            agent_run_id: String::new(),
            request_id: "r1".into(),
            session_id: "s1".into(),
            tool: tool.into(),
            summary: format!("run {tool}"),
            target: "t".into(),
            classes: vec![],
            digest: "d".into(),
            always_ask: false,
            expires_in_secs: 60,
            why: Default::default(),
            preview: None,
            run_grant: None,
        }
    }

    #[test]
    fn it_gives_the_same_answer_every_time_and_remembers_the_questions() {
        let yes = ScriptedApprover::approving(true);
        let no = ScriptedApprover::approving(false);
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        assert_eq!(rt.block_on(yes.request(question("fs.write"))), ApprovalOutcome::Approved(ApprovalScope::Once));
        assert_eq!(rt.block_on(yes.request(question("term.run"))), ApprovalOutcome::Approved(ApprovalScope::Once));
        assert_eq!(rt.block_on(no.request(question("fs.write"))), ApprovalOutcome::Denied);
        assert_eq!(yes.asked_count(), 2);
        assert_eq!(yes.asked()[1].tool, "term.run");
        assert_eq!(no.asked_count(), 1);
    }
}
