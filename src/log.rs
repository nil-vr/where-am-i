use std::{
    ffi::OsStr,
    io,
    path::{Path, PathBuf},
    pin::Pin,
    str,
    task::{self, Poll},
    time::Duration,
};

use anyhow::Context;
use async_stream::try_stream;
use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use futures::{channel::mpsc, Stream, TryStream, TryStreamExt};
use notify::{event::CreateKind, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use pin_project_lite::pin_project;
use tokio::{
    fs::File,
    io::{AsyncBufReadExt, AsyncRead, BufReader, ReadBuf},
    time::{interval, Interval, MissedTickBehavior},
};
use tracing::debug;
#[cfg(windows)]
use windows::Storage::UserDataPaths;

use crate::RoomId;

#[cfg(windows)]
pub fn autodetect_path() -> anyhow::Result<PathBuf> {
    let paths = UserDataPaths::GetDefault().context("UserDataPath error")?;
    let hstring = paths
        .LocalAppDataLow()
        .context("UserDataPath::LocalAppDataLow error")?;
    let mut path = PathBuf::from(hstring.to_os_string());
    path.push("VRChat\\VRChat");

    debug!(?path, "Found VRChat log directory");

    Ok(path)
}

#[cfg(not(windows))]
pub fn autodetect_path() -> anyhow::Result<PathBuf> {
    anyhow::bail!("Set logs_path in where-am-i.toml to the location of your VRChat log files")
}

pub fn log_events(path: impl AsRef<Path>) -> impl Stream<Item = anyhow::Result<LogEvent>> {
    let latest_file = log_files(path);
    Switch::new(latest_file.map_ok(|file| file_log_events(file.path)))
}

fn parse_log_file_name(name: &OsStr) -> Option<NaiveDateTime> {
    let name = name.to_str()?;
    let timestamp = name.strip_prefix("output_log_")?.strip_suffix(".txt")?;
    if timestamp.len() != 19 || !timestamp.is_ascii() {
        return None;
    }
    let bytes = timestamp.as_bytes();
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'_'
        || bytes[13] != b'-'
        || bytes[16] != b'-'
    {
        return None;
    }
    let (year, month, day, hour, min, sec) = (
        &timestamp[0..4],
        &timestamp[5..7],
        &timestamp[8..10],
        &timestamp[11..13],
        &timestamp[14..16],
        &timestamp[17..19],
    );
    let date = NaiveDate::from_ymd_opt(year.parse().ok()?, month.parse().ok()?, day.parse().ok()?)?;
    let time = NaiveTime::from_hms_opt(hour.parse().ok()?, min.parse().ok()?, sec.parse().ok()?)?;
    Some(NaiveDateTime::new(date, time))
}

#[derive(Debug)]
struct LogFile {
    path: PathBuf,
    timestamp: NaiveDateTime,
}

fn log_files(path: impl AsRef<Path>) -> impl Stream<Item = anyhow::Result<LogFile>> {
    try_stream! {
        let path = path.as_ref();
        let (events_sender, events_receiver) = mpsc::unbounded::<Result<LogFile, notify::Error>>();
        let mut watcher = RecommendedWatcher::new(
            move |res| {
                let mut evt: notify::Event = match res {
                    Ok(evt) => evt,
                    Err(e) => {
                        _ = events_sender.unbounded_send(Err(e));
                        return;
                    }
                };
                if !matches!(evt.kind, EventKind::Any | EventKind::Create(CreateKind::Any | CreateKind::File)) {
                    return;
                }
                let Some(path) = evt.paths.pop() else {
                    return;
                };
                let Some(timestamp) = path.file_name().and_then(parse_log_file_name) else {
                    return;
                };
                _ = events_sender.unbounded_send(Ok(LogFile { path, timestamp }));
            },
            notify::Config::default(),
        )?;

        watcher
            .watch(path, RecursiveMode::NonRecursive)
            .context("Directory watcher initialization error")?;

        let mut latest = None::<LogFile>;
        let mut reader = tokio::fs::read_dir(&path)
            .await
            .context("log directory open error")?;
        while let Some(entry) = reader
            .next_entry()
            .await
            .context("log directory read error")?
        {
            let name = entry.file_name();
            let Some(timestamp) = parse_log_file_name(&name) else {
                continue;
            };

            if !entry
                .file_type()
                .await
                .is_ok_and(|file_type| file_type.is_file()) {
                    continue;
                }
            if latest.is_none()
                || latest
                    .as_ref()
                    .is_some_and(|latest| latest.timestamp < timestamp)
            {
                latest = Some(LogFile { path: path.join(name), timestamp });
            }
        }

        let mut latest_ts;
        if let Some(latest) = latest {
            latest_ts = Some(latest.timestamp);
            yield latest;
        } else {
            latest_ts = None;
        }

        for await new in events_receiver {
            let new = new?;
            if latest_ts.is_none()
                || latest_ts
                    .is_some_and(|latest_ts| latest_ts < new.timestamp)
            {
                latest_ts = Some(new.timestamp);
                yield new;
            }
        }
    }
}

pin_project! {
    struct LogReader {
        #[pin]
        file: File,
        #[pin]
        interval: Interval,
    }
}

impl LogReader {
    fn new(file: File) -> Self {
        Self {
            file,
            interval: {
                let mut interval = interval(Duration::from_millis(100));
                interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
                interval
            },
        }
    }
}

impl AsyncRead for LogReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut this = self.project();
        let original_len = buf.filled().len();
        loop {
            match this.file.as_mut().poll_read(cx, buf) {
                Poll::Ready(Ok(())) if buf.filled().len() == original_len => {
                    this.interval.reset();
                    if matches!(this.interval.poll_tick(cx), Poll::Pending) {
                        break Poll::Pending;
                    }
                }
                other => break other,
            }
        }
    }
}

