use redflareproxy::{StreamType, Subscriber, SOCKET_INDEX_SHIFT, SERVER};
use redflareproxy::{ClientToken};
use config::{AdminConfig};

use mio::*;
use bufstream::BufStream;
use mio::tcp::{TcpListener, TcpStream};
use std::collections::*;
use fxhash::FxHashMap as HashMap;
use fxhash::FxHashMap;
use std::io::Write;
use std::cell::Cell;

pub struct AdminPort {
    pub client_sockets: HashMap<ClientToken, BufStream<TcpStream>>,
    pub socket: TcpListener,
    pub config: AdminConfig,
}

impl AdminPort {
    pub fn new(config: AdminConfig, poll : &Poll, subscribers: &mut HashMap<Token, Subscriber>) -> AdminPort {
        /*let mut tcp_backlog = 128; // SOMAXCONN
        if config.get("tcp-backlog") != None {
            tcp_backlog = config["tcp-backlog"].as_integer().unwrap();
        }*/

        let addr = match config.listen.parse() {
            Ok(addr) => addr,
            Err(error) => {
                panic!("Unable to parse the admin listen port from config: {}. Reason: {:?}", config.listen, error);
            }
        };

        // Setup the server socket
        let server_socket = match TcpListener::bind(&addr) {
            Ok(socket) => socket,
            Err(error) => {
                panic!("Unable to bind to admin list port: {:?}. Reason: {:?}", addr, error);
            }
        };

        match poll.register(&server_socket, SERVER, Ready::readable(), PollOpt::edge()) {
            Ok(_) => {}
            Err(error) => {
                panic!("Failed to register admin listener socket to poll. Reason: {:?}", error);
            }
        };
        subscribers.insert(SERVER, Subscriber::AdminListener);
        debug!("Registered admin socket.");

        AdminPort {
            client_sockets: FxHashMap::default(),
            socket: server_socket,
            config: config,
        }
    }

    pub fn accept_client_connection(&mut self, token_index: &Cell<usize>, poll: &mut Poll, subscribers: &mut HashMap<Token, Subscriber>) {
        let (c, _) = match self.socket.accept() {
            Ok(socket) => socket,
            Err(error) => {
                error!("Unable to accept admin client connection. Reason: {:?}", error);
                return;
            }
        };
        token_index.set(token_index.get() + SOCKET_INDEX_SHIFT);
        let token = Token(token_index.get().clone());
        match poll.register(&c, token, Ready::readable(), PollOpt::edge()) {
            Ok(_) => {}
            Err(error) => {
                error!("Failed to register admin client socket to poll. Reason: {:?}", error);
            }
        };
        subscribers.insert(token, Subscriber::AdminClient);
        self.client_sockets.insert(token, BufStream::new(c));
    }

    pub fn write_to_client(&mut self, client_token: ClientToken, message: String, written_sockets: &mut Box<VecDeque<(Token, StreamType)>>) {
        match self.client_sockets.get_mut(&client_token) {
            Some(client_stream) => {
                let _ = client_stream.write(&message.into_bytes()[..]);
                written_sockets.push_back((client_token, StreamType::AdminClient));
            }
            None => {
                debug!("No client found for admin: {:?}. Did a switch_config just occur?", client_token);
            }
        }
    }
}
