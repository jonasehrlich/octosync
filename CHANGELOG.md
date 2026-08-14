# Changelog

## Unreleased

- Archive home directories of deleted users in the data directory
  ([#9](https://github.com/jonasehrlich/octosync/pull/9))
- Only delete users whose GitHub ID is missing from the fetched org member list. Users whose
  processing failed stay in the store unchanged.
  ([#15](https://github.com/jonasehrlich/octosync/pull/15))
- A failed authorized keys fetch no longer fails user processing which could lead to the deletion of
  users whose keys could not be fetched. ([#15](https://github.com/jonasehrlich/octosync/pull/15))
- Fetch public keys through the authenticated GitHub client instead of the anonymous, rate-limited
  endpoint ([#15](https://github.com/jonasehrlich/octosync/pull/15))
- Manage SSH fetched keys in a marked, octosync-managed block inside `authorized_keys`
  ([#15](https://github.com/jonasehrlich/octosync/pull/15))
- Write `authorized_keys` atomically, never follow symlinks and enforce exact permissions on `.ssh`
  and `authorized_keys` ([#15](https://github.com/jonasehrlich/octosync/pull/15))
- Refuse a sync that would delete every stored user
  ([#17](https://github.com/jonasehrlich/octosync/pull/17))
- Implement mapping GitHub teams to Linux groups with `--group <gh-team-slug>:<linux-group>`
  ([#20](https://github.com/jonasehrlich/octosync/pull/20))
- Re-create stored users whose account disappeared from the system, passing the stored name and UID
  to `useradd` so file ownership survives a delete and re-create cycle
  ([#19](https://github.com/jonasehrlich/octosync/pull/19))
- Refuse to update a user whose stored UID or name belongs to a different account, instead of
  renaming that account and handing it the GitHub user's SSH keys
  ([#19](https://github.com/jonasehrlich/octosync/pull/19))
- Kill a user's processes a second time right before `userdel`, so a process spawned while the home
  directory was archived cannot fail the deletion
  ([#19](https://github.com/jonasehrlich/octosync/pull/19))
- Stop user processes gracefully: send SIGTERM first and SIGKILL only the processes still running
  after a grace period, then wait for the killed processes to leave the process table
  ([#19](https://github.com/jonasehrlich/octosync/pull/19))
- Expire an account at the day before its deletion starts, so no new SSH session can begin while the
  home directory is archived. Syncing a user whose account survived a failed deletion lifts an
  expiry that is already in effect. A future expiry date set by an operator stays.
  ([#19](https://github.com/jonasehrlich/octosync/pull/19))
- Store `users.json` in schema v2: users deleted by octosync are kept as tombstones under an
  `archived` key, so a member who leaves and rejoins gets their old UID back. The v1 file is backed
  up to `users-v1.json` once before the first v2 save.
  ([#22](https://github.com/jonasehrlich/octosync/pull/22))
- Save the tombstone before `userdel` runs and re-enqueue archived users whose account still exists
  on every sync, so a deletion interrupted at any point is finished by a later sync instead of
  orphaning a live account ([#22](https://github.com/jonasehrlich/octosync/pull/22))
- Record the home directory archive path on the tombstone, so a rejoin can later restore it
  ([#22](https://github.com/jonasehrlich/octosync/pull/22))
- The `delete` command keeps the store file and writes tombstones instead of removing `users.json`,
  preserving the UID memory of a full wipe ([#22](https://github.com/jonasehrlich/octosync/pull/22))

## v0.3.0

- Added a `--version` flag to display the current version of the application.

## v0.2.0

- Added group sync with the `--group` option.
- Added locking of the data directory to prevent concurrent access issues.

## v0.1.0

- Initial release of the project.
