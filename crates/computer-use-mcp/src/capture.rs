use std::{
    future::Future,
    os::fd::OwnedFd,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use pipewire::{self as pw, properties::properties};
use pw::spa::{
    buffer::{
        ChunkFlags, DataFlags, DataType,
        meta::{
            MetaHeader, MetaHeaderFlags, MetaVideoCrop, MetaVideoTransform,
            MetaVideoTransformValue, Metadata,
        },
    },
    param::{
        ParamType,
        format::{FormatProperties, MediaSubtype, MediaType},
        format_utils,
        video::{VideoFormat, VideoInfoRaw},
    },
    pod::{ChoiceValue, Object, Pod, Property, Value},
    utils::{Choice, ChoiceEnum, ChoiceFlags, Direction, Id, SpaTypes},
};
use tokio::sync::watch;

use crate::geometry::{PixelRect, Transform};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureTarget {
    pub stream_index: usize,
    pub node_id: u32,
    pub pipewire_serial: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedFrame {
    pub metadata: FrameMetadata,
    pub rgba: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimestampAuthority {
    SpaHeader,
    Unavailable,
}

impl TimestampAuthority {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SpaHeader => "spa_header",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamHealth {
    Healthy,
    Degraded,
    Failed,
}

impl StreamHealth {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChangedRect {
    /// Minimum tile-aligned bounding box of changed tiles, in full-frame
    /// pixels. This is advisory change evidence only: it never replaces the
    /// change_epoch/content_hash staleness authority.
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameMetadata {
    /// Local arrival counter. This is never an authority claim about the
    /// compositor or source sequence.
    pub generation: u64,
    pub format_generation: u64,
    pub source_sequence: Option<u64>,
    pub pts_ns: Option<i64>,
    pub arrival_monotonic_ns: u64,
    pub size: (u32, u32),
    pub crop: PixelRect,
    pub transform: Transform,
    pub timestamp_authority: TimestampAuthority,
    pub stream_health: StreamHealth,
    pub content_hash: u64,
    /// Monotonic content-change epoch, retained even when a later frame
    /// returns to an earlier hash so transient changes cannot be missed.
    pub change_epoch: u64,
    pub changed_from_previous: Option<bool>,
    /// Tile-aligned bounding box of changed tiles versus the previous
    /// committed frame, or None when there was no previous frame or nothing
    /// changed. Advisory only; never a staleness authority.
    pub changed_rect: Option<ChangedRect>,
    pub sequence_gap: Option<u64>,
}

pub trait CaptureBackend: Send + Sync + 'static {
    fn start(&self, fd: OwnedFd, target: CaptureTarget) -> Result<Box<dyn CaptureSession>, String>;
}

pub type CaptureFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;

pub trait CaptureSession: Send + 'static {
    fn wait_ready(&mut self) -> CaptureFuture<'_, ()>;
    fn failure(&self) -> Option<String>;
    /// Return the newest complete frame metadata already committed by the
    /// capture thread. This is a non-waiting health/format check; input
    /// preparation must never depend on a frame newer than its source frame.
    fn current_metadata(&self) -> Result<Option<FrameMetadata>, String>;
    fn latest_after(
        &mut self,
        after_generation: Option<u64>,
        wait: Duration,
    ) -> CaptureFuture<'_, OwnedFrame>;
}

#[derive(Debug, Default)]
pub struct PipeWireCapture;

impl CaptureBackend for PipeWireCapture {
    fn start(&self, fd: OwnedFd, target: CaptureTarget) -> Result<Box<dyn CaptureSession>, String> {
        CaptureHandle::spawn(fd, target).map(|capture| Box::new(capture) as Box<dyn CaptureSession>)
    }
}

pub struct CaptureHandle {
    receiver: watch::Receiver<Option<OwnedFrame>>,
    status: watch::Receiver<Option<Result<(), String>>>,
    failure: Arc<Mutex<Option<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    thread_done: std::sync::mpsc::Receiver<()>,
}

impl std::fmt::Debug for CaptureHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CaptureHandle")
            .finish_non_exhaustive()
    }
}

impl CaptureHandle {
    fn spawn(fd: OwnedFd, target: CaptureTarget) -> Result<Self, String> {
        let (sender, receiver) = watch::channel(None);
        let (status_sender, status) = watch::channel(None);
        let failure = Arc::new(Mutex::new(None));
        let thread_failure = Arc::clone(&failure);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let (done_sender, thread_done) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("computer-use-mcp-pipewire".into())
            .spawn(move || {
                let result = run_pipewire(fd, target, sender, thread_failure, &thread_stop);
                if let Err(error) = &result {
                    eprintln!("computer-use-mcp: PipeWire capture stopped: {error}");
                }
                status_sender.send_replace(Some(result));
                let _ = done_sender.send(());
            })
            .map_err(|error| format!("cannot start dedicated PipeWire thread: {error}"))?;
        Ok(Self {
            receiver,
            status,
            failure,
            stop,
            thread: Some(thread),
            thread_done,
        })
    }
}

impl CaptureSession for CaptureHandle {
    fn wait_ready(&mut self) -> CaptureFuture<'_, ()> {
        Box::pin(async move {
            loop {
                if let Some(status) = self.status.borrow().clone() {
                    return status;
                }
                if self.receiver.borrow().is_some() {
                    return Ok(());
                }
                tokio::select! {
                    changed = self.status.changed() => {
                        if changed.is_err() {
                            return Err("PipeWire status channel closed before startup".into());
                        }
                    }
                    () = tokio::time::sleep(Duration::from_millis(10)) => {}
                }
            }
        })
    }

    fn failure(&self) -> Option<String> {
        match self.failure.lock() {
            Ok(failure) if failure.is_some() => return failure.clone(),
            Err(_) => return Some("PipeWire failure state mutex poisoned".into()),
            Ok(_) => {}
        }
        match self.status.borrow().as_ref() {
            Some(Err(error)) => Some(error.clone()),
            _ if self.status.has_changed().is_err() => {
                Some("PipeWire status channel closed unexpectedly".into())
            }
            _ => None,
        }
    }

    fn current_metadata(&self) -> Result<Option<FrameMetadata>, String> {
        if let Some(error) = self.failure() {
            return Err(error);
        }
        Ok(self.receiver.borrow().as_ref().map(|frame| frame.metadata))
    }

    fn latest_after(
        &mut self,
        after_generation: Option<u64>,
        wait: Duration,
    ) -> CaptureFuture<'_, OwnedFrame> {
        Box::pin(async move {
            let (receiver, status) = (&mut self.receiver, &mut self.status);
            let future = async {
                loop {
                    if let Some(Err(error)) = status.borrow().clone() {
                        return Err(error);
                    }
                    let observed = receiver.borrow_and_update().clone();
                    if let Some(frame) = observed
                        && after_generation
                            .is_none_or(|generation| frame.metadata.generation > generation)
                    {
                        return Ok(frame);
                    }
                    tokio::select! {
                        changed = receiver.changed() => {
                            changed.map_err(|_| "PipeWire frame channel closed".to_owned())?;
                        }
                        changed = status.changed() => {
                            changed.map_err(|_| "PipeWire status channel closed during capture".to_owned())?;
                        }
                    }
                }
            };
            tokio::time::timeout(wait, future)
                .await
                .map_err(|_| "timed out waiting for a complete frame".to_owned())?
        })
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if self.thread.is_none() {
            return;
        }
        if self
            .thread_done
            .recv_timeout(Duration::from_secs(1))
            .is_ok()
        {
            if let Some(thread) = self.thread.take()
                && thread.join().is_err()
            {
                eprintln!("computer-use-mcp: PipeWire capture thread panicked during cleanup");
            }
        } else if self.thread.take().is_some() {
            eprintln!(
                "computer-use-mcp: PipeWire capture thread did not stop within one second; detaching it"
            );
        }
    }
}

