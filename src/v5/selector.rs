use std::{
    convert::TryFrom, fmt, future::Future, marker, pin::Pin, rc::Rc, task::Context, task::Poll,
    time,
};

use ntex::codec::{AsyncRead, AsyncWrite};
use ntex::service::{apply_fn_factory, boxed, IntoServiceFactory, Service, ServiceFactory};
use ntex::time::{sleep, Seconds, Sleep};
use ntex::util::{timeout::Timeout, timeout::TimeoutError, Either, PoolId, Ready};

use crate::error::{MqttError, ProtocolError};
use crate::io::{DispatchItem, State};

use super::control::{ControlMessage, ControlResult};
use super::default::{DefaultControlService, DefaultPublishService};
use super::handshake::{Handshake, HandshakeAck};
use super::publish::{Publish, PublishAck};
use super::shared::{MqttShared, MqttSinkPool};
use super::{codec as mqtt, dispatcher::factory, MqttServer, MqttSink, Session};

pub(crate) type SelectItem<Io> = (Handshake<Io>, State, Option<Sleep>);

type ServerFactory<Io, Err, InitErr> = boxed::BoxServiceFactory<
    (),
    SelectItem<Io>,
    Either<SelectItem<Io>, ()>,
    MqttError<Err>,
    InitErr,
>;

type Server<Io, Err> =
    boxed::BoxService<SelectItem<Io>, Either<SelectItem<Io>, ()>, MqttError<Err>>;

/// Mqtt server selector
///
/// Selector allows to choose different mqtt server impls depends on
/// connectt packet.
pub struct Selector<Io, Err, InitErr> {
    servers: Vec<ServerFactory<Io, Err, InitErr>>,
    max_size: u32,
    handshake_timeout: Seconds,
    pool: Rc<MqttSinkPool>,
    _t: marker::PhantomData<(Io, Err, InitErr)>,
}

impl<Io, Err, InitErr> Selector<Io, Err, InitErr> {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Selector {
            servers: Vec::new(),
            max_size: 0,
            handshake_timeout: Seconds::ZERO,
            pool: Default::default(),
            _t: marker::PhantomData,
        }
    }
}

impl<Io, Err, InitErr> Selector<Io, Err, InitErr>
where
    Io: AsyncRead + AsyncWrite + Unpin + 'static,
    Err: 'static,
    InitErr: 'static,
{
    /// Set handshake timeout.
    ///
    /// Handshake includes `connect` packet and response `connect-ack`.
    /// By default handshake timeuot is disabled.
    pub fn handshake_timeout(mut self, timeout: Seconds) -> Self {
        self.handshake_timeout = timeout;
        self
    }

    /// Set max inbound frame size.
    ///
    /// If max size is set to `0`, size is unlimited.
    /// By default max size is set to `0`
    pub fn max_size(mut self, size: u32) -> Self {
        self.max_size = size;
        self
    }

    /// Set memory pool.
    ///
    /// Use specified memory pool for memory allocations. By default P5
    /// memory pool is used.
    pub fn memory_pool(self, id: PoolId) -> Self {
        self.pool.pool.set(id.pool_ref());
        self
    }

    /// Add server variant
    pub fn variant<F, R, St, C, Cn, P>(
        mut self,
        check: F,
        mut server: MqttServer<Io, St, C, Cn, P>,
    ) -> Self
    where
        F: Fn(&Handshake<Io>) -> R + 'static,
        R: Future<Output = Result<bool, Err>> + 'static,
        St: 'static,
        C: ServiceFactory<
                Config = (),
                Request = Handshake<Io>,
                Response = HandshakeAck<Io, St>,
                Error = Err,
                InitError = InitErr,
            > + 'static,
        C::Error: From<Cn::Error>
            + From<Cn::InitError>
            + From<P::Error>
            + From<P::InitError>
            + fmt::Debug,
        Cn: ServiceFactory<
                Config = Session<St>,
                Request = ControlMessage<C::Error>,
                Response = ControlResult,
            > + 'static,

        P: ServiceFactory<Config = Session<St>, Request = Publish, Response = PublishAck>
            + 'static,
        P::Error: fmt::Debug,
        PublishAck: TryFrom<P::Error, Error = C::Error>,
    {
        server.pool = self.pool.clone();
        self.servers.push(boxed::factory(server.finish_selector(check)));
        self
    }

    /// Set service to handle publish packets and create mqtt server factory
    pub(crate) fn finish_server(
        self,
    ) -> impl ServiceFactory<
        Config = (),
        Request = (Io, State, Option<Sleep>),
        Response = (),
        Error = MqttError<Err>,
        InitError = InitErr,
    > {
        Selector2 {
            servers: self.servers,
            max_size: self.max_size,
            pool: self.pool,
            _t: marker::PhantomData,
        }
    }
}

