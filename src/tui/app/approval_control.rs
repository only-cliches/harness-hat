use super::*;
use crate::server::{
    ApprovalAction, ApprovalActionResponse, ApprovalControlError, ApprovalControlItem,
    PendingApprovalRecord, PendingApprovalsResponse, RulesStatusResponse, RulesTrustResponse,
    RulesTrustTarget,
};

impl App {
    pub(crate) fn allocate_approval_id(&mut self) -> Option<String> {
        let pending_exec = &self.pending_exec;
        let pending_net = &self.pending_net;
        let base_rules_changed = &self.base_rules_changed;
        next_available_approval_id(&mut self.next_approval_id, |id| {
            pending_exec.iter().any(|item| item.id == id)
                || pending_net.iter().any(|item| item.approval_id == id)
                || base_rules_changed
                    .as_ref()
                    .is_some_and(|item| item.approval_id == id)
        })
    }

    pub(crate) fn drain_approval_control(&mut self) -> bool {
        let mut changed = false;
        for _ in 0..16 {
            match self.approval_control_rx.try_recv() {
                Ok(ApprovalControlItem::List { response_tx }) => {
                    let _ = response_tx.send(self.pending_approval_records());
                }
                Ok(ApprovalControlItem::Decide {
                    id,
                    action,
                    response_tx,
                }) => {
                    let result = self.decide_approval(&id, action);
                    changed |= result.is_ok();
                    let _ = response_tx.send(result);
                }
                Ok(ApprovalControlItem::RulesStatus {
                    workspace,
                    response_tx,
                }) => {
                    let result = self.rules_status(workspace.as_deref());
                    let _ = response_tx.send(result);
                }
                Ok(ApprovalControlItem::TrustRules {
                    target,
                    response_tx,
                }) => {
                    let result = self.trust_rules_target(target);
                    changed |= result.is_ok();
                    let _ = response_tx.send(result);
                }
                Err(_) => break,
            }
        }
        changed
    }

    fn pending_approval_records(&self) -> PendingApprovalsResponse {
        let mut approvals = Vec::new();
        approvals.extend(
            self.pending_exec
                .iter()
                .map(|item| PendingApprovalRecord::Hostdo {
                    id: item.id.clone(),
                    workspace: item.workspace_name.clone(),
                    argv: item.argv.clone(),
                    reason: item.reason.clone(),
                    cwd: item.cwd.display().to_string(),
                    image: item.image.clone(),
                    timeout_secs: item.timeout_secs,
                }),
        );
        approvals.extend(
            self.pending_net
                .iter()
                .map(|item| PendingApprovalRecord::Network {
                    id: item.approval_id.clone(),
                    workspace: item.source_workspace.clone(),
                    method: item.method.clone(),
                    host: item.host.clone(),
                    port: item.port,
                    path: item.path.clone(),
                }),
        );
        if let Some(item) = &self.base_rules_changed {
            approvals.push(PendingApprovalRecord::RulesChange {
                id: item.approval_id.clone(),
                path: item.path.display().to_string(),
            });
        }
        approvals.sort_by(|left, right| left.id().cmp(right.id()));
        PendingApprovalsResponse { approvals }
    }

