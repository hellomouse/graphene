use std::fmt::Display;

use tracing::{debug, info_span, trace, warn};
use uuid::Uuid;

use crate::flow_table::{Flow, FlowCompare};
use crate::stream::Stream;
use crate::ConnectionHandler;
use crate::TcpMeta;

/// TCP handshake state
#[derive(Debug)]
pub enum ConnectionState {
    /// not yet initialized
    None,
    /// SYN read, expect SYN/ACK
    SynSent {
        /// sequence number of initial SYN
        seq_no: u32,
    },
    /// SYN/ACK read, expect ACK
    SynReceived {
        /// sequence number of SYN/ACK
        seq_no: u32,
        /// acknowledgment number of SYN/ACK
        ack_no: u32,
        /// window size of SYN/ACK
        window_size: u16,
        /// whether or not we saw the first SYN
        syn_seen: bool,
    },
    /// handshake complete, connection established
    Established {
        /// initial sequence number of forward direction
        forward_isn: u32,
        /// initial sequence number of reverse direction
        reverse_isn: u32,
    },
    /// connection closed
    Closed,
}

/// packet direction
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// forward direction: client -> server, assuming client is whoever sent the
    /// first SYN
    Forward,
    /// reverse direction: server -> client, assuming client is whoever sent the
    /// first SYN
    Reverse,
}

impl Direction {
    pub fn swap(self) -> Direction {
        match self {
            Direction::Forward => Direction::Reverse,
            Direction::Reverse => Direction::Forward,
        }
    }
}

impl Display for Direction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Direction::Forward => write!(f, "forward")?,
            Direction::Reverse => write!(f, "reverse")?,
        }
        Ok(())
    }
}

/// object representing TCP connection
pub struct Connection<H: ConnectionHandler> {
    /// unique identifier for connection
    pub uuid: Uuid,
    /// forward direction flow identifier
    pub forward_flow: Flow,
    /// state of connection handshake
    pub conn_state: ConnectionState,

    /// whether the full 3-way handshake was observed
    pub observed_handshake: bool,
    /// whether the connection close was observed (either by FIN or RST)
    pub observed_close: bool,

    /// forward direction stream
    pub forward_stream: Stream,
    /// reverse direction stream
    pub reverse_stream: Stream,

    /// event handler object
    pub event_handler: Option<H>,
}

/// result from Connection::handle_packet
pub enum HandlePacketResult {
    /// everything was fine, probably
    Fine,
}

impl<H: ConnectionHandler> Connection<H> {
    /// create new connection with flow
    pub fn new(forward_flow: Flow) -> Connection<H> {
        let mut conn = Connection {
            uuid: Uuid::new_v4(),
            forward_flow,
            conn_state: ConnectionState::None,
            observed_handshake: false,
            observed_close: false,
            forward_stream: Stream::new(),
            reverse_stream: Stream::new(),
            event_handler: None,
        };
        let handler = H::new(&mut conn);
        conn.event_handler = Some(handler);
        conn
    }

    /// handle a packet supposedly belonging to this connection
    #[tracing::instrument(name = "conn", skip_all, fields(id = %self.uuid))]
    pub fn handle_packet(&mut self, meta: TcpMeta, data: &[u8]) -> bool {
        debug_assert_ne!(self.forward_flow.compare_tcp_meta(&meta), FlowCompare::None);
        if meta.flags.syn {
            self.handle_syn(meta)
        } else if meta.flags.rst {
            self.handle_rst(meta)
        } else {
            // FIN packets handled here too, as they may carry data
            self.handle_data(meta, data)
        }
    }

