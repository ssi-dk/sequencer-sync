# Documentation Hardlinks

This manifest declares shared agent guidance received from the team wiki.
No product documentation mirror is declared here.

## Incoming shared agent guidance

Canonical owner: team wiki. This is an incoming public-safe policy file, not
an outgoing product documentation mirror. Keep it out of product Reference folders.

| Wiki source (workspace-relative) | Repo destination | Required |
| --- | --- | --- |
| `wiki/99_Meta/Agents/Shared/repository-workflow.md` | `docs/agents/shared-workflow.md` | yes |

Commit the destination as ordinary Markdown so standalone clones retain it.
Git does not preserve hardlinks. Reconcile any content differences before
relinking; verify matching device and inode. The wiki source owns shared edits.
Setup and restoration: `wiki/99_Meta/Agents/Shared Agent Hardlinks.md`.
