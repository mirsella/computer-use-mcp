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
        let (text, selected_text) = if interface_set
            .as_ref()
            .is_some_and(|interfaces| interfaces.contains(Interface::Text))
        {
            read_text_metadata(connection, object, text_limit).await
        } else {
            (None, None)
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
) -> (Option<String>, Option<String>) {
    let Some(proxy) = optional(
        object,
        "Text",
        "proxy",
        text_proxy(connection, object).await,
    ) else {
        return (None, None);
    };

    let text = match optional(
        object,
        "Text",
        "CharacterCount",
        proxy.character_count().await,
    ) {
        Some(count) if count >= 0 => {
            let end = count.min(i32::try_from(text_limit).unwrap_or(i32::MAX));
            optional(object, "Text", "GetText", proxy.get_text(0, end).await)
        }
        Some(count) => {
            eprintln!(
                "computer-use-mcp: optional AT-SPI metadata unavailable: object={}{} interface=Text member=CharacterCount error=negative character count {count}",
                object.bus_name, object.path
            );
            None
        }
        None => None,
    };

    let selected_text = match optional(
        object,
        "Text",
        "GetNSelections",
        proxy.get_n_selections().await,
    ) {
        Some(count) if count > 0 => {
            match optional(object, "Text", "GetSelection", proxy.get_selection(0).await) {
                Some((start, end)) => optional(
                    object,
                    "Text",
                    "GetText(selection)",
                    // Toolkits can invalidate a selection between these D-Bus calls.
                    proxy.get_text(start, end).await,
                ),
                None => None,
            }
        }
        _ => None,
    };
    (text, selected_text)
}

async fn read_value_metadata(connection: &Connection, object: &ObjectId) -> Option<String> {
    let proxy = optional(
        object,
        "Value",
        "proxy",
        value_proxy(connection, object).await,
    )?;
    if let Some(display) = optional(object, "Value", "Text", proxy.text().await)
        && !display.is_empty()
    {
        return Some(display);
    }
    optional(object, "Value", "CurrentValue", proxy.current_value().await)
        .map(|value| value.to_string())
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
    use super::{AtspiAdapter, optional};
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