    /// handle packet with SYN flag
    pub fn handle_syn(&mut self, meta: TcpMeta) -> bool {
        debug_assert!(meta.flags.syn);
        if meta.flags.rst {
            // probably shouldn't happen
            warn!("received strange packet with flags {:?}", meta.flags);
        }
        match self.conn_state {
            ConnectionState::None => {
                if meta.flags.ack {
                    // SYN/ACK
                    self.conn_state = ConnectionState::SynReceived {
                        seq_no: meta.seq_number,
                        ack_no: meta.ack_number,
                        window_size: meta.window,
                        syn_seen: false,
                    };
                    trace!(
                        "handle_syn: got SYN/ACK (no SYN), None -> SynReceived (seq {}, ack {})",
                        meta.seq_number,
                        meta.ack_number
                    );
                    if let Some(scale) = meta.option_window_scale {
                        trace!("got window scale (SYN/ACK): {}", scale);
                        self.reverse_stream.window_scale = scale;
                    }
                    if self.forward_flow.compare_tcp_meta(&meta) == FlowCompare::Forward {
                        // SYN/ACK is expected server -> client
                        trace!("handle_syn: got SYN/ACK, reversing forward_flow");
                        self.forward_flow.reverse();
                    }
                    true
                } else {
                    // first SYN
                    self.conn_state = ConnectionState::SynSent {
                        seq_no: meta.seq_number,
                    };
                    trace!(
                        "handle_syn: got SYN, None -> SynSent (seq {})",
                        meta.seq_number
                    );
                    if let Some(scale) = meta.option_window_scale {
                        trace!("got window scale (first SYN): {}", scale);
                        self.forward_stream.window_scale = scale;
                    }
                    if self.forward_flow.compare_tcp_meta(&meta) == FlowCompare::Reverse {
                        // SYN is expected client -> server
                        self.forward_flow.reverse();
                    }
                    true
                }
            }
            ConnectionState::SynSent { seq_no } => {
                // expect: SYN/ACK
                if meta.flags.ack {
                    // SYN/ACK received
                    if self.forward_flow.compare_tcp_meta(&meta) != FlowCompare::Reverse {
                        // wrong direction?
                        trace!("handle_syn: dropped SYN/ACK in wrong direction (state SynSent)");
                        false
                    } else {
                        if meta.ack_number != seq_no + 1 {
                            warn!(
                                "SYN/ACK packet ack number mismatch: expected {}, found {}",
                                seq_no + 1,
                                meta.ack_number
                            );
                        }
                        self.conn_state = ConnectionState::SynReceived {
                            seq_no: meta.seq_number,
                            ack_no: meta.ack_number,
                            window_size: meta.window,
                            syn_seen: true,
                        };
                        trace!(
                            "handle_syn: received SYN/ACK, SynSent -> SynReceived (seq {}, ack {})",
                            meta.seq_number,
                            meta.ack_number
                        );
                        if let Some(scale) = meta.option_window_scale {
                            trace!("got window scale (SYN/ACK): {}", scale);
                            self.reverse_stream.window_scale = scale;
                        }
                        true
                    }
                } else {
                    // likely duplicate SYN
                    false
                }
            }
            ConnectionState::SynReceived { .. } => {
                // either duplicate SYN or SYN/ACK, ignore
                false
            }
            ConnectionState::Established { .. } => {
                // ???
                warn!("received SYN for established connection?");
                false
            }
            _ => false, // ignore
        }
    }

    /// handle packet with RST flag
    pub fn handle_rst(&mut self, meta: TcpMeta) -> bool {
        debug_assert!(meta.flags.rst);
        match self.forward_flow.compare_tcp_meta(&meta) {
            FlowCompare::Forward => {
                debug!("received reset in forward direction");
                self.forward_stream.had_reset = true;
            }
            FlowCompare::Reverse => {
                debug!("received reset in reverse direction");
                self.reverse_stream.had_reset = true;
            }
            _ => unreachable!("got unrelated flow"),
        }

        self.conn_state = ConnectionState::Closed;
        self.observed_close = true;
        true
    }

    /// handle data packet received before SYN/ACK
    pub fn handle_data_hs1(&mut self, meta: TcpMeta, data: &[u8]) -> bool {
        trace!(
            "handle_data_hs1: received data before handshake completion, {:?} -> Established",
            self.conn_state
        );
        let (forward_isn, reverse_isn) = match self.forward_flow.compare_tcp_meta(&meta) {
            FlowCompare::Forward => (meta.seq_number, meta.ack_number),
            FlowCompare::Reverse => (meta.ack_number, meta.seq_number),
            _ => unreachable!("got unrelated flow"),
        };

        self.conn_state = ConnectionState::Established {
            forward_isn,
            reverse_isn,
        };

        self.forward_stream.set_isn(forward_isn, 0);
        self.reverse_stream.set_isn(reverse_isn, 0);

        trace!("handle_data_hs1: assuming forward isn: {forward_isn}, reverse isn: {reverse_isn}");

        self.call_handler(|conn, h| h.handshake_done(conn));

        if !data.is_empty() {
            self.handle_data_established(meta, data)
        } else {
            true
        }
    }

