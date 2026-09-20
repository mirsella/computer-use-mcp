use crate::{
    accessibility::Snapshot,
    capture::StreamHealth,
    portal::{PortalSessionLease, PortalStream},
    screenshot::ScreenshotMapping,
};

#[derive(Debug, Clone, Copy)]
pub struct ValidatedMapping<'a> {
    mapping: &'a ScreenshotMapping,
    mapping_degraded: bool,
}

impl<'a> ValidatedMapping<'a> {
    pub fn new(
        snapshot: &Snapshot,
        mapping: &'a ScreenshotMapping,
        session: &PortalSessionLease,
        stream: &PortalStream,
    ) -> Result<Self, String> {
        let mapping_degraded = validate_mapping(snapshot, mapping, session, stream)?;
        Ok(Self {
            mapping,
            mapping_degraded,
        })
    }

    pub fn mapping_degraded(&self) -> bool {
        self.mapping_degraded
    }

    pub fn eis_mapper(self, region: EisRegion) -> Result<AbsoluteMapper, String> {
        let route = EisRoute::from_stream(&self.mapping.stream)?;
        if !route.matches(&region) {
            return Err("selected EIS region does not match the monitor stream route".into());
        }
        if region.size.0 == 0 || region.size.1 == 0 {
            return Err("selected EIS region has invalid zero size".into());
        }
        let view = WindowCropMapping::new(
            self.mapping.source.size,
            self.mapping.source.crop,
            self.mapping
                .window_crop_source
                .unwrap_or(self.mapping.source.crop),
            self.mapping.source.transform,
        )?;
        Ok(AbsoluteMapper {
            output_size: self.mapping.output_size,
            region,
            view,
        })
    }
}

#[derive(Debug)]
pub(crate) enum EisRoute {
    MappingId(String),
    UniqueResumedRegion,
    ExactGeometry {
        position: (u32, u32),
        size: (u32, u32),
    },
}

impl EisRoute {
    pub(crate) fn from_stream(stream: &PortalStream) -> Result<Self, String> {
        if let Some(mapping_id) = &stream.mapping_id {
            return Ok(Self::MappingId(mapping_id.clone()));
        }
        let Some(position) = stream.position else {
            return Ok(Self::UniqueResumedRegion);
        };
        let size = stream.logical_size.ok_or(
            "monitor stream omitted mapping_id and logical size; generated input cannot be bound by exact geometry",
        )?;
        let nonnegative = |(first, second), error: &str| {
            Ok::<_, String>((
                u32::try_from(first).map_err(|_| error.to_owned())?,
                u32::try_from(second).map_err(|_| error.to_owned())?,
            ))
        };
        let position = nonnegative(
            position,
            "monitor stream omitted mapping_id and has a negative position; generated input cannot be bound by exact geometry",
        )?;
        let size = nonnegative(
            size,
            "monitor stream omitted mapping_id and has a negative logical size",
        )?;
        if size.0 == 0 || size.1 == 0 {
            return Err(
                "monitor stream omitted mapping_id and has an invalid zero logical size".into(),
            );
        }
        Ok(Self::ExactGeometry { position, size })
    }

