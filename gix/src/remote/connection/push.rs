use std::sync::{Arc, atomic::AtomicBool};

use gix_error::{ResultExt, message};
use gix_hash::ObjectId;
use gix_hashtable::HashSet;
use gix_transport::client::blocking_io::Transport;

use crate::{
    Progress, Result,
    bstr::{BString, ByteSlice},
    remote::{
        Connection,
        push::{Options, Outcome, RefUpdate, Rejection, Status, Update},
    },
};

impl<T> Connection<'_, '_, '_, T>
where
    T: Transport,
{
    /// Update the references described by `updates` on the remote, sending all objects it needs, like `git push`.
    ///
    /// The remote is contacted with a handshake for `receive-pack` first, then each update is checked the way
    /// `git push` does: updates that aren't fast-forwards are [rejected](Rejection) unless they are
    /// [forced](Update::force), and updates the remote already has are skipped. The remaining ones are sent along
    /// with a pack of all objects reachable from them that aren't reachable from the remote's references.
    ///
    /// Progress messages of the remote are passed to `remote_progress` along with `true` if they are errors.
    ///
    /// Note that the connection can't be reused after a push, as the remote hangs up once it sent its report.
    pub fn push(
        mut self,
        updates: impl IntoIterator<Item = Update>,
        mut progress: impl Progress,
        remote_progress: impl FnMut(bool, &[u8]),
        options: Options,
    ) -> Result<Outcome> {
        let _span = gix_trace::coarse!("remote::Connection::push()");
        let repo = self.remote.repo;

        let mut credentials_storage;
        let url = self.transport.inner.to_url().into_owned();
        let authenticate = match self.authenticate.as_mut() {
            Some(f) => f,
            None => {
                credentials_storage = self.configured_credentials_for_current_url();
                &mut credentials_storage
            }
        };
        if self.transport_options.is_none() {
            self.transport_options = repo
                .transport_options(url.as_bstr(), self.remote.name().map(crate::remote::Name::as_bstr))
                .or_raise(|| message!("Failed to configure the transport before connecting to {url:?}"))?;
        }
        if let Some(config) = self.transport_options.as_ref() {
            self.transport
                .inner
                .configure(&**config)
                .or_raise(|| message("Failed to configure the transport layer"))?;
        }
        let handshake = gix_protocol::handshake(
            &mut self.transport.inner,
            gix_transport::Service::ReceivePack,
            authenticate,
            Vec::new(),
            &mut progress,
        )?;
        let remote_refs = handshake.refs.clone().unwrap_or_default();
        let remote_value = |name: &BString| -> Option<ObjectId> {
            remote_refs.iter().find_map(|r| match r {
                gix_protocol::handshake::Ref::Direct { full_ref_name, object }
                | gix_protocol::handshake::Ref::Symbolic {
                    full_ref_name, object, ..
                } if full_ref_name == name => Some(*object),
                gix_protocol::handshake::Ref::Peeled { full_ref_name, tag, .. } if full_ref_name == name => Some(*tag),
                _ => None,
            })
        };

        let mut ref_updates = Vec::new();
        let mut commands = Vec::new();
        for update in updates {
            let old = remote_value(&update.remote);
            let mut forced = false;
            let status = if old.is_none() && update.local.is_none() {
                Some(Status::Rejected(Rejection::NoSuchRef))
            } else if update.local == old {
                Some(Status::UpToDate)
            } else {
                match (old, update.local) {
                    (Some(old), Some(new)) if !update.force => {
                        if update.remote.starts_with(b"refs/tags/") {
                            Some(Status::Rejected(Rejection::AlreadyExists))
                        } else if repo.find_header(old).is_err() {
                            Some(Status::Rejected(Rejection::FetchFirst))
                        } else if !is_ancestor(repo, old, new)? {
                            Some(Status::Rejected(Rejection::NonFastForward))
                        } else {
                            None
                        }
                    }
                    (Some(old), Some(new)) => {
                        forced = repo.find_header(old).is_err() || !is_ancestor(repo, old, new)?;
                        None
                    }
                    _ => None,
                }
            };
            if status.is_none() {
                commands.push(gix_protocol::push::Command {
                    old_id: old.unwrap_or_else(|| ObjectId::null(repo.object_hash())),
                    new_id: update.local.unwrap_or_else(|| ObjectId::null(repo.object_hash())),
                    name: update.remote.clone(),
                });
            }
            ref_updates.push(RefUpdate {
                remote: update.remote,
                old,
                new: update.local,
                forced,
                status: status.unwrap_or(Status::Unreported),
            });
        }

        let mut outcome = Outcome {
            updates: ref_updates,
            remote_refs: remote_refs.clone(),
            unpack: Ok(()),
            objects_sent: 0,
            pack_bytes: 0,
        };
        let atomic_rejection =
            options.atomic && outcome.updates.iter().any(|u| matches!(u.status, Status::Rejected(_)));
        if commands.is_empty() || options.dry_run || atomic_rejection {
            if options.dry_run {
                for update in &mut outcome.updates {
                    if update.status == Status::Unreported {
                        update.status = Status::Ok;
                    }
                }
            }
            return Ok(outcome);
        }

        // What the remote has: everything reachable from its references that we have, too.
        let haves: Vec<ObjectId> = remote_refs
            .iter()
            .filter_map(|r| match r {
                gix_protocol::handshake::Ref::Direct { object, .. }
                | gix_protocol::handshake::Ref::Symbolic { object, .. }
                | gix_protocol::handshake::Ref::Peeled { object, .. } => Some(*object),
                gix_protocol::handshake::Ref::Unborn { .. } => None,
            })
            .filter(|id| repo.find_header(*id).is_ok())
            .collect();
        let tips: Vec<ObjectId> = commands.iter().filter(|c| !c.is_delete()).map(|c| c.new_id).collect();
        let objects = objects_to_send(repo, &tips, &haves)?;
        let num_objects = objects.len();

        let objects_dir = repo.objects.store_ref().path().to_owned();
        let object_hash = repo.object_hash();
        let mut pack_bytes = 0u64;
        let agent = repo.config.user_agent_tuple().1;
        let write_pack = |out: &mut dyn std::io::Write| -> gix_error::ExnResult {
            use gix_error::ResultExt as _;
            let store = gix_odb::Store::at_opts(objects_dir, object_hash, &mut std::iter::empty(), Default::default())
                .or_raise_erased(|| message("Could not open the object database to write the pack"))?;
            let db = Arc::new(store).to_cache_arc();
            let mut ids = objects.into_iter().map(Ok);
            let (counts, _) = gix_pack::data::output::count::objects_unthreaded(
                &db,
                &mut ids,
                &gix_features::progress::Discard,
                &AtomicBool::new(false),
                gix_pack::data::output::count::objects::ObjectExpansion::AsIs,
            )?;
            let entries = gix_features::parallel::InOrderIter::from(gix_pack::data::output::entry::iter_from_counts(
                counts,
                db,
                Box::new(gix_features::progress::Discard),
                gix_pack::data::output::entry::iter_from_counts::Options {
                    thread_limit: Some(1),
                    allow_thin_pack: false,
                    ..Default::default()
                },
            ));
            let mut bytes = gix_pack::data::output::bytes::FromEntriesIter::new(
                entries,
                out,
                num_objects as u32,
                gix_pack::data::Version::V2,
                object_hash,
            );
            for written in bytes.by_ref() {
                pack_bytes += written?;
            }
            Ok(())
        };
        let report = gix_protocol::push(
            &mut self.transport.inner,
            &handshake.capabilities,
            &commands,
            write_pack,
            remote_progress,
            &gix_protocol::push::Options {
                atomic: options.atomic,
                quiet: options.quiet,
                agent,
                push_options: options.push_options,
            },
            self.trace,
        )?;
        // The remote hangs up once it sent its report, there is nothing more to say.
        self.transport.assume_end_of_interaction();

        outcome.objects_sent = num_objects;
        outcome.pack_bytes = pack_bytes;
        outcome.unpack = report.unpack.clone();
        for update in &mut outcome.updates {
            if update.status != Status::Unreported {
                continue;
            }
            if !report.reported {
                continue;
            }
            update.status = match report.status_of(update.remote.as_bstr()) {
                Some(gix_protocol::push::RefStatus::Ok) => Status::Ok,
                Some(gix_protocol::push::RefStatus::Rejected(reason)) => Status::RemoteRejected(reason.clone()),
                None => Status::RemoteRejected("no status reported".into()),
            };
        }
        Ok(outcome)
    }
}

