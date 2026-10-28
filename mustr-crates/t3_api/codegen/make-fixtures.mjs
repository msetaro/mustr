// Fixture values copied from T3 tests (packages/contracts/src/pullRequest.test.ts, environment.test.ts @ 8d94203c).
import { writeFileSync } from "node:fs";
const out = process.argv[2];
const LIST_RESULT = {
  viewers: { "github.com": "bilal", "gitlab.com": "bilal.hassan" },
  providers: [
    { host: "github.com", kind: "github", searchesOnHost: true, projectCount: 1, configured: true, detail: null },
    { host: "gitlab.com", kind: "gitlab", searchesOnHost: true, projectCount: 1, configured: false, detail: "glab is not installed." },
  ],
  entries: [{
    provider: "github", host: "github.com", projectId: "project-1", projectTitle: "t3code", repository: "pingdotgg/t3code",
    number: 1, title: "Add a pull requests page", url: "https://github.com/pingdotgg/t3code/pull/1",
    author: { login: "octocat", name: null, avatarUrl: null }, headBranch: "feat/page", baseBranch: "main", state: "open",
    isDraft: false, mergeability: "mergeable", additions: 1, deletions: 0, createdAt: "2026-07-01T00:00:00Z",
    updatedAt: "2026-07-02T00:00:00Z", viewerReviewRequested: false, labels: [{ name: "backend", color: null }],
  }],
  errors: [], truncated: false, nextCursors: { "github.com pingdotgg/t3code": "2026-07-02T00:00:00Z|1|1" },
};
const base = { diff: true, comment: true, actions: [], mergeMethods: [], search: true,
  review: { inlineComment: true, reply: true, resolve: true, verdicts: [] }, reviewers: { request: true, listCandidates: true } };
const options = { actions: ["close", "archive", "merge"], verdicts: ["request-changes", "block", "approve"], updateMethods: ["rebase", "fast-forward", "merge"] };
const detail = { ...LIST_RESULT.entries[0], workspaceRoot: "/workspace/t3code", body: "", changedFiles: 1, mergedAt: null, closedAt: null,
  reviewers: [], checks: [], mergeCapabilities: { merge: true, squash: true, rebase: true },
  capabilities: { ...base, actions: options.actions, mergeMethods: ["squash", "fast-forward", "merge"], updateMethods: options.updateMethods,
    review: { ...base.review, verdicts: options.verdicts } },
  viewerPermissions: { ...options, comment: true, resolve: true, requestReviewers: true } };
const known = { ...detail, capabilities: { ...base, actions: ["close", "merge"], mergeMethods: ["squash", "merge"], updateMethods: ["rebase", "merge"],
    review: { ...base.review, verdicts: ["request-changes", "approve"] } },
  viewerPermissions: { actions: ["close", "merge"], verdicts: ["request-changes", "approve"], updateMethods: ["rebase", "merge"], comment: true, resolve: true, requestReviewers: true } };
const descriptor = { environmentId: "environment-1", label: "Local", platform: { os: "darwin", arch: "arm64" }, serverVersion: "0.0.32", capabilities: { repositoryIdentity: true } };
const w = (n, v) => writeFileSync(`${out}/${n}.json`, JSON.stringify(v, null, 2) + "\n");
w("PullRequestListResult", LIST_RESULT);
w("PullRequestDetail.known-options", known);
w("PullRequestDetail.newer-server", detail);
w("ExecutionEnvironmentDescriptor", descriptor);
w("ExecutionEnvironmentDescriptor.npx", { ...descriptor, capabilities: { ...descriptor.capabilities, serverInstallation: { kind: "npx" } } });
w("ExecutionEnvironmentDescriptor.npm-global", { ...descriptor, capabilities: { ...descriptor.capabilities, serverInstallation: { kind: "npm-global", prefix: "/opt/node" } } });
w("ExecutionEnvironmentDescriptor.future-manager", { ...descriptor, capabilities: { ...descriptor.capabilities, serverInstallation: { kind: "future-manager" } } });
