//! Linux native approval dialogs used by the installed desktop agent.
//!
//! Interactive Linux TUI sessions retain their in-terminal overlay, but the
//! background agent has no terminal and therefore routes prompts here.
//! Any backend error, dismissal, or unrecognized response maps to deny.

use super::{ApprovalRequest, HostdoApprovalRequest, Outcome};

const DENY: &str = "Deny";
const ALLOW: &str = "Allow";
const ALWAYS_ALLOW: &str = "Always allow";
const DENY_ONCE: &str = "Deny once";
const ALWAYS_DENY: &str = "Always deny";

pub fn prompt_network_approval(req: &ApprovalRequest) -> Outcome {
    let port = req.port.map(|port| format!(":{port}")).unwrap_or_default();
    let workspace = req
        .workspace
        .as_deref()
        .map(|workspace| format!("\nWorkspace: {workspace}"))
        .unwrap_or_default();
    approval_outcome(
        "Harness Hat: Network Approval",
        format!(
            "Allow this network request?\n{} {}{}{}{}",
            req.method, req.host, port, req.path, workspace
        ),
    )
}

pub fn prompt_hostdo_approval(req: &HostdoApprovalRequest) -> Outcome {
    let workspace = req
        .workspace
        .as_deref()
        .map(|workspace| format!("\nWorkspace: {workspace}"))
        .unwrap_or_default();
    let command = format!("\nCommand: {}", req.command);
    let reason = req
        .reason
        .as_deref()
        .map(|reason| format!("\nReason: {reason}"))
        .unwrap_or_default();
    approval_outcome(
        "Harness Hat: Host Command Approval",
        format!(
            "Allow this host command?\n{}{}{}",
            reason, command, workspace
        ),
    )
}

fn approval_outcome(title: &str, description: String) -> Outcome {
    let result = rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Warning)
        .set_title(title)
        .set_description(description)
        .set_buttons(rfd::MessageButtons::YesNoCancelCustom(
            ALLOW.to_string(),
            ALWAYS_ALLOW.to_string(),
            DENY.to_string(),
        ))
        .show();

    match selected_decision(&result) {
        Some(Decision::AllowOnce) => Outcome::Allow { remember: false },
        Some(Decision::AlwaysAllow) => Outcome::Allow { remember: true },
        Some(Decision::Deny) => denial_outcome(),
        None => Outcome::Deny { remember: false },
    }
}

fn denial_outcome() -> Outcome {
    let result = rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Warning)
        .set_title("Harness Hat: Remember denial?")
        .set_description(
            "Deny this request once, or always deny it by writing a narrow workspace rule?",
        )
        .set_buttons(rfd::MessageButtons::OkCancelCustom(
            DENY_ONCE.to_string(),
            ALWAYS_DENY.to_string(),
        ))
        .show();
    Outcome::Deny {
        remember: remembers_denial(&result),
    }
}

fn remembers_denial(result: &rfd::MessageDialogResult) -> bool {
    matches!(result, rfd::MessageDialogResult::Custom(label) if label == ALWAYS_DENY)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    AllowOnce,
    AlwaysAllow,
    Deny,
}

fn selected_decision(result: &rfd::MessageDialogResult) -> Option<Decision> {
    match result {
        rfd::MessageDialogResult::Custom(label) if label == ALLOW => Some(Decision::AllowOnce),
        rfd::MessageDialogResult::Custom(label) if label == ALWAYS_ALLOW => {
            Some(Decision::AlwaysAllow)
        }
        rfd::MessageDialogResult::Custom(label) if label == DENY => Some(Decision::Deny),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_decision_requires_an_explicit_button() {
        assert_eq!(
            selected_decision(&rfd::MessageDialogResult::Custom(ALLOW.to_string())),
            Some(Decision::AllowOnce)
        );
        assert_eq!(
            selected_decision(&rfd::MessageDialogResult::Custom(ALWAYS_ALLOW.to_string())),
            Some(Decision::AlwaysAllow)
        );
        assert_eq!(
            selected_decision(&rfd::MessageDialogResult::Custom(DENY.to_string())),
            Some(Decision::Deny)
        );
        assert_eq!(
            selected_decision(&rfd::MessageDialogResult::Custom("unexpected".to_string())),
            None
        );
        assert_eq!(selected_decision(&rfd::MessageDialogResult::Cancel), None);
    }

    #[test]
    fn always_deny_is_the_only_remembered_denial_choice() {
        let always_deny = rfd::MessageDialogResult::Custom(ALWAYS_DENY.to_string());
        let deny_once = rfd::MessageDialogResult::Custom(DENY_ONCE.to_string());
        assert!(remembers_denial(&always_deny));
        assert!(!remembers_denial(&deny_once));
        assert!(!remembers_denial(&rfd::MessageDialogResult::Cancel));
    }
}