    fn decide_approval(
        &mut self,
        raw_id: &str,
        action: ApprovalAction,
    ) -> std::result::Result<ApprovalActionResponse, ApprovalControlError> {
        let id = normalize_approval_id(raw_id)?;

        if let Some(idx) = self.pending_exec.iter().position(|item| item.id == id) {
            if action == ApprovalAction::Trust {
                return Err(incompatible(&id, "trust only applies to rules changes"));
            }
            if matches!(
                action,
                ApprovalAction::AllowOnce | ApprovalAction::AllowForever
            ) {
                let workspace = self.pending_exec[idx].workspace_name.clone();
                self.config
                    .ensure_rules_trusted_for_workspace(Some(&workspace))
                    .map_err(|error| ApprovalControlError {
                        code: "rules_blocked",
                        reason: error.to_string(),
                    })?;
            }
            match action {
                ApprovalAction::AllowOnce => self.approve_exec(idx, false),
                ApprovalAction::AllowForever => self.approve_exec(idx, true),
                ApprovalAction::DenyOnce => self.deny_exec(idx),
                ApprovalAction::DenyForever => self.deny_exec_forever(idx),
                ApprovalAction::Trust => unreachable!(),
            }
            return Ok(decision_response(id, action));
        }

        if let Some(idx) = self
            .pending_net
            .iter()
            .position(|item| item.approval_id == id)
        {
            if action == ApprovalAction::Trust {
                return Err(incompatible(&id, "trust only applies to rules changes"));
            }
            if matches!(
                action,
                ApprovalAction::AllowOnce | ApprovalAction::AllowForever
            ) {
                let workspace = self.pending_net[idx].source_workspace.clone();
                self.config
                    .ensure_rules_trusted_for_workspace(workspace.as_deref())
                    .map_err(|error| ApprovalControlError {
                        code: "rules_blocked",
                        reason: error.to_string(),
                    })?;
            }
            if matches!(
                action,
                ApprovalAction::AllowForever | ApprovalAction::DenyForever
            ) && self.unambiguous_pending_network_workspace(idx).is_none()
            {
                return Err(ApprovalControlError {
                    code: "ambiguous_workspace",
                    reason: format!(
                        "approval {id} has no unambiguous workspace; use a one-shot decision"
                    ),
                });
            }
            match action {
                ApprovalAction::AllowOnce => self.approve_net(idx),
                ApprovalAction::AllowForever => self.approve_net_forever(idx),
                ApprovalAction::DenyOnce => self.deny_net(idx),
                ApprovalAction::DenyForever => self.deny_net_forever(idx),
                ApprovalAction::Trust => unreachable!(),
            }
            return Ok(decision_response(id, action));
        }

        if self
            .base_rules_changed
            .as_ref()
            .is_some_and(|item| item.approval_id == id)
        {
            if action != ApprovalAction::Trust {
                return Err(incompatible(
                    &id,
                    "rules changes require `hat approvals trust ID`",
                ));
            }
            self.trust_changed_rules()?;
            return Ok(decision_response(id, action));
        }

        Err(ApprovalControlError {
            code: "not_found",
            reason: format!("no pending approval with id {id}"),
        })
    }

    pub(crate) fn trust_changed_rules(&mut self) -> std::result::Result<(), ApprovalControlError> {
        let Some(item) = self.base_rules_changed.as_ref() else {
            return Err(ApprovalControlError {
                code: "not_found",
                reason: "no rules change is waiting for review".to_string(),
            });
        };
        let path = item.path.clone();
        let expected_contents = item.expected_contents.clone();
        match self
            .config
            .trust_rules_file_if_bytes(&path, expected_contents.as_deref())
        {
            Ok(()) => {
                self.base_rules_changed = None;
                self.push_log(
                    format!("trusted reviewed rules file: {}", path.display()),
                    false,
                );
                Ok(())
            }
            Err(error) => {
                self.config.block_rules_file(&path);
                if let Some(state) = self.base_rules_changed.as_mut() {
                    state.dialog_dismissed = false;
                }
                self.push_log(
                    format!(
                        "rules file changed before it could be trusted '{}': {error}",
                        path.display()
                    ),
                    true,
                );
                Err(ApprovalControlError {
                    code: "rules_changed",
                    reason: error.to_string(),
                })
            }
        }
    }

    pub(crate) fn dismiss_changed_rules(&mut self) {
        let Some(item) = self.base_rules_changed.as_mut() else {
            return;
        };
        item.dialog_dismissed = true;
        let path = item.path.clone();
        self.config.block_rules_file(&path);
        self.push_log(
            format!("rules remain blocked until reviewed: {}", path.display()),
            true,
        );
    }

    fn rules_status(
        &self,
        workspace: Option<&str>,
    ) -> std::result::Result<RulesStatusResponse, ApprovalControlError> {
        self.config
            .rules_status(workspace)
            .map(|rules| RulesStatusResponse { rules })
            .map_err(|error| ApprovalControlError {
                code: if workspace.is_some() {
                    "unknown_workspace"
                } else {
                    "rules_status_failed"
                },
                reason: error.to_string(),
            })
    }

