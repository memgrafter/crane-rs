---
id: cr-gl7n
status: open
open: true
deps: []
links: []
created: 2026-11-09T18:05:22Z
type: task
priority: 2
assignee: memgrafter
tags: [crane, perf]
---
# Perf gate: cut measurement noise, then tighten from 1.5x

User asked (2026-11-09) how far PERF_MAX_RATIO (scripts/perf.sh, default 1.5) can tighten, and whether something better than a flat 1.5x is possible. Not started.

Machine: Apple M6 (Mac18,5), 2 performance + 4 efficiency cores, 24 GB. With only 2 P-cores, client, registry and test harness compete and macOS moves work to E-cores: a likely source of noise.

Data (4 clean runs, median of 3 each; .bench/perf-1794236155.json, perf-1794245176/214/250.json): worst ratio 1.15 (copy into the rust registry). Per-case ratio sd up to 0.134 (copy into rust registry: mean 1.02; rust time CV 16%); most cases sd 0.01-0.08. Cases are short (0.13-0.62 s), so a few ms of scheduling noise is a big share.
- Flat 1.27 (worst x 1.1, user's first idea): mean + 1.9 sd on the noisiest case -> roughly 1 false failure in 30 runs.
- Flat 1.5 (today): about mean + 3.6 sd on that case; it was picked before any data, it is not derived from noise.

How tests/perf.rs measures today: all go runs of a case, then all rust runs (not interleaved, so drift between the two blocks becomes bias); only medians are saved, no samples.

Plan (agent recommendation):
1. Interleave go and rust runs (g r g r ...), PERF_RUNS default 10, compare cumulative (summed) time per tool (the user's earlier approach; noise falls about with sqrt(N): 16% CV -> about 5%).
2. Save every sample in .bench/perf-*.json; print mean, sd and CV per case.
3. Make short cases longer (e.g. digest x200, bigger many.tar) so each sample is >= 1 s.
4. After 5 runs of the new method: gate per case = baseline ratio + 3 sd (or one flat gate at the max of those), stored in a checked-in table; expected around 1.15-1.2 for the slowest cases.

## Acceptance Criteria

- perf.rs interleaves runs, sums time, saves samples and sd/CV.
- 5 runs of the new method recorded on this ticket.
- New gate(s) set from those numbers with the rule written down; 5 further runs pass with no false failure.
