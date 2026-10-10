# Disposable GCE workflow rollout handoff

Local implementation only. No push, merge, tag, deployment, cloud writes or product actions were performed. Existing source selections, immutable producer tags, tool entrypoints, operational inputs and run titles are preserved.

## Controller enrollment

Repository `perspyc/dustagent`, repositoryId `1411495115`, ownerId `330950725`. Enroll the repository in the isolated organization runner group and route queued webhook/tick events before activating these workflow bytes.

Use controller `github.workflows` keys `repository:workflowId` (Terraform `runner_workflows`), `trustMode: pinned`, `branch: main`, `ref: refs/heads/main`, exact workflow `path`, and `trustedRef` equal to the final reviewed workflow commit SHA. Validate first attempt and numeric provider identities. CI additionally requires `role: ci`, `unprivileged: true`, same-repository PR base main or push main, exact reviewed workflow-byte equality at the provider head, read-only permissions and no secrets/environment/reusable workflow/OIDC.

| Workflow ID | Path | Role |
| --- | --- | --- |
| 379399665 | `.github/workflows/ci.yml` | ci |

All jobs match `self-hosted`, `Linux`, `X64`, `route-one` plus one `r1-run-${{ github.run_id }}-<job key>` label. The controller JIT registration must use that exact unique label; no hosted fallback. SaaS staging uses suffix `deploy-staging`. Four GCE slots and four shared Cloud Build slots are controller-owned; unknown provider occupancy blocks admission.

Preparation requires `route-one-preparation:<stage UUID>`, frozen intent/producer pins, active dispatch-started stage and exact recorded run authority. Operation requires `route-one:<operation UUID>`, immutable approved source hash, active operation and stored external run ID. Configure the exact dispatch App numeric `dispatchActorId` for these roles, plus the independent authority reader.

## Required nonsecret repository variables

None; Dust CI has no cloud identity, secrets or environment.

Pinned via read-only GitHub commit resolution: checkout v4 `11d5960a326750d5838078e36cf38b85af677262`; Rust stable action `686976e191b89faba57d3206551f0f330d8cb249`; rust-cache v2 `6323deb102c322ba6fcbdcafc7e3dddab59af2b6`. Rust tool installation is delegated to that pinned action with rustfmt/clippy. CI is main-only (provider default main), same-repository PR only, explicit contents read and credential persistence disabled.

Validate: `uv run --no-project --python 3.11 --with PyYAML python -m unittest discover -s tests/workflows -q`. Rust application code is unchanged.

## Validation

Focused offline workflow contract: one test passes, checking labels, event/base guards, privilege restrictions, exact action SHA pins and Rust tool installation.