    pub(crate) fn trust_rules_target(
        &mut self,
        target: RulesTrustTarget,
    ) -> std::result::Result<RulesTrustResponse, ApprovalControlError> {
        let cfg = self.config.get();
        let (path, description) = match target {
            RulesTrustTarget::Global => (
                cfg.manager.global_rules_file.clone(),
                "global rules".to_string(),
            ),
            RulesTrustTarget::Workspace { workspace } => {
                let workspace = cfg
                    .workspaces
                    .iter()
                    .find(|candidate| candidate.name == workspace)
                    .ok_or_else(|| ApprovalControlError {
                        code: "unknown_workspace",
                        reason: format!("no workspace named {workspace:?}"),
                    })?;
                (
                    workspace.canonical_path.join("harness-rules.toml"),
                    format!("rules for workspace '{}'", workspace.name),
                )
            }
        };

        self.config
            .trust_rules_file(&path)
            .map_err(|error| ApprovalControlError {
                code: "rules_changed",
                reason: error.to_string(),
            })?;

        // The explicit command trusts the bytes present now. Make the
        // filesystem watcher regard those same bytes as its new baseline, or
        // its next scan would recreate the just-cleared block and alert.
        self.watched_rules_stamps
            .insert(path.clone(), Self::watched_file_stamp(&path));
        self.pending_base_rules_internal_write.remove(&path);
        if self
            .base_rules_changed
            .as_ref()
            .is_some_and(|item| item.path == path)
        {
            self.base_rules_changed = None;
        }
        self.push_log(
            format!("trusted current {description}: {}", path.display()),
            false,
        );

        let rule = self
            .config
            .rules_status(None)
            .map_err(|error| ApprovalControlError {
                code: "rules_status_failed",
                reason: error.to_string(),
            })?
            .into_iter()
            .find(|rule| rule.path == path.display().to_string())
            .ok_or_else(|| ApprovalControlError {
                code: "not_found",
                reason: format!(
                    "trusted rules file is no longer configured: {}",
                    path.display()
                ),
            })?;

        Ok(RulesTrustResponse {
            ok: true,
            message: format!("trusted current {description}: {}", path.display()),
            rule,
        })
    }
}

fn next_available_approval_id(
    next: &mut u16,
    mut is_used: impl FnMut(&str) -> bool,
) -> Option<String> {
    for _ in 0..10_000 {
        let id = format!("{next:04}");
        *next = (*next + 1) % 10_000;
        if !is_used(&id) {
            return Some(id);
        }
    }
    None
}

fn normalize_approval_id(raw: &str) -> std::result::Result<String, ApprovalControlError> {
    if raw.is_empty() || raw.len() > 4 || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ApprovalControlError {
            code: "invalid_id",
            reason: "approval IDs are one to four decimal digits".to_string(),
        });
    }
    let value = raw.parse::<u16>().map_err(|_| ApprovalControlError {
        code: "invalid_id",
        reason: "approval ID is outside 0000-9999".to_string(),
    })?;
    Ok(format!("{value:04}"))
}

fn incompatible(id: &str, reason: &str) -> ApprovalControlError {
    ApprovalControlError {
        code: "incompatible_action",
        reason: format!("approval {id}: {reason}"),
    }
}

fn decision_response(id: String, action: ApprovalAction) -> ApprovalActionResponse {
    let message = match action {
        ApprovalAction::AllowOnce => "approval allowed once",
        ApprovalAction::AllowForever => "approval allowed and remembered",
        ApprovalAction::DenyOnce => "approval denied once",
        ApprovalAction::DenyForever => "approval denied and remembered",
        ApprovalAction::Trust => "rules file trusted",
    };
    ApprovalActionResponse {
        ok: true,
        id,
        message: message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{next_available_approval_id, normalize_approval_id};
    use std::collections::HashSet;

    #[test]
    fn approval_ids_are_zero_padded() {
        assert_eq!(normalize_approval_id("0").unwrap(), "0000");
        assert_eq!(normalize_approval_id("42").unwrap(), "0042");
        assert_eq!(normalize_approval_id("9999").unwrap(), "9999");
        assert!(normalize_approval_id("").is_err());
        assert!(normalize_approval_id("10000").is_err());
        assert!(normalize_approval_id("abcd").is_err());
    }

    #[test]
    fn approval_id_allocation_wraps_and_skips_active_ids() {
        let mut next = 9999;
        let used = HashSet::from(["9999".to_string(), "0000".to_string()]);
        assert_eq!(
            next_available_approval_id(&mut next, |id| used.contains(id)),
            Some("0001".to_string())
        );
        assert_eq!(next, 2);
    }

    #[test]
    fn approval_id_allocation_reports_exhaustion() {
        let mut next = 42;
        assert_eq!(next_available_approval_id(&mut next, |_| true), None);
        assert_eq!(next, 42);
    }
}
