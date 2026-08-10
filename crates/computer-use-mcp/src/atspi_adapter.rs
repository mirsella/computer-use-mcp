use std::{collections::BTreeSet, future::Future};

use atspi::{CoordType, Interface};
use atspi_proxies::{
    accessible::AccessibleProxy,
    action::ActionProxy,
    bus::{BusProxy, StatusProxy},
    component::ComponentProxy,
    editable_text::EditableTextProxy,
    text::TextProxy,
    value::ValueProxy,
};
use tokio::sync::OnceCell;
use zbus::{Connection, fdo::DBusProxy, names::BusName, proxy::CacheProperties};

use crate::{
    accessibility::{
        AccessibilityAdapter, ActionCapabilities, ActionInfo, AppInfo, InspectedCapabilities,
        NodeCapabilities, NodeInfo, ObjectId, Rect, SemanticAction, WindowInfo,
    },
    errors::RuntimeError,
};

#[derive(Debug, Default)]
pub struct AtspiAdapter {
    connection: OnceCell<Connection>,
}

impl AtspiAdapter {
    async fn connection(&self) -> Result<&Connection, RuntimeError> {
        self.connection
            .get_or_try_init(|| async {
                let session = Connection::session().await.map_err(|error| {
                    runtime_error(format!(
                        "cannot connect to the signed-in user's D-Bus session: {error}; start this process inside the graphical login session"
                    ))
                })?;
                let status = StatusProxy::new(&session).await.map_err(|error| {
                    runtime_error(format!(
                        "AT-SPI status service is unavailable: {error}; enable accessibility in the desktop settings and ensure at-spi2-core is running"
                    ))
                })?;
                let enabled = status.is_enabled().await.map_err(|error| {
                    runtime_error(format!(
                        "cannot read AT-SPI status: {error}; enable accessibility in the desktop settings"
                    ))
                })?;
                if !enabled {
                    return Err(runtime_error(
                        "AT-SPI is disabled for this login session; enable accessibility in the desktop settings, then restart the app and this server",
                    ));
                }
                let bus = BusProxy::new(&session).await.map_err(|error| {
                    runtime_error(format!(
                        "AT-SPI bus service org.a11y.Bus is unavailable: {error}; ensure at-spi2-core is installed and running"
                    ))
                })?;
                let address = bus.get_address().await.map_err(|error| {
                    runtime_error(format!(
                        "AT-SPI did not provide an accessibility bus address: {error}; restart the accessibility service in the graphical session"
                    ))
                })?;
                zbus::connection::Builder::address(address.as_str())
                    .map_err(|error| {
                        runtime_error(format!("AT-SPI returned an invalid bus address: {error}"))
                    })?
                    .build()
                    .await
                    .map_err(|error| {
                        runtime_error(format!(
                            "cannot connect to the user's AT-SPI accessibility bus: {error}; verify the accessibility service is running in this login session"
                        ))
                    })
            })
            .await
    }