#[derive(Debug)]
pub struct LogEvent {
    pub kind: LogEventKind,
}

#[derive(Debug)]
pub enum LogEventKind {
    // Log        -  [Behaviour] Successfully left room
    LeftRoom,
    // Log        -  [Behaviour] Joining wrld_900dd077-1337-c0fe-babe-71de05ea12c4:46115~hidden(usr_38116327-5a34-4fd8-ace0-21c93fb3f163)
    JoiningRoom(RoomId),
}

fn parse_line(line: &str) -> Option<LogEvent> {
    const TS_LEN: usize = "YYYY.MM.DD HH:MM:SS ".len();
    if line.len() < TS_LEN || !line.is_char_boundary(TS_LEN) {
        return None;
    }
    let (ts, rest) = line.split_at(TS_LEN);
    let ts_bytes = ts.as_bytes();
    if !ts.is_ascii()
        || ts_bytes[4] != b'.'
        || ts_bytes[7] != b'.'
        || ts_bytes[10] != b' '
        || ts_bytes[13] != b':'
        || ts_bytes[16] != b':'
        || ts_bytes[19] != b' '
    {
        return None;
    }

    let year = ts[0..4].parse().ok()?;
    let month = ts[5..7].parse().ok()?;
    let day = ts[8..10].parse().ok()?;
    let hour = ts[11..13].parse().ok()?;
    let min = ts[14..16].parse().ok()?;
    let sec = ts[17..19].parse().ok()?;

    NaiveDate::from_ymd_opt(year, month, day)?;
    NaiveTime::from_hms_opt(hour, min, sec)?;

    let (source, message) = rest.split_once("-  ")?;
    let source = source.trim_end();

    if source != "Debug" {
        return None;
    }

    let kind = if message == "[Behaviour] Successfully left room" {
        LogEventKind::LeftRoom
    } else if let Some(room) = message
        .strip_prefix("[Behaviour] Joining ")
        .and_then(|id| id.parse().ok())
    {
        LogEventKind::JoiningRoom(room)
    } else {
        return None;
    };

    Some(LogEvent { kind })
}

fn file_log_events(path: impl AsRef<Path>) -> impl Stream<Item = anyhow::Result<LogEvent>> {
    try_stream! {
        let path = path.as_ref();
        debug!(?path, "Reading log");
        let mut file = BufReader::new(LogReader::new(File::open(path).await?));

        let mut buffer = Vec::new();

        const END: &[u8; 2] = b"\r\n";

        loop {
            buffer.clear();
            loop {
                file.read_until(END[END.len() - 1], &mut buffer).await?;
                if buffer.ends_with(END) {
                    break;
                }
            }
            buffer.truncate(buffer.len() - END.len());
            let Ok(line) = str::from_utf8(&buffer) else {
                continue;
            };

            if let Some(event) = parse_line(line) {
                yield event;
            }
        }
    }
}

pin_project! {
    struct Switch<T, U> {
        #[pin]
        stream_stream: T,
        #[pin]
        current_stream: Option<U>,
    }
}

impl<T, U> Switch<T, U>
where
    T: TryStream<Ok = U, Error = anyhow::Error>,
    U: TryStream<Error = anyhow::Error>,
{
    fn new(stream_stream: T) -> Self {
        Self {
            stream_stream,
            current_stream: None,
        }
    }
}

impl<T, U> Stream for Switch<T, U>
where
    T: TryStream<Ok = U, Error = anyhow::Error>,
    U: TryStream<Error = anyhow::Error>,
{
    type Item = anyhow::Result<U::Ok>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();
        match this.stream_stream.try_poll_next(cx) {
            Poll::Ready(Some(Ok(new))) => {
                this.current_stream.set(Some(new));
            }
            Poll::Ready(Some(Err(e))) => {
                return Poll::Ready(Some(Err(e)));
            }
            _ => {}
        }
        if let Some(current_stream) = this.current_stream.as_pin_mut() {
            current_stream.try_poll_next(cx)
        } else {
            Poll::Pending
        }
    }
}
