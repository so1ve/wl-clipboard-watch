use std::collections::HashMap;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::os::fd::{AsFd, BorrowedFd};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use rustix::event::{PollFd, PollFlags, poll};
use rustix::pipe::pipe;
use rustix::time::Timespec;
use wayland_client::backend::ObjectId;
use wayland_client::globals::{GlobalList, GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop, event_created_child,
};
use wayland_protocols::ext::data_control::v1::client::ext_data_control_device_v1::{
    self as ext_device, ExtDataControlDeviceV1,
};
use wayland_protocols::ext::data_control::v1::client::ext_data_control_manager_v1::ExtDataControlManagerV1;
use wayland_protocols::ext::data_control::v1::client::ext_data_control_offer_v1::{
    self as ext_offer, ExtDataControlOfferV1,
};
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_device_v1::{
    self as wlr_device, ZwlrDataControlDeviceV1,
};
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1;
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_offer_v1::{
    self as wlr_offer, ZwlrDataControlOfferV1,
};

use crate::{Config, Event, Protocol, Selection, Transfer};

const MAX_MIME_TYPES: usize = 4096;
const TRANSFER_BUFFER_SIZE: usize = 64 * 1024;

/// A blocking Wayland clipboard selection watcher.
///
/// The watcher owns its Wayland event queue. It is intended to be moved to a
/// dedicated thread when used from an asynchronous application.
pub struct Watcher {
    device: Device,
    state: State,
    queue: EventQueue<State>,
    config: Config,
}

impl Watcher {
    /// Connects to the compositor using [`Config::default`].
    pub fn connect() -> Result<Self> {
        Self::connect_with(Config::default())
    }

    /// Connects to the compositor using `config`.
    pub fn connect_with(config: Config) -> Result<Self> {
        let connection =
            Connection::connect_to_env().context("cannot connect to the Wayland compositor")?;
        let (globals, mut queue) = registry_queue_init::<State>(&connection)
            .context("cannot initialize the Wayland registry")?;
        let handle = queue.handle();

        let seat = globals
            .bind::<WlSeat, _, _>(&handle, 1..=9, ())
            .context("Wayland compositor has no seat")?;
        let device = Device::bind(&globals, &seat, &handle)?;
        let mut state = State::default();

        queue
            .roundtrip(&mut state)
            .context("cannot retrieve the initial Wayland clipboard selection")?;

        let watcher = Self {
            device,
            state,
            queue,
            config,
        };
        watcher.ensure_healthy()?;

        Ok(watcher)
    }

    /// Returns the data-control protocol selected for this connection.
    #[must_use]
    pub const fn protocol(&self) -> Protocol {
        match self.device {
            Device::Ext(_) => Protocol::ExtDataControlV1,
            Device::Wlr(_) => Protocol::WlrDataControlV1,
        }
    }

    /// Blocks until the clipboard selection changes.
    ///
    /// The first call returns the selection announced by the compositor while
    /// the watcher was connecting.
    pub fn next_event(&mut self) -> Result<Event> {
        Ok(self.wait_for_event(None)?.unwrap())
    }

    /// Waits up to `timeout` for the clipboard selection to change.
    ///
    /// Pending events are returned even when `timeout` is zero.
    pub fn next_event_timeout(&mut self, timeout: Duration) -> Result<Option<Event>> {
        self.wait_for_event(Some(checked_deadline(timeout, "event timeout")?))
    }

    /// Receives one MIME type from `selection`.
    ///
    /// [`Transfer::Stale`] is returned when a newer selection arrives before
    /// the transfer completes.
    pub fn receive(&mut self, selection: &Selection, mime_type: &str) -> Result<Transfer> {
        ensure!(
            selection.offers(mime_type),
            "selection did not advertise MIME type {mime_type:?}"
        );

        self.dispatch_pending()?;
        if selection.generation != self.state.generation {
            return Ok(Transfer::Stale);
        }

        let (reader, writer) = pipe().context("cannot create a Wayland clipboard transfer pipe")?;
        let id = self.state.selection.as_ref().unwrap();
        self.state
            .offers
            .get(id)
            .unwrap()
            .proxy
            .receive(mime_type.to_owned(), writer.as_fd());
        drop(writer);
        self.queue
            .flush()
            .with_context(|| format!("cannot request Wayland MIME type {mime_type:?}"))?;

        self.read_transfer(selection, mime_type, File::from(reader))
    }

