//! An extensible MySQL Proxy Server based on tokio-core

#[macro_use]
extern crate log;
extern crate env_logger;
#[macro_use]
extern crate futures;
#[macro_use]
extern crate tokio_core;
extern crate byteorder;

use std::rc::Rc;
use std::io::{self, Read, Write, Error, ErrorKind};
use std::net::Shutdown;

use futures::{Future, Poll, Async};
use tokio_core::net::{TcpStream};
use byteorder::*;

/// Handlers return a variant of this enum to indicate how the proxy should handle the packet.
pub enum Action {
    /// forward the packet unmodified
    Forward,
    /// forward a mutated packet
    Mutate(Packet),
    /// respond to the packet without forwarding
    Respond(Vec<Packet>)
}

/// Packet handlers need to implement this trait
pub trait PacketHandler {
    fn handle_request(&self, p: &Packet) -> Action;
    fn handle_response(&self, p: &Packet) -> Action;
}

/// A packet is just a wrapper for a Vec<u8>
pub struct Packet {
    pub bytes: Vec<u8>
}

impl Packet {

    /// Create an error packet
    pub fn error_packet(code: u16, state: [u8; 5], msg: String) -> Self {

        // start building payload
        let mut payload: Vec<u8> = Vec::with_capacity(9 + msg.len());
        payload.push(0xff);  // packet type
        payload.write_u16::<LittleEndian>(code).unwrap(); // error code
        payload.extend_from_slice("#".as_bytes()); // sql_state_marker
        payload.extend_from_slice(&state); // SQL STATE
        payload.extend_from_slice(msg.as_bytes());

        // create header with length and sequence id
        let mut header: Vec<u8> = Vec::with_capacity(4 + 9 + msg.len());
        header.write_u32::<LittleEndian>(payload.len() as u32).unwrap();
        header.pop(); // we need 3 byte length, so discard last byte
        header.push(1); // sequence_id

        // combine the vectors
        header.extend_from_slice(&payload);

        // now move the vector into the packet
        Packet { bytes: header }
    }

    pub fn sequence_id(&self) -> u8 {
        self.bytes[3]
    }

    /// Determine the type of packet
    pub fn packet_type(&self) -> Result<PacketType, Error> {
        match self.bytes[4] {
            0x00 => Ok(PacketType::ComSleep),
            0x01 => Ok(PacketType::ComQuit),
            0x02 => Ok(PacketType::ComInitDb),
            0x03 => Ok(PacketType::ComQuery),
            0x04 => Ok(PacketType::ComFieldList),
            0x05 => Ok(PacketType::ComCreateDb),
            0x06 => Ok(PacketType::ComDropDb),
            0x07 => Ok(PacketType::ComRefresh),
            0x08 => Ok(PacketType::ComShutdown),
            0x09 => Ok(PacketType::ComStatistics),
            0x0a => Ok(PacketType::ComProcessInfo),
            0x0b => Ok(PacketType::ComConnect),
            0x0c => Ok(PacketType::ComProcessKill),
            0x0d => Ok(PacketType::ComDebug),
            0x0e => Ok(PacketType::ComPing),
            0x0f => Ok(PacketType::ComTime),
            0x10 => Ok(PacketType::ComDelayedInsert),
            0x11 => Ok(PacketType::ComChangeUser),
            0x12 => Ok(PacketType::ComBinlogDump),
            0x13 => Ok(PacketType::ComTableDump),
            0x14 => Ok(PacketType::ComConnectOut),
            0x15 => Ok(PacketType::ComRegisterSlave),
            0x16 => Ok(PacketType::ComStmtPrepare),
            0x17 => Ok(PacketType::ComStmtExecute),
            0x18 => Ok(PacketType::ComStmtSendLongData),
            0x19 => Ok(PacketType::ComStmtClose),
            0x1a => Ok(PacketType::ComStmtReset),
            0x1d => Ok(PacketType::ComDaemon),
            0x1e => Ok(PacketType::ComBinlogDumpGtid),
            0x1f => Ok(PacketType::ComResetConnection),
            _ => Err(Error::new(ErrorKind::Other, "Invalid packet type"))
        }
    }

}

#[derive(Copy,Clone)]
pub enum PacketType {
    ComSleep = 0x00,
    ComQuit = 0x01,
    ComInitDb = 0x02,
    ComQuery = 0x03,
    ComFieldList = 0x04,
    ComCreateDb = 0x05,
    ComDropDb = 0x06,
    ComRefresh = 0x07,
    ComShutdown = 0x08,
    ComStatistics = 0x09,
    ComProcessInfo = 0x0a,
    ComConnect = 0x0b,
    ComProcessKill= 0x0c,
    ComDebug = 0x0d,
    ComPing = 0x0e,
    ComTime = 0x0f,
    ComDelayedInsert = 0x10,
    ComChangeUser = 0x11,
    ComBinlogDump = 0x12,
    ComTableDump = 0x13,
    ComConnectOut = 0x14,
    ComRegisterSlave = 0x15,
    ComStmtPrepare = 0x16,
    ComStmtExecute = 0x17,
    ComStmtSendLongData = 0x18,
    ComStmtClose = 0x19,
    ComStmtReset = 0x1a,
    ComDaemon= 0x1d,
    ComBinlogDumpGtid = 0x1e,
    ComResetConnection = 0x1f,
}


/// Wrapper for TcpStream with some built-in buffering
struct ConnReader {
    stream: Rc<TcpStream>,
    read_buf: Vec<u8>,
    read_pos: usize,
}

