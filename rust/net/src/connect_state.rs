//
// Copyright 2024 Signal Messenger, LLC.
// SPDX-License-Identifier: AGPL-3.0-only
//

use std::default::Default;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::Duration;

use http::HeaderName;
use itertools::Itertools as _;
use libsignal_net_infra::connection_manager::{ErrorClass, ErrorClassifier as _};
use libsignal_net_infra::dns::DnsResolver;
use libsignal_net_infra::errors::LogSafeDisplay;
use libsignal_net_infra::route::{
    ComposedConnector, ConnectError, ConnectionOutcomeParams, ConnectionOutcomes, Connector,
    DescribedRouteConnector, HttpRouteFragment, ResolveWithSavedDescription, RouteProvider,
    RouteProviderContext, RouteProviderExt as _, RouteResolver, StatelessTransportConnector,
    TransportRoute, UnresolvedRouteDescription, UnresolvedWebsocketServiceRoute,
    WebSocketRouteFragment, WebSocketServiceRoute, WithLoggableDescription,
    WithoutLoggableDescription,
};
use libsignal_net_infra::timeouts::{TimeoutOr, ONE_ROUTE_CONNECTION_TIMEOUT};
use libsignal_net_infra::ws::{WebSocketConnectError, WebSocketStreamLike};
use libsignal_net_infra::ws2::attested::AttestedConnection;
use libsignal_net_infra::{AsHttpHeader as _, AsyncDuplexStream};
use rand::Rng;
use rand_core::OsRng;
use static_assertions::assert_eq_size_val;
use tokio::time::Instant;

use crate::auth::Auth;
use crate::enclave::{EndpointParams, NewHandshake};
use crate::ws::WebSocketServiceConnectError;

/// Suggested values for [`ConnectionOutcomeParams`].
pub const SUGGESTED_CONNECT_PARAMS: ConnectionOutcomeParams = ConnectionOutcomeParams {
    age_cutoff: Duration::from_secs(5 * 60),
    cooldown_growth_factor: 10.0,
    max_count: 5,
    max_delay: Duration::from_secs(30),
    count_growth_factor: 10.0,
};

/// Suggested values for [`Config`].
pub const SUGGESTED_CONNECT_CONFIG: Config = Config {
    connect_params: SUGGESTED_CONNECT_PARAMS,
    connect_timeout: ONE_ROUTE_CONNECTION_TIMEOUT,
};

/// Endpoint-agnostic state for establishing a connection with
/// [`crate::infra::route::connect`].
///
/// Templated over the type of the transport connector to support testing.
pub struct ConnectState<TC = StatelessTransportConnector> {
    pub route_resolver: RouteResolver,
    /// The amount of time allowed for each connection attempt.
    pub connect_timeout: Duration,
    /// Transport-level connector used for all connections.
    transport_connector: TC,
    /// Record of connection outcomes.
    attempts_record: ConnectionOutcomes<WebSocketServiceRoute>,
    /// [`RouteProviderContext`] passed to route providers.
    route_provider_context: RouteProviderContextImpl,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub connect_params: ConnectionOutcomeParams,
    pub connect_timeout: Duration,
}

impl ConnectState {
    pub fn new(config: Config) -> tokio::sync::RwLock<Self> {
        Self::new_with_transport_connector(config, StatelessTransportConnector::default())
    }
}

impl<TC> ConnectState<TC> {
    #[cfg_attr(feature = "test-util", visibility::make(pub))]
    fn new_with_transport_connector(
        config: Config,
        transport_connector: TC,
    ) -> tokio::sync::RwLock<Self> {
        let Config {
            connect_params,
            connect_timeout,
        } = config;
        Self {
            route_resolver: RouteResolver::default(),
            connect_timeout,
            transport_connector,
            attempts_record: ConnectionOutcomes::new(connect_params),
            route_provider_context: RouteProviderContextImpl::default(),
        }
        .into()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RouteInfo {
    unresolved: UnresolvedRouteDescription,
}

impl LogSafeDisplay for RouteInfo {}
impl std::fmt::Display for RouteInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { unresolved } = self;
        (unresolved as &dyn LogSafeDisplay).fmt(f)
    }
}