    async fn accessible<'a>(
        connection: &'a Connection,
        object: &'a ObjectId,
    ) -> Result<AccessibleProxy<'a>, RuntimeError> {
        AccessibleProxy::builder(connection)
            .destination(object.bus_name.as_str())
            .map_err(atspi_call_error)?
            .path(object.path.as_str())
            .map_err(atspi_call_error)?
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .map_err(atspi_call_error)
    }

    async fn discover_inner(&self) -> Result<Vec<AppInfo>, RuntimeError> {
        let connection = self.connection().await?;
        let root = AccessibleProxy::builder(connection)
            .destination("org.a11y.atspi.Registry")
            .map_err(atspi_call_error)?
            .path("/org/a11y/atspi/accessible/root")
            .map_err(atspi_call_error)?
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .map_err(|error| {
                runtime_error(format!(
                    "AT-SPI registry is unavailable on the accessibility bus: {error}; verify at-spi2-registryd is running"
                ))
            })?;
        let roots = root.get_children().await.map_err(atspi_call_error)?;
        let dbus = DBusProxy::new(connection).await.map_err(atspi_call_error)?;
        let mut apps = Vec::new();
        for root in roots {
            if root.is_null() {
                eprintln!(
                    "computer-use-mcp: AT-SPI registry returned a null application root; skipping it"
                );
                continue;
            }
            let object = match object_id(&root) {
                Ok(object) => object,
                Err(error) => {
                    eprintln!("computer-use-mcp: invalid AT-SPI application identity: {error}");
                    continue;
                }
            };
            match Self::discover_app(connection, &dbus, object).await {
                Ok(Some(app)) => apps.push(app),
                Ok(None) => {}
                Err(error) => {
                    eprintln!(
                        "computer-use-mcp: accessible application vanished or is invalid; skipping it: {error}"
                    );
                }
            }
        }
        Ok(apps)
    }

    async fn discover_app(
        connection: &Connection,
        dbus: &DBusProxy<'_>,
        object: ObjectId,
    ) -> Result<Option<AppInfo>, RuntimeError> {
        let proxy = Self::accessible(connection, &object).await?;
        let name = proxy.name().await.map_err(atspi_call_error)?;
        let bus_name = BusName::try_from(object.bus_name.as_str()).map_err(|error| {
            runtime_error(format!("invalid AT-SPI application bus name: {error}"))
        })?;
        let pid = dbus
            .get_connection_unix_process_id(bus_name)
            .await
            .map_err(|error| {
                runtime_error(format!(
                    "cannot bind AT-SPI application {:?} to a process ID: {error}",
                    name
                ))
            })?;
        let children = proxy.get_children().await.map_err(atspi_call_error)?;
        let mut windows = Vec::new();
        for child in children {
            if child.is_null() {
                eprintln!(
                    "computer-use-mcp: application returned a null top-level child; skipping it"
                );
                continue;
            }
            let child = object_id(&child)?;
            match Self::window_info(connection, child).await {
                Ok(Some(window)) => windows.push(window),
                Ok(None) => {}
                Err(error) => {
                    eprintln!(
                        "computer-use-mcp: top-level AT-SPI object became stale; skipping it: {error}"
                    );
                }
            }
        }
        if windows.is_empty() {
            return Ok(None);
        }
        Ok(Some(AppInfo {
            object,
            name,
            pid,
            windows,
        }))
    }

    async fn window_info(
        connection: &Connection,
        object: ObjectId,
    ) -> Result<Option<WindowInfo>, RuntimeError> {
        let proxy = Self::accessible(connection, &object).await?;
        let role = proxy.get_role().await.map_err(atspi_call_error)?;
        let role = role.name().to_owned();
        if !matches!(
            role.as_str(),
            "frame" | "dialog" | "window" | "alert" | "file chooser"
        ) {
            return Ok(None);
        }
        let title = proxy.name().await.map_err(atspi_call_error)?;
        let states = proxy
            .get_state()
            .await
            .map_err(atspi_call_error)?
            .into_iter()
            .map(|state| state.to_string())
            .collect();
        Ok(Some(WindowInfo {
            object,
            title,
            states,
        }))
    }

    async fn read_node_inner(
        &self,
        object: &ObjectId,
        text_limit: usize,
    ) -> Result<NodeInfo, RuntimeError> {
        let connection = self.connection().await?;
        let proxy = Self::accessible(connection, object).await?;
        let role = proxy
            .get_role()
            .await
            .map_err(atspi_call_error)?
            .name()
            .to_owned();
        let name = proxy.name().await.map_err(atspi_call_error)?;
        let states: BTreeSet<_> = proxy
            .get_state()
            .await
            .map_err(atspi_call_error)?
            .into_iter()
            .map(|state| state.to_string())
            .collect();
        let interface_set = optional(
            object,
            "Accessible",
            "GetInterfaces",
            proxy.get_interfaces().await,
        );
        let children = proxy
            .get_children()
            .await
            .map_err(atspi_call_error)?
            .into_iter()
            .filter(|child| !child.is_null())
            .map(|child| object_id(&child))
            .collect::<Result<Vec<_>, _>>()?;

        let actions = if interface_set
            .as_ref()
            .is_some_and(|interfaces| interfaces.contains(Interface::Action))
        {
            match optional(
                object,
                "Action",
                "proxy",
                action_proxy(connection, object).await,
            ) {
                Some(action) => {
                    match optional(object, "Action", "GetActions", action.get_actions().await) {
                        Some(actions) => ActionCapabilities::Inspected(
                            actions
                                .into_iter()
                                .map(|action| ActionInfo {
                                    name: action.name,
                                    description: action.description,
                                })
                                .collect(),
                        ),
                        None => ActionCapabilities::InspectionFailed,
                    }
                }
                None => ActionCapabilities::InspectionFailed,
            }
        } else {
            ActionCapabilities::Unsupported
        };
        let has_component = interface_set
            .as_ref()
            .is_some_and(|interfaces| interfaces.contains(Interface::Component));
        let window_frame = if has_component {
            match optional(
                object,
                "Component",
                "proxy",
                component_proxy(connection, object).await,
            ) {
                Some(component) => optional(
                    object,
                    "Component",
                    "GetExtents(Window)",
                    component.get_extents(CoordType::Window).await,
                )
                .map(rect),
                None => None,
            }
        } else {
            None
        };
        let (text, text_truncated, selected_text) = if interface_set
            .as_ref()
            .is_some_and(|interfaces| interfaces.contains(Interface::Text))
        {
            read_text_metadata(connection, object, text_limit).await
        } else {
            (None, None, None)
        };
        let has_value = interface_set
            .as_ref()
            .is_some_and(|interfaces| interfaces.contains(Interface::Value));
        let value = if has_value {
            read_value_metadata(connection, object).await
        } else {
            None
        };

        let capabilities = match interface_set {
            Some(interfaces) => NodeCapabilities::Inspected(InspectedCapabilities {
                actions,
                component: has_component,
                editable_text: interfaces.contains(Interface::EditableText),
                value: has_value,
            }),
            None => NodeCapabilities::InspectionFailed,
        };
        Ok(NodeInfo {
            object: object.clone(),
            role,
            name,
            value,
            text,
            text_truncated,
            selected_text,
            states,
            capabilities,
            window_frame,
            children,
        })
    }

    async fn act_inner(
        &self,
        object: &ObjectId,
        action: SemanticAction,
    ) -> Result<(), RuntimeError> {
        let connection = self.connection().await?;
        match action {
            SemanticAction::InvokeAction(index) => {
                let proxy = action_proxy(connection, object).await?;
                if !proxy.do_action(index).await.map_err(atspi_call_error)? {
                    return Err(runtime_error("AT-SPI action reported failure"));
                }
            }
            SemanticAction::GrabFocus => {
                let proxy = component_proxy(connection, object).await?;
                if !proxy.grab_focus().await.map_err(atspi_call_error)? {
                    return Err(runtime_error("AT-SPI Component.GrabFocus reported failure"));
                }
            }
            SemanticAction::ReplaceText(value) => {
                let proxy = editable_text_proxy(connection, object).await?;
                if !proxy
                    .set_text_contents(&value)
                    .await
                    .map_err(atspi_call_error)?
                {
                    return Err(runtime_error(
                        "AT-SPI EditableText replacement reported failure",
                    ));
                }
            }
            SemanticAction::SetNumericValue(value) => {
                value_proxy(connection, object)
                    .await?
                    .set_current_value(value)
                    .await
                    .map_err(atspi_call_error)?;
            }
        }
        Ok(())
    }

    async fn activate_inner(&self, object: &ObjectId) -> Result<(), RuntimeError> {
        let connection = self.connection().await?;
        let proxy = component_proxy(connection, object).await?;
        if !proxy.grab_focus().await.map_err(atspi_call_error)? {
            return Err(runtime_error(
                "AT-SPI Component.GrabFocus did not activate the window",
            ));
        }
        Ok(())
    }
}

