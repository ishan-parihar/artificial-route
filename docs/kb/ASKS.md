# Operator questions and rulings

`genome q add "<question>"` · `genome q rule Q1 "<ruling>" --applied <owner path>`.
A ruling is done only when its owner doc or record carries it.

| id | asked | status | question | ruling | applied |
|---|---|---|---|---|---|
| Q1 | 2026-10-18 | open | Provide a live key for `zenmux-free` so the anthropic-wire auth (`Authorization: Bearer` plus `anthropic-version`) can be checked. | | |
| Q2 | 2026-10-18 | open | Provide Bedrock API key and region for a live Converse check of `bedrock` (`/model/{id}/converse[-stream]`, eventstream decode). | | |
| Q3 | 2026-10-18 | open | Provide a GitLab Duo token for a live `code_suggestions` check of `gitlab-duo`. | | |
| Q4 | 2026-10-18 | open | D10 free-stack stall: `free-stack=150` is live, and the D4 probe still hit the 150s deadline. Raise the per-combo value, or stop at 150? | | |
| Q5 | 2026-10-18 | open | GitLab Duo tool emulation (`<tool>` blocks) and OAuth refresh are not ported; tool calls are refused with `UnsupportedContent`. Port them, or keep the refusal? | | |
| Q6 | 2026-10-18 | open | Push the 9 local commits (1114420 through 5575e35 plus the worklog commit) to origin? | | |
