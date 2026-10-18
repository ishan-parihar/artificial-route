# Worklog

Append-only: `genome log "<entry>"`, one per unit of work.
- 2026-10-18 swap: live aroute replaced 5c00ea8 with release build of 5575e35 (rollback copy ~/.local/bin/aroute.pre-e6c6015.bak). Service active on 127.0.0.1:20128, healthz ok, 10 combos. Included: bedrock (e6c6015), gitlab-duo (e6c6015), zenmux-free wire anthropic + anthropic-version header (5575e35). D10: free-stack=150 per-combo already live in ar.env; D4 probe still hit the 150s deadline, value unchanged. Not verified live: bedrock, gitlab-duo, zenmux-free auth. Unpushed: 1114420 f68110f bf56cc1 9bb4471 0de7df5 3e0547a 4791c5a e6c6015 5575e35.
