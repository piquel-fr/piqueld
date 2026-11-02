//! Transport-independent logic of the dashboard's deployment and preview
//! views: deployment history paging, attempt lists, promotion confirmation,
//! and preview creation.

use piqueld_client::{
    CreatePreviewRequest, DeploymentOrigin, DeploymentView, Operation, Page, PlanView,
    PromoteRequest, ReleaseAvailability,
};

/// The deployments of one environment shown in its history, newest first:
/// its newest page, polled, and older pages appended on request.
pub trait DeploymentList {
    /// Merges a freshly polled newest page into a list that also holds older
    /// pages: deployments are replaced or added by operation ID, keeping the
    /// older ones, and the `current_target` and `last_successful` flags are
    /// cleared on listed deployments when the page carries them, so only one
    /// deployment holds each flag.
    fn merge_newest(&mut self, page: Vec<DeploymentView>);

    /// Appends an older page, skipping deployments already listed.
    fn append_older(&mut self, page: Vec<DeploymentView>);
}

impl DeploymentList for Vec<DeploymentView> {
    fn merge_newest(&mut self, page: Vec<DeploymentView>) {
        let has_current = page.iter().any(|item| item.current_target);
        let has_successful = page.iter().any(|item| item.last_successful);
        for item in self.iter_mut() {
            item.current_target &= !has_current;
            item.last_successful &= !has_successful;
        }
        for item in page {
            match self
                .iter_mut()
                .find(|old| old.operation.id == item.operation.id)
            {
                Some(old) => *old = item,
                None => self.push(item),
            }
        }
        // Operation IDs are time-ordered, as the daemon pages them.
        self.sort_by(|a, b| b.operation.id.cmp(&a.operation.id));
    }

    fn append_older(&mut self, page: Vec<DeploymentView>) {
        for item in page {
            if !self.iter().any(|old| old.operation.id == item.operation.id) {
                self.push(item);
            }
        }
    }
}

/// The retained attempt outcomes of one deployment loaded so far, newest
/// first, and the cursor of the next older page.
#[derive(Clone, Debug, Default)]
pub struct Attempts {
    items: Vec<Operation>,
    /// Cursor of the next older page, if any.
    pub cursor: Option<String>,
}

impl Attempts {
    /// Shows `page`: the newest attempts, replacing those shown, or with
    /// `older` the attempts following them.
    pub fn show(&mut self, page: Page<Operation>, older: bool) {
        if older {
            self.items.extend(page.items);
        } else {
            self.items = page.items;
        }
        self.cursor = page.next_cursor;
    }

    /// The attempts to list for a deployment whose operation is `live`: the
    /// API retains only completed outcomes, so the attempt it is running
    /// leads the list until its outcome is retained.
    #[must_use]
    pub fn visible(&self, live: &Operation) -> Vec<Operation> {
        let running =
            live.attempt > 0 && !self.items.iter().any(|item| item.attempt == live.attempt);
        running
            .then(|| live.clone())
            .into_iter()
            .chain(self.items.iter().cloned())
            .collect()
    }
}

/// What a promotion into an environment deploys: its source's current
/// deployment, or an earlier `release`.
#[derive(Clone, Debug)]
pub struct Promotion {
    release: Option<String>,
}

impl Promotion {
    /// Promotes the source's current deployment, or deploys `release`.
    #[must_use]
    pub const fn new(release: Option<String>) -> Self {
        Self { release }
    }

    /// "Deploy" for an earlier release, else "Promote".
    #[must_use]
    pub const fn verb(&self) -> &'static str {
        if self.release.is_some() {
            "Deploy"
        } else {
            "Promote"
        }
    }

    /// The request planning this promotion.
    #[must_use]
    pub fn request(&self) -> PromoteRequest {
        PromoteRequest {
            release: self.release.clone(),
            ..PromoteRequest::default()
        }
    }

    /// The request promoting exactly what `plan` reviewed: pinned to the
    /// source deployment it planned and the application revision it was
    /// planned at. None when the plan has no release, or the release mounts
    /// secrets the environment lacks or may not use, or its images are gone.
    #[must_use]
    pub fn confirmed(&self, plan: &PlanView) -> Option<PromoteRequest> {
        let release = plan.release.as_ref()?;
        if !release.secrets.is_empty()
            || matches!(
                release.release.availability,
                Some(ReleaseAvailability::Unavailable { .. })
            )
        {
            return None;
        }
        let mut request = self.request();
        if let DeploymentOrigin::Promotion { deployment, .. } = &release.origin {
            request.deployment = Some(deployment.clone());
        }
        request.expected_generation = Some(plan.generation);
        Some(request)
    }
}

