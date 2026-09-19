# Scenarios

One document per family, written by the scenario wave. Each answers the same
question for its own sources: **what is worth measuring here, and how do you
know the answer is right?**

They are not a test index. The tests live in `tests/scenario_<family>.rs` and
the sessions in `examples/<family>-*.yml`; these documents are what a person
reads before choosing a measurement, and what the next wave reads instead of a
diff.

Each carries four things:

- **Scenarios**, by the question they answer, with the session that answers it
  and the property of the workload that makes the answer checkable.
- **Workloads**, with what each one guarantees — phase two reuses them.
- **What cannot be verified here**, and why. A scenario that states its limit is
  worth more than one that hides it.
- **Defects found**, with the session that shows each and the output it
  produced. A scenario suite that quietly worked around three bugs has measured
  nothing.

See `BRIEF.md` for the four scenario classes every family covers, and
`docs/contributing.md` for what ground truth means in this repository.