struct StreamUserData {
    stream_index: usize,
    generation: u64,
    format_generation: u64,
    format: Option<RawFormat>,
    last_source_sequence: Option<u64>,
    last_content_hash: Option<u64>,
    change_epoch: u64,
    /// Downsampled per-tile hashes of the last committed frame, used to
    /// derive the advisory changed_rect without retaining a full frame copy.
    prev_tiles: Option<TileGrid>,
    sender: watch::Sender<Option<OwnedFrame>>,
    failure: Arc<Mutex<Option<String>>>,
}

impl StreamUserData {
    fn invalidate_frame_state(&mut self) {
        self.format = None;
        self.last_source_sequence = None;
        self.last_content_hash = None;
        self.prev_tiles = None;
        self.sender.send_replace(None);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RawFormat {
    format: VideoFormat,
    width: u32,
    height: u32,
}

fn run_pipewire(
    fd: OwnedFd,
    target: CaptureTarget,
    sender: watch::Sender<Option<OwnedFrame>>,
    failure: Arc<Mutex<Option<String>>>,
    stop: &AtomicBool,
) -> Result<(), String> {
    pw::init();
    let main_loop = pw::main_loop::MainLoopRc::new(None).map_err(pw_error)?;
    let context = pw::context::ContextRc::new(&main_loop, None).map_err(pw_error)?;
    let core = context
        .connect_fd_rc(fd, None)
        .map_err(|error| format!("cannot open the portal-restricted PipeWire remote: {error}"))?;
    let mut props = properties! {
        *pw::keys::MEDIA_TYPE => "Video",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Screen",
    };
    if let Some(serial) = target.pipewire_serial {
        props.insert(*pw::keys::TARGET_OBJECT, serial.to_string());
    }
    let stream = pw::stream::StreamBox::new(
        &core,
        &format!("computer-use-mcp-{}", target.stream_index),
        props,
    )
    .map_err(pw_error)?;
    let user_data = StreamUserData {
        stream_index: target.stream_index,
        generation: 0,
        format_generation: 0,
        format: None,
        last_source_sequence: None,
        last_content_hash: None,
        change_epoch: 0,
        prev_tiles: None,
        sender,
        failure: Arc::clone(&failure),
    };
    let listener = stream
        .add_local_listener_with_user_data(user_data)
        .state_changed(|_, data, old, new| {
            let error = stream_state_failure(data.stream_index, &old, &new);
            if let Some(error) = error {
                report_failure(&data.failure, error);
            }
        })
        .param_changed(|stream, data, id, param| {
            if id != ParamType::Format.as_raw() {
                return;
            }
            let Some(param) = param else {
                invalidate_format(data);
                data.format = None;
                report_failure(
                    &data.failure,
                    format!(
                        "PipeWire cleared the negotiated format for stream {}",
                        data.stream_index
                    ),
                );
                return;
            };
            match parse_raw_format(param) {
                Ok(format) => {
                    if let Err(error) = begin_format(data) {
                        report_failure(&data.failure, error);
                        return;
                    }
                    match negotiated_parameter_pods(format)
                        .and_then(|pods| update_stream_params(stream, &pods))
                    {
                        Ok(()) => data.format = Some(format),
                        Err(error) => {
                            invalidate_format(data);
                            report_failure(
                                &data.failure,
                                format!(
                                    "cannot negotiate shared-memory buffers for stream {}: {error}",
                                    data.stream_index
                                ),
                            );
                        }
                    }
                }
                Err(error) => {
                    invalidate_format(data);
                    report_failure(
                        &data.failure,
                        format!(
                            "rejecting PipeWire format for stream {}: {error}",
                            data.stream_index
                        ),
                    );
                }
            }
        })
        .process(process_frame)
        .register()
        .map_err(pw_error)?;

    let format_pod = raw_format_pod()?;
    let mut params = [Pod::from_bytes(&format_pod)
        .ok_or_else(|| "generated an invalid PipeWire format pod".to_owned())?];
    let node = target.pipewire_serial.is_none().then_some(target.node_id);
    stream
        .connect(
            Direction::Input,
            node,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(pw_error)?;

    while !stop.load(Ordering::Acquire) {
        let dispatched = main_loop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(100)));
        if dispatched < 0 {
            return Err(format!("PipeWire loop iteration failed with {dispatched}"));
        }
        if let Some(error) = capture_failure(&failure) {
            return Err(error);
        }
    }
    if let Err(error) = stream.disconnect() {
        eprintln!("computer-use-mcp: failed to disconnect PipeWire stream: {error}");
    }
    drop(listener);
    Ok(())
}

fn stream_state_failure(
    stream_index: usize,
    old: &pw::stream::StreamState,
    new: &pw::stream::StreamState,
) -> Option<String> {
    match new {
        pw::stream::StreamState::Error(error) => Some(format!(
            "PipeWire stream {stream_index} entered error state: {error}"
        )),
        pw::stream::StreamState::Unconnected if old != &pw::stream::StreamState::Unconnected => {
            Some(format!(
                "PipeWire stream {stream_index} disconnected or its target node disappeared"
            ))
        }
        _ => None,
    }
}

fn begin_format(data: &mut StreamUserData) -> Result<(), String> {
    data.format_generation = data.format_generation.checked_add(1).ok_or_else(|| {
        format!(
            "format generation overflow for stream {}",
            data.stream_index
        )
    })?;
    // Keep change_epoch monotonic across format generations. Stability waits
    // bind both epochs, so renegotiation is independently observable.
    data.invalidate_frame_state();
    Ok(())
}

fn invalidate_format(data: &mut StreamUserData) {
    data.invalidate_frame_state();
}

fn report_failure(failure: &Mutex<Option<String>>, error: String) {
    let Ok(mut failure) = failure.lock() else {
        eprintln!("computer-use-mcp: PipeWire failure state mutex poisoned");
        return;
    };
    if failure.is_none() {
        eprintln!("computer-use-mcp: {error}");
        *failure = Some(error);
    }
}

fn capture_failure(failure: &Mutex<Option<String>>) -> Option<String> {
    match failure.lock() {
        Ok(failure) => failure.clone(),
        Err(_) => Some("PipeWire failure state mutex poisoned".into()),
    }
}

fn parse_raw_format(param: &Pod) -> Result<RawFormat, String> {
    let (media_type, media_subtype) = format_utils::parse_format(param).map_err(pw_error)?;
    if media_type != MediaType::Video || media_subtype != MediaSubtype::Raw {
        return Err("portal offered a non-raw video format".into());
    }
    let mut info = VideoInfoRaw::new();
    info.parse(param).map_err(pw_error)?;
    let format = info.format();
    if !matches!(
        format,
        VideoFormat::BGRx | VideoFormat::RGBx | VideoFormat::BGRA | VideoFormat::RGBA
    ) {
        return Err(format!("unsupported raw pixel format {format:?}"));
    }
    let size = info.size();
    if size.width == 0 || size.height == 0 {
        return Err("raw format has zero dimensions".into());
    }
    Ok(RawFormat {
        format,
        width: size.width,
        height: size.height,
    })
}

fn raw_format_pod() -> Result<Vec<u8>, String> {
    let object = pw::spa::pod::object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        pw::spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pw::spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pw::spa::pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::RGBx,
            VideoFormat::BGRA,
            VideoFormat::RGBA,
        ),
    );
    pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(object),
    )
    .map(|(cursor, _)| cursor.into_inner())
    .map_err(|error| format!("cannot serialize PipeWire format pod: {error}"))
}

