//! The client side of `git push` (the `send-pack` half of the `receive-pack` protocol), for protocol V0 and V1.
//!
//! Generally, there is the following order of operations.
//!
//! * create a `Transport`, either blocking or async
//! * perform a [`handshake()`](crate::handshake()) for [`Service::ReceivePack`](gix_transport::Service::ReceivePack),
//!   which yields the references of the remote
//! * decide which remote references to update, producing one [`Command`] per reference
//! * call [`push()`](function::push()), which sends the commands followed by a pack produced by the caller,
//!   and parses the server's [report](Outcome)
//!
//! Receiving a pack is the job of the server, so all this module needs from the caller is a way to write
//! the pack. That keeps it independent of how objects are stored and of how the pack is generated.
//!
//! Note that `receive-pack` doesn't support protocol V2, and servers always answer with a V0/V1 advertisement.
use bstr::{BString, ByteSlice};

/// A request to change a single reference on the remote.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Command {
    /// The value the remote reference has according to its advertisement, or the null id if it doesn't exist yet.
    ///
    /// The server refuses the update if this doesn't match its current value, which protects against
    /// concurrent pushes.
    pub old_id: gix_hash::ObjectId,
    /// The value to set the reference to, or the null id to delete it.
    pub new_id: gix_hash::ObjectId,
    /// The full name of the reference on the remote, like `refs/heads/main`.
    pub name: BString,
}

impl Command {
    /// Return `true` if this command deletes the remote reference.
    pub fn is_delete(&self) -> bool {
        self.new_id.is_null()
    }

    /// Return `true` if this command creates the remote reference.
    pub fn is_create(&self) -> bool {
        self.old_id.is_null()
    }

    /// Serialize this command as the payload of a packet line, without trailing newline.
    fn to_line(&self) -> BString {
        let mut out = BString::default();
        out.extend_from_slice(self.old_id.to_hex().to_string().as_bytes());
        out.push(b' ');
        out.extend_from_slice(self.new_id.to_hex().to_string().as_bytes());
        out.push(b' ');
        out.extend_from_slice(&self.name);
        out
    }
}

/// Options for [`push()`](function::push()).
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// If `true`, ask the server to apply all updates or none of them, which fails if the server doesn't support it.
    pub atomic: bool,
    /// If `true`, ask the server not to send progress messages.
    pub quiet: bool,
    /// The value of the `agent` capability, like `git/gix-1.0`. If `None`, no agent is sent.
    pub agent: Option<String>,
    /// Values to send with the `push-options` capability, which fails if the server doesn't support it.
    pub push_options: Vec<String>,
}

/// The result of a single reference update as reported by the server.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RefStatus {
    /// The update was applied.
    Ok,
    /// The update was refused, with the reason the server gave, like `non-fast-forward`.
    Rejected(BString),
}

/// The report of the server after it received the commands and the pack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// `Ok(())` if the server could unpack the pack, or `Err(reason)` if it couldn't.
    ///
    /// If `report-status` isn't supported by the server, this is always `Ok(())`, as the server
    /// has no way to report failure.
    pub unpack: Result<(), BString>,
    /// One entry per command, in the order of the server's report. Empty if the server doesn't support `report-status`,
    /// in which case the caller can only assume all updates were applied.
    pub refs: Vec<(BString, RefStatus)>,
    /// `true` if the server supported `report-status`, i.e. if `unpack` and `refs` carry the server's view.
    pub reported: bool,
}

impl Outcome {
    /// Return the status the server reported for the reference `name`, if any.
    pub fn status_of(&self, name: &bstr::BStr) -> Option<&RefStatus> {
        self.refs
            .iter()
            .find_map(|(n, status)| (n.as_bstr() == name).then_some(status))
    }

    /// Parse the lines of a `report-status` response, one line per call to `lines`.
    pub fn from_report_lines<'a>(lines: impl IntoIterator<Item = &'a [u8]>) -> Result<Self, gix_error::Message> {
        let mut unpack = None;
        let mut refs = Vec::new();
        for line in lines {
            let line = line.trim_end_with(|c| c == '\n');
            if let Some(rest) = line.strip_prefix(b"unpack ") {
                unpack = Some(if rest == b"ok" { Ok(()) } else { Err(rest.into()) });
            } else if let Some(rest) = line.strip_prefix(b"ok ") {
                refs.push((rest.into(), RefStatus::Ok));
            } else if let Some(rest) = line.strip_prefix(b"ng ") {
                let (name, reason) = match rest.find_byte(b' ') {
                    Some(pos) => (&rest[..pos], &rest[pos + 1..]),
                    None => (rest, &b""[..]),
                };
                refs.push((name.into(), RefStatus::Rejected(reason.into())));
            } else if line.is_empty() {
                continue;
            } else {
                return Err(gix_error::message!(
                    "Unexpected line in report-status of the remote: {:?}",
                    line.as_bstr()
                ));
            }
        }
        let unpack =
            unpack.ok_or_else(|| gix_error::message("The remote didn't report whether it could unpack the pack"))?;
        Ok(Outcome {
            unpack,
            refs,
            reported: true,
        })
    }
}

#[cfg(feature = "blocking-client")]
pub(crate) mod function;

#[cfg(test)]
mod tests {
    use super::{Outcome, RefStatus};

    #[test]
    fn report_status_is_parsed() {
        let outcome = Outcome::from_report_lines([
            &b"unpack ok\n"[..],
            b"ok refs/heads/main\n",
            b"ng refs/heads/other pre-receive hook declined\n",
        ])
        .unwrap();
        assert_eq!(outcome.unpack, Ok(()));
        assert_eq!(outcome.status_of("refs/heads/main".into()), Some(&RefStatus::Ok));
        assert_eq!(
            outcome.status_of("refs/heads/other".into()),
            Some(&RefStatus::Rejected("pre-receive hook declined".into()))
        );
        assert!(outcome.reported);
    }

    #[test]
    fn unpack_failure_is_kept() {
        let outcome = Outcome::from_report_lines([
            &b"unpack index-pack abnormal exit\n"[..],
            b"ng refs/heads/main unpacker error\n",
        ])
        .unwrap();
        assert_eq!(outcome.unpack, Err("index-pack abnormal exit".into()));
    }

    #[test]
    fn missing_unpack_line_is_an_error() {
        assert!(Outcome::from_report_lines([&b"ok refs/heads/main\n"[..]]).is_err());
    }
}