impl RouteInfo {
    pub fn fake() -> Self {
        Self {
            unresolved: UnresolvedRouteDescription::fake(),
        }
    }
}

impl<TC> ConnectState<TC>
where
    TC: Connector<TransportRoute, ()> + Sync,
    TC::Error: Into<WebSocketConnectError>,
{
    pub async fn connect_ws<WC, E>(
        this: &tokio::sync::RwLock<Self>,
        routes: impl RouteProvider<Route = UnresolvedWebsocketServiceRoute>,
        ws_connector: WC,
        resolver: &DnsResolver,
        confirmation_header_name: Option<&HeaderName>,
        log_tag: Arc<str>,
        mut on_error: impl FnMut(WebSocketServiceConnectError) -> ControlFlow<E>,
    ) -> Result<(WC::Connection, RouteInfo), TimeoutOr<ConnectError<E>>>
    where
        WC: Connector<
                (WebSocketRouteFragment, HttpRouteFragment),
                TC::Connection,
                Error = tungstenite::Error,
            > + Send
            + Sync,
        E: LogSafeDisplay,
    {
        let connect_read = this.read().await;

        let Self {
            route_resolver,
            connect_timeout,
            transport_connector,
            attempts_record,
            route_provider_context,
        } = &*connect_read;

        let routes = routes.routes(route_provider_context).collect_vec();

        log::info!(
            "[{log_tag}] starting connection attempt with {} routes",
            routes.len()
        );

        let route_provider = routes.into_iter().map(ResolveWithSavedDescription);
        let connector =
            DescribedRouteConnector(ComposedConnector::new(ws_connector, &transport_connector));
        let delay_policy = WithoutLoggableDescription(&attempts_record);

        let start = Instant::now();
        let connect = crate::infra::route::connect(
            route_resolver,
            delay_policy,
            route_provider,
            resolver,
            connector,
            log_tag.clone(),
            |error| {
                let error = WebSocketServiceConnectError::from_websocket_error(
                    error,
                    confirmation_header_name,
                    Instant::now(),
                );
                on_error(error)
            },
        );

        let (result, updates) = tokio::time::timeout(*connect_timeout, connect)
            .await
            .map_err(|_: tokio::time::error::Elapsed| TimeoutOr::Timeout {
                attempt_duration: *connect_timeout,
            })?;

        // Drop our read lock so we can re-acquire as a writer. It's okay if we
        // race with other writers since the order in which updates are applied
        // doesn't matter.
        drop(connect_read);

        match &result {
            Ok((_connection, route)) => log::info!(
                "[{log_tag}] connection through {route} succeeded after {:.3?}",
                start.elapsed()
            ),
            Err(e) => log::info!("[{log_tag}] connection failed with {e}"),
        }

        this.write().await.attempts_record.apply_outcome_updates(
            updates.outcomes.into_iter().map(
                |(
                    WithLoggableDescription {
                        route,
                        description: _,
                    },
                    outcome,
                )| (route, outcome),
            ),
            updates.finished_at,
        );

        let (connection, description) = result?;
        Ok((
            connection,
            RouteInfo {
                unresolved: description,
            },
        ))
    }

    pub(crate) async fn connect_attested_ws<E, WC>(
        connect: &tokio::sync::RwLock<Self>,
        routes: impl RouteProvider<Route = UnresolvedWebsocketServiceRoute>,
        auth: Auth,
        resolver: &DnsResolver,
        confirmation_header_name: Option<HeaderName>,
        (ws_config, ws_connector): (libsignal_net_infra::ws2::Config, WC),
        log_tag: Arc<str>,
        params: &EndpointParams<'_, E>,
    ) -> Result<(AttestedConnection, RouteInfo), crate::enclave::Error>
    where
        TC::Connection: AsyncDuplexStream + 'static,
        WC: Connector<
                (WebSocketRouteFragment, HttpRouteFragment),
                TC::Connection,
                Error = tungstenite::Error,
            > + Send
            + Sync,
        WC::Connection: WebSocketStreamLike + Send + 'static,
        E: NewHandshake,
    {
        let ws_routes = routes.map_routes(|mut route| {
            route.fragment.headers.extend([auth.as_header()]);
            route
        });

        let (ws, route_info) = ConnectState::connect_ws(
            connect,
            ws_routes,
            ws_connector,
            resolver,
            confirmation_header_name.as_ref(),
            log_tag.clone(),
            |error| match error.classify() {
                ErrorClass::Intermittent => ControlFlow::Continue(()),
                ErrorClass::RetryAt(_) | ErrorClass::Fatal => {
                    ControlFlow::Break(crate::enclave::Error::WebSocketConnect(error))
                }
            },
        )
        .await
        .map_err(|e| match e {
            TimeoutOr::Other(ConnectError::NoResolvedRoutes | ConnectError::AllAttemptsFailed)
            | TimeoutOr::Timeout {
                attempt_duration: _,
            } => crate::enclave::Error::ConnectionTimedOut,
            TimeoutOr::Other(ConnectError::FatalConnect(e)) => e,
        })?;

        let connection =
            AttestedConnection::connect(ws, ws_config, log_tag, move |attestation_message| {
                E::new_handshake(params, attestation_message)
            })
            .await?;
        Ok((connection, route_info))
    }
}