fn negotiated_parameter_pods(format: RawFormat) -> Result<Vec<Vec<u8>>, String> {
    let stride = format
        .width
        .checked_mul(4)
        .and_then(|value| i32::try_from(value).ok())
        .ok_or_else(|| "negotiated video row size is too large".to_owned())?;
    let size = u32::try_from(stride)
        .ok()
        .and_then(|stride| stride.checked_mul(format.height))
        .and_then(|value| i32::try_from(value).ok())
        .ok_or_else(|| "negotiated video buffer size is too large".to_owned())?;
    let data_type_mask = data_type_mask(&supported_data_types())?;
    let buffer = Object {
        type_: SpaTypes::ObjectParamBuffers.as_raw(),
        id: ParamType::Buffers.as_raw(),
        properties: vec![
            Property::new(
                pw::spa::sys::SPA_PARAM_BUFFERS_buffers,
                Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: 8,
                        min: 2,
                        max: 16,
                    },
                ))),
            ),
            Property::new(pw::spa::sys::SPA_PARAM_BUFFERS_blocks, Value::Int(1)),
            Property::new(pw::spa::sys::SPA_PARAM_BUFFERS_size, Value::Int(size)),
            Property::new(pw::spa::sys::SPA_PARAM_BUFFERS_stride, Value::Int(stride)),
            Property::new(pw::spa::sys::SPA_PARAM_BUFFERS_align, Value::Int(16)),
            Property::new(
                pw::spa::sys::SPA_PARAM_BUFFERS_dataType,
                Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Flags {
                        default: data_type_mask,
                        flags: Vec::new(),
                    },
                ))),
            ),
        ],
    };
    let mut pods = vec![serialize_object(buffer, "buffer parameters")?];
    for (meta_type, size, label) in [
        (
            MetaHeader::META_TYPE,
            std::mem::size_of::<MetaHeader>(),
            "header",
        ),
        (
            MetaVideoCrop::META_TYPE,
            std::mem::size_of::<MetaVideoCrop>(),
            "video crop",
        ),
        (
            MetaVideoTransform::META_TYPE,
            std::mem::size_of::<MetaVideoTransform>(),
            "video transform",
        ),
    ] {
        let size = i32::try_from(size).map_err(|_| format!("{label} metadata size overflow"))?;
        pods.push(serialize_object(
            Object {
                type_: SpaTypes::ObjectParamMeta.as_raw(),
                id: ParamType::Meta.as_raw(),
                properties: vec![
                    Property::new(pw::spa::sys::SPA_PARAM_META_type, Value::Id(Id(meta_type))),
                    Property::new(pw::spa::sys::SPA_PARAM_META_size, Value::Int(size)),
                ],
            },
            label,
        )?);
    }
    Ok(pods)
}

/// SPA data types this build can consume on the CPU. This is converted to a
/// capability mask; the array order has no negotiation meaning. DMA-BUF
/// (GPU-only) buffers are deliberately never negotiated because this build has
/// no GPU import path (no libgbm/EGL/Vulkan) and forbids unsafe code.
fn supported_data_types() -> [DataType; 2] {
    [DataType::MemFd, DataType::MemPtr]
}

fn data_type_mask(types: &[DataType]) -> Result<i32, String> {
    types
        .iter()
        .try_fold(0_u32, |mask, data_type| {
            1_u32
                .checked_shl(data_type.as_raw())
                .map(|bit| mask | bit)
                .ok_or_else(|| {
                    format!(
                        "SPA data type {:?} cannot be represented as a mask",
                        data_type
                    )
                })
        })
        .and_then(|mask| i32::try_from(mask).map_err(|_| "SPA data type mask exceeds i32".into()))
}

fn serialize_object(object: Object, label: &str) -> Result<Vec<u8>, String> {
    pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(object),
    )
    .map(|(cursor, _)| cursor.into_inner())
    .map_err(|error| format!("cannot serialize PipeWire {label}: {error}"))
}