    fn wait_for_event(&mut self, deadline: Option<Instant>) -> Result<Option<Event>> {
        loop {
            self.dispatch_pending()?;
            if let Some(event) = self.state.take_event() {
                return Ok(Some(event));
            }
            if matches!(self.wait(None, deadline)?, WaitResult::Timeout) {
                return Ok(None);
            }
        }
    }

    fn read_transfer(
        &mut self,
        selection: &Selection,
        mime_type: &str,
        mut reader: File,
    ) -> Result<Transfer> {
        let deadline = checked_deadline(self.config.transfer_timeout, "transfer timeout")?;
        let mut bytes = Vec::new();
        let mut buffer = vec![0_u8; TRANSFER_BUFFER_SIZE];

        loop {
            self.dispatch_pending()?;
            if selection.generation != self.state.generation {
                return Ok(Transfer::Stale);
            }

            match self.wait(Some(&reader), Some(deadline))? {
                WaitResult::Timeout => {
                    bail!("timed out receiving Wayland MIME type {mime_type:?}");
                }
                WaitResult::Ready { transfer: false } => continue,
                WaitResult::Ready { transfer: true } => {}
            }
            if selection.generation != self.state.generation {
                return Ok(Transfer::Stale);
            }

            let remaining_bytes = self.config.max_mime_bytes - bytes.len();
            let read_size = remaining_bytes.min(buffer.len() - 1) + 1;
            match reader.read(&mut buffer[..read_size]) {
                Ok(0) => return Ok(Transfer::Complete(bytes)),
                Ok(read) if read > remaining_bytes => {
                    bail!(
                        "Wayland MIME type {mime_type:?} exceeds its {}-byte limit",
                        self.config.max_mime_bytes
                    );
                }
                Ok(read) => bytes.extend_from_slice(&buffer[..read]),
                Err(source) if source.kind() == ErrorKind::Interrupted => continue,
                Err(source) => {
                    return Err(source)
                        .with_context(|| format!("cannot read Wayland MIME type {mime_type:?}"));
                }
            }
        }
    }

    fn wait(&mut self, reader: Option<&File>, deadline: Option<Instant>) -> Result<WaitResult> {
        loop {
            let Some(guard) = self.queue.prepare_read() else {
                return Ok(WaitResult::Ready { transfer: false });
            };
            self.queue
                .flush()
                .context("cannot flush Wayland clipboard requests")?;

            let timeout = match deadline {
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Ok(WaitResult::Timeout);
                    }
                    Some(
                        Timespec::try_from(remaining)
                            .context("wait timeout is too large for this platform")?,
                    )
                }
                None => None,
            };

            let (poll_result, wayland_revents, transfer_revents) = {
                let transfer_descriptor = match reader {
                    Some(reader) => PollFd::new(reader, PollFlags::IN),
                    None => PollFd::new(&self.queue, PollFlags::empty()),
                };
                let mut descriptors =
                    [PollFd::new(&self.queue, PollFlags::IN), transfer_descriptor];
                let descriptor_count = if reader.is_some() { 2 } else { 1 };

                (
                    poll(&mut descriptors[..descriptor_count], timeout.as_ref()),
                    descriptors[0].revents(),
                    descriptors[1].revents(),
                )
            };

            match poll_result {
                Ok(0) => return Ok(WaitResult::Timeout),
                Ok(_) => {}
                Err(rustix::io::Errno::INTR) => continue,
                Err(source) => {
                    return Err(source).context("cannot poll clipboard descriptors");
                }
            }

