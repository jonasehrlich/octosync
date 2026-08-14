# Changelog

## Unreleased

- Departed members are now locked out instead of immediately deleted: octosync expires their
  account, ends their sessions, removes their crontab and queued `at` jobs, and strips supplementary
  groups while keeping the account and home directory intact. Rejoining reverses the expiry and
  restores access to the same files and IDs. The `delete` command now follows this reversible
  departure flow as well. ([#25](https://github.com/jonasehrlich/octosync/pull/25))
- Expired accounts are permanently purged after a configurable retention period (180 days by
  default), either during sync or with the new `purge` command. Purging removes the home directory
  without an archive, but reserves the departed member's UID and GID for a later rejoin.
  ([#25](https://github.com/jonasehrlich/octosync/pull/25))
- Sync no longer offboards members because their processing or key fetch failed, and refuses a sync
  that would expire every stored user. Dry runs no longer write preview state to `users.json`.
  ([#15](https://github.com/jonasehrlich/octosync/pull/15),
  [#17](https://github.com/jonasehrlich/octosync/pull/17),
  [#25](https://github.com/jonasehrlich/octosync/pull/25))
- Account identity is preserved across departures, manual account removal and rejoins. octosync
  restores stored UIDs and GIDs, prevents their reuse by other members, and refuses unsafe login or
  ID collisions rather than modifying the wrong account.
  ([#19](https://github.com/jonasehrlich/octosync/pull/19),
  [#22](https://github.com/jonasehrlich/octosync/pull/22),
  [#25](https://github.com/jonasehrlich/octosync/pull/25))
- GitHub SSH keys are now fetched through the authenticated client and updated inside an
  octosync-managed block, preserving manually managed keys. Existing keys survive fetch failures,
  and updates are atomic and protected against unsafe permissions and symlinks.
  ([#15](https://github.com/jonasehrlich/octosync/pull/15))
- GitHub teams can now be mapped to Linux groups with `--group <gh-team-slug>:<linux-group>`.
  ([#20](https://github.com/jonasehrlich/octosync/pull/20))
- `users.json` is automatically migrated to a versioned v2 format that records departed and purged
  members. The original v1 file is backed up once as `users-v1.json`.
  ([#22](https://github.com/jonasehrlich/octosync/pull/22),
  [#25](https://github.com/jonasehrlich/octosync/pull/25))

## v0.3.0

- Added a `--version` flag to display the current version of the application.

## v0.2.0

- Added group sync with the `--group` option.
- Added locking of the data directory to prevent concurrent access issues.

## v0.1.0

- Initial release of the project.