#[derive(Debug, Default)]
struct RouteProviderContextImpl(OsRng);

impl RouteProviderContext for RouteProviderContextImpl {
    fn random_usize(&self) -> usize {
        // OsRng is zero-sized, so we're not losing random values by copying it.
        let mut owned_rng: OsRng = self.0;
        assert_eq_size_val!(owned_rng, ());
        owned_rng.gen()
    }
}

#[cfg(test)]
mod test {
    use std::collections::HashMap;
    use std::convert::Infallible;
    use std::sync::Arc;
    use std::time::Duration;

    use assert_matches::assert_matches;
    use const_str::ip_addr;
    use http::uri::PathAndQuery;
    use http::HeaderMap;
    use lazy_static::lazy_static;
    use libsignal_net_infra::certs::RootCertificates;
    use libsignal_net_infra::dns::lookup_result::LookupResult;
    use libsignal_net_infra::host::Host;
    use libsignal_net_infra::route::testutils::ConnectFn;
    use libsignal_net_infra::route::{
        DirectOrProxyRoute, HttpsTlsRoute, TcpRoute, TlsRoute, TlsRouteFragment, UnresolvedHost,
        UnresolvedTransportRoute, WebSocketRoute,
    };
    use libsignal_net_infra::{Alpn, DnsSource, RouteType};
    use nonzero_ext::nonzero;

    use super::*;

    const FAKE_HOST_NAME: &str = "direct-host";
    lazy_static! {
        static ref FAKE_TRANSPORT_ROUTE: UnresolvedTransportRoute = TlsRoute {
            fragment: TlsRouteFragment {
                root_certs: RootCertificates::Native,
                sni: Host::Domain("fake-sni".into()),
                alpn: Some(Alpn::Http1_1),
            },
            inner: DirectOrProxyRoute::Direct(TcpRoute {
                address: UnresolvedHost::from(Arc::from(FAKE_HOST_NAME)),
                port: nonzero!(1234u16),
            }),
        };
        static ref FAKE_WEBSOCKET_ROUTES: [UnresolvedWebsocketServiceRoute; 2] = [
            WebSocketRoute {
                fragment: WebSocketRouteFragment {
                    ws_config: Default::default(),
                    endpoint: PathAndQuery::from_static("/first"),
                    headers: HeaderMap::new(),
                },
                inner: HttpsTlsRoute {
                    fragment: HttpRouteFragment {
                        host_header: "first-host".into(),
                        path_prefix: "".into(),
                        front_name: None,
                    },
                    inner: (*FAKE_TRANSPORT_ROUTE).clone(),
                },
            },
            WebSocketRoute {
                fragment: WebSocketRouteFragment {
                    ws_config: Default::default(),
                    endpoint: PathAndQuery::from_static("/second"),
                    headers: HeaderMap::new(),
                },
                inner: HttpsTlsRoute {
                    fragment: HttpRouteFragment {
                        host_header: "second-host".into(),
                        path_prefix: "".into(),
                        front_name: Some(RouteType::ProxyF.into()),
                    },
                    inner: (*FAKE_TRANSPORT_ROUTE).clone(),
                },
            }
        ];
    }