/// Return `true` if `ancestor` can be reached from `descendant`, peeling both to commits first.
fn is_ancestor(repo: &crate::Repository, ancestor: ObjectId, descendant: ObjectId) -> Result<bool> {
    let peel = |id: ObjectId| -> Option<ObjectId> {
        repo.find_object(id)
            .ok()?
            .peel_to_kind(gix_object::Kind::Commit)
            .ok()
            .map(|c| c.id)
    };
    let (Some(ancestor), Some(descendant)) = (peel(ancestor), peel(descendant)) else {
        return Ok(false);
    };
    if ancestor == descendant {
        return Ok(true);
    }
    for info in repo.rev_walk([descendant]).all()? {
        if info?.id == ancestor {
            return Ok(true);
        }
    }
    Ok(false)
}

/// All objects reachable from `tips` which aren't reachable from `haves`, commits first, in the order `git` would send them.
fn objects_to_send(repo: &crate::Repository, tips: &[ObjectId], haves: &[ObjectId]) -> Result<Vec<ObjectId>> {
    let mut out = Vec::new();
    let mut seen = HashSet::default();
    let mut commit_tips = Vec::new();
    // Annotated tags are sent along with what they point to.
    for tip in tips {
        let mut id = *tip;
        loop {
            let object = repo.find_object(id)?;
            match object.kind {
                gix_object::Kind::Tag => {
                    if seen.insert(id) {
                        out.push(id);
                    }
                    id = object.into_tag().target_id()?.detach();
                }
                gix_object::Kind::Commit => {
                    commit_tips.push(id);
                    break;
                }
                gix_object::Kind::Tree | gix_object::Kind::Blob => {
                    add_tree_or_blob(repo, id, &HashSet::default(), &mut seen, &mut out)?;
                    break;
                }
            }
        }
    }
    let mut hidden_commits = Vec::new();
    for have in haves {
        if let Ok(object) = repo.find_object(*have) {
            if let Ok(commit) = object.peel_to_kind(gix_object::Kind::Commit) {
                hidden_commits.push(commit.id);
            }
        }
    }
    // Objects the remote already has, approximated by the trees of the commits it points to.
    let mut uninteresting = HashSet::default();
    for commit in &hidden_commits {
        let tree = repo.find_commit(*commit)?.tree_id()?.detach();
        collect_tree(repo, tree, &mut uninteresting)?;
    }
    uninteresting.extend(hidden_commits.iter().copied());

    let walk = repo.rev_walk(commit_tips).with_hidden(hidden_commits).all()?;
    let mut commits = Vec::new();
    for info in walk {
        commits.push(info?.id);
    }
    for commit in &commits {
        if seen.insert(*commit) {
            out.push(*commit);
        }
    }
    for commit in commits {
        let tree = repo.find_commit(commit)?.tree_id()?.detach();
        add_tree_or_blob(repo, tree, &uninteresting, &mut seen, &mut out)?;
    }
    Ok(out)
}

