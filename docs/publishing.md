# Publishing the repository

The repository is private until its owner decides otherwise. This is what
was checked on 2026-10-21 (1453 commits, every branch) and what is left to do
on the day.

## What the history holds

| Check | Result |
|---|---|
| Keys and tokens (AWS, GitHub, Slack, OpenAI, Anthropic, Google, PEM private keys) | none, in any commit |
| Files that usually hold secrets (`.env`, `.pem`, `.key`, `id_*`, keystores, databases) | none ever added |
| Largest files | `tests/view_fixtures/*/results.json`, 2 to 5.3 MB each: test fixtures, nothing to remove |
| Commit authors | one person, under two addresses: `moha.bekh.pro@gmail.com` and `mohabekpro@gmail.com`. Both become public with the history |
| Local paths | `/home/moha/...` in 43 files, mostly `STATE.md` and `docs/briefs/`: working notes, harmless, a little untidy |
| Private addresses | two LAN addresses (`192.168.1.20`, `192.168.1.36`) in `tests/reference/captures/bpftrace/`: real captures of the development machine's traffic |
| Scratch files | `tmp.sh` is tracked (an NVIDIA profiling check) |

Nothing has to be rewritten out of the history. The addresses, the local paths
and `tmp.sh` are choices, listed below.

## On the day

1. Optional, before publishing:
   - remove `tmp.sh`, or move the check it holds into `scripts/`;
   - replace the two LAN addresses in the bpftrace captures with documentation
     addresses (`192.0.2.x`), if they should not be public;
   - set one commit address for the future (`git config user.email`); the
     history keeps both.
2. Decide where it lives. The site builds every repository link as
   `github.com/devutils-org/<repo>`, so either transfer the repository to the
   `devutils-org` organisation (GitHub redirects the old URL and the links in
   `docs/distroproof.md`), or point the site at `moha-bekh/omniscient`.
3. Make it public (repository settings), then on the site
   (`devutils-org.github.io`): in `src/tools.js`, remove `repoPrivate: true`
   from Omniscient and drop "repository not public yet" from its
   `statusNote`; in `src/practice/cases.js` and the Together page, the
   sources cited by path can become links. The Omniscient page and the home
   card switch their links on from that one flag.
4. CI: once public, GitHub-hosted runs cost nothing, so `ci.yml` can go back
   to running on `push` and `pull_request` (it is `workflow_dispatch` only
   to save the private account's minutes).