    #[tokio::test(start_paused = true)]
    async fn connect_ws_successful() {
        // This doesn't actually matter since we're using a fake connector, but
        // using the real route type is easier than trying to add yet more
        // generic parameters.
        let [failing_route, succeeding_route] = (*FAKE_WEBSOCKET_ROUTES).clone();

        let ws_connector = ConnectFn(|(), route, _log_tag| {
            let (ws, http) = &route;
            std::future::ready(
                if (ws, http) == (&failing_route.fragment, &failing_route.inner.fragment) {
                    Err(tungstenite::Error::ConnectionClosed)
                } else {
                    Ok(route)
                },
            )
        });
        let resolver = DnsResolver::new_from_static_map(HashMap::from([(
            FAKE_HOST_NAME,
            LookupResult::new(DnsSource::Static, vec![ip_addr!(v4, "1.1.1.1")], vec![]),
        )]));

        let fake_transport_connector =
            ConnectFn(move |(), _, _| std::future::ready(Ok::<_, WebSocketConnectError>(())));

        let state = ConnectState {
            connect_timeout: Duration::MAX,
            route_resolver: RouteResolver::default(),
            attempts_record: ConnectionOutcomes::new(SUGGESTED_CONNECT_PARAMS),
            transport_connector: fake_transport_connector,
            route_provider_context: Default::default(),
        }
        .into();

        let result = ConnectState::connect_ws(
            &state,
            vec![failing_route.clone(), succeeding_route.clone()],
            ws_connector,
            &resolver,
            None,
            "test".into(),
            |e| {
                let e = assert_matches!(e, WebSocketServiceConnectError::Connect(e, _) => e);
                assert_matches!(
                    e,
                    WebSocketConnectError::WebSocketError(tungstenite::Error::ConnectionClosed)
                );
                ControlFlow::<Infallible>::Continue(())
            },
        )
        // This previously hung forever due to a deadlock bug.
        .await;

        let (connection, info) = result.expect("succeeded");
        assert_eq!(
            connection,
            (succeeding_route.fragment, succeeding_route.inner.fragment)
        );
        let RouteInfo { unresolved } = info;

        assert_eq!(unresolved.to_string(), "REDACTED:1234 fronted by proxyf");
    }

    #[tokio::test(start_paused = true)]
    async fn connect_ws_timeout() {
        let ws_connector = crate::infra::ws::Stateless;
        let resolver = DnsResolver::new_from_static_map(HashMap::from([(
            FAKE_HOST_NAME,
            LookupResult::new(DnsSource::Static, vec![ip_addr!(v4, "1.1.1.1")], vec![]),
        )]));

        let always_hangs_connector = ConnectFn(|(), _, _| {
            std::future::pending::<Result<tokio::io::DuplexStream, WebSocketConnectError>>()
        });

        const CONNECT_TIMEOUT: Duration = Duration::from_secs(31);

        let state = ConnectState {
            connect_timeout: CONNECT_TIMEOUT,
            route_resolver: RouteResolver::default(),
            attempts_record: ConnectionOutcomes::new(SUGGESTED_CONNECT_PARAMS),
            transport_connector: always_hangs_connector,
            route_provider_context: Default::default(),
        }
        .into();

        let [failing_route, succeeding_route] = (*FAKE_WEBSOCKET_ROUTES).clone();

        let connect = ConnectState::connect_ws(
            &state,
            vec![failing_route.clone(), succeeding_route.clone()],
            ws_connector,
            &resolver,
            None,
            "test".into(),
            |_| unreachable!("no errors should be produced"),
        );

        let start = Instant::now();
        let result: Result<_, TimeoutOr<ConnectError<Infallible>>> = connect.await;

        assert_matches!(
            result,
            Err(TimeoutOr::Timeout {
                attempt_duration: CONNECT_TIMEOUT
            })
        );
        assert_eq!(start.elapsed(), CONNECT_TIMEOUT);
    }
}
