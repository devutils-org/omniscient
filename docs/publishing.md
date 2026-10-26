# Publishing the repository

The repository is public since 2026-10-26, at `devutils-org/omniscient`.
This is what was checked on 2026-10-21 (1453 commits, every branch), and what
was done on the day.

## What the history holds

| Check | Result |
|---|---|
| Keys and tokens (AWS, GitHub, Slack, OpenAI, Anthropic, Google, PEM private keys) | none, in any commit |
| Files that usually hold secrets (`.env`, `.pem`, `.key`, `id_*`, keystores, databases) | none ever added |
| Largest files | `tests/view_fixtures/*/results.json`, 2 to 5.3 MB each: test fixtures, nothing to remove |
| Commit authors | one person, under two addresses: `moha.bekh.pro@gmail.com` and `mohabekpro@gmail.com`. Both become public with the history |
| Local paths | `/home/moha/...` in 43 files, mostly `docs/dev/STATE.md` and `docs/dev/briefs/`: working notes, harmless, a little untidy |
| Private addresses | two LAN addresses (`192.168.1.20`, `192.168.1.36`) in `tests/reference/captures/bpftrace/`: real captures of the development machine's traffic |
| Scratch files | `tmp.sh` was tracked (an NVIDIA profiling check); removed on 2026-10-26 |

Nothing has to be rewritten out of the history. The addresses and the local
paths are choices, listed below.

## On the day

1. Done on 2026-10-26, before publishing: `tmp.sh` removed; the root tidied
   to `README.md`, `REFERENCE.md`, `CLAUDE.md` and the licenses, with
   `ROADMAP.md`, `DECISIONS.md`, `CATALOG.md`, `GUIDE.md` and `DEMO.md` moved
   into `docs/`, and the agents' working notes (`STATE.md`, `BRIEF.md`,
   `briefs/`) into `docs/dev/`, every reference rewritten in one commit; the
   README says what `docs/dev/` is. Left as choices:
   - replace the two LAN addresses in the bpftrace captures with documentation
     addresses (`192.0.2.x`), if they should not be public;
   - set one commit address for the future (`git config user.email`, the
     GitHub `noreply` one); the history keeps both;
   - add `CHANGELOG.md` and `SECURITY.md` (`docs/contributing.md` can move to
     the root as `CONTRIBUTING.md`, where GitHub looks for it).
2. Done on 2026-10-26: the repository was transferred to `devutils-org`
   (GitHub redirects `moha-bekh/omniscient`), the links in `docs/distroproof.md`
   and its French copy were rewritten to the new owner, and it was made
   public. On the site (`devutils-org.com`), Omniscient's `repoPrivate` flag
   is gone and the Together page links the repository; distroproof's README,
   the organisation's profile and the owner's profile link it too.
3. No release yet, on purpose: the code is public, the tool is not declared
   finished.
4. CI: once public, GitHub-hosted runs cost nothing, so `ci.yml` can go back
   to running on `push` and `pull_request` (it is `workflow_dispatch` only
   to save the private account's minutes).
