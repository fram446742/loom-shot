//! xdg-desktop-portal ScreenCast negotiation for the `portal` recording backend.
//!
//! The portal owns source selection and PipeWire stream creation. We only run
//! the request/response dance and hand the resulting PipeWire node to
//! `gst-launch-1.0`, which does the decoding and encoding. No PipeWire client
//! or encoder is linked into loom-shot.

use std::collections::HashMap;

use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedValue, Value};

const DEST: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";

/// What the portal picker should offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Monitor/output selection (`types=1`).
    Monitor,
    /// Window selection (`types=2`).
    Window,
}

/// A negotiated PipeWire stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stream {
    pub node_id: u32,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

impl Stream {
    /// Placeholder used by dry runs; never negotiates.
    pub fn placeholder() -> Self {
        Self {
            node_id: 0,
            width: None,
            height: None,
        }
    }
}

/// A negotiated stream plus its D-Bus session.
///
/// The session must stay alive for as long as the PipeWire node is consumed;
/// dropping the connection would close the portal session and stop the stream.
pub struct Session {
    pub stream: Stream,
    connection: Connection,
    path: zbus::zvariant::OwnedObjectPath,
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Ok(proxy) = Proxy::new(
            &self.connection,
            DEST,
            self.path.as_str(),
            "org.freedesktop.portal.Session",
        ) {
            let _ = proxy.call::<_, _, ()>("Close", &());
        }
    }
}

/// Negotiates one ScreenCast stream.
///
/// This blocks while the user picks a source in the portal dialog.
pub fn open(source: Source) -> Result<Session, String> {
    let connection =
        Connection::session().map_err(|err| format!("cannot connect to the session bus: {err}"))?;
    let sender = connection
        .unique_name()
        .map(|name| name.as_str().trim_start_matches(':').replace('.', "_"))
        .ok_or_else(|| "the session bus has no unique name".to_owned())?;
    let screencast = Proxy::new(&connection, DEST, PATH, "org.freedesktop.portal.ScreenCast")
        .map_err(|err| format!("cannot create the ScreenCast proxy: {err}"))?;

    let base = format!("loom_shot_{}", std::process::id());
    let identify = format!("{base}_id");
    let (request, mut responses) = request_proxy(&connection, &sender, &identify)?;
    let options = make_options([
        ("handle_token", Value::from(identify.clone())),
        (
            "session_handle_token",
            Value::from(format!("{base}_session")),
        ),
    ]);
    let _: zbus::zvariant::OwnedObjectPath = screencast
        .call("CreateSession", &(options,))
        .map_err(|err| format!("CreateSession failed: {err}"))?;
    drop(request);
    let results = wait_response(&mut responses, "CreateSession")?;
    drop(responses);
    let session = results
        .get("session_handle")
        .and_then(|value| {
            zbus::zvariant::OwnedObjectPath::try_from(value.clone())
                .ok()
                .or_else(|| {
                    String::try_from(value.clone())
                        .ok()
                        .and_then(|path| zbus::zvariant::OwnedObjectPath::try_from(path).ok())
                })
        })
        .ok_or_else(|| "the portal returned no session handle".to_owned())?;

    let select = format!("{base}_select");
    let (request, mut responses) = request_proxy(&connection, &sender, &select)?;
    let options = make_options([
        ("handle_token", Value::from(select)),
        (
            "types",
            Value::from(match source {
                Source::Monitor => 1u32,
                Source::Window => 2u32,
            }),
        ),
        ("multiple", Value::from(false)),
        ("cursor_mode", Value::from(2u32)),
    ]);
    let _: zbus::zvariant::OwnedObjectPath = screencast
        .call("SelectSources", &(session.clone(), options))
        .map_err(|err| format!("SelectSources failed: {err}"))?;
    drop(request);
    wait_response(&mut responses, "SelectSources")?;
    drop(responses);

    let start = format!("{base}_start");
    let (request, mut responses) = request_proxy(&connection, &sender, &start)?;
    let options = make_options([("handle_token", Value::from(start))]);
    let _: zbus::zvariant::OwnedObjectPath = screencast
        .call("Start", &(session.clone(), "", options))
        .map_err(|err| format!("Start failed: {err}"))?;
    drop(request);
    let results = wait_response(&mut responses, "Start")?;
    drop(responses);

    let streams = results
        .get("streams")
        .and_then(|value| Vec::<(u32, HashMap<String, OwnedValue>)>::try_from(value.clone()).ok())
        .ok_or_else(|| "the portal returned no screencast streams".to_owned())?;
    let Some((node_id, properties)) = streams.into_iter().next() else {
        return Err("the portal returned an empty screencast stream list".to_owned());
    };
    // The optional size/position properties are geometry hints, not needed for
    // the pipeline (pipewiresrc negotiates its own caps).
    let _ = properties;
    drop(screencast);
    Ok(Session {
        stream: Stream {
            node_id,
            width: None,
            height: None,
        },
        connection,
        path: session,
    })
}