    /// handle data packet received after SYN/ACK
    pub fn handle_data_hs2(
        &mut self,
        meta: TcpMeta,
        data: &[u8],
        seq_no: u32,
        ack_no: u32,
        forward_window: u16,
        syn_seen: bool,
    ) -> bool {
        let mut reverse_window: u16 = 0;
        let (forward_isn, reverse_isn) = match self.forward_flow.compare_tcp_meta(&meta) {
            FlowCompare::Forward => {
                if meta.flags.ack && meta.seq_number == ack_no && meta.ack_number == seq_no + 1 {
                    if syn_seen {
                        self.observed_handshake = true;
                        reverse_window = meta.window;
                        trace!("handle_data_hs2: got complete handshake");
                    } else {
                        trace!("handle_data_hs2: got SYN/ACK and ACK of handshake");
                    }
                } else {
                    trace!("handle_data_hs2: probably lost final packet of handshake")
                }
                (meta.seq_number, meta.ack_number)
            }
            FlowCompare::Reverse => {
                trace!("handle_data_hs2: received reverse direction packet instead of final handshake ACK");
                (meta.ack_number, meta.seq_number)
            }
            _ => unreachable!("got unrelated flow"),
        };
        trace!(
            "handle_data_hs2: received data packet, SynReceived -> Established \
            (forward_isn: {forward_isn}, reverse_isn: {reverse_isn})"
        );

        self.conn_state = ConnectionState::Established {
            forward_isn,
            reverse_isn,
        };
        self.forward_stream.set_isn(forward_isn, forward_window);
        self.reverse_stream.set_isn(reverse_isn, reverse_window);
        self.call_handler(|conn, h| h.handshake_done(conn));

        if !data.is_empty() {
            self.handle_data_established(meta, data)
        } else {
            true
        }
    }

    /// handle data after handshake is completed
    pub fn handle_data_established(&mut self, meta: TcpMeta, data: &[u8]) -> bool {
        let dir;
        let (data_stream, ack_stream) = match self.forward_flow.compare_tcp_meta(&meta) {
            FlowCompare::Forward => {
                dir = Direction::Forward;
                (&mut self.forward_stream, &mut self.reverse_stream)
            }
            FlowCompare::Reverse => {
                dir = Direction::Reverse;
                (&mut self.reverse_stream, &mut self.forward_stream)
            }
            _ => unreachable!("got unrelated flow"),
        };

        let mut did_something = false;
        let mut got_data = false;
        if !data.is_empty() {
            // write data to stream
            let sp = info_span!("stream", %dir);
            got_data = sp.in_scope(|| data_stream.handle_data_packet(meta.seq_number, data));
            did_something |= got_data;
        }
        let mut ack_stream_got_end = false;
        if meta.flags.ack {
            let was_ended = ack_stream.has_ended;
            // send ack to the stream in the opposite direction
            let sp = info_span!("stream", dir = %dir.swap());
            did_something |=
                sp.in_scope(|| ack_stream.handle_ack_packet(meta.ack_number, meta.window));
            if !was_ended && ack_stream.has_ended {
                ack_stream_got_end = true;
                trace!("handle_data: {:?} received ACK for FIN", dir.swap());
            }
        }
        let data_stream_has_ended = data_stream.has_ended;
        let mut got_fin = false;
        if meta.flags.fin {
            // notify stream of fin
            let sp = info_span!("stream", %dir);
            got_fin = sp.in_scope(|| data_stream.handle_fin_packet(meta.seq_number, data.len()));
            did_something |= got_fin;
        }

        // call event handlers
        if got_data {
            self.call_handler(|conn, h| h.data_received(conn, dir))
        }
        if got_fin {
            self.call_handler(|conn, h| h.fin_received(conn, dir));
        }

        if ack_stream_got_end {
            self.call_handler(|conn, h| h.stream_end(conn, dir.swap()));

            // update state if both sides closed
            if data_stream_has_ended {
                self.conn_state = ConnectionState::Closed;
                self.observed_close = true;
            }
        }

        did_something
    }

    /// handle ordinary data packet
    pub fn handle_data(&mut self, meta: TcpMeta, data: &[u8]) -> bool {
        match self.conn_state {
            ConnectionState::None | ConnectionState::SynSent { .. } => {
                self.handle_data_hs1(meta, data)
            }
            ConnectionState::SynReceived {
                seq_no,
                ack_no,
                window_size,
                syn_seen,
            } => self.handle_data_hs2(meta, data, seq_no, ack_no, window_size, syn_seen),
            _ => {
                // established or (closed but more data)
                self.handle_data_established(meta, data)
            }
        }
    }

