use std::io::{Read, Write};

use bstr::ByteSlice;
use gix_error::{ErrorExt, ExnResult, ResultExt, message};
use gix_transport::{
    client::{
        Capabilities, MessageKind, WriteMode,
        blocking_io::{ExtendedBufRead, HandleProgress, Transport},
    },
    packetline::{
        self,
        blocking_io::encode::{data_to_write, flush_to_write},
    },
};

use super::{Command, Options, Outcome};

/// Send `commands` to the remote on the other end of `transport`, whose `capabilities` were obtained with
/// a [`handshake()`](crate::handshake()) for [`Service::ReceivePack`](gix_transport::Service::ReceivePack),
/// followed by the pack written by `write_pack`, and return the server's report.
///
/// `write_pack` is called with the writer to put the pack into, unless all `commands` are deletions, in which case
/// no pack is sent (just like `git send-pack`).
/// Progress messages of the remote are passed to `remote_progress` along with `true` if they denote an error.
/// If `trace` is `true`, all packetlines sent and received are traced using `gix-trace`.
///
/// `commands` must not be empty, as there is nothing to do then.
pub fn push<'t, T, P>(
    transport: &'t mut T,
    capabilities: &Capabilities,
    commands: &[Command],
    write_pack: P,
    remote_progress: impl FnMut(bool, &[u8]) + 't,
    options: &Options,
    trace: bool,
) -> ExnResult<Outcome>
where
    T: Transport,
    P: FnOnce(&mut dyn Write) -> ExnResult,
{
    if commands.is_empty() {
        return Err(message("There are no reference updates to send to the remote").raise_erased());
    }
    let supports = |name: &str| capabilities.contains(name);
    let report_status = supports("report-status");
    let side_band = if supports("side-band-64k") {
        Some("side-band-64k")
    } else if supports("side-band") {
        Some("side-band")
    } else {
        None
    };
    if options.atomic && !supports("atomic") {
        return Err(message("The remote doesn't support atomic pushes").raise_erased());
    }
    if !options.push_options.is_empty() && !supports("push-options") {
        return Err(message("The remote doesn't support push options").raise_erased());
    }
    if commands.iter().any(Command::is_delete) && !supports("delete-refs") {
        return Err(message("The remote doesn't support deleting references").raise_erased());
    }

    let mut requested = Vec::new();
    if report_status {
        requested.push("report-status".to_owned());
    }
    if let Some(side_band) = side_band {
        requested.push(side_band.to_owned());
    }
    if options.quiet && supports("quiet") {
        requested.push("quiet".to_owned());
    }
    if options.atomic {
        requested.push("atomic".to_owned());
    }
    if commands.iter().any(Command::is_delete) {
        requested.push("delete-refs".to_owned());
    }
    if !options.push_options.is_empty() {
        requested.push("push-options".to_owned());
    }
    if supports("ofs-delta") {
        requested.push("ofs-delta".to_owned());
    }
    if let Some(format) = capabilities
        .capability("object-format")
        .and_then(|c| c.value().map(ToOwned::to_owned))
    {
        requested.push(format!("object-format={}", format.to_str_lossy()));
    }
    if let Some(agent) = &options.agent {
        requested.push(format!("agent={agent}"));
    }

    let writer = transport
        .request(WriteMode::Binary, MessageKind::Flush, trace)
        .or_raise_erased(|| message("Could not start sending the push request"))?;
    // Write the request verbatim: the commands are packet lines, but the pack that follows isn't.
    let (mut writer, mut reader) = writer.into_parts();
    for (index, command) in commands.iter().enumerate() {
        let mut line = command.to_line();
        if index == 0 {
            line.push(0);
            line.extend_from_slice(requested.join(" ").as_bytes());
        }
        line.push(b'\n');
        if trace {
            gix_trace::trace!(">> {}", line.as_bstr());
        }
        data_to_write(&line, &mut writer).or_raise_erased(|| message("Could not send a push command"))?;
    }
    flush_to_write(&mut writer).or_raise_erased(|| message("Could not send the end of the push commands"))?;
    if !options.push_options.is_empty() {
        for push_option in &options.push_options {
            data_to_write(format!("{push_option}\n").as_bytes(), &mut writer)
                .or_raise_erased(|| message("Could not send a push option"))?;
        }
        flush_to_write(&mut writer).or_raise_erased(|| message("Could not send the end of the push options"))?;
    }
    if !commands.iter().all(Command::is_delete) {
        write_pack(&mut writer)?;
    }
    writer.flush().or_raise_erased(|| message("Could not send the pack"))?;
    // Some transports only send the request once the writer is gone, so it has to be dropped before reading.
    drop(writer);

    if !report_status {
        // Without `report-status` the server has no way to tell us anything, but may still send progress.
        if side_band.is_some() {
            reader.set_progress_handler(Some(progress_handler(remote_progress)));
            let mut sink = Vec::new();
            reader.read_to_end(&mut sink).ok();
        }
        return Ok(Outcome {
            unpack: Ok(()),
            refs: Vec::new(),
            reported: false,
        });
    }

    let report = match side_band {
        Some(_) => {
            reader.set_progress_handler(Some(progress_handler(remote_progress)));
            let mut data = Vec::new();
            reader
                .read_to_end(&mut data)
                .or_raise_erased(|| message("Could not read the report of the remote"))?;
            // The report itself is a sequence of packet lines within the data band.
            decode_lines(&data)?
        }
        None => read_lines(&mut *reader)?,
    };
    if trace {
        for _line in &report {
            gix_trace::trace!("<< {}", _line.as_bstr());
        }
    }
    Outcome::from_report_lines(report.iter().map(Vec::as_slice))
        .or_raise_erased(|| message("Could not parse the report of the remote"))
}

fn progress_handler<'a>(mut f: impl FnMut(bool, &[u8]) + 'a) -> HandleProgress<'a> {
    Box::new(move |is_error: bool, text: &[u8]| {
        f(is_error, text);
        std::ops::ControlFlow::Continue(())
    })
}

/// Split `data` into the payloads of the packet lines it contains, stopping at the first flush packet.
fn decode_lines(mut data: &[u8]) -> ExnResult<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    while !data.is_empty() {
        let line =
            match packetline::decode::streaming(data).or_raise_erased(|| message("Invalid packet line in report"))? {
                packetline::decode::Stream::Complete { line, bytes_consumed } => {
                    data = &data[bytes_consumed..];
                    line
                }
                packetline::decode::Stream::Incomplete { .. } => {
                    return Err(message("The report of the remote ended in the middle of a packet line").raise_erased());
                }
            };
        match line.as_slice() {
            Some(payload) => out.push(payload.to_vec()),
            None => break,
        }
    }
    Ok(out)
}

/// Read packet lines until the flush packet the reader stops at.
fn read_lines(reader: &mut (dyn ExtendedBufRead<'_> + Unpin + '_)) -> ExnResult<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    while let Some(line) = reader.readline() {
        let line = line
            .or_raise_erased(|| message("Could not read the report of the remote"))?
            .or_raise_erased(|| message("Invalid packet line in report"))?;
        match line.as_slice() {
            Some(payload) => out.push(payload.to_vec()),
            None => break,
        }
    }
    Ok(out)
}