impl AccessibilityAdapter for AtspiAdapter {
    fn discover(&self) -> impl Future<Output = Result<Vec<AppInfo>, RuntimeError>> + Send + '_ {
        self.discover_inner()
    }

    fn read_node<'a>(
        &'a self,
        object: &'a ObjectId,
        text_limit: usize,
    ) -> impl Future<Output = Result<NodeInfo, RuntimeError>> + Send + 'a {
        self.read_node_inner(object, text_limit)
    }

    fn act<'a>(
        &'a self,
        object: &'a ObjectId,
        action: SemanticAction,
    ) -> impl Future<Output = Result<(), RuntimeError>> + Send + 'a {
        self.act_inner(object, action)
    }

    fn activate<'a>(
        &'a self,
        object: &'a ObjectId,
    ) -> impl Future<Output = Result<(), RuntimeError>> + Send + 'a {
        self.activate_inner(object)
    }
}

async fn action_proxy<'a>(
    connection: &'a Connection,
    object: &'a ObjectId,
) -> Result<ActionProxy<'a>, RuntimeError> {
    ActionProxy::builder(connection)
        .destination(object.bus_name.as_str())
        .map_err(atspi_call_error)?
        .path(object.path.as_str())
        .map_err(atspi_call_error)?
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .map_err(atspi_call_error)
}

async fn component_proxy<'a>(
    connection: &'a Connection,
    object: &'a ObjectId,
) -> Result<ComponentProxy<'a>, RuntimeError> {
    ComponentProxy::builder(connection)
        .destination(object.bus_name.as_str())
        .map_err(atspi_call_error)?
        .path(object.path.as_str())
        .map_err(atspi_call_error)?
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .map_err(atspi_call_error)
}

async fn text_proxy<'a>(
    connection: &'a Connection,
    object: &'a ObjectId,
) -> Result<TextProxy<'a>, RuntimeError> {
    TextProxy::builder(connection)
        .destination(object.bus_name.as_str())
        .map_err(atspi_call_error)?
        .path(object.path.as_str())
        .map_err(atspi_call_error)?
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .map_err(atspi_call_error)
}