    /// call the event handler, if one exists
    pub fn call_handler(&mut self, do_thing: impl FnOnce(&mut Self, &mut H)) {
        if let Some(mut handler) = self.event_handler.take() {
            do_thing(self, &mut handler);
            self.event_handler = Some(handler);
        }
    }

    /// called before connection is removed from hashtable
    pub fn will_retire(&mut self) {
        self.call_handler(|conn, h| h.will_retire(conn));
    }
}

#[cfg(test)]
mod test {
    use crate::{initialize_logging, ConnectionHandler, TcpFlags, TcpMeta};
    use parking_lot::Mutex;
    use std::mem;

    use super::{Connection, Direction};

    /// swap src/dest ip/port and seq/ack
    fn swap_meta(meta: &TcpMeta) -> TcpMeta {
        let mut out = meta.clone();
        // crimes against something, idk what, but it's crimes
        macro_rules! swap {
            ($i1:ident, $i2:ident) => {
                mem::swap(&mut out.$i1, &mut out.$i2)
            };
        }
        swap!(src_addr, dst_addr);
        swap!(src_port, dst_port);
        swap!(seq_number, ack_number);
        out
    }

    static HANDSHAKE_DONE: Mutex<bool> = Mutex::new(false);
    static DATA_RECEIVED: Mutex<Option<Direction>> = Mutex::new(None);
    static FIN_RECEIVED: Mutex<Option<Direction>> = Mutex::new(None);
    static RST_RECEIVED: Mutex<Option<Direction>> = Mutex::new(None);
    static STREAM_END: Mutex<Option<Direction>> = Mutex::new(None);
    static WILL_RETIRE: Mutex<bool> = Mutex::new(false);

    struct TestHandler;
    impl ConnectionHandler for TestHandler {
        fn new(_conn: &mut Connection<Self>) -> Self {
            TestHandler
        }
        fn handshake_done(&mut self, _conn: &mut Connection<Self>) {
            let mut guard = HANDSHAKE_DONE.lock();
            *guard = true;
        }
        fn data_received(&mut self, _connection: &mut Connection<Self>, direction: Direction) {
            let mut guard = DATA_RECEIVED.lock();
            *guard = Some(direction);
        }
        fn fin_received(&mut self, _connection: &mut Connection<Self>, direction: Direction) {
            let mut guard = FIN_RECEIVED.lock();
            *guard = Some(direction);
        }
        fn rst_received(&mut self, _connection: &mut Connection<Self>, direction: Direction) {
            let mut guard = RST_RECEIVED.lock();
            *guard = Some(direction);
        }
        fn stream_end(&mut self, _connection: &mut Connection<Self>, direction: Direction) {
            let mut guard = STREAM_END.lock();
            *guard = Some(direction);
        }
        fn will_retire(&mut self, _connection: &mut Connection<Self>) {
            let mut guard = WILL_RETIRE.lock();
            *guard = true;
        }
    }

    #[test]
    fn simple() {
        initialize_logging();

        let hs1 = TcpMeta {
            src_addr: [91, 92, 144, 105].into(),
            src_port: 3161,
            dst_addr: [23, 146, 104, 1].into(),
            dst_port: 45143,
            seq_number: 1587232,
            ack_number: 0,
            flags: TcpFlags {
                syn: true,
                ..Default::default()
            },
            window: 256,
            option_window_scale: Some(2),
            option_timestamp: None,
        };

        let mut conn: Connection<TestHandler> = Connection::new(hs1.clone().into());
        assert!(conn.handle_packet(hs1.clone(), &[]));
        let mut hs2 = swap_meta(&hs1);
        hs2.seq_number = 315848;
        hs2.ack_number += 1;
        hs2.flags.ack = true;
        assert!(conn.handle_packet(hs2.clone(), &[]));
        let mut hs3 = swap_meta(&hs2);
        hs3.ack_number += 1;
        hs3.flags.syn = false;
        assert!(conn.handle_packet(hs3.clone(), &[]));

        let mut hs_done = HANDSHAKE_DONE.lock();
        assert!(*hs_done);
        *hs_done = false;

        let data1 = hs3.clone();
        assert!(conn.handle_packet(data1.clone(), b"test"));
        assert_eq!(conn.forward_stream.readable_buffered_length(), 4);
    }
}