    pub(crate) fn matches(&self, region: &EisRegion) -> bool {
        match self {
            Self::MappingId(mapping_id) => region.mapping_id.as_deref() == Some(mapping_id),
            Self::UniqueResumedRegion => true,
            Self::ExactGeometry { position, size } => {
                region.position == *position && region.size == *size
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EisRegion {
    pub position: (u32, u32),
    pub size: (u32, u32),
    pub mapping_id: Option<String>,
}

impl EisRegion {
    fn contains_protocol_point(&self, x: f64, y: f64) -> bool {
        x >= f64::from(self.position.0)
            && y >= f64::from(self.position.1)
            && x < f64::from(self.position.0) + f64::from(self.size.0)
            && y < f64::from(self.position.1) + f64::from(self.size.1)
    }
}

/// Monitor-ish one-line description of a single EIS region for evidence
/// strings, e.g. `HDMI-A-1@(1920,0)+1080x1920` or `(0,421)+1920x1080` when
/// the compositor advertised no mapping id.
pub fn describe_region(region: &EisRegion) -> String {
    match &region.mapping_id {
        Some(id) => format!(
            "{id}@({},{})+{}x{}",
            region.position.0, region.position.1, region.size.0, region.size.1
        ),
        None => format!(
            "({},{})+{}x{}",
            region.position.0, region.position.1, region.size.0, region.size.1
        ),
    }
}

/// Maximum relative aspect deviation between the EIS region union bounding
/// box and the PNG frame for union disambiguation to engage.
pub(crate) const UNION_ASPECT_TOLERANCE: f64 = 0.01;

/// Authoritative extent advertised by the portal for a stream that may cover
/// several EIS regions. The extent must equal the region union; matching only
/// the PNG aspect ratio is not sufficient because equal 2x2 monitor layouts
/// can have the same ratio as one monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamExtent {
    pub position: (u32, u32),
    pub size: (u32, u32),
}

impl StreamExtent {
    /// Convert the portal's signed stream geometry into the non-negative EIS
    /// coordinate space. Missing or negative metadata is not authoritative
    /// enough to justify a multi-region union.
    pub(crate) fn from_portal_stream(stream: &PortalStream) -> Option<Self> {
        let position = stream.position?;
        let size = stream.logical_size?;
        Some(Self {
            position: (
                u32::try_from(position.0).ok()?,
                u32::try_from(position.1).ok()?,
            ),
            size: (u32::try_from(size.0).ok()?, u32::try_from(size.1).ok()?),
        })
    }
}

/// Resolve an ambiguous multi-region EIS advertisement (approach (b)) only
/// when the caller supplies the portal's authoritative stream extent.
pub(crate) fn resolve_union_tiling_with_extent(
    regions: &[EisRegion],
    output_size: (u32, u32),
    stream_extent: StreamExtent,
    window_cropped: bool,
    png_points: &[(f64, f64)],
) -> Result<(EisRegion, EisRegion), String> {
    resolve_union_tiling_checked(
        regions,
        output_size,
        stream_extent,
        window_cropped,
        png_points,
    )
}

fn resolve_union_tiling_checked(
    regions: &[EisRegion],
    output_size: (u32, u32),
    stream_extent: StreamExtent,
    window_cropped: bool,
    png_points: &[(f64, f64)],
) -> Result<(EisRegion, EisRegion), String> {
    if regions.len() < 2 {
        return Err("EIS region disambiguation requires at least two matching regions".into());
    }
    if regions
        .iter()
        .any(|region| region.size.0 == 0 || region.size.1 == 0)
    {
        return Err(
            "EIS regions advertise an invalid zero size; refusing union disambiguation".into(),
        );
    }
    if regions.iter().any(|region| {
        region.position.0.checked_add(region.size.0).is_none()
            || region.position.1.checked_add(region.size.1).is_none()
    }) {
        return Err(
            "EIS region position and size overflow their coordinate space; refusing union disambiguation"
                .into(),
        );
    }
    if window_cropped {
        return Err(
            "advisory window crop is active; the PNG is a sub-frame whose geometry cannot be checked against the EIS region union"
                .into(),
        );
    }
    for (index, first) in regions.iter().enumerate() {
        for second in &regions[index + 1..] {
            if regions_overlap(first, second) {
                return Err(format!(
                    "EIS regions overlap ({} vs {}); refusing union disambiguation",
                    describe_region(first),
                    describe_region(second)
                ));
            }
        }
    }
    let union = union_bbox(regions);
    if union.position != stream_extent.position || union.size != stream_extent.size {
        return Err(format!(
            "EIS region union {:?}+{:?} does not match authoritative stream extent {:?}+{:?}; refusing union disambiguation",
            union.position, union.size, stream_extent.position, stream_extent.size
        ));
    }
    if output_size.0 == 0 || output_size.1 == 0 {
        return Err("screenshot PNG has an invalid zero size".into());
    }
    let union_aspect = f64::from(union.size.0) / f64::from(union.size.1);
    let png_aspect = f64::from(output_size.0) / f64::from(output_size.1);
    if ((union_aspect - png_aspect) / png_aspect).abs() > UNION_ASPECT_TOLERANCE {
        return Err(format!(
            "EIS region union aspect {union_aspect:.4} does not match the PNG aspect {png_aspect:.4}; refusing union disambiguation"
        ));
    }
    if png_points.is_empty() {
        return Err("EIS region disambiguation requires at least one action point".into());
    }
    let mut containing: Option<&EisRegion> = None;
    for (x, y) in png_points {
        if !x.is_finite() || !y.is_finite() {
            return Err("screenshot coordinates must be finite".into());
        }
        let global_x =
            f64::from(union.position.0) + (*x / f64::from(output_size.0)) * f64::from(union.size.0);
        let global_y =
            f64::from(union.position.1) + (*y / f64::from(output_size.1)) * f64::from(union.size.1);
        // Quantize exactly like AbsoluteMapper::point so containment agrees
        // with the coordinates the protocol will actually carry.
        let protocol_x = f64::from(global_x as f32);
        let protocol_y = f64::from(global_y as f32);
        let mut hit: Option<&EisRegion> = None;
        for region in regions {
            if region.contains_protocol_point(protocol_x, protocol_y) {
                if hit.is_some() {
                    return Err(format!(
                        "screenshot point ({x}, {y}) lands in several EIS regions; refusing union disambiguation"
                    ));
                }
                hit = Some(region);
            }
        }
        let hit = hit.ok_or_else(|| {
            format!(
                "screenshot point ({x}, {y}) lands outside every resumed EIS region; refusing union disambiguation"
            )
        })?;
        match containing {
            Some(current) if current == hit => {}
            Some(current) => {
                return Err(format!(
                    "action points span several EIS regions ({} vs {}); refusing union disambiguation",
                    describe_region(current),
                    describe_region(hit)
                ));
            }
            None => containing = Some(hit),
        }
    }
    Ok((union, containing.expect("points checked non-empty").clone()))
}

/// Bounding box of non-empty region slices. Callers verify non-overlap and
/// aspect first; the box may cover gaps, which per-point containment rejects.
fn union_bbox(regions: &[EisRegion]) -> EisRegion {
    let min_x = regions
        .iter()
        .map(|region| region.position.0)
        .min()
        .unwrap_or(0);
    let min_y = regions
        .iter()
        .map(|region| region.position.1)
        .min()
        .unwrap_or(0);
    let max_x = regions
        .iter()
        .map(|region| region.position.0.saturating_add(region.size.0))
        .max()
        .unwrap_or(0);
    let max_y = regions
        .iter()
        .map(|region| region.position.1.saturating_add(region.size.1))
        .max()
        .unwrap_or(0);
    EisRegion {
        position: (min_x, min_y),
        size: (max_x.saturating_sub(min_x), max_y.saturating_sub(min_y)),
        mapping_id: None,
    }
}

fn regions_overlap(first: &EisRegion, second: &EisRegion) -> bool {
    let first_right = first.position.0.saturating_add(first.size.0);
    let second_right = second.position.0.saturating_add(second.size.0);
    let first_bottom = first.position.1.saturating_add(first.size.1);
    let second_bottom = second.position.1.saturating_add(second.size.1);
    first.position.0 < second_right
        && second.position.0 < first_right
        && first.position.1 < second_bottom
        && second.position.1 < first_bottom
}

#[derive(Debug, Clone)]
pub struct AbsoluteMapper {
    output_size: (u32, u32),
    region: EisRegion,
    view: WindowCropMapping,
}

/// Remaps PNG coordinates back to full-frame monitor fractions. `view_rect` is
/// the pre-transform source-pixel rect actually encoded. `source_crop` is the
/// authoritative SPA crop, which matters when the compositor delivers only a
/// subregion of the negotiated frame. Transform inversion happens before the
/// source pixels are divided by the full frame size.
#[derive(Debug, Clone, Copy)]
struct WindowCropMapping {
    source_size: (u32, u32),
    view_rect: crate::geometry::PixelRect,
    transform: crate::geometry::Transform,
}

impl WindowCropMapping {
    fn new(
        source_size: (u32, u32),
        source_crop: crate::geometry::PixelRect,
        view_rect: crate::geometry::PixelRect,
        transform: crate::geometry::Transform,
    ) -> Result<Self, String> {
        if !source_crop.is_valid_within(source_size) {
            return Err("source crop escapes the captured frame".into());
        }
        let source_right = source_crop.x.saturating_add(source_crop.width);
        let source_bottom = source_crop.y.saturating_add(source_crop.height);
        if !view_rect.is_valid_within(source_size)
            || view_rect.x < source_crop.x
            || view_rect.y < source_crop.y
            || view_rect
                .x
                .checked_add(view_rect.width)
                .is_none_or(|right| right > source_right)
            || view_rect
                .y
                .checked_add(view_rect.height)
                .is_none_or(|bottom| bottom > source_bottom)
        {
            return Err("encoded source view escapes the captured source crop".into());
        }
        Ok(Self {
            source_size,
            view_rect,
            transform,
        })
    }

    fn monitor_fractions(&self, png_x: f64, png_y: f64, output_size: (u32, u32)) -> (f64, f64) {
        let output_u = png_x / f64::from(output_size.0);
        let output_v = png_y / f64::from(output_size.1);
        let (source_u, source_v) = self.transform.source_fraction(output_u, output_v);
        let source_x = f64::from(self.view_rect.x) + source_u * f64::from(self.view_rect.width);
        let source_y = f64::from(self.view_rect.y) + source_v * f64::from(self.view_rect.height);
        (
            source_x / f64::from(self.source_size.0),
            source_y / f64::from(self.source_size.1),
        )
    }
}

impl AbsoluteMapper {
    pub fn point(&self, png_x: f64, png_y: f64) -> Result<(f64, f64), String> {
        if !png_x.is_finite() || !png_y.is_finite() {
            return Err("screenshot coordinates must be finite".into());
        }
        let (png_width, png_height) = self.output_size;
        if png_x < 0.0
            || png_y < 0.0
            || png_x >= f64::from(png_width)
            || png_y >= f64::from(png_height)
        {
            return Err(format!(
                "screenshot coordinate ({png_x}, {png_y}) is outside exact PNG bounds [0, {png_width}) x [0, {png_height})"
            ));
        }

        let (local_x, local_y) = self.view.monitor_fractions(png_x, png_y, self.output_size);
        let global_x = f64::from(self.region.position.0) + local_x * f64::from(self.region.size.0);
        let global_y = f64::from(self.region.position.1) + local_y * f64::from(self.region.size.1);
        let protocol_x = f64::from(global_x as f32);
        let protocol_y = f64::from(global_y as f32);
        let right = f64::from(self.region.position.0) + f64::from(self.region.size.0);
        let bottom = f64::from(self.region.position.1) + f64::from(self.region.size.1);
        if protocol_x < f64::from(self.region.position.0)
            || protocol_y < f64::from(self.region.position.1)
            || protocol_x >= right
            || protocol_y >= bottom
        {
            return Err("screenshot coordinate cannot be represented inside the EIS region".into());
        }
        Ok((protocol_x, protocol_y))
    }
}

fn validate_mapping(
    snapshot: &Snapshot,
    mapping: &ScreenshotMapping,
    session: &PortalSessionLease,
    stream: &PortalStream,
) -> Result<bool, String> {
    if mapping.app_pid != snapshot.app.pid
        || mapping.app_identity != snapshot.app.object
        || mapping.window_identity != snapshot.window.object
        || mapping.accessibility_generation != snapshot.generation
    {
        return Err("screenshot mapping is stale for the current app/window generation".into());
    }
    if session.is_closed()
        || session.identity() != mapping.portal_session_identity
        || session.generation() != mapping.portal_session_generation
    {
        return Err("portal session identity is stale or closed".into());
    }
    if mapping.output_size.0 == 0 || mapping.output_size.1 == 0 {
        return Err("screenshot mapping has invalid output bounds".into());
    }
    if mapping.source.stream_health == StreamHealth::Failed {
        return Err("screenshot mapping stream health is failed".into());
    }
    let mapping_degraded = mapping.source.stream_health == StreamHealth::Degraded;
    if mapping_degraded {
        eprintln!(
            "computer-use-mcp: proceeding with degraded screenshot mapping; frame discontinuity risk — coordinates may misdeliver"
        );
    }
    if stream != &mapping.stream {
        return Err("live portal stream metadata changed".into());
    }
    Ok(mapping_degraded)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::{
        accessibility::{AppInfo, ObjectId, Snapshot, SnapshotLimits, WindowInfo},
        capture::{FrameMetadata, StreamHealth},
        geometry::{PixelRect, Transform},
    };

    fn fixture() -> (Snapshot, ScreenshotMapping, PortalStream) {
        let window = ObjectId {
            bus_name: ":1.2".into(),
            path: "/window".into(),
        };
        let snapshot = Snapshot {
            view: crate::validation::AccessibilityScope::Full,
            element_query: None,
            app: AppInfo {
                object: ObjectId {
                    bus_name: ":1.2".into(),
                    path: "/app".into(),
                },
                name: "App".into(),
                pid: 9,
                windows: Vec::new(),
            },
            window: WindowInfo {
                object: window.clone(),
                title: "Window".into(),
                states: BTreeSet::from(["active".into()]),
            },
            generation: 7,
            elements: Vec::new(),
            element_ids: Vec::new(),
            node_limit_reached: false,
            depth_limit_reached: false,
            limits: SnapshotLimits {
                text: 20,
                nodes: 20,
                depth: 5,
            },
            target_ref: None,
            accessibility_ready: true,
            accessibility_reason: None,
            requires_atspi_revalidation: true,
            screenshot_requested: true,
            crop: crate::validation::ObserveCrop::Monitor,
        };
        let mapping = ScreenshotMapping {
            app_pid: 9,
            app_identity: snapshot.app.object.clone(),
            window_identity: window,
            accessibility_generation: 7,
            portal_session_identity: "/session/test".into(),
            portal_session_generation: 4,
            stream: PortalStream {
                stream_index: 0,
                node_id: 22,
                pipewire_serial: Some(33),
                id: Some("stream".into()),
                mapping_id: Some("map".into()),
                position: Some((-1600, 0)),
                logical_size: Some((400, 300)),
            },
            source: FrameMetadata {
                generation: 8,
                format_generation: 1,
                source_sequence: Some(8),
                pts_ns: Some(8),
                arrival_monotonic_ns: 8,
                size: (600, 450),
                crop: PixelRect {
                    x: 0,
                    y: 0,
                    width: 600,
                    height: 450,
                },
                transform: Transform::Normal,
                timestamp_authority: crate::capture::TimestampAuthority::SpaHeader,
                stream_health: crate::capture::StreamHealth::Healthy,
                content_hash: 0,
                change_epoch: 0,
                changed_from_previous: None,
                changed_rect: None,
                sequence_gap: None,
            },
            output_size: (600, 450),
            crop: crate::validation::ObserveCrop::Monitor,
            window_crop_source: None,
            window_crop_geometry: None,
            window_crop_is_preencoded: false,
        };
        let stream = mapping.stream.clone();
        (snapshot, mapping, stream)
    }

    fn map_png_point(
        snapshot: &Snapshot,
        mapping: &ScreenshotMapping,
        session: &PortalSessionLease,
        stream: &PortalStream,
        x: f64,
        y: f64,
    ) -> Result<(f64, f64), String> {
        ValidatedMapping::new(snapshot, mapping, session, stream)?
            .eis_mapper(EisRegion {
                position: (800, 200),
                size: (1200, 900),
                mapping_id: mapping.stream.mapping_id.clone(),
            })?
            .point(x, y)
    }

    #[test]
    fn maps_png_fraction_into_private_eis_region_without_portal_geometry() {
        let (snapshot, mut mapping, mut stream) = fixture();
        mapping.stream.position = None;
        mapping.stream.logical_size = None;
        stream.position = None;
        stream.logical_size = None;
        let (session, _) = PortalSessionLease::for_test("/session/test", 4);
        let point = ValidatedMapping::new(&snapshot, &mapping, &session, &stream)
            .unwrap()
            .eis_mapper(EisRegion {
                position: (800, 200),
                size: (1200, 900),
                mapping_id: Some("map".into()),
            })
            .unwrap()
            .point(75.0, 30.0)
            .unwrap();
        assert_eq!(point, (950.0, 260.0));
    }

    #[test]
    fn rejects_exact_edges_stale_state_and_changed_streams() {
        let (snapshot, mapping, stream) = fixture();
        let (session, closed) = PortalSessionLease::for_test("/session/test", 4);
        assert!(map_png_point(&snapshot, &mapping, &session, &stream, 599.999, 0.0).is_ok());
        assert!(
            map_png_point(&snapshot, &mapping, &session, &stream, 600.0, 0.0,)
                .unwrap_err()
                .contains("bounds")
        );
        assert!(map_png_point(&snapshot, &mapping, &session, &stream, -0.01, 0.0,).is_err());

        let mut stale = snapshot.clone();
        stale.generation += 1;
        assert!(
            map_png_point(&stale, &mapping, &session, &stream, 1.0, 1.0,)
                .unwrap_err()
                .contains("stale")
        );
        let mut changed = stream.clone();
        changed.logical_size = Some((401, 300));
        assert!(
            map_png_point(&snapshot, &mapping, &session, &changed, 1.0, 1.0)
                .unwrap_err()
                .contains("changed")
        );
        let mut failed = mapping.clone();
        failed.source.stream_health = StreamHealth::Failed;
        assert!(
            map_png_point(&snapshot, &failed, &session, &stream, 1.0, 1.0)
                .unwrap_err()
                .contains("health")
        );
        closed.send_replace(true);
        assert!(
            map_png_point(&snapshot, &mapping, &session, &stream, 1.0, 1.0)
                .unwrap_err()
                .contains("closed")
        );
    }

    #[test]
    fn maps_kde_stream_without_mapping_id_by_exact_geometry() {
        let (snapshot, mut mapping, _) = fixture();
        mapping.stream.mapping_id = None;
        mapping.stream.position = Some((800, 200));
        mapping.stream.logical_size = Some((1200, 900));
        let stream = mapping.stream.clone();
        let (session, _) = PortalSessionLease::for_test("/session/test", 4);

        let point = map_png_point(&snapshot, &mapping, &session, &stream, 75.0, 30.0).unwrap();

        assert_eq!(point, (950.0, 260.0));
    }

    #[test]
    fn missing_mapping_metadata_uses_unique_resumed_region() {
        let (_, mut mapping, _) = fixture();
        mapping.stream.mapping_id = None;
        mapping.stream.position = None;
        let route = EisRoute::from_stream(&mapping.stream).unwrap();
        assert!(route.matches(&EisRegion {
            position: (800, 200),
            size: (1200, 900),
            mapping_id: None,
        }));

        mapping.stream.position = Some((-1, 0));
        let error = EisRoute::from_stream(&mapping.stream).unwrap_err();
        assert!(error.contains("negative position"));
    }

    #[test]
    fn cropped_png_coordinates_remap_through_source_rect_to_monitor_space() {
        let (snapshot, mut mapping, stream) = fixture();
        mapping.window_crop_source = Some(PixelRect {
            x: 150,
            y: 90,
            width: 300,
            height: 180,
        });
        mapping.output_size = (300, 180);
        let (session, _) = PortalSessionLease::for_test("/session/test", 4);
        // PNG center (150, 90) is source pixel (300, 180): fractions
        // (0.5, 0.4) of the full frame, so the EIS point must equal the
        // full-monitor mapping of PNG (300, 180).
        let cropped = ValidatedMapping::new(&snapshot, &mapping, &session, &stream)
            .unwrap()
            .eis_mapper(EisRegion {
                position: (800, 200),
                size: (1200, 900),
                mapping_id: Some("map".into()),
            })
            .unwrap()
            .point(150.0, 90.0)
            .unwrap();
        let full = map_png_point(&snapshot, &fixture().1, &session, &stream, 300.0, 180.0).unwrap();
        assert_eq!(cropped, full);
        assert_eq!(cropped, (1400.0, 560.0));
    }

    #[test]
    fn rotated_fractionally_downscaled_crop_maps_through_source_crop_and_inverse_transform() {
        let (snapshot, mut mapping, stream) = fixture();
        mapping.source.crop = PixelRect {
            x: 50,
            y: 25,
            width: 500,
            height: 400,
        };
        mapping.source.transform = Transform::Rotate90;
        mapping.window_crop_source = Some(PixelRect {
            x: 150,
            y: 90,
            width: 300,
            height: 180,
        });
        // Rotate90 swaps the 300x180 view to 180x300; this is a real encoded
        // view size, not a marker coordinate. The point is also downscaled
        // relative to the source crop.
        mapping.output_size = (180, 300);
        let (session, _) = PortalSessionLease::for_test("/session/test", 4);
        let point = ValidatedMapping::new(&snapshot, &mapping, &session, &stream)
            .unwrap()
            .eis_mapper(EisRegion {
                position: (800, 200),
                size: (1200, 900),
                mapping_id: Some("map".into()),
            })
            .unwrap()
            .point(45.0, 75.0)
            .unwrap();
        // (45/180, 75/300) inverse-rotates to (0.75, 0.25), hence source
        // pixel (375, 135), then full-frame fractions (0.625, 0.3).
        assert_eq!(point, (1550.0, 470.0));
    }

    #[test]
    fn degraded_mapping_is_accepted_with_downgrade_flag_and_failed_stays_refused() {
        let (snapshot, mut mapping, stream) = fixture();
        let (session, _) = PortalSessionLease::for_test("/session/test", 4);
        let healthy = ValidatedMapping::new(&snapshot, &mapping, &session, &stream).unwrap();
        assert!(!healthy.mapping_degraded());
        assert!(map_png_point(&snapshot, &mapping, &session, &stream, 1.0, 1.0).is_ok());

        mapping.source.stream_health = StreamHealth::Degraded;
        let degraded = ValidatedMapping::new(&snapshot, &mapping, &session, &stream).unwrap();
        assert!(degraded.mapping_degraded());
        assert!(map_png_point(&snapshot, &mapping, &session, &stream, 1.0, 1.0).is_ok());

        mapping.source.stream_health = StreamHealth::Failed;
        let error = ValidatedMapping::new(&snapshot, &mapping, &session, &stream).unwrap_err();
        assert!(error.contains("health"));
        assert!(
            map_png_point(&snapshot, &mapping, &session, &stream, 1.0, 1.0)
                .unwrap_err()
                .contains("health")
        );
    }

    #[test]
    fn window_crop_source_escaping_the_frame_refuses_input() {
        let (snapshot, mut mapping, stream) = fixture();
        mapping.window_crop_source = Some(PixelRect {
            x: 500,
            y: 0,
            width: 200,
            height: 100,
        });
        let (session, _) = PortalSessionLease::for_test("/session/test", 4);
        let error = ValidatedMapping::new(&snapshot, &mapping, &session, &stream)
            .unwrap()
            .eis_mapper(EisRegion {
                position: (800, 200),
                size: (1200, 900),
                mapping_id: Some("map".into()),
            })
            .unwrap_err();
        assert!(error.contains("escapes"));
    }

    /// Live ground truth (multi-monitor KDE): one EIS pointer device
    /// advertises `(0,421) 1920x1080 mapping_id=DP-2` and
    /// `(1920,0) 1080x1920 mapping_id=HDMI-A-1` while the monitor stream
    /// shows the combined 3000x1920 desktop as a 1280x819 PNG.
    fn ground_truth_regions() -> Vec<EisRegion> {
        vec![
            EisRegion {
                position: (0, 421),
                size: (1920, 1080),
                mapping_id: Some("DP-2".into()),
            },
            EisRegion {
                position: (1920, 0),
                size: (1080, 1920),
                mapping_id: Some("HDMI-A-1".into()),
            },
        ]
    }

    fn ground_truth_extent() -> StreamExtent {
        StreamExtent {
            position: (0, 0),
            size: (3000, 1920),
        }
    }

    #[test]
    fn union_tiling_resolves_ground_truth_layout() {
        let (union, containing) = resolve_union_tiling_with_extent(
            &ground_truth_regions(),
            (1280, 819),
            ground_truth_extent(),
            false,
            &[(640.0, 400.0)],
        )
        .unwrap();
        assert_eq!(union.position, (0, 0));
        assert_eq!(union.size, (3000, 1920));
        assert_eq!(union.mapping_id, None);
        assert_eq!(containing.mapping_id.as_deref(), Some("DP-2"));
        assert_eq!(describe_region(&containing), "DP-2@(0,421)+1920x1080");
        // A point in the right-hand monitor resolves to the other region.
        let (_, containing) = resolve_union_tiling_with_extent(
            &ground_truth_regions(),
            (1280, 819),
            ground_truth_extent(),
            false,
            &[(1100.0, 200.0)],
        )
        .unwrap();
        assert_eq!(containing.mapping_id.as_deref(), Some("HDMI-A-1"));
        assert_eq!(describe_region(&containing), "HDMI-A-1@(1920,0)+1080x1920");
    }

    #[test]
    fn union_gap_point_refuses() {
        // (640,100) maps into the union bounding box above DP-2, where no
        // resumed region exists (the union box covers a gap).
        let error = resolve_union_tiling_with_extent(
            &ground_truth_regions(),
            (1280, 819),
            ground_truth_extent(),
            false,
            &[(640.0, 100.0)],
        )
        .unwrap_err();
        assert!(
            error.contains("outside every resumed EIS region"),
            "{error}"
        );
    }

    #[test]
    fn union_aspect_mismatch_refuses() {
        // Two identical-size side-by-side monitors whose union does not look
        // like the PNG frame must stay fail-closed.
        let regions = vec![
            EisRegion {
                position: (0, 0),
                size: (1920, 1080),
                mapping_id: None,
            },
            EisRegion {
                position: (1920, 0),
                size: (1920, 1080),
                mapping_id: None,
            },
        ];
        let error = resolve_union_tiling_with_extent(
            &regions,
            (1280, 819),
            StreamExtent {
                position: (0, 0),
                size: (3840, 1080),
            },
            false,
            &[(640.0, 400.0)],
        )
        .unwrap_err();
        assert!(error.contains("aspect"), "{error}");
    }

    #[test]
    fn equal_two_by_two_layout_is_not_proven_by_single_monitor_ratio() {
        let regions = vec![
            EisRegion {
                position: (0, 0),
                size: (1920, 1080),
                mapping_id: None,
            },
            EisRegion {
                position: (1920, 0),
                size: (1920, 1080),
                mapping_id: None,
            },
            EisRegion {
                position: (0, 1080),
                size: (1920, 1080),
                mapping_id: None,
            },
            EisRegion {
                position: (1920, 1080),
                size: (1920, 1080),
                mapping_id: None,
            },
        ];
        let error = resolve_union_tiling_with_extent(
            &regions,
            (1920, 1080),
            StreamExtent {
                position: (0, 0),
                size: (1920, 1080),
            },
            false,
            &[(100.0, 100.0)],
        )
        .unwrap_err();
        assert!(error.contains("does not match authoritative"), "{error}");

        let (_, containing) = resolve_union_tiling_with_extent(
            &regions,
            (1920, 1080),
            StreamExtent {
                position: (0, 0),
                size: (3840, 2160),
            },
            false,
            &[(100.0, 100.0)],
        )
        .unwrap();
        assert_eq!(containing.position, (0, 0));
    }

    #[test]
    fn union_identical_duplicate_regions_refuse() {
        let region = EisRegion {
            position: (0, 0),
            size: (1920, 1080),
            mapping_id: Some("DP-2".into()),
        };
        let error = resolve_union_tiling_with_extent(
            &[region.clone(), region],
            (1920, 1080),
            StreamExtent {
                position: (0, 0),
                size: (1920, 1080),
            },
            false,
            &[(100.0, 100.0)],
        )
        .unwrap_err();
        assert!(error.contains("overlap"), "{error}");
    }

    #[test]
    fn union_cross_region_drag_refuses() {
        // Each endpoint is inside exactly one region, but they disagree:
        // a single-region binding cannot carry this path.
        let error = resolve_union_tiling_with_extent(
            &ground_truth_regions(),
            (1280, 819),
            ground_truth_extent(),
            false,
            &[(640.0, 400.0), (1100.0, 200.0)],
        )
        .unwrap_err();
        assert!(error.contains("span"), "{error}");
    }

    #[test]
    fn union_window_crop_refuses() {
        let error = resolve_union_tiling_with_extent(
            &ground_truth_regions(),
            (1280, 819),
            ground_truth_extent(),
            true,
            &[(640.0, 400.0)],
        )
        .unwrap_err();
        assert!(error.contains("sub-frame"), "{error}");
    }

    #[test]
    fn union_single_region_refuses_union_resolution() {
        let error = resolve_union_tiling_with_extent(
            &ground_truth_regions()[..1],
            (1280, 819),
            ground_truth_extent(),
            false,
            &[(640.0, 400.0)],
        )
        .unwrap_err();
        assert!(error.contains("at least two"), "{error}");
    }
}