impl<Io, Err, InitErr> ServiceFactory for Selector<Io, Err, InitErr>
where
    Io: AsyncRead + AsyncWrite + Unpin + 'static,
    Err: 'static,
    InitErr: 'static,
{
    type Config = ();
    type Request = Io;
    type Response = ();
    type Error = MqttError<Err>;
    type InitError = InitErr;
    type Service = SelectorService<Io, Err>;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Service, Self::InitError>>>>;

    fn new_service(&self, _: ()) -> Self::Future {
        let futs: Vec<_> = self.servers.iter().map(|srv| srv.new_service(())).collect();
        let max_size = self.max_size;
        let handshake_timeout = self.handshake_timeout;
        let pool = self.pool.clone();

        Box::pin(async move {
            let mut servers = Vec::new();
            for fut in futs {
                servers.push(fut.await?);
            }
            Ok(SelectorService { max_size, handshake_timeout, pool, servers: Rc::new(servers) })
        })
    }
}

pub struct SelectorService<Io, Err> {
    servers: Rc<Vec<Server<Io, Err>>>,
    max_size: u32,
    handshake_timeout: Seconds,
    pool: Rc<MqttSinkPool>,
}

impl<Io, Err> Service for SelectorService<Io, Err>
where
    Io: AsyncRead + AsyncWrite + Unpin + 'static,
    Err: 'static,
{
    type Request = Io;
    type Response = ();
    type Error = MqttError<Err>;
    type Future = Pin<Box<dyn Future<Output = Result<(), MqttError<Err>>>>>;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let mut ready = true;
        for srv in self.servers.iter() {
            ready &= srv.poll_ready(cx)?.is_ready();
        }
        if ready {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    #[inline]
    fn poll_shutdown(&self, cx: &mut Context<'_>, is_error: bool) -> Poll<()> {
        let mut ready = true;
        for srv in self.servers.iter() {
            ready &= srv.poll_shutdown(cx, is_error).is_ready()
        }
        if ready {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }

    #[inline]
    fn call(&self, mut io: Io) -> Self::Future {
        let servers = self.servers.clone();
        let state = State::with_memory_pool(self.pool.pool.get());
        let shared = Rc::new(MqttShared::new(
            state.clone(),
            mqtt::Codec::default().max_inbound_size(self.max_size),
            0,
            self.pool.clone(),
        ));

        let delay = self.handshake_timeout.map(sleep);
        Box::pin(async move {
            // read first packet
            let packet = state
                .next(&mut io, &shared.codec)
                .await
                .map_err(|err| {
                    log::trace!("Error is received during mqtt handshake: {:?}", err);
                    MqttError::from(err)
                })
                .and_then(|res| {
                    res.ok_or_else(|| {
                        log::trace!("Server mqtt is disconnected during handshake");
                        MqttError::Disconnected
                    })
                })?;

            let connect = match packet {
                mqtt::Packet::Connect(connect) => connect,
                packet => {
                    log::info!("MQTT-3.1.0-1: Expected CONNECT packet, received {}", 1);
                    return Err(MqttError::Protocol(ProtocolError::Unexpected(
                        packet.packet_type(),
                        "MQTT-3.1.0-1: Expected CONNECT packet",
                    )));
                }
            };

            // call servers
            let mut item = (Handshake::new(connect, io, shared, 0, 0, 0), state, delay);
            for srv in servers.iter() {
                match srv.call(item).await? {
                    Either::Left(result) => {
                        item = result;
                    }
                    Either::Right(_) => return Ok(()),
                }
            }
            log::error!("Cannot handle CONNECT packet {:?}", item.0);
            Err(MqttError::ServerError("Cannot handle CONNECT packet"))
        })
    }
}

pub(crate) struct Selector2<Io, Err, InitErr> {
    servers: Vec<ServerFactory<Io, Err, InitErr>>,
    max_size: u32,
    pool: Rc<MqttSinkPool>,
    _t: marker::PhantomData<(Io, Err, InitErr)>,
}

impl<Io, Err, InitErr> ServiceFactory for Selector2<Io, Err, InitErr>
where
    Io: AsyncRead + AsyncWrite + Unpin + 'static,
    Err: 'static,
    InitErr: 'static,
{
    type Config = ();
    type Request = (Io, State, Option<Sleep>);
    type Response = ();
    type Error = MqttError<Err>;
    type InitError = InitErr;
    type Service = SelectorService2<Io, Err>;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Service, Self::InitError>>>>;

    fn new_service(&self, _: ()) -> Self::Future {
        let futs: Vec<_> = self.servers.iter().map(|srv| srv.new_service(())).collect();
        let max_size = self.max_size;
        let pool = self.pool.clone();

        Box::pin(async move {
            let mut servers = Vec::new();
            for fut in futs {
                servers.push(fut.await?);
            }
            Ok(SelectorService2 { max_size, pool, servers: Rc::new(servers) })
        })
    }
}

pub(crate) struct SelectorService2<Io, Err> {
    servers: Rc<Vec<Server<Io, Err>>>,
    max_size: u32,
    pool: Rc<MqttSinkPool>,
}

impl<Io, Err> Service for SelectorService2<Io, Err>
where
    Io: AsyncRead + AsyncWrite + Unpin + 'static,
    Err: 'static,
{
    type Request = (Io, State, Option<Sleep>);
    type Response = ();
    type Error = MqttError<Err>;
    type Future = Pin<Box<dyn Future<Output = Result<(), MqttError<Err>>>>>;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let mut ready = true;
        for srv in self.servers.iter() {
            ready &= srv.poll_ready(cx)?.is_ready();
        }
        if ready {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    #[inline]
    fn poll_shutdown(&self, cx: &mut Context<'_>, is_error: bool) -> Poll<()> {
        let mut ready = true;
        for srv in self.servers.iter() {
            ready &= srv.poll_shutdown(cx, is_error).is_ready()
        }
        if ready {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }

    #[inline]
    fn call(&self, (mut io, state, delay): Self::Request) -> Self::Future {
        let servers = self.servers.clone();
        let shared = Rc::new(MqttShared::new(
            state.clone(),
            mqtt::Codec::default().max_inbound_size(self.max_size),
            0,
            self.pool.clone(),
        ));

        Box::pin(async move {
            // read first packet
            let packet = state
                .next(&mut io, &shared.codec)
                .await
                .map_err(|err| {
                    log::trace!("Error is received during mqtt handshake: {:?}", err);
                    MqttError::from(err)
                })
                .and_then(|res| {
                    res.ok_or_else(|| {
                        log::trace!("Server mqtt is disconnected during handshake");
                        MqttError::Disconnected
                    })
                })?;

            let connect = match packet {
                mqtt::Packet::Connect(connect) => connect,
                packet => {
                    log::info!("MQTT-3.1.0-1: Expected CONNECT packet, received {:?}", packet);
                    return Err(MqttError::Protocol(ProtocolError::Unexpected(
                        packet.packet_type(),
                        "MQTT-3.1.0-1: Expected CONNECT packet",
                    )));
                }
            };

            // call servers
            let mut item = (Handshake::new(connect, io, shared, 0, 0, 0), state, delay);
            for srv in servers.iter() {
                match srv.call(item).await? {
                    Either::Left(result) => {
                        item = result;
                    }
                    Either::Right(_) => return Ok(()),
                }
            }
            log::error!("Cannot handle CONNECT packet {:?}", item.0);
            Err(MqttError::ServerError("Cannot handle CONNECT packet"))
        })
    }
}
