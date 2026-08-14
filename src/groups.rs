//! Resolves `--group` arguments into per-user Linux group assignments.

use anyhow::Context as _;
use futures::{StreamExt as _, TryStreamExt as _, stream};
use std::{collections, str};

const MAX_CONCURRENT_TEAM_FETCHES: usize = 8;

/// A global Linux group or a GitHub-team-to-Linux-group mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupMapping {
    AddGroup(String),
    MapGitHubTeam {
        gh_team: String,
        linux_group: String,
    },
}

impl str::FromStr for GroupMapping {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some((gh_team, linux_group)) = s.split_once(':') {
            Ok(Self::MapGitHubTeam {
                gh_team: validate_team_slug(gh_team)?,
                linux_group: validate_group_name(linux_group)?,
            })
        } else {
            Ok(Self::AddGroup(validate_group_name(s)?))
        }
    }
}

/// True when `s` is non-empty and contains only lowercase letters, digits, '_' and '-',
/// the character set shared by GitHub team slugs and portable Linux group names
fn is_valid_name_charset(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Validate a GitHub team slug. Rejecting anything but slug characters at parse time
/// keeps a team name typed by mistake from reaching the API, where it would break the
/// request URI or resolve to a missing team.
fn validate_team_slug(slug: &str) -> anyhow::Result<String> {
    if !is_valid_name_charset(slug) {
        return Err(anyhow::anyhow!(
            "Invalid GitHub team slug '{}'. Use the team slug (lowercase letters, digits, '-' \
             and '_'), not the team name.",
            slug
        ));
    }
    Ok(slug.to_string())
}

/// Validate a Linux group name against the portable rules `groupadd` enforces on
/// common distros, so an invalid name fails at CLI parse time instead of failing
/// every sync once `groupadd` runs. On top of the shared charset, group names must
/// not start with a digit or end with '-', and are limited to 32 characters.
fn validate_group_name(group: &str) -> anyhow::Result<String> {
    let is_valid = is_valid_name_charset(group)
        && group.len() <= 32
        // Check that the first character is not a digit, which is equivalent to checking that it is
        // either a lowercase letter or '_', since the charset check already passed.
        && group
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && !group.ends_with('-');

    if !is_valid {
        return Err(anyhow::anyhow!(
            "Invalid group name '{}'. Group names must match [a-z_][a-z0-9_-]*, not end with \
             '-' and be at most 32 characters long.",
            group
        ));
    }
    Ok(group.to_string())
}

/// The Linux groups each synced user should be a member of, resolved once per sync
/// from the CLI mappings and the GitHub team memberships.
#[derive(Debug, Default)]
pub struct GroupAssignments {
    /// Groups every synced user is added to
    global: Vec<String>,
    /// Linux groups mapped from GitHub teams that exist in the org, including teams
    /// without members
    team_groups: Vec<String>,
    /// Additional groups per user, derived from GitHub team memberships
    per_user: collections::HashMap<octocrab::models::UserId, collections::BTreeSet<String>>,
}

impl GroupAssignments {
    /// Resolve mappings after checking mapped teams against the org's team list.
    pub async fn resolve(
        octocrab: &octocrab::Octocrab,
        org: &str,
        mappings: &[GroupMapping],
    ) -> anyhow::Result<Self> {
        let team_members = get_members_of_mapped_teams(octocrab, org, mappings).await?;
        Ok(Self::from_mappings(mappings, &team_members))
    }

    fn from_mappings(
        mappings: &[GroupMapping],
        team_members: &collections::HashMap<String, Vec<octocrab::models::UserId>>,
    ) -> Self {
        let mut assignments = Self::default();
        for mapping in mappings {
            match mapping {
                GroupMapping::AddGroup(group) => assignments.global.push(group.clone()),
                GroupMapping::MapGitHubTeam {
                    gh_team,
                    linux_group,
                } => {
                    let Some(members) = team_members.get(gh_team) else {
                        tracing::warn!(
                            team = gh_team,
                            group = linux_group,
                            "GitHub team does not exist in org, skipping mapping: the Linux \
                             group is not created and no longer assigned to any synced user"
                        );
                        continue;
                    };
                    assignments.team_groups.push(linux_group.clone());
                    for id in members {
                        assignments
                            .per_user
                            .entry(*id)
                            .or_default()
                            .insert(linux_group.clone());
                    }
                }
            }
        }
        assignments
    }

    /// All groups that must exist on the system before users are processed
    pub fn all_groups(&self) -> Vec<String> {
        let groups: collections::BTreeSet<&String> =
            self.global.iter().chain(self.team_groups.iter()).collect();
        groups.into_iter().cloned().collect()
    }

    /// The supplementary groups for a single user. This consists of the global groups plus the
    /// groups of the GitHub teams the user is a member of
    pub fn user_groups(&self, id: octocrab::models::UserId) -> Vec<String> {
        let mut groups: collections::BTreeSet<&String> = self.global.iter().collect();
        if let Some(team_groups) = self.per_user.get(&id) {
            groups.extend(team_groups.iter());
        }
        groups.into_iter().cloned().collect()
    }
}

/// Fetch the member IDs of every GitHub team referenced by a team mapping.
async fn get_members_of_mapped_teams(
    octocrab: &octocrab::Octocrab,
    org: &str,
    mappings: &[GroupMapping],
) -> anyhow::Result<collections::HashMap<String, Vec<octocrab::models::UserId>>> {
    let mapped_slugs: collections::BTreeSet<&str> = mappings
        .iter()
        .filter_map(|mapping| match mapping {
            GroupMapping::MapGitHubTeam { gh_team, .. } => Some(gh_team.as_str()),
            GroupMapping::AddGroup(_) => None,
        })
        .collect();
    if mapped_slugs.is_empty() {
        return Ok(collections::HashMap::new());
    }

    let existing_slugs = get_org_team_slugs(octocrab, org).await?;
    stream::iter(
        mapped_slugs
            .into_iter()
            .filter(|slug| existing_slugs.contains(*slug)),
    )
    .map(|slug| async move {
        get_team_members(octocrab, org, slug)
            .await
            .map(|members| (slug.to_string(), members))
    })
    .buffer_unordered(MAX_CONCURRENT_TEAM_FETCHES)
    .try_collect()
    .await
}

/// The slugs of all teams that currently exist in the org
async fn get_org_team_slugs(
    octocrab: &octocrab::Octocrab,
    org: &str,
) -> anyhow::Result<collections::HashSet<String>> {
    let context = || format!("Failed to list teams of org '{org}'");
    let teams: Vec<octocrab::models::teams::RequestedTeam> = octocrab
        .teams(org)
        .list()
        .per_page(crate::octosync::GITHUB_MAX_PER_PAGE)
        .send()
        .await
        .with_context(context)?
        .into_stream(octocrab)
        .try_collect()
        .await
        .with_context(context)?;
    Ok(teams.into_iter().map(|team| team.slug).collect())
}

async fn get_team_members(
    octocrab: &octocrab::Octocrab,
    org: &str,
    team_slug: &str,
) -> anyhow::Result<Vec<octocrab::models::UserId>> {
    let context = || format!("Failed to list members of team '{team_slug}' in org '{org}'");
    let members: Vec<octocrab::models::Author> = octocrab
        .teams(org)
        .members(team_slug)
        .per_page(crate::octosync::GITHUB_MAX_PER_PAGE)
        .send()
        .await
        .with_context(context)?
        .into_stream(octocrab)
        .try_collect()
        .await
        .with_context(context)?;
    Ok(members.into_iter().map(|member| member.id).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    mod validate_group_name {
        use super::*;

        #[test]
        fn valid_groups() {
            let groups = vec![
                "developers".to_string(),
                "team_alpha".to_string(),
                "ops-team".to_string(),
                "group123".to_string(),
            ];
            let result = groups
                .iter()
                .map(|group| validate_group_name(group))
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(result.len(), 4);
            assert_eq!(result, groups);
        }

        #[test]
        fn invalid_groups() {
            let groups = vec![
                "invalid group".to_string(),
                "toolonggroupname_exceeding_32_characters".to_string(),
                "invalid,comma".to_string(),
                "invalid$char".to_string(),
                "-foobar".to_string(),
                "foo-".to_string(),
                "Developers".to_string(),
                "1group".to_string(),
                "".to_string(),
            ];

            for group in groups {
                let result = validate_group_name(&group);
                assert!(
                    result.is_err(),
                    "Expected group '{}' to be invalid, but it was accepted",
                    group
                );
            }
        }
    }

    mod validate_team_slug {
        use super::*;

        #[test]
        fn valid_slugs() {
            for slug in ["backend", "ops-team", "team_2", "a"] {
                assert!(
                    validate_team_slug(slug).is_ok(),
                    "Expected slug '{}' to be valid, but it was rejected",
                    slug
                );
            }
        }

        #[test]
        fn invalid_slugs() {
            for slug in ["Backend Team", "Backend", "team/x", "", "a..b", "team?x"] {
                assert!(
                    validate_team_slug(slug).is_err(),
                    "Expected slug '{}' to be invalid, but it was accepted",
                    slug
                );
            }
        }
    }

    mod group_mapping_from_str {
        use super::*;
        use std::str::FromStr as _;

        #[test]
        fn plain_group_is_add_group() {
            let mapping = GroupMapping::from_str("developers").unwrap();
            assert_eq!(mapping, GroupMapping::AddGroup("developers".to_string()));
        }

        #[test]
        fn team_mapping_is_parsed() {
            let mapping = GroupMapping::from_str("gh-team:linux-group").unwrap();
            assert_eq!(
                mapping,
                GroupMapping::MapGitHubTeam {
                    gh_team: "gh-team".to_string(),
                    linux_group: "linux-group".to_string(),
                }
            );
        }

        #[test]
        fn empty_team_slug_is_rejected() {
            assert!(GroupMapping::from_str(":linux-group").is_err());
        }

        #[test]
        fn team_name_instead_of_slug_is_rejected() {
            assert!(GroupMapping::from_str("Backend Team:linux-group").is_err());
        }

        #[test]
        fn invalid_linux_group_in_mapping_is_rejected() {
            assert!(GroupMapping::from_str("gh-team:invalid group").is_err());
            assert!(GroupMapping::from_str("gh-team:").is_err());
        }
    }

    mod group_assignments {
        use super::*;

        fn add(group: &str) -> GroupMapping {
            GroupMapping::AddGroup(group.to_string())
        }

        fn map(gh_team: &str, linux_group: &str) -> GroupMapping {
            GroupMapping::MapGitHubTeam {
                gh_team: gh_team.to_string(),
                linux_group: linux_group.to_string(),
            }
        }

        fn teams(
            entries: &[(&str, &[u64])],
        ) -> collections::HashMap<String, Vec<octocrab::models::UserId>> {
            entries
                .iter()
                .map(|(team, ids)| {
                    (
                        team.to_string(),
                        ids.iter().map(|id| octocrab::models::UserId(*id)).collect(),
                    )
                })
                .collect()
        }

        #[test]
        fn global_groups_apply_to_every_user() {
            let assignments =
                GroupAssignments::from_mappings(&[add("developers"), add("ops")], &teams(&[]));

            assert_eq!(assignments.all_groups(), vec!["developers", "ops"]);
            assert_eq!(
                assignments.user_groups(octocrab::models::UserId(1)),
                vec!["developers", "ops"]
            );
        }

        #[test]
        fn team_members_get_the_mapped_group() {
            let assignments = GroupAssignments::from_mappings(
                &[map("backend", "backend-devs")],
                &teams(&[("backend", &[1, 2])]),
            );

            assert_eq!(assignments.all_groups(), vec!["backend-devs"]);
            assert_eq!(
                assignments.user_groups(octocrab::models::UserId(1)),
                vec!["backend-devs"]
            );
            assert!(
                assignments
                    .user_groups(octocrab::models::UserId(3))
                    .is_empty()
            );
        }

        /// A mapping whose GitHub team does not exist must neither create the Linux
        /// group nor assign it to any user.
        #[test]
        fn missing_team_is_skipped_entirely() {
            let assignments = GroupAssignments::from_mappings(
                &[map("no-such-team", "ghosts"), add("developers")],
                &teams(&[]),
            );

            assert_eq!(assignments.all_groups(), vec!["developers"]);
            assert_eq!(
                assignments.user_groups(octocrab::models::UserId(1)),
                vec!["developers"]
            );
        }

        #[test]
        fn existing_team_without_members_still_creates_the_group() {
            let assignments = GroupAssignments::from_mappings(
                &[map("backend", "backend-devs")],
                &teams(&[("backend", &[])]),
            );

            assert_eq!(assignments.all_groups(), vec!["backend-devs"]);
            assert!(
                assignments
                    .user_groups(octocrab::models::UserId(1))
                    .is_empty()
            );
        }

        #[test]
        fn user_in_multiple_teams_gets_all_mapped_groups() {
            let assignments = GroupAssignments::from_mappings(
                &[
                    add("developers"),
                    map("backend", "backend-devs"),
                    map("ops", "ops-team"),
                ],
                &teams(&[("backend", &[1]), ("ops", &[1, 2])]),
            );

            assert_eq!(
                assignments.all_groups(),
                vec!["backend-devs", "developers", "ops-team"]
            );
            assert_eq!(
                assignments.user_groups(octocrab::models::UserId(1)),
                vec!["backend-devs", "developers", "ops-team"]
            );
            assert_eq!(
                assignments.user_groups(octocrab::models::UserId(2)),
                vec!["developers", "ops-team"]
            );
        }

        #[test]
        fn duplicate_groups_are_deduplicated() {
            let assignments = GroupAssignments::from_mappings(
                &[add("devs"), map("backend", "devs"), map("frontend", "devs")],
                &teams(&[("backend", &[1]), ("frontend", &[1])]),
            );

            assert_eq!(assignments.all_groups(), vec!["devs"]);
            assert_eq!(
                assignments.user_groups(octocrab::models::UserId(1)),
                vec!["devs"]
            );
        }
    }
}
