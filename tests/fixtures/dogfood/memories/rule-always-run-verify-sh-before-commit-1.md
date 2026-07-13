---
id: rule-always-run-verify-sh-before-commit-1
type: rule
created: 2026-07-01T09:00:00Z
title: Always run verify.sh before commit
tags: [ci, discipline]
---
The gate is scripts/verify.sh. Committing without the gate is how
regressions ship.