fn make_options(
    entries: impl IntoIterator<Item = (&'static str, Value<'static>)>,
) -> HashMap<String, Value<'static>> {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

/// Subscribes to the request's Response before the method is called.
///
/// The portal returns the request handle immediately but emits Response on a
/// predictable object path, so subscribing first avoids the race.
fn request_proxy<'a>(
    connection: &'a Connection,
    sender: &str,
    token: &str,
) -> Result<(Proxy<'a>, zbus::blocking::proxy::SignalIterator<'a>), String> {
    let path = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");
    let proxy = Proxy::new_owned(
        connection.clone(),
        DEST,
        zbus::zvariant::OwnedObjectPath::try_from(path)
            .map_err(|err| format!("invalid portal request path: {err}"))?,
        "org.freedesktop.portal.Request",
    )
    .map_err(|err| format!("cannot create the request proxy: {err}"))?;
    let responses = proxy
        .receive_signal("Response")
        .map_err(|err| format!("cannot subscribe to the portal response: {err}"))?;
    Ok((proxy, responses))
}

fn wait_response(
    responses: &mut zbus::blocking::proxy::SignalIterator<'_>,
    method: &str,
) -> Result<HashMap<String, OwnedValue>, String> {
    let message = responses
        .next()
        .ok_or_else(|| format!("the portal closed the {method} response stream"))?;
    let (code, results): (u32, HashMap<String, OwnedValue>) = message
        .body()
        .deserialize()
        .map_err(|err| format!("cannot parse the {method} response: {err}"))?;
    match code {
        0 => Ok(results),
        1 => Err(format!("{method} was cancelled in the portal dialog")),
        other => Err(format!("{method} failed with portal response code {other}")),
    }
}

/// Builds the `gst-launch-1.0` arguments for a negotiated stream.
pub fn pipeline_args(
    stream: &Stream,
    codec: crate::record::Codec,
    container: crate::record::Container,
    fps: Option<u32>,
    path: &std::path::Path,
) -> Vec<String> {
    use crate::record::{Codec, Container};

    let node = if stream.node_id == 0 {
        "PORTAL".to_owned()
    } else {
        stream.node_id.to_string()
    };
    let caps = match fps {
        Some(fps) => format!("video/x-raw,format=I420,framerate={fps}/1"),
        None => "video/x-raw,format=I420".to_owned(),
    };
    let (encoder, parser) = match codec {
        Codec::Auto | Codec::H264 => ("x264enc", "h264parse"),
        Codec::Hevc => ("x265enc", "h265parse"),
        Codec::Av1 => ("av1enc", "av1parse"),
    };
    let muxer = match container {
        Container::Mp4 => "mp4mux",
        Container::Mkv => "matroskamux",
        Container::WebM => "webmmux",
    };

    let mut args: Vec<String> = [
        "pipewiresrc",
        &format!("path={node}"),
        "!",
        "videoconvert",
        "!",
        "videoscale",
        "!",
        &caps,
        "!",
        encoder,
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    if matches!(codec, Codec::H264 | Codec::Hevc) {
        args.push("tune=zerolatency".to_owned());
        args.push("speed-preset=veryfast".to_owned());
    }
    args.extend(["!", parser, "!", muxer].map(str::to_owned));
    args.push("!".to_owned());
    args.push("filesink".to_owned());
    args.push(format!("location={}", path.display()));
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{Codec, Container};

    #[test]
    fn pipeline_uses_the_stream_node_and_requested_codec() {
        let stream = Stream {
            node_id: 42,
            width: Some(1920),
            height: Some(1080),
        };
        let args = pipeline_args(
            &stream,
            Codec::Hevc,
            Container::Mkv,
            Some(30),
            std::path::Path::new("/tmp/out.mkv"),
        );
        let line = args.join(" ");
        assert!(line.contains("pipewiresrc path=42"), "{line}");
        assert!(line.contains("framerate=30/1"), "{line}");
        assert!(line.contains("x265enc"), "{line}");
        assert!(line.contains("h265parse"), "{line}");
        assert!(line.contains("matroskamux"), "{line}");
        assert!(line.ends_with("location=/tmp/out.mkv"), "{line}");
    }

    #[test]
    fn pipeline_placeholder_is_explicit_for_dry_runs() {
        let args = pipeline_args(
            &Stream::placeholder(),
            Codec::H264,
            Container::Mp4,
            None,
            std::path::Path::new("/tmp/out.mp4"),
        );
        let line = args.join(" ");
        assert!(line.contains("pipewiresrc path=PORTAL"), "{line}");
        assert!(line.contains("x264enc"), "{line}");
        assert!(line.contains("mp4mux"), "{line}");
    }
}
