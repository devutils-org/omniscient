# Fixes

One document per stream of the fix wave, the counterpart to `docs/scenarios/`.
Those recorded sixty-three defects; these record what became of each.

Each carries three things:

- **Fixed**, with the defect it closes and the scenario that now asserts the new
  behaviour. A fix whose scenario was not updated is not finished.
- **Something else**, for what turned out not to be the defect it was filed as.
  A wrong diagnosis corrected is worth as much as a fix.
- **Left**, with why. A machine that cannot exercise a fix, a change that needs
  another stream first, a defect that is really a decision.

See `docs/briefs/wave-5-defects.md` for how the sixty-three were divided --
it was `BRIEF.md` while that wave ran -- and `docs/scenarios/<family>.md` for the
defects themselves.