fn update_stream_params(stream: &pw::stream::Stream, pods: &[Vec<u8>]) -> Result<(), String> {
    let parsed = pods
        .iter()
        .map(|bytes| {
            Pod::from_bytes(bytes)
                .ok_or_else(|| "generated an invalid SPA parameter pod".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut params = parsed;
    stream.update_params(&mut params).map_err(pw_error)
}

fn process_frame(stream: &pw::stream::Stream, user_data: &mut StreamUserData) {
    let Some(format) = user_data.format else {
        return;
    };
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    if !header_is_usable(buffer.find_meta::<MetaHeader>().map(MetaHeader::flags)) {
        eprintln!(
            "computer-use-mcp: stream {} frame header marks the frame corrupted or empty",
            user_data.stream_index
        );
        return;
    }
    let crop = match optional_crop(buffer.find_meta::<MetaVideoCrop>(), format) {
        Ok(crop) => crop,
        Err(error) => {
            eprintln!(
                "computer-use-mcp: stream {} frame has invalid crop metadata: {error}",
                user_data.stream_index
            );
            return;
        }
    };
    let transform = match buffer.find_meta::<MetaVideoTransform>() {
        Some(meta) => match spa_transform(meta.transform()) {
            Some(transform) => transform,
            None => {
                eprintln!(
                    "computer-use-mcp: stream {} frame has an unknown video transform",
                    user_data.stream_index
                );
                return;
            }
        },
        None => Transform::Normal,
    };

    let datas = buffer.datas_mut();
    if datas.len() != 1 {
        eprintln!(
            "computer-use-mcp: stream {} frame has {} data planes; expected one",
            user_data.stream_index,
            datas.len()
        );
        return;
    }
    let data = &mut datas[0];
    if data.type_() == DataType::DmaBuf {
        // Best effort before failing closed: some PipeWire versions leave a
        // DMA-BUF CPU-mapped, in which case the shared-memory conversion
        // below applies unchanged. Otherwise this is a GPU-only buffer this
        // build cannot import, which is distinct from a corrupt frame.
        match try_dmabuf_cpu_frame(data, format) {
            Ok(Some(rgba)) => {
                let header = buffer.find_meta::<MetaHeader>();
                publish_frame(user_data, format, crop, transform, header, rgba);
            }
            Ok(None) => report_failure(
                &user_data.failure,
                dmabuf_only_error(user_data.stream_index),
            ),
            Err(error) => eprintln!(
                "computer-use-mcp: rejecting corrupt DMA-BUF frame for stream {}: {error}",
                user_data.stream_index
            ),
        }
        return;
    }
    if !matches!(data.type_(), DataType::MemFd | DataType::MemPtr) {
        eprintln!(
            "computer-use-mcp: stream {} offered unsupported SPA data type {:?}",
            user_data.stream_index,
            data.type_()
        );
        return;
    }
    if !data.flags().contains(DataFlags::READABLE) {
        eprintln!(
            "computer-use-mcp: stream {} shared-memory frame is not marked readable",
            user_data.stream_index
        );
        return;
    }
    let chunk = data.chunk();
    if chunk.flags().contains(ChunkFlags::CORRUPTED) {
        return;
    }
    if chunk.size() == 0 {
        eprintln!(
            "computer-use-mcp: stream {} supplied an empty SPA chunk",
            user_data.stream_index
        );
        return;
    }
    let layout = RawLayout {
        width: format.width,
        height: format.height,
        offset: chunk.offset(),
        size: chunk.size(),
        stride: chunk.stride(),
        format: format.format,
    };
    let Some(bytes) = data.data() else {
        eprintln!(
            "computer-use-mcp: stream {} shared-memory frame was not mapped",
            user_data.stream_index
        );
        return;
    };
    let rgba = match convert_raw_frame(bytes, layout) {
        Ok(rgba) => rgba,
        Err(error) => {
            eprintln!(
                "computer-use-mcp: rejecting incomplete frame for stream {}: {error}",
                user_data.stream_index
            );
            return;
        }
    };
    let header = buffer.find_meta::<MetaHeader>();
    publish_frame(user_data, format, crop, transform, header, rgba);
}

/// Best-effort CPU read of a DMA-BUF plane using the same conversion as the
/// shared-memory path. `Ok(None)` means the buffer is not CPU-mapped or is not
/// readable, so the caller reports the unsupported GPU-only transport;
/// conversion and corruption errors are returned separately and skipped
/// without mislabelling them as GPU-only. Never touches GPU import APIs.
fn try_dmabuf_cpu_frame(
    data: &mut pw::spa::buffer::Data,
    format: RawFormat,
) -> Result<Option<Vec<u8>>, String> {
    if !data.flags().contains(DataFlags::READABLE) {
        return Ok(None);
    }
    let chunk = data.chunk();
    if chunk.flags().contains(ChunkFlags::CORRUPTED) {
        return Err("SPA chunk is marked corrupted".into());
    }
    if chunk.size() == 0 {
        return Err("SPA chunk is empty".into());
    }
    let layout = RawLayout {
        width: format.width,
        height: format.height,
        offset: chunk.offset(),
        size: chunk.size(),
        stride: chunk.stride(),
        format: format.format,
    };
    let Some(bytes) = data.data() else {
        return Ok(None);
    };
    convert_raw_frame(bytes, layout).map(Some)
}

/// Fail-closed diagnostic for GPU-only DMA-BUF streams. This is distinct from
/// a corrupt or empty frame (which only skips that frame): a DMA-BUF-only
/// stream can never be consumed by this build, so it is terminal.
fn dmabuf_only_error(stream_index: usize) -> String {
    format!(
        "PipeWire stream {stream_index} supplied DMA-BUF-only data with no CPU mapping. \
         This build negotiates the supported shared-memory buffers (MemFd and MemPtr) and has no GPU import path \
         (no libgbm/EGL/Vulkan), so GPU-only buffers cannot be read; failing closed. \
         Recovery: restart the MCP server to renegotiate shared memory, or use a compositor/driver offering SHM screencast buffers."
    )
}

fn publish_frame(
    user_data: &mut StreamUserData,
    format: RawFormat,
    crop: PixelRect,
    transform: Transform,
    header: Option<&MetaHeader>,
    rgba: Vec<u8>,
) {
    let source_sequence = header.map(|header| header.seq());
    let pts_ns = header.map(|header| header.pts()).filter(|pts| *pts >= 0);
    let sequence_gap = source_sequence.and_then(|sequence| {
        user_data.last_source_sequence.and_then(|previous| {
            (sequence > previous.saturating_add(1)).then_some(sequence - previous - 1)
        })
    });
    let discontinuity = header
        .is_some_and(|header| header.flags().contains(MetaHeaderFlags::DISCONT))
        || sequence_gap.is_some();
    let (content_hash, grid) = hash_rgba_and_tile_grid(&rgba, format);
    let changed_from_previous = user_data
        .last_content_hash
        .map(|previous| previous != content_hash || discontinuity);
    if changed_from_previous == Some(true) {
        user_data.change_epoch = match user_data.change_epoch.checked_add(1) {
            Some(epoch) => epoch,
            None => {
                eprintln!(
                    "computer-use-mcp: frame change epoch overflow for stream {}",
                    user_data.stream_index
                );
                return;
            }
        };
    }
    user_data.generation = match user_data.generation.checked_add(1) {
        Some(generation) => generation,
        None => {
            eprintln!(
                "computer-use-mcp: frame generation overflow for stream {}",
                user_data.stream_index
            );
            return;
        }
    };
    user_data.last_source_sequence = source_sequence;
    user_data.last_content_hash = Some(content_hash);
    // Advisory dirty bounding box: compare downsampled tile hashes against
    // the previous committed frame. Never a staleness authority. The content
    // and tile hashes were computed in the same RGBA traversal above.
    let changed_rect = match (&user_data.prev_tiles, &grid) {
        (Some(previous), Some(current)) => changed_rect_between(previous, current),
        _ => None,
    };
    if let Some(grid) = grid {
        user_data.prev_tiles = Some(grid);
    }
    let stream_health = if discontinuity {
        StreamHealth::Degraded
    } else {
        StreamHealth::Healthy
    };
    user_data.sender.send_replace(Some(OwnedFrame {
        metadata: FrameMetadata {
            generation: user_data.generation,
            format_generation: user_data.format_generation,
            source_sequence,
            pts_ns,
            arrival_monotonic_ns: monotonic_nanoseconds(),
            size: (format.width, format.height),
            crop,
            transform,
            timestamp_authority: if header.is_some() {
                TimestampAuthority::SpaHeader
            } else {
                TimestampAuthority::Unavailable
            },
            stream_health,
            content_hash,
            change_epoch: user_data.change_epoch,
            changed_from_previous,
            changed_rect,
            sequence_gap,
        },
        rgba,
    }));
}

fn monotonic_nanoseconds() -> u64 {
    let time = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    let seconds = u64::try_from(time.tv_sec).unwrap_or_default();
    let nanos = u64::try_from(time.tv_nsec).unwrap_or_default();
    seconds.saturating_mul(1_000_000_000).saturating_add(nanos)
}

/// Deterministic non-cryptographic change evidence. This is not an identity or
/// security digest.
fn hash_rgba(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Tile edge in pixels for the advisory dirty-rect grid. Per-tile hashes keep
/// per-frame change detection without retaining a full copy of the previous
/// frame.
const CHANGE_TILE_EDGE: u32 = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
struct TileGrid {
    tiles_x: u32,
    tiles_y: u32,
    frame_width: u32,
    frame_height: u32,
    hashes: Vec<u64>,
}

fn hash_rgba_and_tile_grid(rgba: &[u8], format: RawFormat) -> (u64, Option<TileGrid>) {
    if format.width == 0 || format.height == 0 {
        return (hash_rgba(rgba), None);
    }
    let (Ok(width), Ok(height), Ok(edge)) = (
        usize::try_from(format.width),
        usize::try_from(format.height),
        usize::try_from(CHANGE_TILE_EDGE),
    ) else {
        return (hash_rgba(rgba), None);
    };
    let Some(expected_len) = width
        .checked_mul(height)
        .and_then(|value| value.checked_mul(4))
    else {
        return (hash_rgba(rgba), None);
    };
    if rgba.len() != expected_len {
        return (hash_rgba(rgba), None);
    }
    let tiles_x = width.div_ceil(edge);
    let tiles_y = height.div_ceil(edge);
    let Some(tile_count) = tiles_x.checked_mul(tiles_y) else {
        return (hash_rgba(rgba), None);
    };
    let mut content_hash = 0xcbf29ce484222325_u64;
    let mut hashes = vec![0xcbf29ce484222325_u64; tile_count];
    for y in 0..height {
        let row = y * width * 4;
        let tile_y = y / edge;
        for x in 0..width {
            let tile_index = tile_y * tiles_x + x / edge;
            for byte in &rgba[row + x * 4..row + x * 4 + 4] {
                let byte = u64::from(*byte);
                content_hash ^= byte;
                content_hash = content_hash.wrapping_mul(0x100000001b3);
                hashes[tile_index] ^= byte;
                hashes[tile_index] = hashes[tile_index].wrapping_mul(0x100000001b3);
            }
        }
    }
    let (Ok(tiles_x), Ok(tiles_y)) = (u32::try_from(tiles_x), u32::try_from(tiles_y)) else {
        return (content_hash, None);
    };
    (
        content_hash,
        Some(TileGrid {
            tiles_x,
            tiles_y,
            frame_width: format.width,
            frame_height: format.height,
            hashes,
        }),
    )
}

#[cfg(test)]
fn tile_grid_for_frame(rgba: &[u8], format: RawFormat) -> Option<TileGrid> {
    hash_rgba_and_tile_grid(rgba, format).1
}

/// Minimum tile-aligned bounding box covering every changed tile, in
/// full-frame pixels. Returns None when nothing changed. A dimension or tile
/// mismatch conservatively reports the whole current frame.
fn changed_rect_between(previous: &TileGrid, current: &TileGrid) -> Option<ChangedRect> {
    if previous.hashes.len() != current.hashes.len()
        || previous.tiles_x != current.tiles_x
        || previous.tiles_y != current.tiles_y
        || previous.frame_width != current.frame_width
        || previous.frame_height != current.frame_height
    {
        return Some(ChangedRect {
            x: 0,
            y: 0,
            width: current.frame_width,
            height: current.frame_height,
        });
    }
    let mut min_tx: Option<u32> = None;
    let mut max_tx: u32 = 0;
    let mut min_ty: Option<u32> = None;
    let mut max_ty: u32 = 0;
    let tiles_x = usize::try_from(current.tiles_x).ok()?;
    for (index, (before, after)) in previous
        .hashes
        .iter()
        .zip(current.hashes.iter())
        .enumerate()
    {
        if before == after {
            continue;
        }
        let tile_x = u32::try_from(index % tiles_x).ok()?;
        let tile_y = u32::try_from(index / tiles_x).ok()?;
        min_tx = Some(min_tx.map_or(tile_x, |value| value.min(tile_x)));
        max_tx = max_tx.max(tile_x);
        min_ty = Some(min_ty.map_or(tile_y, |value| value.min(tile_y)));
        max_ty = max_ty.max(tile_y);
    }
    let (min_tx, min_ty) = match (min_tx, min_ty) {
        (Some(x), Some(y)) => (x, y),
        _ => return None,
    };
    let right = (max_tx.checked_add(1)?.checked_mul(CHANGE_TILE_EDGE)?).min(current.frame_width);
    let bottom = (max_ty.checked_add(1)?.checked_mul(CHANGE_TILE_EDGE)?).min(current.frame_height);
    let x = min_tx.checked_mul(CHANGE_TILE_EDGE)?;
    let y = min_ty.checked_mul(CHANGE_TILE_EDGE)?;
    Some(ChangedRect {
        x,
        y,
        width: right.checked_sub(x)?,
        height: bottom.checked_sub(y)?,
    })
}

pub fn frame_id(metadata: &FrameMetadata) -> String {
    format!("frame-{:016x}", metadata.generation)
}

fn header_is_usable(flags: Option<MetaHeaderFlags>) -> bool {
    flags.is_none_or(|flags| !flags.intersects(MetaHeaderFlags::CORRUPTED | MetaHeaderFlags::GAP))
}

fn optional_crop(meta: Option<&MetaVideoCrop>, format: RawFormat) -> Result<PixelRect, String> {
    let full = PixelRect {
        x: 0,
        y: 0,
        width: format.width,
        height: format.height,
    };
    let Some(meta) = meta else {
        return Ok(full);
    };
    if !meta.meta_region().is_valid() {
        return Err("region is invalid".into());
    }
    let position = meta.meta_region().position();
    let size = meta.meta_region().size();
    if position.x < 0 || position.y < 0 {
        return Err("origin is negative".into());
    }
    let crop = PixelRect {
        x: position.x as u32,
        y: position.y as u32,
        width: size.width,
        height: size.height,
    };
    if !crop.is_valid_within((format.width, format.height)) {
        return Err("region lies outside the negotiated frame".into());
    }
    Ok(crop)
}

#[derive(Debug, Clone, Copy)]
struct RawLayout {
    width: u32,
    height: u32,
    offset: u32,
    size: u32,
    stride: i32,
    format: VideoFormat,
}

fn convert_raw_frame(data: &[u8], layout: RawLayout) -> Result<Vec<u8>, String> {
    if layout.width == 0 || layout.height == 0 || layout.stride == 0 {
        return Err("invalid dimensions or zero stride".into());
    }
    let row_bytes = usize::try_from(layout.width)
        .ok()
        .and_then(|width| width.checked_mul(4))
        .ok_or_else(|| "row size overflow".to_owned())?;
    let stride = usize::try_from(layout.stride.unsigned_abs())
        .map_err(|_| "stride is too large".to_owned())?;
    if stride < row_bytes {
        return Err("stride is shorter than a pixel row".into());
    }
    let height = usize::try_from(layout.height).map_err(|_| "height is too large".to_owned())?;
    let required = stride
        .checked_mul(height.saturating_sub(1))
        .and_then(|value| value.checked_add(row_bytes))
        .ok_or_else(|| "frame size overflow".to_owned())?;
    let chunk_size =
        usize::try_from(layout.size).map_err(|_| "chunk size is too large".to_owned())?;
    if chunk_size < required {
        return Err("SPA chunk size does not contain all rows".into());
    }
    if data.is_empty() {
        return Err("mapped SPA data has zero maxsize".into());
    }
    if chunk_size > data.len() {
        return Err("SPA chunk size exceeds mapped maxsize".into());
    }
    let offset = usize::try_from(layout.offset).map_err(|_| "offset is too large".to_owned())?;
    if offset >= data.len() {
        return Err("SPA chunk offset exceeds mapped maxsize".into());
    }
    let mut rgba = Vec::with_capacity(
        row_bytes
            .checked_mul(height)
            .ok_or_else(|| "output frame size overflow".to_owned())?,
    );
    for row in 0..height {
        let displacement = row
            .checked_mul(stride)
            .ok_or_else(|| "row offset overflow".to_owned())?;
        let start = if layout.stride > 0 {
            offset
                .checked_add(displacement)
                .ok_or_else(|| "row offset overflow".to_owned())?
        } else {
            offset
                .checked_sub(displacement)
                .ok_or_else(|| "negative-stride row precedes mapped data".to_owned())?
        };
        let end = start
            .checked_add(row_bytes)
            .ok_or_else(|| "pixel row end overflow".to_owned())?;
        if end > data.len() {
            return Err("pixel row exceeds mapped maxsize".into());
        }
        append_rgba_pixels(&data[start..end], layout.format, &mut rgba)?;
    }
    Ok(rgba)
}

fn append_rgba_pixels(
    source: &[u8],
    format: VideoFormat,
    rgba: &mut Vec<u8>,
) -> Result<(), String> {
    if !source.len().is_multiple_of(4) {
        return Err("raw pixel segment is not four-byte aligned".into());
    }
    for pixel in source.as_chunks::<4>().0 {
        match format {
            VideoFormat::BGRx => rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]),
            VideoFormat::RGBx => rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]),
            VideoFormat::BGRA => rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]),
            VideoFormat::RGBA => rgba.extend_from_slice(pixel),
            other => return Err(format!("unsupported raw pixel format {other:?}")),
        }
    }
    Ok(())
}