            ensure_valid_descriptor(wayland_revents, "Wayland connection")?;
            if reader.is_some() {
                ensure_valid_descriptor(transfer_revents, "clipboard transfer pipe")?;
            }

            if wayland_revents.intersects(PollFlags::IN | PollFlags::HUP) {
                guard
                    .read()
                    .context("cannot read Wayland clipboard events")?;
                self.dispatch_pending()?;
            }

            return Ok(WaitResult::Ready {
                transfer: reader.is_some()
                    && transfer_revents.intersects(PollFlags::IN | PollFlags::HUP),
            });
        }
    }

    fn dispatch_pending(&mut self) -> Result<()> {
        self.queue
            .dispatch_pending(&mut self.state)
            .context("cannot dispatch Wayland clipboard events")?;

        self.ensure_healthy()
    }

    fn ensure_healthy(&self) -> Result<()> {
        if let Some(selection) = self.state.selection.as_ref() {
            let offer = self.state.offers.get(selection).unwrap();
            ensure!(
                !offer.too_many_mime_types,
                "Wayland source advertised more than {MAX_MIME_TYPES} MIME types"
            );
        }
        ensure!(
            !self.state.finished,
            "Wayland data-control device was removed"
        );

        Ok(())
    }
}

enum WaitResult {
    Timeout,
    Ready { transfer: bool },
}

enum Device {
    Ext(ExtDataControlDeviceV1),
    Wlr(ZwlrDataControlDeviceV1),
}

impl Device {
    fn bind(globals: &GlobalList, seat: &WlSeat, handle: &QueueHandle<State>) -> Result<Self> {
        if let Ok(manager) = globals.bind::<ExtDataControlManagerV1, _, _>(handle, 1..=1, ()) {
            let device = manager.get_data_device(seat, handle, ());
            manager.destroy();

            return Ok(Self::Ext(device));
        }

        let manager = globals
            .bind::<ZwlrDataControlManagerV1, _, _>(handle, 1..=1, ())
            .context("compositor supports neither ext-data-control-v1 nor wlr-data-control-v1")?;
        let device = manager.get_data_device(seat, handle, ());
        manager.destroy();

        Ok(Self::Wlr(device))
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        match self {
            Self::Ext(device) => device.destroy(),
            Self::Wlr(device) => device.destroy(),
        }
    }
}

struct Offer {
    proxy: OfferProxy,
    mime_types: Vec<String>,
    too_many_mime_types: bool,
}

enum OfferProxy {
    Ext(ExtDataControlOfferV1),
    Wlr(ZwlrDataControlOfferV1),
}

impl OfferProxy {
    fn id(&self) -> ObjectId {
        match self {
            Self::Ext(proxy) => proxy.id(),
            Self::Wlr(proxy) => proxy.id(),
        }
    }

    fn receive(&self, mime_type: String, fd: BorrowedFd<'_>) {
        match self {
            Self::Ext(proxy) => proxy.receive(mime_type, fd),
            Self::Wlr(proxy) => proxy.receive(mime_type, fd),
        }
    }

    fn destroy(self) {
        match self {
            Self::Ext(proxy) => proxy.destroy(),
            Self::Wlr(proxy) => proxy.destroy(),
        }
    }
}

#[derive(Default)]
struct State {
    offers: HashMap<ObjectId, Offer>,
    selection: Option<ObjectId>,
    selection_changed: bool,
    generation: u64,
    finished: bool,
}

impl State {
    fn add_offer(&mut self, proxy: OfferProxy) {
        let replaced = self.offers.insert(
            proxy.id(),
            Offer {
                proxy,
                mime_types: Vec::new(),
                too_many_mime_types: false,
            },
        );
        assert!(replaced.is_none());
    }

    fn add_mime_type(&mut self, id: &ObjectId, mime_type: String) {
        let offer = self.offers.get_mut(id).unwrap();
        if offer.mime_types.len() == MAX_MIME_TYPES {
            offer.too_many_mime_types = true;
        } else {
            offer.mime_types.push(mime_type);
        }
    }

