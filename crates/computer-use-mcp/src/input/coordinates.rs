use crate::{
    accessibility::Snapshot,
    portal::{PortalSessionLease, PortalStream},
    screenshot::ScreenshotMapping,
};

#[derive(Debug, Clone, Copy)]
pub struct ValidatedMapping<'a> {
    mapping: &'a ScreenshotMapping,
}

impl<'a> ValidatedMapping<'a> {
    pub fn new(
        snapshot: &Snapshot,
        mapping: &'a ScreenshotMapping,
        session: &PortalSessionLease,
        stream: &PortalStream,
    ) -> Result<Self, String> {
        validate_mapping(snapshot, mapping, session, stream)?;
        Ok(Self { mapping })
    }

    pub fn eis_mapper(self, region: EisRegion) -> Result<AbsoluteMapper, String> {
        let route = EisRoute::from_stream(&self.mapping.stream)?;
        if !route.matches(&region) {
            return Err("selected EIS region does not match the monitor stream route".into());
        }
        if region.size.0 == 0 || region.size.1 == 0 {
            return Err("selected EIS region has invalid zero size".into());
        }
        Ok(AbsoluteMapper {
            output_size: self.mapping.output_size,
            region,
        })
    }
}

#[derive(Debug)]
pub(crate) enum EisRoute {
    MappingId(String),
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
        let position = stream.position.ok_or(
            "monitor stream omitted mapping_id and position; generated input cannot be bound to the approved monitor",
        )?;
        let size = stream.logical_size.ok_or(
            "monitor stream omitted mapping_id and logical size; generated input cannot be bound to the approved monitor",
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

#[derive(Debug, Clone)]
pub struct AbsoluteMapper {
    output_size: (u32, u32),
    region: EisRegion,
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

        let local_x = png_x / f64::from(png_width);
        let local_y = png_y / f64::from(png_height);
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
) -> Result<(), String> {
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
    if stream != &mapping.stream {
        return Err("live portal stream metadata changed".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::{
        accessibility::{AppInfo, ObjectId, Snapshot, SnapshotLimits, WindowInfo},
        capture::FrameMetadata,
        geometry::{PixelRect, Transform},
    };

    fn fixture() -> (Snapshot, ScreenshotMapping, PortalStream) {
        let window = ObjectId {
            bus_name: ":1.2".into(),
            path: "/window".into(),
        };
        let snapshot = Snapshot {
            app_query: "App".into(),
            view: crate::validation::ObservationView::Full,
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
            node_limit_reached: false,
            depth_limit_reached: false,
            limits: SnapshotLimits {
                text: 20,
                nodes: 20,
                depth: 5,
            },
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
                size: (600, 450),
                crop: PixelRect {
                    x: 0,
                    y: 0,
                    width: 600,
                    height: 450,
                },
                transform: Transform::Normal,
            },
            output_size: (600, 450),
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
    fn missing_mapping_id_requires_usable_exact_geometry() {
        let (_, mut mapping, _) = fixture();
        mapping.stream.mapping_id = None;
        mapping.stream.position = None;
        assert!(
            EisRoute::from_stream(&mapping.stream)
                .unwrap_err()
                .contains("omitted mapping_id and position")
        );

        mapping.stream.position = Some((-1, 0));
        assert!(
            EisRoute::from_stream(&mapping.stream)
                .unwrap_err()
                .contains("negative position")
        );
    }
}