fn spa_transform(value: MetaVideoTransformValue) -> Option<Transform> {
    Some(match value {
        MetaVideoTransformValue::NONE => Transform::Normal,
        MetaVideoTransformValue::ROTATED90 => Transform::Rotate90,
        MetaVideoTransformValue::ROTATED180 => Transform::Rotate180,
        MetaVideoTransformValue::ROTATED270 => Transform::Rotate270,
        MetaVideoTransformValue::FLIPPED => Transform::Flip,
        MetaVideoTransformValue::FLIPPED90 => Transform::FlipRotate90,
        MetaVideoTransformValue::FLIPPED180 => Transform::FlipRotate180,
        MetaVideoTransformValue::FLIPPED270 => Transform::FlipRotate270,
        _ => return None,
    })
}

fn pw_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pw::spa::pod::deserialize::PodDeserializer;

    #[test]
    fn change_epoch_survives_format_renegotiation() {
        let (sender, _receiver) = watch::channel(None);
        let mut data = StreamUserData {
            stream_index: 0,
            generation: 5,
            format_generation: 2,
            format: None,
            last_source_sequence: Some(8),
            last_content_hash: Some(9),
            change_epoch: 7,
            prev_tiles: None,
            sender,
            failure: Arc::new(Mutex::new(None)),
        };

        begin_format(&mut data).unwrap();
        assert_eq!(data.format_generation, 3);
        assert_eq!(data.change_epoch, 7);
        invalidate_format(&mut data);
        assert_eq!(data.change_epoch, 7);
    }

    #[test]
    fn converts_formats_stride_offset_padding_and_negative_stride() {
        let data = [
            9, 9, 3, 2, 1, 0, 6, 5, 4, 0, 8, 8, 8, 8, 9, 8, 7, 0, 12, 11, 10, 0, 7, 7,
        ];
        let positive = convert_raw_frame(
            &data,
            RawLayout {
                width: 2,
                height: 2,
                offset: 2,
                size: 20,
                stride: 12,
                format: VideoFormat::BGRx,
            },
        )
        .unwrap();
        assert_eq!(
            positive,
            [1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255]
        );

        let negative = convert_raw_frame(
            &data,
            RawLayout {
                width: 2,
                height: 2,
                offset: 14,
                size: 20,
                stride: -12,
                format: VideoFormat::BGRA,
            },
        )
        .unwrap();
        assert_eq!(
            negative,
            [7, 8, 9, 0, 10, 11, 12, 0, 1, 2, 3, 0, 4, 5, 6, 0]
        );
    }

    #[test]
    fn rejects_incomplete_and_bad_stride_frames() {
        let layout = RawLayout {
            width: 2,
            height: 2,
            offset: 0,
            size: 8,
            stride: 8,
            format: VideoFormat::RGBA,
        };
        assert!(
            convert_raw_frame(&[0; 16], layout)
                .unwrap_err()
                .contains("chunk size")
        );
        assert!(
            convert_raw_frame(
                &[0; 16],
                RawLayout {
                    stride: 4,
                    size: 16,
                    ..layout
                }
            )
            .unwrap_err()
            .contains("stride")
        );
    }

    #[test]
    fn chunk_offsets_and_rows_must_stay_inside_mapped_maxsize() {
        let pixels = [1, 0, 0, 255, 2, 0, 0, 255, 3, 0, 0, 255, 4, 0, 0, 255];
        let error = convert_raw_frame(
            &pixels,
            RawLayout {
                width: 2,
                height: 1,
                offset: 20,
                size: 8,
                stride: 8,
                format: VideoFormat::RGBA,
            },
        )
        .unwrap_err();
        assert!(error.contains("offset"), "{error}");

        let error = convert_raw_frame(
            &pixels,
            RawLayout {
                width: 2,
                height: 1,
                offset: 12,
                size: 8,
                stride: 8,
                format: VideoFormat::RGBA,
            },
        )
        .unwrap_err();
        assert!(error.contains("exceeds"), "{error}");

        let error = convert_raw_frame(
            &pixels,
            RawLayout {
                offset: 14,
                ..RawLayout {
                    width: 2,
                    height: 1,
                    offset: 0,
                    size: 8,
                    stride: 8,
                    format: VideoFormat::RGBA,
                }
            },
        )
        .unwrap_err();
        assert!(
            error.contains("exceeds") || error.contains("offset"),
            "{error}"
        );
    }

    #[test]
    fn negotiated_params_request_shared_memory_and_all_supported_metadata() {
        let pods = negotiated_parameter_pods(RawFormat {
            format: VideoFormat::RGBA,
            width: 10,
            height: 20,
        })
        .unwrap();
        assert_eq!(pods.len(), 4);
        let (_, Value::Object(buffers)) = PodDeserializer::deserialize_any_from(&pods[0]).unwrap()
        else {
            panic!("buffer parameter was not an object")
        };
        assert_eq!(buffers.type_, SpaTypes::ObjectParamBuffers.as_raw());
        assert_eq!(buffers.id, ParamType::Buffers.as_raw());
        let data_types = buffers
            .properties
            .iter()
            .find(|property| property.key == pw::spa::sys::SPA_PARAM_BUFFERS_dataType)
            .unwrap();
        let Value::Choice(ChoiceValue::Int(Choice(_, ChoiceEnum::Flags { default, flags }))) =
            &data_types.value
        else {
            panic!("dataType was not a flags choice")
        };
        let expected = data_type_mask(&[DataType::MemFd, DataType::MemPtr]).unwrap();
        assert_eq!(*default, expected);
        assert!(flags.is_empty());

        let meta_types = pods[1..]
            .iter()
            .map(|pod| {
                let (_, Value::Object(meta)) = PodDeserializer::deserialize_any_from(pod).unwrap()
                else {
                    panic!("metadata parameter was not an object")
                };
                assert_eq!(meta.id, ParamType::Meta.as_raw());
                let property = meta
                    .properties
                    .iter()
                    .find(|property| property.key == pw::spa::sys::SPA_PARAM_META_type)
                    .unwrap();
                let Value::Id(Id(meta_type)) = property.value else {
                    panic!("metadata type was not an ID")
                };
                meta_type
            })
            .collect::<Vec<_>>();
        assert_eq!(
            meta_types,
            [
                MetaHeader::META_TYPE,
                MetaVideoCrop::META_TYPE,
                MetaVideoTransform::META_TYPE
            ]
        );
    }

    #[test]
    fn header_and_chunk_corruption_are_rejected() {
        assert!(header_is_usable(None));
        assert!(header_is_usable(Some(MetaHeaderFlags::DISCONT)));
        assert!(!header_is_usable(Some(MetaHeaderFlags::CORRUPTED)));
        assert!(!header_is_usable(Some(MetaHeaderFlags::GAP)));
    }

    #[test]
    fn frame_id_uses_local_generation_without_source_headers() {
        let mut metadata = frame(42, 1, (1, 1), 0).metadata;
        metadata.source_sequence = None;
        metadata.pts_ns = None;
        metadata.timestamp_authority = TimestampAuthority::Unavailable;

        assert_eq!(frame_id(&metadata), "frame-000000000000002a");
    }

    #[test]
    fn stream_errors_disconnects_and_node_loss_are_capture_failures() {
        assert!(
            stream_state_failure(
                3,
                &pw::stream::StreamState::Streaming,
                &pw::stream::StreamState::Error("broken".into())
            )
            .unwrap()
            .contains("broken")
        );
        assert!(
            stream_state_failure(
                3,
                &pw::stream::StreamState::Streaming,
                &pw::stream::StreamState::Unconnected
            )
            .unwrap()
            .contains("node disappeared")
        );
        assert!(
            stream_state_failure(
                3,
                &pw::stream::StreamState::Unconnected,
                &pw::stream::StreamState::Unconnected
            )
            .is_none()
        );
    }

    #[test]
    fn absent_crop_uses_the_full_negotiated_frame() {
        let format = RawFormat {
            format: VideoFormat::RGBA,
            width: 1920,
            height: 1080,
        };
        assert_eq!(
            optional_crop(None, format).unwrap(),
            PixelRect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            }
        );
    }

    fn frame(generation: u64, format_generation: u64, size: (u32, u32), value: u8) -> OwnedFrame {
        OwnedFrame {
            metadata: FrameMetadata {
                generation,
                format_generation,
                source_sequence: Some(generation),
                pts_ns: Some(i64::try_from(generation).unwrap_or_default()),
                arrival_monotonic_ns: generation,
                size,
                crop: PixelRect {
                    x: 0,
                    y: 0,
                    width: size.0,
                    height: size.1,
                },
                transform: Transform::Normal,
                timestamp_authority: TimestampAuthority::SpaHeader,
                stream_health: StreamHealth::Healthy,
                content_hash: hash_rgba(&vec![value; (size.0 * size.1 * 4) as usize]),
                change_epoch: 0,
                changed_from_previous: None,
                changed_rect: None,
                sequence_gap: None,
            },
            rgba: vec![value; (size.0 * size.1 * 4) as usize],
        }
    }

    #[tokio::test]
    async fn handle_failure_interrupts_frame_wait() {
        let (_frame_sender, frame_receiver) = watch::channel(None);
        let (status_sender, status) = watch::channel(None);
        let (_done_sender, thread_done) = std::sync::mpsc::channel();
        let mut handle = CaptureHandle {
            receiver: frame_receiver,
            status,
            failure: Arc::new(Mutex::new(None)),
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
            thread_done,
        };
        status_sender.send_replace(Some(Err("node disappeared".into())));
        let error = handle
            .latest_after(None, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert_eq!(error, "node disappeared");
        assert_eq!(handle.failure().as_deref(), Some("node disappeared"));
    }

    #[test]
    fn watch_channel_keeps_only_the_newest_complete_frame() {
        let (sender, receiver) = watch::channel(None);
        for generation in 1..=100 {
            sender.send_replace(Some(frame(generation, 1, (1, 1), 0)));
        }
        assert_eq!(receiver.borrow().as_ref().unwrap().metadata.generation, 100);
    }

    #[tokio::test]
    async fn renegotiation_clears_old_frame_until_current_format_frame_arrives() {
        let (sender, receiver) = watch::channel(Some(frame(1, 1, (1, 1), 1)));
        let failure = Arc::new(Mutex::new(None));
        let mut data = StreamUserData {
            stream_index: 0,
            generation: 1,
            format_generation: 1,
            format: Some(RawFormat {
                format: VideoFormat::RGBA,
                width: 1,
                height: 1,
            }),
            last_source_sequence: None,
            last_content_hash: None,
            change_epoch: 0,
            prev_tiles: None,
            sender: sender.clone(),
            failure,
        };
        begin_format(&mut data).unwrap();
        assert_eq!(data.format_generation, 2);
        assert!(receiver.borrow().is_none());

        let (_status_sender, status) = watch::channel(None);
        let (_done_sender, thread_done) = std::sync::mpsc::channel();
        let mut handle = CaptureHandle {
            receiver,
            status,
            failure: Arc::new(Mutex::new(None)),
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
            thread_done,
        };
        let producer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            sender.send_replace(Some(frame(2, 2, (2, 1), 2)));
        });
        let frame = handle
            .latest_after(None, Duration::from_secs(1))
            .await
            .unwrap();
        producer.await.unwrap();
        assert_eq!(
            (frame.metadata.generation, frame.metadata.format_generation),
            (2, 2)
        );
        assert_eq!(frame.rgba, vec![2; 8]);
    }

    #[test]
    fn same_format_renegotiation_also_invalidates_the_old_frame() {
        let format = RawFormat {
            format: VideoFormat::RGBA,
            width: 1,
            height: 1,
        };
        let (sender, receiver) = watch::channel(Some(frame(1, 1, (1, 1), 1)));
        let mut data = StreamUserData {
            stream_index: 0,
            generation: 1,
            format_generation: 1,
            format: Some(format),
            last_source_sequence: None,
            last_content_hash: None,
            change_epoch: 0,
            prev_tiles: None,
            sender,
            failure: Arc::new(Mutex::new(None)),
        };

        begin_format(&mut data).unwrap();

        assert_eq!(data.format_generation, 2);
        assert!(data.format.is_none());
        assert!(receiver.borrow().is_none());
    }

    #[test]
    fn format_negotiation_supports_shared_memory_and_excludes_dmabuf() {
        assert_eq!(supported_data_types(), [DataType::MemFd, DataType::MemPtr]);
        let mask = data_type_mask(&supported_data_types()).unwrap();
        let bit = |data_type: DataType| 1_i32 << data_type.as_raw();
        assert_ne!(mask & bit(DataType::MemFd), 0);
        assert_ne!(mask & bit(DataType::MemPtr), 0);
        assert_eq!(mask & bit(DataType::DmaBuf), 0);
        let pods = negotiated_parameter_pods(RawFormat {
            format: VideoFormat::BGRx,
            width: 64,
            height: 64,
        })
        .unwrap();
        assert!(!pods.is_empty());
    }

    #[test]
    fn dmabuf_only_error_is_terminal_and_distinct_from_corruption() {
        let error = dmabuf_only_error(3);
        assert!(error.contains("stream 3"), "{error}");
        assert!(error.contains("DMA-BUF-only"), "{error}");
        assert!(error.contains("shared-memory"), "{error}");
        assert!(error.contains("no GPU import path"), "{error}");
        assert!(error.contains("Recovery"), "{error}");
        assert!(!error.contains("corrupt"), "{error}");
    }

    fn solid_rgba(width: u32, height: u32, value: u8) -> Vec<u8> {
        vec![value; (width as usize) * (height as usize) * 4]
    }

    fn raw_format(width: u32, height: u32) -> RawFormat {
        RawFormat {
            format: VideoFormat::RGBA,
            width,
            height,
        }
    }

    #[test]
    fn first_published_frame_has_no_previous_comparison() {
        let (sender, receiver) = watch::channel(None);
        let mut user_data = StreamUserData {
            stream_index: 0,
            generation: 0,
            format_generation: 1,
            format: Some(raw_format(1, 1)),
            last_source_sequence: None,
            last_content_hash: None,
            change_epoch: 0,
            prev_tiles: None,
            sender,
            failure: Arc::new(Mutex::new(None)),
        };
        let rgba = vec![1, 2, 3, 255];
        let crop = PixelRect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        };
        publish_frame(
            &mut user_data,
            raw_format(1, 1),
            crop,
            Transform::Normal,
            None,
            rgba.clone(),
        );
        let first = receiver.borrow().clone().expect("first frame");
        assert_eq!(first.metadata.changed_from_previous, None);
        assert_eq!(first.metadata.changed_rect, None);
        assert_eq!(first.metadata.content_hash, hash_rgba(&rgba));

        publish_frame(
            &mut user_data,
            raw_format(1, 1),
            crop,
            Transform::Normal,
            None,
            rgba,
        );
        let second = receiver.borrow().clone().expect("second frame");
        assert_eq!(second.metadata.changed_from_previous, Some(false));
        assert_eq!(second.metadata.changed_rect, None);
    }

    #[test]
    fn changed_rect_is_none_without_a_previous_frame() {
        let grid = tile_grid_for_frame(&solid_rgba(64, 64, 7), raw_format(64, 64)).unwrap();
        assert_eq!((grid.tiles_x, grid.tiles_y), (2, 2));
        assert!(changed_rect_between(&grid, &grid).is_none());
    }

    #[test]
    fn changed_rect_bounds_only_changed_tiles() {
        let format = raw_format(64, 64);
        let before = tile_grid_for_frame(&solid_rgba(64, 64, 7), format).unwrap();
        let mut changed = solid_rgba(64, 64, 7);
        // Pixel inside tile (1, 0).
        let offset = (10 * 64 + 40) * 4;
        changed[offset] = 8;
        let after = tile_grid_for_frame(&changed, format).unwrap();
        assert_eq!(
            changed_rect_between(&before, &after),
            Some(ChangedRect {
                x: 32,
                y: 0,
                width: 32,
                height: 32,
            })
        );
    }

    #[test]
    fn changed_rect_spans_all_changed_tiles() {
        let format = raw_format(64, 64);
        let before = tile_grid_for_frame(&solid_rgba(64, 64, 7), format).unwrap();
        let mut changed = solid_rgba(64, 64, 7);
        changed[(5 * 64 + 5) * 4] = 8;
        changed[(50 * 64 + 50) * 4] = 9;
        let after = tile_grid_for_frame(&changed, format).unwrap();
        assert_eq!(
            changed_rect_between(&before, &after),
            Some(ChangedRect {
                x: 0,
                y: 0,
                width: 64,
                height: 64,
            })
        );
    }

    #[test]
    fn changed_rect_clamps_partial_edge_tiles() {
        let format = raw_format(48, 48);
        let before = tile_grid_for_frame(&solid_rgba(48, 48, 7), format).unwrap();
        let mut changed = solid_rgba(48, 48, 7);
        changed[(40 * 48 + 40) * 4] = 8;
        let after = tile_grid_for_frame(&changed, format).unwrap();
        assert_eq!(
            changed_rect_between(&before, &after),
            Some(ChangedRect {
                x: 32,
                y: 32,
                width: 16,
                height: 16,
            })
        );
    }

    #[test]
    fn changed_rect_reports_full_frame_on_dimension_mismatch() {
        let before = tile_grid_for_frame(&solid_rgba(64, 64, 7), raw_format(64, 64)).unwrap();
        let after = tile_grid_for_frame(&solid_rgba(32, 32, 7), raw_format(32, 32)).unwrap();
        assert_eq!(
            changed_rect_between(&before, &after),
            Some(ChangedRect {
                x: 0,
                y: 0,
                width: 32,
                height: 32,
            })
        );
    }

    #[test]
    fn tile_grid_rejects_size_mismatches() {
        assert!(tile_grid_for_frame(&solid_rgba(64, 64, 7), raw_format(32, 32)).is_none());
        assert!(
            tile_grid_for_frame(
                &solid_rgba(64, 64, 7),
                RawFormat {
                    format: VideoFormat::RGBA,
                    width: 0,
                    height: 64,
                }
            )
            .is_none()
        );
    }

    #[test]
    fn format_renegotiation_clears_tile_state() {
        let (sender, _receiver) = watch::channel(None);
        let format = raw_format(64, 64);
        let mut data = StreamUserData {
            stream_index: 0,
            generation: 5,
            format_generation: 2,
            format: Some(format),
            last_source_sequence: Some(8),
            last_content_hash: Some(9),
            change_epoch: 7,
            prev_tiles: tile_grid_for_frame(&solid_rgba(64, 64, 7), format),
            sender,
            failure: Arc::new(Mutex::new(None)),
        };
        assert!(data.prev_tiles.is_some());
        begin_format(&mut data).unwrap();
        assert!(data.prev_tiles.is_none());
        assert_eq!(data.change_epoch, 7);
    }
}
