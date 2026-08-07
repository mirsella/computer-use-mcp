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
pub struct FrameMetadata {
    pub generation: u64,
    pub format_generation: u64,
    pub size: (u32, u32),
    pub crop: PixelRect,
    pub transform: Transform,
}

pub trait CaptureBackend: Send + Sync + 'static {
    fn start(&self, fd: OwnedFd, target: CaptureTarget) -> Result<Box<dyn CaptureSession>, String>;
}

pub type CaptureFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;

pub trait CaptureSession: Send + 'static {
    fn wait_ready(&mut self) -> CaptureFuture<'_, ()>;
    fn failure(&self) -> Option<String>;
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
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let (done_sender, thread_done) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("computer-use-mcp-pipewire".into())
            .spawn(move || {
                let result = run_pipewire(fd, target, sender, &thread_stop);
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
        match self.status.borrow().as_ref() {
            Some(Err(error)) => Some(error.clone()),
            _ if self.status.has_changed().is_err() => {
                Some("PipeWire status channel closed unexpectedly".into())
            }
            _ => None,
        }
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
                    if let Some(frame) = receiver.borrow().clone()
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
    sender: watch::Sender<Option<OwnedFrame>>,
    failure: Arc<Mutex<Option<String>>>,
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
    stop: &AtomicBool,
) -> Result<(), String> {
    pw::init();
    let main_loop = pw::main_loop::MainLoopRc::new(None).map_err(pw_error)?;
    let context = pw::context::ContextRc::new(&main_loop, None).map_err(pw_error)?;
    let core = context
        .connect_fd_rc(fd, None)
        .map_err(|error| format!("cannot open the portal-restricted PipeWire remote: {error}"))?;
    let failure = Arc::new(Mutex::new(None));

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
        if let Some(error) = take_failure(&failure) {
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
    data.format = None;
    data.sender.send_replace(None);
    Ok(())
}

fn invalidate_format(data: &mut StreamUserData) {
    data.format = None;
    data.sender.send_replace(None);
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

fn take_failure(failure: &Mutex<Option<String>>) -> Option<String> {
    match failure.lock() {
        Ok(mut failure) => failure.take(),
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
    let data_type_mask = data_type_mask(&[DataType::MemFd, DataType::MemPtr])?;
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
        report_failure(
            &user_data.failure,
            format!(
                "PipeWire stream {} ignored the shared-memory request and supplied DMA-BUF-only data",
                user_data.stream_index
            ),
        );
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
    user_data.sender.send_replace(Some(OwnedFrame {
        metadata: FrameMetadata {
            generation: user_data.generation,
            format_generation: user_data.format_generation,
            size: (format.width, format.height),
            crop,
            transform,
        },
        rgba,
    }));
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
    let offset =
        usize::try_from(layout.offset).map_err(|_| "offset is too large".to_owned())? % data.len();
    let mut rgba = Vec::with_capacity(
        row_bytes
            .checked_mul(height)
            .ok_or_else(|| "output frame size overflow".to_owned())?,
    );
    for row in 0..height {
        let displacement = row
            .checked_mul(stride)
            .ok_or_else(|| "row offset overflow".to_owned())?;
        let distance = displacement % data.len();
        let start = if layout.stride > 0 {
            (offset + distance) % data.len()
        } else if distance <= offset {
            offset - distance
        } else {
            data.len() - (distance - offset)
        };
        if row_bytes > data.len() {
            return Err("a pixel row is larger than mapped maxsize".into());
        }
        let first_len = row_bytes.min(data.len() - start);
        if first_len < row_bytes && first_len % 4 != 0 {
            return Err("wrapped SPA row splits a pixel and is not safely contiguous".into());
        }
        append_rgba_pixels(&data[start..start + first_len], layout.format, &mut rgba)?;
        if first_len < row_bytes {
            append_rgba_pixels(&data[..row_bytes - first_len], layout.format, &mut rgba)?;
        }
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
    fn chunk_offset_is_modulo_maxsize_and_aligned_wrapped_rows_are_copied() {
        let pixels = [1, 0, 0, 255, 2, 0, 0, 255, 3, 0, 0, 255, 4, 0, 0, 255];
        let modulo = convert_raw_frame(
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
        .unwrap();
        assert_eq!(modulo, pixels[4..12]);

        let wrapped = convert_raw_frame(
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
        .unwrap();
        assert_eq!(wrapped, [4, 0, 0, 255, 1, 0, 0, 255]);

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
        assert!(error.contains("splits a pixel"));
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
                size,
                crop: PixelRect {
                    x: 0,
                    y: 0,
                    width: size.0,
                    height: size.1,
                },
                transform: Transform::Normal,
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
            sender,
            failure: Arc::new(Mutex::new(None)),
        };

        begin_format(&mut data).unwrap();

        assert_eq!(data.format_generation, 2);
        assert!(data.format.is_none());
        assert!(receiver.borrow().is_none());
    }
}
