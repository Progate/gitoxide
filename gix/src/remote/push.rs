//! Types for [`Connection::push()`](crate::remote::Connection::push()), the client side of `git push`.
use crate::bstr::BString;

/// A single reference to update on the remote.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Update {
    /// The object to point the remote reference to, typically a commit, or `None` to delete the remote reference.
    pub local: Option<gix_hash::ObjectId>,
    /// The full name of the reference on the remote, like `refs/heads/main`.
    pub remote: BString,
    /// If `true`, the update is performed even if it isn't a fast-forward, like `+refs/heads/main:refs/heads/main`.
    pub force: bool,
}

/// Why an [`Update`] was refused before anything was sent to the remote, mirroring the reasons `git push` gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Rejection {
    /// The remote reference points to an object we don't have, so the update can't be known to be a fast-forward.
    ///
    /// This is `git`'s `fetch first`.
    FetchFirst,
    /// The remote reference points to a commit that isn't an ancestor of the local one.
    NonFastForward,
    /// A tag already exists on the remote and would be changed.
    AlreadyExists,
    /// A deletion was requested for a reference the remote doesn't have.
    NoSuchRef,
}

/// What happened to an [`Update`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Status {
    /// The remote reference already had the desired value, nothing was sent for it.
    UpToDate,
    /// The update was refused locally, and nothing was sent for it.
    Rejected(Rejection),
    /// The remote applied the update.
    Ok,
    /// The remote refused the update, with the reason it gave, like `pre-receive hook declined`.
    RemoteRejected(BString),
    /// The update was sent, but the remote didn't report whether it applied it because it
    /// doesn't support `report-status`.
    Unreported,
}

/// The fate of a single [`Update`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RefUpdate {
    /// The full name of the remote reference.
    pub remote: BString,
    /// The value of the remote reference before the push, or `None` if it didn't exist.
    pub old: Option<gix_hash::ObjectId>,
    /// The value the remote reference was supposed to receive, or `None` for deletions.
    pub new: Option<gix_hash::ObjectId>,
    /// `true` if the update isn't a fast-forward and was forced.
    pub forced: bool,
    /// What happened to it.
    pub status: Status,
}

/// Options for [`Connection::push()`](crate::remote::Connection::push()).
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// If `true`, ask the remote to apply all updates or none.
    pub atomic: bool,
    /// If `true`, ask the remote to not send progress messages.
    pub quiet: bool,
    /// If `true`, determine what would be done, but don't send anything.
    pub dry_run: bool,
    /// Push options to pass to the hooks of the remote (`git push -o`).
    pub push_options: Vec<String>,
}

/// The outcome of [`Connection::push()`](crate::remote::Connection::push()).
#[derive(Debug, Clone)]
pub struct Outcome {
    /// One entry per requested [`Update`], in the order they were given.
    pub updates: Vec<RefUpdate>,
    /// The references the remote advertised before the push.
    pub remote_refs: Vec<gix_protocol::handshake::Ref>,
    /// `Ok(())` if the remote could unpack the pack or nothing had to be sent, or `Err(reason)` otherwise.
    pub unpack: Result<(), BString>,
    /// The amount of objects in the pack that was sent, which is `0` if no pack was sent.
    pub objects_sent: usize,
    /// The size of the pack in bytes.
    pub pack_bytes: u64,
}

impl Outcome {
    /// Return `true` if every update was either up to date or applied.
    pub fn is_success(&self) -> bool {
        self.unpack.is_ok()
            && self
                .updates
                .iter()
                .all(|u| matches!(u.status, Status::UpToDate | Status::Ok | Status::Unreported))
    }
}
