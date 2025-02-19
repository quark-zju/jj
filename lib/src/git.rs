// Copyright 2020 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::{BTreeMap, HashSet};
use std::default::Default;
use std::path::PathBuf;

use git2::Oid;
use itertools::Itertools;
use thiserror::Error;

use crate::backend::{CommitId, ObjectId};
use crate::commit::Commit;
use crate::git_backend::NO_GC_REF_NAMESPACE;
use crate::op_store::RefTarget;
use crate::repo::{MutableRepo, Repo};
use crate::settings::GitSettings;
use crate::view::RefName;

#[derive(Error, Debug, PartialEq)]
pub enum GitImportError {
    #[error("Unexpected git error when importing refs: {0}")]
    InternalGitError(#[from] git2::Error),
}

fn parse_git_ref(ref_name: &str) -> Option<RefName> {
    if let Some(branch_name) = ref_name.strip_prefix("refs/heads/") {
        Some(RefName::LocalBranch(branch_name.to_string()))
    } else if let Some(remote_and_branch) = ref_name.strip_prefix("refs/remotes/") {
        remote_and_branch
            .split_once('/')
            .map(|(remote, branch)| RefName::RemoteBranch {
                remote: remote.to_string(),
                branch: branch.to_string(),
            })
    } else {
        ref_name
            .strip_prefix("refs/tags/")
            .map(|tag_name| RefName::Tag(tag_name.to_string()))
    }
}

fn prevent_gc(git_repo: &git2::Repository, id: &CommitId) {
    git_repo
        .reference(
            &format!("{}{}", NO_GC_REF_NAMESPACE, id.hex()),
            Oid::from_bytes(id.as_bytes()).unwrap(),
            true,
            "used by jj",
        )
        .unwrap();
}

/// Reflect changes made in the underlying Git repo in the Jujutsu repo.
pub fn import_refs(
    mut_repo: &mut MutableRepo,
    git_repo: &git2::Repository,
    git_settings: &GitSettings,
) -> Result<(), GitImportError> {
    import_some_refs(mut_repo, git_repo, git_settings, |_| true)
}

/// Reflect changes made in the underlying Git repo in the Jujutsu repo.
/// Only branches whose git full reference name pass the filter will be
/// considered for addition, update, or deletion.
pub fn import_some_refs(
    mut_repo: &mut MutableRepo,
    git_repo: &git2::Repository,
    git_settings: &GitSettings,
    git_ref_filter: impl Fn(&str) -> bool,
) -> Result<(), GitImportError> {
    let store = mut_repo.store().clone();
    let mut existing_git_refs = mut_repo.view().git_refs().clone();
    let mut old_git_heads = vec![];
    let mut new_git_heads = HashSet::new();
    for (ref_name, old_target) in &existing_git_refs {
        if git_ref_filter(ref_name) {
            old_git_heads.extend(old_target.adds());
        } else {
            new_git_heads.extend(old_target.adds());
        }
    }
    if let Some(old_git_head) = mut_repo.view().git_head() {
        old_git_heads.extend(old_git_head.adds());
    }

    // TODO: Should this be a separate function? We may not always want to import
    // the Git HEAD (and add it to our set of heads).
    if let Ok(head_git_commit) = git_repo
        .head()
        .and_then(|head_ref| head_ref.peel_to_commit())
    {
        let head_commit_id = CommitId::from_bytes(head_git_commit.id().as_bytes());
        let head_commit = store.get_commit(&head_commit_id).unwrap();
        new_git_heads.insert(head_commit_id.clone());
        prevent_gc(git_repo, &head_commit_id);
        mut_repo.add_head(&head_commit);
        mut_repo.set_git_head(RefTarget::Normal(head_commit_id));
    } else {
        mut_repo.clear_git_head();
    }

    let mut changed_git_refs = BTreeMap::new();
    let git_refs = git_repo.references()?;
    for git_ref in git_refs {
        let git_ref = git_ref?;
        if !(git_ref.is_tag() || git_ref.is_branch() || git_ref.is_remote())
            || git_ref.name().is_none()
        {
            // Skip other refs (such as notes) and symbolic refs, as well as non-utf8 refs.
            continue;
        }
        let full_name = git_ref.name().unwrap().to_string();
        if let Some(RefName::RemoteBranch { branch, remote: _ }) = parse_git_ref(&full_name) {
            // "refs/remotes/origin/HEAD" isn't a real remote-tracking branch
            if &branch == "HEAD" {
                continue;
            }
        }
        let git_commit = match git_ref.peel_to_commit() {
            Ok(git_commit) => git_commit,
            Err(_) => {
                // Perhaps a tag pointing to a GPG key or similar. Just skip it.
                continue;
            }
        };
        let id = CommitId::from_bytes(git_commit.id().as_bytes());
        new_git_heads.insert(id.clone());
        if !git_ref_filter(&full_name) {
            continue;
        }
        // TODO: Make it configurable which remotes are publishing and update public
        // heads here.
        let old_target = existing_git_refs.remove(&full_name);
        let new_target = Some(RefTarget::Normal(id.clone()));
        if new_target != old_target {
            prevent_gc(git_repo, &id);
            mut_repo.set_git_ref(full_name.clone(), RefTarget::Normal(id.clone()));
            let commit = store.get_commit(&id).unwrap();
            mut_repo.add_head(&commit);
            changed_git_refs.insert(full_name, (old_target, new_target));
        }
    }
    for (full_name, target) in existing_git_refs {
        if git_ref_filter(&full_name) {
            mut_repo.remove_git_ref(&full_name);
            changed_git_refs.insert(full_name, (Some(target), None));
        }
    }
    for (full_name, (old_git_target, new_git_target)) in changed_git_refs {
        if let Some(ref_name) = parse_git_ref(&full_name) {
            // Apply the change that happened in git since last time we imported refs
            mut_repo.merge_single_ref(&ref_name, old_git_target.as_ref(), new_git_target.as_ref());
            // If a git remote-tracking branch changed, apply the change to the local branch
            // as well
            if !git_settings.auto_local_branch {
                continue;
            }
            if let RefName::RemoteBranch { branch, remote: _ } = ref_name {
                mut_repo.merge_single_ref(
                    &RefName::LocalBranch(branch),
                    old_git_target.as_ref(),
                    new_git_target.as_ref(),
                );
            }
        }
    }

    // Find commits that are no longer referenced in the git repo and abandon them
    // in jj as well. We must remove non-existing commits from new_git_heads, as
    // they could have come from branches which were never fetched.
    let new_git_heads = new_git_heads
        .into_iter()
        .filter(|id| mut_repo.index().has_id(id))
        .collect_vec();
    // We could use mut_repo.record_rewrites() here but we know we only need to care
    // about abandoned commits for now. We may want to change this if we ever
    // add a way of preserving change IDs across rewrites by `git` (e.g. by
    // putting them in the commit message).
    let abandoned_commits = mut_repo
        .index()
        .walk_revs(&old_git_heads, &new_git_heads)
        .map(|entry| entry.commit_id())
        .collect_vec();
    let root_commit_id = mut_repo.store().root_commit_id().clone();
    for abandoned_commit in abandoned_commits {
        if abandoned_commit != root_commit_id {
            mut_repo.record_abandoned_commit(abandoned_commit);
        }
    }

    Ok(())
}

#[derive(Error, Debug, PartialEq)]
pub enum GitExportError {
    #[error("Cannot export conflicted branch '{0}'")]
    ConflictedBranch(String),
    #[error("Failed to read export state: {0}")]
    ReadStateError(String),
    #[error("Failed to write export state: {0}")]
    WriteStateError(String),
    #[error("Git error: {0}")]
    InternalGitError(#[from] git2::Error),
}

/// Reflect changes made in the Jujutsu repo compared to our current view of the
/// Git repo in `mut_repo.view().git_refs()`. Returns a list of names of
/// branches that failed to export.
// TODO: Also indicate why we failed to export these branches
pub fn export_refs(
    mut_repo: &mut MutableRepo,
    git_repo: &git2::Repository,
) -> Result<Vec<String>, GitExportError> {
    // First find the changes we want need to make without modifying mut_repo
    let mut branches_to_update = BTreeMap::new();
    let mut branches_to_delete = BTreeMap::new();
    let mut failed_branches = vec![];
    let view = mut_repo.view();
    let all_branch_names: HashSet<&str> = view
        .git_refs()
        .keys()
        .filter_map(|git_ref| git_ref.strip_prefix("refs/heads/"))
        .chain(view.branches().keys().map(AsRef::as_ref))
        .collect();
    for branch_name in all_branch_names {
        let old_branch = view.get_git_ref(&format!("refs/heads/{branch_name}"));
        let new_branch = view.get_local_branch(branch_name);
        if new_branch == old_branch {
            continue;
        }
        let old_oid = match old_branch {
            None => None,
            Some(RefTarget::Normal(id)) => Some(Oid::from_bytes(id.as_bytes()).unwrap()),
            Some(RefTarget::Conflict { .. }) => {
                // The old git ref should only be a conflict if there were concurrent import
                // operations while the value changed. Don't overwrite these values.
                failed_branches.push(branch_name.to_owned());
                continue;
            }
        };
        if let Some(new_branch) = new_branch {
            match new_branch {
                RefTarget::Normal(id) => {
                    let new_oid = Oid::from_bytes(id.as_bytes());
                    branches_to_update.insert(branch_name.to_owned(), (old_oid, new_oid.unwrap()));
                }
                RefTarget::Conflict { .. } => {
                    // Skip conflicts and leave the old value in git_refs
                    continue;
                }
            }
        } else {
            branches_to_delete.insert(branch_name.to_owned(), old_oid.unwrap());
        }
    }
    // TODO: Also check other worktrees' HEAD.
    if let Ok(head_ref) = git_repo.find_reference("HEAD") {
        if let (Some(head_git_ref), Ok(current_git_commit)) =
            (head_ref.symbolic_target(), head_ref.peel_to_commit())
        {
            if let Some(branch_name) = head_git_ref.strip_prefix("refs/heads/") {
                let detach_head =
                    if let Some((_old_oid, new_oid)) = branches_to_update.get(branch_name) {
                        *new_oid != current_git_commit.id()
                    } else {
                        branches_to_delete.contains_key(branch_name)
                    };
                if detach_head {
                    git_repo.set_head_detached(current_git_commit.id())?;
                }
            }
        }
    }
    for (branch_name, old_oid) in branches_to_delete {
        let git_ref_name = format!("refs/heads/{branch_name}");
        let success = if let Ok(mut git_ref) = git_repo.find_reference(&git_ref_name) {
            if git_ref.target() == Some(old_oid) {
                // The branch has not been updated by git, so go ahead and delete it
                git_ref.delete().is_ok()
            } else {
                // The branch was updated by git
                false
            }
        } else {
            // The branch is already deleted
            true
        };
        if success {
            mut_repo.remove_git_ref(&git_ref_name);
        } else {
            failed_branches.push(branch_name);
        }
    }
    for (branch_name, (old_oid, new_oid)) in branches_to_update {
        let git_ref_name = format!("refs/heads/{branch_name}");
        let success = match old_oid {
            None => {
                if let Ok(git_ref) = git_repo.find_reference(&git_ref_name) {
                    // The branch was added in jj and in git. Iff git already pointed it to our
                    // desired target, we're good
                    git_ref.target() == Some(new_oid)
                } else {
                    // The branch was added in jj but still doesn't exist in git, so add it
                    git_repo
                        .reference(&git_ref_name, new_oid, true, "export from jj")
                        .is_ok()
                }
            }
            Some(old_oid) => {
                // The branch was modified in jj. We can use libgit2's API for updating under a
                // lock.
                if git_repo
                    .reference_matching(&git_ref_name, new_oid, true, old_oid, "export from jj")
                    .is_ok()
                {
                    // Successfully updated from old_oid to new_oid (unchanged in git)
                    true
                } else {
                    // The reference was probably updated in git
                    if let Ok(git_ref) = git_repo.find_reference(&git_ref_name) {
                        // Iff it was updated to our desired target, we still consider it a success
                        git_ref.target() == Some(new_oid)
                    } else {
                        // The reference was deleted in git
                        false
                    }
                }
            }
        };
        if success {
            mut_repo.set_git_ref(
                git_ref_name,
                RefTarget::Normal(CommitId::from_bytes(new_oid.as_bytes())),
            );
        } else {
            failed_branches.push(branch_name);
        }
    }
    Ok(failed_branches)
}

#[derive(Error, Debug, PartialEq)]
pub enum GitFetchError {
    #[error("No git remote named '{0}'")]
    NoSuchRemote(String),
    #[error("Invalid glob provided. Globs may not contain the characters `:` or `^`.")]
    InvalidGlob,
    // TODO: I'm sure there are other errors possible, such as transport-level errors.
    #[error("Unexpected git error when fetching: {0}")]
    InternalGitError(#[from] git2::Error),
}

#[tracing::instrument(skip(mut_repo, git_repo, callbacks))]
pub fn fetch(
    mut_repo: &mut MutableRepo,
    git_repo: &git2::Repository,
    remote_name: &str,
    branch_name_globs: Option<&[&str]>,
    callbacks: RemoteCallbacks<'_>,
    git_settings: &GitSettings,
) -> Result<Option<String>, GitFetchError> {
    let mut remote =
        git_repo
            .find_remote(remote_name)
            .map_err(|err| match (err.class(), err.code()) {
                (git2::ErrorClass::Config, git2::ErrorCode::NotFound) => {
                    GitFetchError::NoSuchRemote(remote_name.to_string())
                }
                (git2::ErrorClass::Config, git2::ErrorCode::InvalidSpec) => {
                    GitFetchError::NoSuchRemote(remote_name.to_string())
                }
                _ => GitFetchError::InternalGitError(err),
            })?;
    let mut fetch_options = git2::FetchOptions::new();
    let mut proxy_options = git2::ProxyOptions::new();
    proxy_options.auto();
    fetch_options.proxy_options(proxy_options);
    let callbacks = callbacks.into_git();
    fetch_options.remote_callbacks(callbacks);
    let refspecs = {
        // If no globs have been given, import all branches
        let globs = branch_name_globs.unwrap_or(&["*"]);
        if globs.iter().any(|g| g.contains(|c| ":^".contains(c))) {
            return Err(GitFetchError::InvalidGlob);
        }
        // At this point, we are only updating Git's remote tracking branches, not the
        // local branches.
        globs
            .iter()
            .map(|glob| format!("+refs/heads/{glob}:refs/remotes/{remote_name}/{glob}"))
            .collect_vec()
    };
    tracing::debug!("remote.download");
    remote.download(&refspecs, Some(&mut fetch_options))?;
    tracing::debug!("remote.prune");
    remote.prune(None)?;
    tracing::debug!("remote.update_tips");
    remote.update_tips(None, false, git2::AutotagOption::Unspecified, None)?;
    // TODO: We could make it optional to get the default branch since we only care
    // about it on clone.
    let mut default_branch = None;
    if let Ok(default_ref_buf) = remote.default_branch() {
        if let Some(default_ref) = default_ref_buf.as_str() {
            // LocalBranch here is the local branch on the remote, so it's really the remote
            // branch
            if let Some(RefName::LocalBranch(branch_name)) = parse_git_ref(default_ref) {
                tracing::debug!(default_branch = branch_name);
                default_branch = Some(branch_name);
            }
        }
    }
    tracing::debug!("remote.disconnect");
    remote.disconnect()?;
    tracing::debug!("import_refs");
    if let Some(globs) = branch_name_globs {
        let pattern = format!(
            "^refs/remotes/{remote_name}/({})$",
            globs.iter().map(|glob| glob.replace('*', ".*")).join("|")
        );
        tracing::debug!(?globs, ?pattern, "globs as regex");
        let regex = regex::Regex::new(&pattern).map_err(|_| GitFetchError::InvalidGlob)?;
        import_some_refs(
            mut_repo,
            git_repo,
            git_settings,
            move |git_ref_name: &str| -> bool { regex.is_match(git_ref_name) },
        )
    } else {
        import_refs(mut_repo, git_repo, git_settings)
    }
    .map_err(|err| match err {
        GitImportError::InternalGitError(source) => GitFetchError::InternalGitError(source),
    })?;
    Ok(default_branch)
}

#[derive(Error, Debug, PartialEq)]
pub enum GitPushError {
    #[error("No git remote named '{0}'")]
    NoSuchRemote(String),
    #[error("Push is not fast-forwardable")]
    NotFastForward,
    #[error("Remote rejected the update of some refs (do you have permission to push to {0:?}?)")]
    RefUpdateRejected(Vec<String>),
    // TODO: I'm sure there are other errors possible, such as transport-level errors,
    // and errors caused by the remote rejecting the push.
    #[error("Unexpected git error when pushing: {0}")]
    InternalGitError(#[from] git2::Error),
}

pub fn push_commit(
    git_repo: &git2::Repository,
    target: &Commit,
    remote_name: &str,
    remote_branch: &str,
    // TODO: We want this to be an Option<CommitId> for the expected current commit on the remote.
    // It's a blunt "force" option instead until git2-rs supports the "push negotiation" callback
    // (https://github.com/rust-lang/git2-rs/issues/733).
    force: bool,
    callbacks: RemoteCallbacks<'_>,
) -> Result<(), GitPushError> {
    push_updates(
        git_repo,
        remote_name,
        &[GitRefUpdate {
            qualified_name: format!("refs/heads/{remote_branch}"),
            force,
            new_target: Some(target.id().clone()),
        }],
        callbacks,
    )
}

pub struct GitRefUpdate {
    pub qualified_name: String,
    // TODO: We want this to be a `current_target: Option<CommitId>` for the expected current
    // commit on the remote. It's a blunt "force" option instead until git2-rs supports the
    // "push negotiation" callback (https://github.com/rust-lang/git2-rs/issues/733).
    pub force: bool,
    pub new_target: Option<CommitId>,
}

pub fn push_updates(
    git_repo: &git2::Repository,
    remote_name: &str,
    updates: &[GitRefUpdate],
    callbacks: RemoteCallbacks<'_>,
) -> Result<(), GitPushError> {
    let mut temp_refs = vec![];
    let mut qualified_remote_refs = vec![];
    let mut refspecs = vec![];
    for update in updates {
        qualified_remote_refs.push(update.qualified_name.as_str());
        if let Some(new_target) = &update.new_target {
            // Create a temporary ref to work around https://github.com/libgit2/libgit2/issues/3178
            let temp_ref_name = format!("refs/jj/git-push/{}", new_target.hex());
            temp_refs.push(git_repo.reference(
                &temp_ref_name,
                git2::Oid::from_bytes(new_target.as_bytes()).unwrap(),
                true,
                "temporary reference for git push",
            )?);
            refspecs.push(format!(
                "{}{}:{}",
                (if update.force { "+" } else { "" }),
                temp_ref_name,
                update.qualified_name
            ));
        } else {
            refspecs.push(format!(":{}", update.qualified_name));
        }
    }
    let result = push_refs(
        git_repo,
        remote_name,
        &qualified_remote_refs,
        &refspecs,
        callbacks,
    );
    for mut temp_ref in temp_refs {
        // TODO: Figure out how to do the equivalent of absl::Cleanup for
        // temp_ref.delete().
        if let Err(err) = temp_ref.delete() {
            // Propagate error only if we don't already have an error to return and it's not
            // NotFound (there may be duplicates if the list if multiple branches moved to
            // the same commit).
            if result.is_ok() && err.code() != git2::ErrorCode::NotFound {
                return Err(GitPushError::InternalGitError(err));
            }
        }
    }
    result
}

fn push_refs(
    git_repo: &git2::Repository,
    remote_name: &str,
    qualified_remote_refs: &[&str],
    refspecs: &[String],
    callbacks: RemoteCallbacks<'_>,
) -> Result<(), GitPushError> {
    let mut remote =
        git_repo
            .find_remote(remote_name)
            .map_err(|err| match (err.class(), err.code()) {
                (git2::ErrorClass::Config, git2::ErrorCode::NotFound) => {
                    GitPushError::NoSuchRemote(remote_name.to_string())
                }
                (git2::ErrorClass::Config, git2::ErrorCode::InvalidSpec) => {
                    GitPushError::NoSuchRemote(remote_name.to_string())
                }
                _ => GitPushError::InternalGitError(err),
            })?;
    let mut remaining_remote_refs: HashSet<_> = qualified_remote_refs.iter().copied().collect();
    let mut push_options = git2::PushOptions::new();
    let mut proxy_options = git2::ProxyOptions::new();
    proxy_options.auto();
    push_options.proxy_options(proxy_options);
    let mut callbacks = callbacks.into_git();
    callbacks.push_update_reference(|refname, status| {
        // The status is Some if the ref update was rejected
        if status.is_none() {
            remaining_remote_refs.remove(refname);
        }
        Ok(())
    });
    push_options.remote_callbacks(callbacks);
    remote
        .push(refspecs, Some(&mut push_options))
        .map_err(|err| match (err.class(), err.code()) {
            (git2::ErrorClass::Reference, git2::ErrorCode::NotFastForward) => {
                GitPushError::NotFastForward
            }
            _ => GitPushError::InternalGitError(err),
        })?;
    drop(push_options);
    if remaining_remote_refs.is_empty() {
        Ok(())
    } else {
        Err(GitPushError::RefUpdateRejected(
            remaining_remote_refs
                .iter()
                .sorted()
                .map(|name| name.to_string())
                .collect(),
        ))
    }
}

#[non_exhaustive]
#[derive(Default)]
#[allow(clippy::type_complexity)]
pub struct RemoteCallbacks<'a> {
    pub progress: Option<&'a mut dyn FnMut(&Progress)>,
    pub get_ssh_key: Option<&'a mut dyn FnMut(&str) -> Option<PathBuf>>,
    pub get_password: Option<&'a mut dyn FnMut(&str, &str) -> Option<String>>,
    pub get_username_password: Option<&'a mut dyn FnMut(&str) -> Option<(String, String)>>,
}

impl<'a> RemoteCallbacks<'a> {
    fn into_git(mut self) -> git2::RemoteCallbacks<'a> {
        let mut callbacks = git2::RemoteCallbacks::new();
        if let Some(progress_cb) = self.progress {
            callbacks.transfer_progress(move |progress| {
                progress_cb(&Progress {
                    bytes_downloaded: (progress.received_objects() < progress.total_objects())
                        .then(|| progress.received_bytes() as u64),
                    overall: (progress.indexed_objects() + progress.indexed_deltas()) as f32
                        / (progress.total_objects() + progress.total_deltas()) as f32,
                });
                true
            });
        }
        // TODO: We should expose the callbacks to the caller instead -- the library
        // crate shouldn't read environment variables.
        callbacks.credentials(move |url, username_from_url, allowed_types| {
            let span = tracing::debug_span!("RemoteCallbacks.credentials");
            let _ = span.enter();

            let git_config = git2::Config::open_default();
            let credential_helper = git_config
                .and_then(|conf| git2::Cred::credential_helper(&conf, url, username_from_url));
            if let Ok(creds) = credential_helper {
                tracing::debug!("using credential_helper");
                return Ok(creds);
            } else if let Some(username) = username_from_url {
                if allowed_types.contains(git2::CredentialType::SSH_KEY) {
                    if std::env::var("SSH_AUTH_SOCK").is_ok()
                        || std::env::var("SSH_AGENT_PID").is_ok()
                    {
                        tracing::debug!(username, "using ssh_key_from_agent");
                        return git2::Cred::ssh_key_from_agent(username).map_err(|err| {
                            tracing::error!(err = %err);
                            err
                        });
                    }
                    if let Some(ref mut cb) = self.get_ssh_key {
                        if let Some(path) = cb(username) {
                            tracing::debug!(username, path = ?path, "using ssh_key");
                            return git2::Cred::ssh_key(username, None, &path, None).map_err(
                                |err| {
                                    tracing::error!(err = %err);
                                    err
                                },
                            );
                        }
                    }
                }
                if allowed_types.contains(git2::CredentialType::USER_PASS_PLAINTEXT) {
                    if let Some(ref mut cb) = self.get_password {
                        if let Some(pw) = cb(url, username) {
                            tracing::debug!(
                                username,
                                "using userpass_plaintext with username from url"
                            );
                            return git2::Cred::userpass_plaintext(username, &pw).map_err(|err| {
                                tracing::error!(err = %err);
                                err
                            });
                        }
                    }
                }
            } else if allowed_types.contains(git2::CredentialType::USER_PASS_PLAINTEXT) {
                if let Some(ref mut cb) = self.get_username_password {
                    if let Some((username, pw)) = cb(url) {
                        tracing::debug!(username, "using userpass_plaintext");
                        return git2::Cred::userpass_plaintext(&username, &pw).map_err(|err| {
                            tracing::error!(err = %err);
                            err
                        });
                    }
                }
            }
            tracing::debug!("using default");
            git2::Cred::default()
        });
        callbacks
    }
}

pub struct Progress {
    /// `Some` iff data transfer is currently in progress
    pub bytes_downloaded: Option<u64>,
    pub overall: f32,
}
