---
id: decision-chose-fixed-point-scoring-over-floats-1
type: decision
created: 2026-06-27T14:00:00Z
title: Chose fixed-point scoring over floats
tags: [determinism, scoring]
---
We avoid floats in scoring: floats round differently across FPUs and
optimization levels, which breaks byte-stable golden tests. All scores are
i64 micro-units instead.
