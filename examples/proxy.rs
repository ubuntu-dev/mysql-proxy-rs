//! MySQL Proxy Server
extern crate mysql_proxy;
use mysql_proxy::*;

#[macro_use]
extern crate log;
extern crate env_logger;
#[macro_use]
extern crate futures;
#[macro_use]
extern crate tokio_core;
extern crate byteorder;

use std::rc::Rc;
use std::env;
use std::net::{SocketAddr};
use std::str;

use futures::{Future};
use futures::stream::Stream;
use tokio_core::net::{TcpStream, TcpListener};
use tokio_core::reactor::{Core};

fn main() {

    env_logger::init().unwrap();

    // determine address for the proxy to bind to
    let bind_addr = env::args().nth(1).unwrap_or("127.0.0.1:3307".to_string());
    let bind_addr = bind_addr.parse::<SocketAddr>().unwrap();

    // determine address of the MySQL instance we are proxying for
    let mysql_addr = env::args().nth(2).unwrap_or("127.0.0.1:3306".to_string());
    let mysql_addr = mysql_addr.parse::<SocketAddr>().unwrap();

    // choose which packet handler to run
    let packet_handler = env::args().nth(3).unwrap_or("noop".to_string());

    // Create the tokio event loop that will drive this server
    let mut l = Core::new().unwrap();

    // Get a reference to the reactor event loop
    let handle = l.handle();

    // Create a TCP listener which will listen for incoming connections
    let socket = TcpListener::bind(&bind_addr, &l.handle()).unwrap();
    println!("Listening on: {}", bind_addr);

    // for each incoming connection
    let done = socket.incoming().for_each(move |(socket, _)| {

        // create a future to serve requests
        let future = TcpStream::connect(&mysql_addr, &handle).and_then(move |mysql| {
            Ok((socket, mysql))
        }).and_then(move |(client, server)| {

            // create a handler based on cmd-line arg chosen
            let handler : Box<PacketHandler> = match packet_handler {
                String::from("noop") => Box::new(NoopHandler {}),
                String::from("logging") => Box::new(PacketLoggingHandler {}),
                String::from("avocado") => Box::new(AvocadoHandler {}),
                _ => panic!("Invalid packet handler name {}", packet_handler)
            };

            // return the future to handle this connection pair
            Pipe::new(Rc::new(client), Rc::new(server), handler)
        });

        // tell the tokio reactor to run the future
        handle.spawn(future.map_err(|err| {
            println!("Error: {:?}", err);
        }));

        // everything is great!
        Ok(())

    });
    l.run(done).unwrap();
}

/// This handler simply passes packets through without adding any new behavior and is here
/// to demonstrate the API as well as provide a simple way to perform benchmarks.
struct NoopHandler {}

impl PacketHandler for NoopHandler {

    fn handle_request(&mut self, p: &Packet) -> Action {
        Action::Forward
    }

    fn handle_response(&mut self, p: &Packet) -> Action {
        Action::Forward
    }
}

/// This handler logs all SQL queries issued by clients
struct PacketLoggingHandler {}

impl PacketHandler for PacketLoggingHandler {
    fn handle_request(&mut self, p: &Packet) -> Action {
        match p.packet_type() {
            Ok(PacketType::ComQuery) => {
                let slice = &p.bytes[5..];
                let sql = String::from_utf8(slice.to_vec()).expect("Invalid UTF-8");
                println!("SQL: {}", sql);
            }
        }
        Action::Forward
    }

    fn handle_response(&mut self, p: &Packet) -> Action {
        Action::Forward
    }
}


/// This handler logs all SQL queries issued by clients, and rejects queries containing the
/// the word 'avocado'.
struct AvocadoHandler {}

impl PacketHandler for AvocadoHandler {

    fn handle_request(&mut self, p: &Packet) -> Action {
        print_packet_chars(&p.bytes);
        match p.packet_type() {
            Ok(PacketType::ComQuery) => {
                // ComQuery packets just contain a SQL string as the payload
                let slice = &p.bytes[5..];

                // convert the slice to a String object
                let sql = String::from_utf8(slice.to_vec()).expect("Invalid UTF-8");

                // log the query
                println!("SQL: {}", sql);

                // dumb example of conditional proxy behavior
                if sql.contains("avocado") {
                    // take over processing of this packet and return an error packet
                    // to the client
                    Action::Error {
                        code: 1064, // error code
                        state: [0x31, 0x32, 0x33, 0x34, 0x35], // sql state
                        msg: String::from("Proxy rejecting any avocado-related queries")
                    }
                } else {
                    // pass the packet to MySQL unmodified
                    Action::Forward
                }
            },
            _ => Action::Forward
        }
    }

    fn handle_response(&mut self, _: &Packet) -> Action {
        // forward all responses to the client
        Action::Forward
    }

}

#[allow(dead_code)]
pub fn print_packet_chars(buf: &[u8]) {
    print!("[");
    for i in 0..buf.len() {
        print!("{} ", buf[i] as char);
    }
    println!("]");
}

#[allow(dead_code)]
pub fn print_packet_bytes(buf: &[u8]) {
    print!("[");
    for i in 0..buf.len() {
        if i%8==0 { println!(""); }
        print!("{:#04x} ",buf[i]);
    }
    println!("]");
}