/// Wrapper for TcpStream with some built-in buffering
struct ConnWriter {
    stream: Rc<TcpStream>,
    write_buf: Vec<u8>,
    write_pos: usize,
}

impl ConnReader {

    fn new(stream: Rc<TcpStream>) -> Self {
        ConnReader {
            stream: stream,
            read_buf: vec![0u8; 4096],
            read_pos: 0,
        }
    }

    /// Read from the socket until the status is NotReady
    fn read(&mut self) -> Poll<(), io::Error> {
        loop {
            match self.stream.poll_read() {
                Async::Ready(_) => {
                    //TODO: ensure capacity first
                    let n = try_nb!((&*self.stream).read(&mut self.read_buf[self.read_pos..]));
                    if n == 0 {
                        return Err(Error::new(ErrorKind::Other, "connection closed"));
                    }
                    self.read_pos += n;
                },
                _ => return Ok(Async::NotReady),
            }
        }
    }

    fn next(&mut self) -> Option<Packet> {
        // do we have a header
        if self.read_pos > 3 {
            let l = parse_packet_length(&self.read_buf);
            // do we have the whole packet?
            let s = 4 + l;
            if self.read_pos >= s {
                let mut temp : Vec<u8> = Vec::with_capacity(s);
                temp.extend_from_slice(&self.read_buf[0..s]);
                let p = Packet { bytes: temp };

                // shift data down
                let mut j = 0;
                for i in s .. self.read_pos {
                    self.read_buf[j] = self.read_buf[i];
                    j += 1;
                }
                self.read_pos -= s;

                Some(p)
            } else {
                None
            }
        } else {
            None
        }
    }
}

impl ConnWriter {

    fn new(stream: Rc<TcpStream>) -> Self {
        ConnWriter{
            stream: stream,
            write_buf: vec![0u8; 4096],
            write_pos: 0,
        }
    }

    /// Write a packet to the write buffer
    fn push(&mut self, p: &Packet) {
        for i in 0 .. p.bytes.len() {
            self.write_buf[self.write_pos + i] = p.bytes[i];
        }
        self.write_pos += p.bytes.len();
    }

    /// Writes the contents of the write buffer to the socket
    fn write(&mut self) -> Poll<(), io::Error> {
        while self.write_pos > 0 {
            match self.stream.poll_write() {
                Async::Ready(_) => {
                    let s = try!((&*self.stream).write(&self.write_buf[0..self.write_pos]));

                    let mut j = 0;
                    for i in s..self.write_pos {
                        self.write_buf[j] = self.write_buf[i];
                        j += 1;
                    }
                    self.write_pos -= s;
                },
                _ => return Ok(Async::NotReady)
            }
        }
        return Ok(Async::Ready(()));
    }
}

pub struct Pipe<H: PacketHandler + 'static> {
    client_reader: ConnReader,
    client_writer: ConnWriter,
    server_reader: ConnReader,
    server_writer: ConnWriter,
    handler: H,
}

impl<H> Pipe<H> where H: PacketHandler + 'static {
    pub fn new(client: Rc<TcpStream>,
           server: Rc<TcpStream>,
           handler: H
           ) -> Pipe<H> {

        Pipe {
            client_reader: ConnReader::new(client.clone()),
            client_writer: ConnWriter::new(client),
            server_reader: ConnReader::new(server.clone()),
            server_writer: ConnWriter::new(server),
            handler: handler,
        }
    }
}

impl<H> Future for Pipe<H> where H: PacketHandler + 'static {
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<(), Error> {
        loop {
            let client_read = self.client_reader.read();

            // if the client connection has closed, close the server connection too
            match &client_read {
                &Err(ref e) => { self.server_writer.stream.shutdown(Shutdown::Write).unwrap(); },
                _ => {}
            }

            // process buffered requests
            while let Some(request) = self.client_reader.next() {
                match self.handler.handle_request(&request) {
                    Action::Forward => self.server_writer.push(&request),
                    Action::Mutate(ref p2) => self.server_writer.push(p2),
                    Action::Respond(ref v) => {
                        for p in v {
                            self.client_writer.push(&p);
                        }
                    }
                };
            }

            // try reading from server
            let server_read = self.server_reader.read();

            // if the server connection has closed, close the client connection too
            match &server_read {
                &Err(ref e) => { self.client_writer.stream.shutdown(Shutdown::Write).unwrap(); },
                _ => {}
            }

            // process buffered responses
            while let Some(response) = self.server_reader.next() {
                match self.handler.handle_response(&response) {
                    Action::Forward => self.client_writer.push(&response),
                    Action::Mutate(ref p2) => self.client_writer.push(p2),
                    Action::Respond(ref v) => {
                        for p in v {
                            self.server_writer.push(&p);
                        }
                    }
                };
            }

            // perform all of the writes at the end, since the request handlers may have
            // queued packets in either, or both directions

            // try writing to client
            let client_write = self.client_writer.write();

            // try writing to server
            let server_write = self.server_writer.write();

            try_ready!(client_read);
            try_ready!(client_write);
            try_ready!(server_read);
            try_ready!(server_write);
        }

    }

}

/// Parse the MySQL packet length (3 byte little-endian)
fn parse_packet_length(header: &[u8]) -> usize {
    (((header[2] as u32) << 16) |
        ((header[1] as u32) << 8) |
        header[0] as u32) as usize
}