async fn editable_text_proxy<'a>(
    connection: &'a Connection,
    object: &'a ObjectId,
) -> Result<EditableTextProxy<'a>, RuntimeError> {
    EditableTextProxy::builder(connection)
        .destination(object.bus_name.as_str())
        .map_err(atspi_call_error)?
        .path(object.path.as_str())
        .map_err(atspi_call_error)?
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .map_err(atspi_call_error)
}

async fn value_proxy<'a>(
    connection: &'a Connection,
    object: &'a ObjectId,
) -> Result<ValueProxy<'a>, RuntimeError> {
    ValueProxy::builder(connection)
        .destination(object.bus_name.as_str())
        .map_err(atspi_call_error)?
        .path(object.path.as_str())
        .map_err(atspi_call_error)?
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .map_err(atspi_call_error)
}

async fn read_text_metadata(
    connection: &Connection,
    object: &ObjectId,
    text_limit: usize,
) -> (Option<String>, Option<bool>, Option<String>) {
    let Some(proxy) = optional(
        object,
        "Text",
        "proxy",
        text_proxy(connection, object).await,
    ) else {
        return (None, None, None);
    };

    let (text, text_truncated) = match optional(
        object,
        "Text",
        "CharacterCount",
        proxy.character_count().await,
    ) {
        Some(count) => match bounded_text_end(count, text_limit) {
            Some((end, truncated)) => {
                match optional(object, "Text", "GetText", proxy.get_text(0, end).await) {
                    Some(value) => {
                        let (value, provider_truncated) =
                            limit_metadata_with_truncation(value, text_limit);
                        if provider_truncated {
                            eprintln!(
                                "computer-use-mcp: AT-SPI Text.GetText provider exceeded the configured {text_limit}-character budget for object={}{}; clamped locally",
                                object.bus_name, object.path
                            );
                        }
                        (Some(value), Some(truncated || provider_truncated))
                    }
                    None => (None, None),
                }
            }
            None => {
                eprintln!(
                    "computer-use-mcp: optional AT-SPI metadata unavailable: object={}{} interface=Text member=CharacterCount error=negative character count {count}",
                    object.bus_name, object.path
                );
                (None, None)
            }
        },
        None => (None, None),
    };

    let selected_text = match optional(
        object,
        "Text",
        "GetNSelections",
        proxy.get_n_selections().await,
    ) {
        Some(count) if count > 0 => {
            match optional(object, "Text", "GetSelection", proxy.get_selection(0).await) {
                Some((start, end)) if start >= 0 && end >= start => {
                    let bounded_end = bounded_selection_end(start, end, text_limit)
                        .expect("selection range was checked above");
                    if bounded_end < end {
                        eprintln!(
                            "computer-use-mcp: capping AT-SPI selected text read at {text_limit} characters for object={}{}",
                            object.bus_name, object.path
                        );
                    }
                    optional(
                        object,
                        "Text",
                        "GetText(selection)",
                        // Toolkits can invalidate a selection between these D-Bus calls.
                        proxy
                            .get_text(start, bounded_end)
                            .await
                            .map(|value| limit_metadata(value, text_limit)),
                    )
                }
                Some((start, end)) => {
                    eprintln!(
                        "computer-use-mcp: optional AT-SPI selected text unavailable: invalid range ({start}, {end}) for object={}{}",
                        object.bus_name, object.path
                    );
                    None
                }
                None => None,
            }
        }
        _ => None,
    };
    (text, text_truncated, selected_text)
}

fn bounded_selection_end(start: i32, end: i32, text_limit: usize) -> Option<i32> {
    (start >= 0 && end >= start)
        .then(|| end.min(start.saturating_add(i32::try_from(text_limit).unwrap_or(i32::MAX))))
}

fn bounded_text_end(count: i32, text_limit: usize) -> Option<(i32, bool)> {
    (count >= 0).then(|| {
        let end = count.min(i32::try_from(text_limit).unwrap_or(i32::MAX));
        (end, count > end)
    })
}

async fn read_value_metadata(connection: &Connection, object: &ObjectId) -> Option<String> {
    let proxy = optional(
        object,
        "Value",
        "proxy",
        value_proxy(connection, object).await,
    )?;
    // Value.Text has no range argument in the AT-SPI API. Do not request an
    // unrestricted provider string; CurrentValue is the bounded source we can
    // safely expose at this boundary.
    optional(object, "Value", "CurrentValue", proxy.current_value().await)
        .map(|value| value.to_string())
}