fn collect_tree(repo: &crate::Repository, tree: ObjectId, out: &mut HashSet<ObjectId>) -> Result<()> {
    if !out.insert(tree) {
        return Ok(());
    }
    let tree = repo.find_tree(tree)?;
    for entry in tree.decode()?.entries {
        if entry.mode.is_commit() {
            continue;
        }
        if entry.mode.is_tree() {
            collect_tree(repo, entry.oid.to_owned(), out)?;
        } else {
            out.insert(entry.oid.to_owned());
        }
    }
    Ok(())
}

fn add_tree_or_blob(
    repo: &crate::Repository,
    id: ObjectId,
    uninteresting: &HashSet<ObjectId>,
    seen: &mut HashSet<ObjectId>,
    out: &mut Vec<ObjectId>,
) -> Result<()> {
    if uninteresting.contains(&id) || !seen.insert(id) {
        return Ok(());
    }
    out.push(id);
    let object = repo.find_object(id)?;
    if object.kind != gix_object::Kind::Tree {
        return Ok(());
    }
    let tree = object.into_tree();
    for entry in tree.decode()?.entries {
        if entry.mode.is_commit() {
            continue;
        }
        add_tree_or_blob(repo, entry.oid.to_owned(), uninteresting, seen, out)?;
    }
    Ok(())
}
