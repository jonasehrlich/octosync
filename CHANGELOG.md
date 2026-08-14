# Changelog

## Unreleased

- Only expire users whose GitHub ID is missing from the fetched org member list. Users whose
  processing failed stay in the store unchanged.
  ([#15](https://github.com/jonasehrlich/octosync/pull/15))
- A failed authorized keys fetch no longer fails user processing which could lead to the removal of
  users whose keys could not be fetched. ([#15](https://github.com/jonasehrlich/octosync/pull/15))
- Fetch public keys through the authenticated GitHub client instead of the anonymous, rate-limited
  endpoint ([#15](https://github.com/jonasehrlich/octosync/pull/15))
- Manage SSH fetched keys in a marked, octosync-managed block inside `authorized_keys`
  ([#15](https://github.com/jonasehrlich/octosync/pull/15))
- Write `authorized_keys` atomically, never follow symlinks and enforce exact permissions on `.ssh`
  and `authorized_keys` ([#15](https://github.com/jonasehrlich/octosync/pull/15))
- Refuse a sync that would expire every stored user
  ([#17](https://github.com/jonasehrlich/octosync/pull/17))
- Implement mapping GitHub teams to Linux groups with `--group <gh-team-slug>:<linux-group>`
  ([#20](https://github.com/jonasehrlich/octosync/pull/20))
- Re-create stored users whose account disappeared from the system, passing the stored name and UID
  to `useradd` so file ownership survives a removal and re-create cycle
  ([#19](https://github.com/jonasehrlich/octosync/pull/19))
- Refuse to update a user whose stored UID or name belongs to a different account, instead of
  renaming that account and handing it the GitHub user's SSH keys
  ([#19](https://github.com/jonasehrlich/octosync/pull/19))
- Stop user processes gracefully: send SIGTERM first and SIGKILL only the processes still running
  after a grace period, then wait for the killed processes to leave the process table
  ([#19](https://github.com/jonasehrlich/octosync/pull/19))
- Replace account deletion with expiry: a departed member's account is expired with
  `usermod --expiredate`, which blocks password and pubkey SSH logins, their sessions are ended
  through logind (with the process sweep as the catch-all) and their supplementary groups are
  removed. The account and home directory stay on the machine as the durable departure record, so a
  wrong departure decision is a reversible lockout that the next sync of a rejoining member heals,
  with files, UID and GID intact. Expiry is re-applied on every sync, so an interrupted departure
  converges. A future expiry date set by an operator stays.
  ([#25](https://github.com/jonasehrlich/octosync/pull/25))
- Purge accounts that have been expired for longer than the retention period (180 days by default,
  `--purge-after-days`) at the end of each sync and through the new `purge` command: the account and
  home directory are removed permanently and without an archive. A purge requires the tombstone's
  departure timestamp and the account's own shadow expiry to agree on the age, and the member to be
  absent from the fetched member list. The tombstone survives the purge, so even a member rejoining
  later gets their old UID and GID back. ([#25](https://github.com/jonasehrlich/octosync/pull/25))
- Refuse to create a user whose GitHub login a departed member's tombstone reserves under a
  different GitHub ID: a recycled login must not adopt the previous owner's expired account and home
  directory. ([#25](https://github.com/jonasehrlich/octosync/pull/25))
- Store `users.json` in a versioned v2 schema: members who left are kept as tombstones under a
  `departed` key while their account is expired, and under a `purged` key after the purge, so a
  member who leaves and rejoins gets their old UID back. The v1 file is backed up to `users-v1.json`
  once before the first v2 save. ([#22](https://github.com/jonasehrlich/octosync/pull/22),
  [#25](https://github.com/jonasehrlich/octosync/pull/25))
- The `delete` command expires all managed users and keeps the store file with tombstones instead of
  removing `users.json`, preserving the UID memory of a full wipe
  ([#22](https://github.com/jonasehrlich/octosync/pull/22),
  [#25](https://github.com/jonasehrlich/octosync/pull/25))
- A dry run no longer writes the users database: previously new members were persisted with invented
  mock UIDs and leavers were tombstoned for real, so a later real run acted on preview data.
  ([#25](https://github.com/jonasehrlich/octosync/pull/25))
- Every mutating operation resolves the account through one shared name and UID cross-check. Syncing
  supplementary groups previously trusted the stored UID alone, so a stale UID could strip the
  supplementary groups (e.g. `sudo`) of an unrelated account that reuses it.
  ([#25](https://github.com/jonasehrlich/octosync/pull/25))
- Track the primary group GID in the store and re-create a rejoining member's private group with
  `groupadd --gid <stored>` before `useradd --gid <stored>`, so group ownership of files outside the
  home directory also survives a removal and re-create cycle. A GID that meanwhile belongs to
  another group fails the re-creation loudly instead of falling back to a fresh one. Entries
  migrated from a v1 store carry no GID and are backfilled on their next update.
  ([#22](https://github.com/jonasehrlich/octosync/pull/22))
- Never allocate a departed user's UID or GID to a brand-new member: `useradd` hands out the highest
  ID in range plus one, which is exactly what purging the highest-UID user frees. A new account
  whose auto-allocated IDs collide with a tombstone is removed while still empty and re-created with
  explicitly chosen free IDs. ([#22](https://github.com/jonasehrlich/octosync/pull/22))

## v0.3.0

- Added a `--version` flag to display the current version of the application.

## v0.2.0

- Added group sync with the `--group` option.
- Added locking of the data directory to prevent concurrent access issues.

## v0.1.0

- Initial release of the project.