    fn select(&mut self, selection: Option<ObjectId>) {
        if self.selection != selection {
            if let Some(previous) = self.selection.take() {
                self.offers.remove(&previous).unwrap().proxy.destroy();
            }
            self.selection = selection;
        }

        self.generation = self.generation.wrapping_add(1);
        self.selection_changed = true;
    }

    fn discard(&mut self, id: Option<ObjectId>) {
        if let Some(id) = id {
            self.offers.remove(&id).unwrap().proxy.destroy();
        }
    }

    fn take_event(&mut self) -> Option<Event> {
        if !std::mem::take(&mut self.selection_changed) {
            return None;
        }
        let Some(id) = self.selection.as_ref() else {
            return Some(Event::Cleared);
        };
        let offer = self.offers.get(id).unwrap();

        Some(Event::Selection(Selection {
            generation: self.generation,
            mime_types: offer.mime_types.clone(),
        }))
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlRegistry,
        _event: <WlRegistry as Proxy>::Event,
        _data: &GlobalListContents,
        _connection: &Connection,
        _handle: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: ignore WlSeat);
delegate_noop!(State: ExtDataControlManagerV1);
delegate_noop!(State: ZwlrDataControlManagerV1);

impl Dispatch<ExtDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ExtDataControlDeviceV1,
        event: ext_device::Event,
        _data: &(),
        _connection: &Connection,
        _handle: &QueueHandle<Self>,
    ) {
        match event {
            ext_device::Event::DataOffer { id } => state.add_offer(OfferProxy::Ext(id)),
            ext_device::Event::Selection { id } => {
                state.select(id.map(|offer| offer.id()));
            }
            ext_device::Event::PrimarySelection { id } => {
                state.discard(id.map(|offer| offer.id()));
            }
            ext_device::Event::Finished => state.finished = true,
            _ => {}
        }
    }

    event_created_child!(State, ExtDataControlDeviceV1, [
        ext_device::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ())
    ]);
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrDataControlDeviceV1,
        event: wlr_device::Event,
        _data: &(),
        _connection: &Connection,
        _handle: &QueueHandle<Self>,
    ) {
        match event {
            wlr_device::Event::DataOffer { id } => state.add_offer(OfferProxy::Wlr(id)),
            wlr_device::Event::Selection { id } => {
                state.select(id.map(|offer| offer.id()));
            }
            wlr_device::Event::PrimarySelection { id } => {
                state.discard(id.map(|offer| offer.id()));
            }
            wlr_device::Event::Finished => state.finished = true,
            _ => {}
        }
    }

    event_created_child!(State, ZwlrDataControlDeviceV1, [
        wlr_device::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ())
    ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ExtDataControlOfferV1,
        event: ext_offer::Event,
        _data: &(),
        _connection: &Connection,
        _handle: &QueueHandle<Self>,
    ) {
        if let ext_offer::Event::Offer { mime_type } = event {
            state.add_mime_type(&proxy.id(), mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ZwlrDataControlOfferV1,
        event: wlr_offer::Event,
        _data: &(),
        _connection: &Connection,
        _handle: &QueueHandle<Self>,
    ) {
        if let wlr_offer::Event::Offer { mime_type } = event {
            state.add_mime_type(&proxy.id(), mime_type);
        }
    }
}

fn checked_deadline(timeout: Duration, name: &str) -> Result<Instant> {
    Instant::now()
        .checked_add(timeout)
        .with_context(|| format!("{name} is too large"))
}

fn ensure_valid_descriptor(revents: PollFlags, name: &str) -> Result<()> {
    ensure!(!revents.contains(PollFlags::NVAL), "{name} became invalid");
    ensure!(
        !revents.contains(PollFlags::ERR) || revents.intersects(PollFlags::IN | PollFlags::HUP),
        "{name} failed"
    );

    Ok(())
}