/// The "Create preview" form: a branch, and an optional slot telling apart
/// several previews of it.
#[derive(Clone, Debug, Default)]
pub struct PreviewForm {
    /// Branch to deploy.
    pub branch: String,
    /// Slot, blank for none.
    pub slot: String,
}

impl PreviewForm {
    /// The request creating the preview, trimmed; a blank slot is none.
    #[must_use]
    pub fn request(&self) -> CreatePreviewRequest {
        CreatePreviewRequest {
            branch: self.branch.trim().to_owned(),
            slot: Some(self.slot.trim().to_owned()).filter(|slot| !slot.is_empty()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use piqueld_client::{OperationKind, OperationState};
    use serde_json::{Value, json};

    /// The manifest every fixture deploys.
    fn template() -> Value {
        json!({
            "id": "app-0123456789abcdef0123456789abcdef",
            "api_version": "piqueld.dev/v1alpha1",
            "kind": "Application",
            "metadata": { "name": "shop" },
            "spec": { "services": [{ "name": "web", "source": { "type": "image", "image": "nginx:alpine" } }] },
        })
    }

    /// Attempt `attempt` of operation `id`, in `state`.
    fn operation(id: &str, attempt: u64, state: OperationState) -> Operation {
        Operation {
            id: id.into(),
            environment_id: "env-0123456789abcdef0123456789abcdef".parse().unwrap(),
            kind: OperationKind::Refresh,
            generation: 1,
            attempt,
            consecutive_failures: 0,
            state,
            phase: None,
            resource: None,
            error_code: None,
            error_message: None,
            created_at_ms: 0,
            updated_at_ms: 0,
            started_at_ms: None,
            finished_at_ms: None,
        }
    }

    /// Deployment `id`, with its `current_target` and `last_successful` flags.
    fn deployment(id: &str, current: bool, successful: bool) -> DeploymentView {
        serde_json::from_value(json!({
            "operation": operation(id, 1, OperationState::Succeeded),
            "template": template(),
            "variables": {},
            "application": null,
            "succeeded_at_ms": 1,
            "current_target": current,
            "last_successful": successful,
        }))
        .unwrap()
    }

    fn ids(list: &[DeploymentView]) -> Vec<&str> {
        list.iter().map(|item| item.operation.id.as_str()).collect()
    }

    #[test]
    fn a_polled_newest_page_keeps_older_pages_and_moves_the_flags() {
        let mut list = vec![
            deployment("operation-2", true, true),
            deployment("operation-1", false, false),
        ];
        list.append_older(vec![
            deployment("operation-1", false, false),
            deployment("operation-0", false, false),
        ]);
        assert_eq!(ids(&list), ["operation-2", "operation-1", "operation-0"]);

        let flags = |list: &[DeploymentView]| {
            list.iter()
                .map(|item| (item.current_target, item.last_successful))
                .collect::<Vec<_>>()
        };
        // A newer deployment became the current target, but hasn't succeeded
        // yet: deployment 2, off the polled page, stays the last successful.
        list.merge_newest(vec![deployment("operation-3", true, false)]);
        assert_eq!(
            ids(&list),
            ["operation-3", "operation-2", "operation-1", "operation-0"]
        );
        assert_eq!(
            flags(&list),
            [(true, false), (false, true), (false, false), (false, false)]
        );
        // Then a newer one succeeded without becoming the current target.
        list.merge_newest(vec![deployment("operation-4", false, true)]);
        assert_eq!(
            flags(&list)[..3],
            [(false, true), (true, false), (false, false)]
        );
    }

    #[test]
    fn the_running_attempt_leads_until_its_outcome_is_retained() {
        let mut attempts = Attempts::default();
        let retained = |attempt| operation("operation-1", attempt, OperationState::Failed);
        attempts.show(
            Page {
                items: vec![retained(2)],
                next_cursor: Some("older".into()),
            },
            false,
        );
        attempts.show(
            Page {
                items: vec![retained(1)],
                next_cursor: None,
            },
            true,
        );
        assert_eq!(attempts.cursor, None);

        let listed = |attempts: &Attempts, live: &Operation| {
            attempts
                .visible(live)
                .iter()
                .map(|item| (item.attempt, item.state))
                .collect::<Vec<_>>()
        };
        let running = operation("operation-1", 3, OperationState::Running);
        assert_eq!(
            listed(&attempts, &running),
            [
                (3, OperationState::Running),
                (2, OperationState::Failed),
                (1, OperationState::Failed)
            ]
        );
        // Refreshing shows the newest page alone; a retained outcome is listed
        // once, and nothing ran before the first attempt.
        attempts.show(
            Page {
                items: vec![retained(3)],
                next_cursor: Some("older".into()),
            },
            false,
        );
        assert_eq!(listed(&attempts, &running), [(3, OperationState::Failed)]);
        let requested = operation("operation-1", 0, OperationState::Requested);
        assert_eq!(listed(&Attempts::default(), &requested), []);
    }

    /// A plan of promoting a release from `origin`, at revision 7, whose
    /// images are `availability` and which lacks `secrets`.
    fn plan(origin: &Value, availability: &Value, secrets: &Value) -> PlanView {
        serde_json::from_value(json!({
            "application_id": "app-0123456789abcdef0123456789abcdef",
            "generation": 7,
            "identical": false,
            "operation": null,
            "changes": [],
            "plan": { "actions": [], "diagnostics": [] },
            "rollouts": [],
            "release": {
                "release": {
                    "id": "rel-0123456789abcdef0123456789abcdef",
                    "application_id": "app-0123456789abcdef0123456789abcdef",
                    "content_hash": format!("sha256:{}", "b".repeat(64)),
                    "created_at_ms": 0,
                    "release": {
                        "template": template(),
                        "sources": { "web": {
                            "type": "image",
                            "requested": "nginx:alpine",
                            "digest_reference": format!("docker.io/library/nginx@sha256:{}", "a".repeat(64)),
                        } },
                    },
                    "fingerprint": { "web": { "source.image": "nginx:alpine" } },
                    "availability": availability,
                },
                "origin": origin,
                "new_volumes": [],
                "secrets": secrets,
            },
        }))
        .unwrap()
    }

    #[test]
    fn a_promotion_confirms_exactly_the_reviewed_source_and_revision() {
        let promoted = json!({
            "type": "promotion",
            "environment": "env-0123456789abcdef0123456789abcdef",
            "deployment": "operation-1",
        });
        let (present, none) = (json!({ "state": "present" }), json!([]));
        let request = Promotion::new(None)
            .confirmed(&plan(&promoted, &present, &none))
            .unwrap();
        assert_eq!(request.deployment.as_deref(), Some("operation-1"));
        assert_eq!(request.expected_generation, Some(7));
        assert_eq!(request.release, None);

        // An earlier release is deployed as itself, not pinned to a deployment.
        let id = "rel-0123456789abcdef0123456789abcdef";
        let release = Promotion::new(Some(id.into()))
            .confirmed(&plan(&json!({ "type": "release" }), &present, &none))
            .unwrap();
        assert_eq!(release.deployment, None);
        assert_eq!(release.release.as_deref(), Some(id));

        // Missing secrets, or built images that are gone, block it; missing
        // registry images are pulled again by digest.
        let missing = json!([{ "problem": "missing", "secret": "api-key" }]);
        let confirmed = |availability: &str, secrets: &Value| {
            let availability = json!({ "state": availability, "missing": [] });
            Promotion::new(None)
                .confirmed(&plan(&promoted, &availability, secrets))
                .is_some()
        };
        assert!(!confirmed("present", &missing));
        assert!(!confirmed("unavailable", &none));
        assert!(confirmed("pullable", &none));
    }

    #[test]
    fn a_blank_preview_slot_is_none() {
        let form = PreviewForm {
            branch: " feat/login ".into(),
            slot: "  ".into(),
        };
        let request = form.request();
        assert_eq!(request.branch, "feat/login");
        assert_eq!(request.slot, None);
        let slotted = PreviewForm {
            slot: " agent-2 ".into(),
            ..form
        };
        assert_eq!(slotted.request().slot.as_deref(), Some("agent-2"));
    }
}