fn limit_metadata(value: String, text_limit: usize) -> String {
    limit_metadata_with_truncation(value, text_limit).0
}

fn limit_metadata_with_truncation(value: String, text_limit: usize) -> (String, bool) {
    let mut chars = value.chars();
    let value = chars.by_ref().take(text_limit).collect();
    (value, chars.next().is_some())
}

fn optional<T, E: std::fmt::Display>(
    object: &ObjectId,
    interface: &str,
    member: &str,
    result: Result<T, E>,
) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(error) => {
            eprintln!(
                "computer-use-mcp: optional AT-SPI metadata unavailable: object={}{} interface={interface} member={member} error={error}",
                object.bus_name, object.path
            );
            None
        }
    }
}

fn object_id(reference: &atspi::ObjectRefOwned) -> Result<ObjectId, RuntimeError> {
    let bus_name = reference
        .name_as_str()
        .ok_or_else(|| runtime_error("AT-SPI object reference has no bus name"))?;
    Ok(ObjectId {
        bus_name: bus_name.to_owned(),
        path: reference.path_as_str().to_owned(),
    })
}

fn rect((x, y, width, height): (i32, i32, i32, i32)) -> Rect {
    Rect {
        x,
        y,
        width,
        height,
    }
}

fn atspi_call_error(error: impl std::fmt::Display) -> RuntimeError {
    runtime_error(format!(
        "AT-SPI call failed (the object may be stale or its advertised interface may be unsupported): {error}"
    ))
}

fn runtime_error(message: impl Into<String>) -> RuntimeError {
    RuntimeError::new(
        "backend_failed",
        message,
        crate::errors::ToolOutcome::NotStarted,
        true,
        "Retry after calling observe. If the failure persists, inspect server diagnostics.",
    )
}

#[cfg(test)]
mod tests {
    use super::{
        AtspiAdapter, bounded_selection_end, bounded_text_end, limit_metadata_with_truncation,
        optional,
    };
    use crate::accessibility::{AccessibilityAdapter, ObjectId};

    #[test]
    fn inconsistent_advertised_interfaces_are_optional_metadata_loss() {
        let object = ObjectId {
            bus_name: ":1.2".into(),
            path: "/stale".into(),
        };
        for error in [
            zbus::fdo::Error::UnknownInterface("Value disappeared".into()),
            zbus::fdo::Error::UnknownObject("object disappeared".into()),
            zbus::fdo::Error::UnknownProperty("old toolkit".into()),
            zbus::fdo::Error::UnknownMethod("old toolkit".into()),
        ] {
            assert!(optional::<(), _>(&object, "Value", "Text", Err(error)).is_none());
        }
    }

    #[test]
    fn selected_text_range_is_bounded_before_the_dbus_read() {
        assert_eq!(bounded_selection_end(10, 100, 5), Some(15));
        assert_eq!(bounded_selection_end(10, 100, 0), Some(10));
        assert_eq!(bounded_selection_end(-1, 10, 5), None);
        assert_eq!(bounded_selection_end(10, 9, 5), None);
    }

    #[test]
    fn ordinary_text_range_is_bounded_before_the_dbus_read_and_reports_truncation() {
        assert_eq!(bounded_text_end(100, 5), Some((5, true)));
        assert_eq!(bounded_text_end(5, 5), Some((5, false)));
        assert_eq!(bounded_text_end(0, 0), Some((0, false)));
        assert_eq!(bounded_text_end(-1, 5), None);
    }

    #[test]
    fn ordinary_text_read_reclamps_an_overlong_provider_response() {
        let (requested_end, source_truncated) = bounded_text_end(2, 2).expect("valid count");
        let mut requested_range = None;
        let mut provider = |start, end| {
            requested_range = Some((start, end));
            // This fake provider ignores the bounded end and returns one extra
            // Unicode scalar value, as an out-of-contract toolkit may do.
            "A😀B".to_owned()
        };

        let (value, provider_truncated) =
            limit_metadata_with_truncation(provider(0, requested_end), 2);

        assert_eq!(requested_range, Some((0, 2)));
        assert_eq!(value, "A😀");
        assert!(!source_truncated);
        assert!(provider_truncated);
    }

    #[tokio::test]
    #[ignore = "requires a live graphical session with AT-SPI enabled"]
    async fn live_discovery_is_non_mutating() {
        let apps = AtspiAdapter::default()
            .discover()
            .await
            .expect("discover live AT-SPI apps");
        for app in apps {
            assert!(app.pid > 0);
            assert!(!app.name.is_empty());
            assert!(!app.windows.is_empty());
        }
    }
}
