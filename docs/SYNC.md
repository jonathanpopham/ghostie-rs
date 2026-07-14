# ghostie sync model (normative)

Cross-device memory through *your own* git remote. No proprietary service, no
account, no server that holds your context. Sync shells to the system `git`
binary (a tool present on every developer machine, not a crate dependency), so
the toolbox stays std-only, exactly as Lockstep shells to POSIX `kill`.

## What syncs, and what does not

- **Memory files** (`memories/*.md`) sync. They are the data.
- **The provenance log** (`.provenance/log.jsonl`) syncs. It is the evidence
  and it must travel with the memories it vouches for.
- **The derived index** (`.index/`) does NOT sync. It is rebuildable from the
  memory files at any time, and syncing it would only manufacture conflicts. A
  `.gitignore` entry excludes it, written on `sync --init`.

## One command to wire it

```
ghostie sync --init <your-git-remote>   # once per device
ghostie sync                            # commit local, rebase remote, push
```

`sync --init` makes the store a git repo pointed at your remote, sets a local
identity so commits never fail on a machine with no global git config, and
pins the branch to `main` regardless of this machine's `init.defaultBranch`.
`ghostie setup <remote>` does the init plus the hooks plus a first push.

## The sync algorithm

On each `ghostie sync`:

1. Resolve the branch. If the remote advertises a default branch, follow it, so
   two devices with different local defaults still agree. A fresh, unborn HEAD
   is pinned to that branch before the first commit.
2. Stage everything and commit, but only if something changed. The commit
   message timestamp comes from the injected clock, so runs are reproducible.
3. Fetch the remote, then rebase onto its branch when that branch exists (a
   brand-new remote has nothing to pull).
4. Push, setting upstream on the first push.

## Offline semantics

Sync is optional and the store works fully offline without it. A memory is a
plain file the moment it is written; `remember`, `recall`, `capture`, and the
hooks never need the network. `sync` is the only command that touches a remote.
Away from a network, keep working; the next `sync` reconciles.

## Conflict policy: report, never auto-resolve

A rebase that collides is aborted cleanly and surfaced as a conflict with exit
code **3** (reserved for exactly this). The working tree is left untouched, so
nothing is lost and nothing is silently merged. You resolve the conflict in the
store directory by hand, then re-run `ghostie sync`.

Because two devices rarely edit the *same* memory file, conflicts are rare in
practice: distinct memories are distinct files, and captured memories carry a
deterministic `<harness>:<session>` identity, so re-capturing the same session
is idempotent rather than a duplicate. When a genuine same-file conflict does
happen, it is yours to resolve, not the tool's to guess.

## Failure visibility

An automatic `sync` on a session-end hook that fails (auth, network, a bad
remote, a conflict) does not vanish. It writes a `.sync-error` marker in the
store and shouts on stderr, so a stalled backup surfaces rather than rotting
unnoticed; the marker clears on the next success. `ghostie setup` fails loudly
if its initial push fails, rather than reporting success while your memory
stays local.

## Encrypted and private remotes

The memory files hold prompts, decisions, and rationales. Two layers protect
them before they leave the machine: the secret-redaction gate scrubs credential-
shaped strings out of every write, and (where wired) `sync --encrypt` encrypts
the files before they are pushed. Point the remote at a private repository you
control; nothing about the transport requires it to be public.
