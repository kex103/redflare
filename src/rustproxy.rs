use std::time::Instant;
use admin;
use config::{RustProxyConfig, BackendPoolConfig, load_config};
use backendpool;
use backendpool::BackendPool;
use mio::*;
use mio::unix::{UnixReady};
use std::collections::*;
use std::io::{Write};
use std::mem;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

// For admin reqs.
use backend::parse_redis_command;
use toml;
use std::process;

pub const NULL_TOKEN: Token = Token(0);
pub const SERVER: Token = Token(1);

const FIRST_SOCKET_INDEX: usize = 10;
pub const SOCKET_INDEX_SHIFT: usize = 2;

pub type BackendToken = Token;
pub type PoolToken = Token;
pub type ClientToken = Token;

pub enum StreamType {
    AdminClient,
    PoolClient,
    PoolServer,
}

#[derive(Clone, Copy, Debug)]
pub enum Subscriber {
    Timeout(PoolToken),
    RequestTimeout(PoolToken, Instant),
    PoolServer(PoolToken),
    PoolListener,
    PoolClient(PoolToken),
    AdminListener,
    AdminClient,
}

pub fn generate_backend_token(
    next_socket_index: &Cell<usize>,
    backend_tokens: &RefCell<HashMap<BackendToken, PoolToken>>,
    pool_token: PoolToken
) -> BackendToken {
    next_socket_index.set(next_socket_index.get() + SOCKET_INDEX_SHIFT);
    let new_token = Token(next_socket_index.get());
    backend_tokens.borrow_mut().insert(new_token, pool_token);
    return new_token.clone();
}

pub fn generate_client_token(next_socket_index: &Cell<usize>) -> ClientToken {
    next_socket_index.set(next_socket_index.get() + SOCKET_INDEX_SHIFT);
    let new_token = Token(next_socket_index.get());
    new_token
}

// High-level struct that contains everything for a rustproxy instance.
pub struct RustProxy {
    // This may just get integrated back into RustProxy.
    pub admin: admin::AdminPort,

    // Configs
    pub config: RustProxyConfig,
    pub staged_config: Option<RustProxyConfig>,

    // Child structs.
    pub backendpools: HashMap<PoolToken, BackendPool>,

    // Registry...
    backend_configs: HashMap<BackendPoolConfig, PoolToken>,
    backend_tokens: Rc<RefCell<HashMap<BackendToken, PoolToken>>>,
    subscribers: Rc<RefCell<HashMap<Token, Subscriber>>>,
    pub written_sockets: Box<VecDeque<(Token, StreamType)>>,
    poll: Rc<RefCell<Poll>>,
    next_socket_index: Rc<Cell<usize>>,
}
impl RustProxy {
    pub fn new(config_path: String) -> Result<RustProxy, String> {
        let config = try!(load_config(config_path));
        let poll = match Poll::new() {
            Ok(poll) => Rc::new(RefCell::new(poll)),
            Err(error) => {
                return Err(format!("Failed to init poll: {:?}", error));
            }
        };
        let subscribers = Rc::new(RefCell::new(HashMap::new()));
        let admin = admin::AdminPort::new(config.admin.clone(), &poll.borrow(), &mut subscribers.borrow_mut());

        let mut rustproxy = RustProxy {
            admin: admin,
            next_socket_index: Rc::new(Cell::new(FIRST_SOCKET_INDEX)),
            backendpools: HashMap::with_capacity(config.pools.len()),
            config: config,
            staged_config: None,
            backend_tokens: Rc::new(RefCell::new(HashMap::new())),
            backend_configs: HashMap::new(),
            subscribers: subscribers,
            written_sockets: Box::new(VecDeque::new()),
            poll: poll,
        };
        // Populate backend pools.
        let pools_config = rustproxy.config.pools.clone();
        for (pool_name, pool_config) in pools_config {
            rustproxy.init_backend_pool(&pool_name, &pool_config);
        }
        debug!("Initialized rustproxy");

        Ok(rustproxy)
    }

    pub fn switch_config(&mut self) -> Result<(), String> {
        if self.staged_config.is_none() {
            return Err("No staged config".to_owned());
        }
        // Check that configs aren't the same.
        {
            match self.staged_config {
                Some(ref staged_config) => {
                    if staged_config == &self.config {
                        return Err("The configs are the same!".to_owned());
                    }
                }
                None => {}
            }
        }
        let staged_config = mem::replace(&mut self.staged_config, None);
        self.config = staged_config.unwrap();

        // Replace admin.
        if self.config.admin != self.admin.config {
            let admin = admin::AdminPort::new(self.config.admin.clone(), &self.poll.borrow(), &mut self.subscribers.borrow_mut());
            self.admin = admin; // TODO: what to do with old admin?
        }

        // Remove pools if they do not exist in config.
        let mut expired_pools = Vec::new();
        for pool_token in self.backendpools.keys() {
            let ref config = self.backendpools.get(&pool_token).unwrap().config;
            let mut should_keep = false;
            for (_, p_config) in self.config.pools.iter() {
                if p_config == config {
                    should_keep = true;
                    break;
                }
            }
            if !should_keep {
                expired_pools.push(pool_token.clone());
            }
        }
        for pool_token in expired_pools {
            self.remove_pool(pool_token.clone());
        }

        // Add pools if they are new.
        for (pool_name, pool_config) in self.config.clone().pools.iter() {
            if self.backend_configs.contains_key(&pool_config) {
                // Do we really need to set the name? Shouldn't they be set properly already?
                let pool = self.backendpools.get_mut(self.backend_configs.get(&pool_config).unwrap()).unwrap();
                pool.name = pool_name.clone();
            } else {
                // Create backend pool.
                self.init_backend_pool(pool_name, pool_config);
            }
        }

        // Clean up registries?
        Ok(())
    }

    pub fn run(&mut self) {
        let mut events = Events::with_capacity(1024);
        loop {
            {
            match self.poll.borrow_mut().poll(&mut events, None) {
                Ok(_poll_size) => {}
                Err(error) => {
                    panic!("Error polling. Shutting down: {:?}", error);
                }
            };}
            for event in events.iter() {
                debug!("Event detected: {:?} {:?}", &event.token(), event.readiness());
                self.handle_event(&event);
            }
            self.write_to_sockets();
        }
    }

    fn write_to_sockets(&mut self) {
        loop {
            let temp = self.written_sockets.pop_front();
            debug!("Flushed writing to sockets.");
            let (stream_token, stream_type) = match temp {
                Some(socket_token) => socket_token,
                None => break,
            };
            match stream_type {
                StreamType::AdminClient => {
                    match self.admin.client_sockets.get_mut(&stream_token) {
                        Some(stream) => {
                            let _ = stream.flush();
                        }
                        None => {
                            debug!("write_to_sockets: AdminClient {:?} no longer registered. Did a switch_config occur?", stream_token);
                        }
                    }
                }
                StreamType::PoolClient => {
                    match self.subscribers.borrow().get(&stream_token) {
                        Some(sub) => {
                            let subscriber = sub.clone();
                            match subscriber {
                                Subscriber::PoolClient(pool_token) => {
                                    let pool = self.backendpools.get_mut(&pool_token).unwrap();
                                    debug!("Writing out to {:?}", stream_token);
                                    let _ = pool.client_sockets.get_mut(&stream_token).unwrap().flush();
                                }

                                _ => panic!("write_to_sockets: Mismatch between StreamType and Subscriber: {:?}. Shutting down.", stream_token),
                            }
                        }
                        None => {
                            debug!("write_to_sockets: PoolClient {:?} no longer registered as a subscriber. Did a switch_config occur?", stream_token);
                        }
                    }
                }
                StreamType::PoolServer => {
                    match self.backend_tokens.borrow_mut().get_mut(&stream_token) {
                        Some(p_token) => {
                            let pool_token = p_token.clone();
                            let pool = self.backendpools.get_mut(&pool_token).unwrap();
                            let backend = pool.get_backend(stream_token);
                            backend.flush_stream();
                        }
                        None => {
                            debug!("write_to_sockets: PoolServer {:?} no longer registered. Did a switch_config occur?", stream_token);
                        }
                    }
                }
            }
        }
    }

    fn handle_event(&mut self, event: &Event) {
        let token = event.token();
        debug!("Event: {:?}", token);
        if event.readiness().contains(UnixReady::error()) {
            // TODO: Don't want to do mark backend down for client connections.
            /* Why does the errror occur? How does a backend socket just error? Timeout? Is this on establishing connection?*/
            // TODO: We want to make sure these tokens that fail are actualy backend tokens. It could be something else, like timers.
            let backend_tokens = self.backend_tokens.borrow();
            let pool_token = match backend_tokens.get(&token) {
                Some(pool_token) => pool_token,
                None => {
                    error!("Unable to find backend_token for error token: {:?}", token);
                    return;
                }
            };
            let pool = match self.backendpools.get_mut(&pool_token) {
                Some(pool) => pool,
                None => {
                    error!("Unable to find pool for pool token: {:?}", pool_token);
                    return;
                }
            };
            let backend = match pool.backend_map.get_mut(&token) {
                Some(backend) => backend,
                None => {
                    error!("Unable to find backend from token: {:?}", token);
                    return;
                }
            };
            backend.handle_backend_failure(token);
            return;
        }
        let subscriber = match self.subscribers.borrow().get(&token) {
            Some(subscriber) => subscriber.clone(),
            None => {
                debug!("Subscriber does not contain key: {:?}", token);
                return;
            }
        };

        match subscriber {
            Subscriber::Timeout(pool_token) => {
                debug!("Timeout {:?} for Pool {:?}", token, pool_token);
                match self.backendpools.get_mut(&pool_token.clone()) {
                    Some(pool) => {
                        let backend_token = Token(token.0 - 1);
                        pool.handle_reconnect(backend_token)
                    }
                    None => error!("Hashmap says it has token but it really doesn't! {:?}",subscriber),
                }
            }
            Subscriber::RequestTimeout(pool_token, timestamp) => {
                debug!("RequestTimeout {:?} for Pool {:?}", token, pool_token);
                match self.backendpools.get_mut(&pool_token.clone()) {
                    Some(pool) => {
                        let backend_token = Token(token.0 - 1);
                        pool.handle_timeout(backend_token, timestamp);
                    }
                    None => error!("Hashmap says it has token but it really doesn't! {:?}",subscriber),
                }
            }
            Subscriber::PoolListener => {
                debug!("PoolListener {:?}", token);
                match self.backendpools.get_mut(&token) {
                    Some(pool) => pool.accept_client_connection(&self.next_socket_index, &mut self.subscribers.borrow_mut(), &self.poll, token),
                    None => error!("Hashmap says it has token but it really doesn't!"),
                }
            }
            Subscriber::PoolClient(pool_token) => {
                debug!("PoolClient {:?} for Pool {:?}", token, pool_token);
                match self.backendpools.get_mut(&pool_token) {
                    Some(pool) => pool.handle_client_readable(&mut self.written_sockets, token),
                    None => error!("Hashmap says it has token but it really doesn't!"),
                }
            }
            Subscriber::PoolServer(pool_token) => {
                debug!("PoolServer {:?} for Pool {:?}", token, pool_token);
                match self.backendpools.get_mut(&pool_token) {
                    Some(pool) => pool.get_backend(token).handle_backend_response(token),
                    None => error!("Hashmap says it has token but it really doesn't!"),
                }
            }
            Subscriber::AdminClient => {
                debug!("AdminClient {:?}", token);
                self.handle_client_socket(token);
            }
            Subscriber::AdminListener => {
                debug!("AdminListener {:?}", token);
                self.admin.accept_client_connection(&self.next_socket_index, &mut self.poll.borrow_mut(), &mut self.subscribers.borrow_mut());
            }
        }
        return;
    }

    fn init_backend_pool(
        &mut self,
        pool_name: &String,
        pool_config: &BackendPoolConfig)
    {
        let pool_token = Token(self.get_socket_index());
        let pool = backendpool::BackendPool::new(pool_name.clone(), pool_token, pool_config.clone());
        self.backendpools.insert(pool_token, pool);

        let ref mut backendpools = self.backendpools;
        
        let moved_pool = match backendpools.get_mut(&pool_token) {
            Some(pool) => pool,
            None => {
                panic!("This should be impossible. The pool was just inserted into the map");
            }
        };
        moved_pool.connect(&self.backend_tokens, &self.next_socket_index, &mut self.poll, &self.subscribers, &mut self.written_sockets);

        self.backend_configs.insert(pool_config.clone(), pool_token);
    }

    fn remove_pool(&mut self, pool_token: Token) {
        self.backendpools.remove(&pool_token);

        self.backend_tokens.borrow_mut().retain(|&_, token| token != &pool_token);
        self.backend_configs.retain(|&_, token| token != &pool_token);
        
        self.subscribers.borrow_mut().retain(
            |&token, subscriber| -> bool {
                match subscriber {
                    &mut Subscriber::Timeout(timeout_token) => {
                        return timeout_token != pool_token;
                    }
                    &mut Subscriber::PoolListener => {
                        return token != pool_token;
                    }
                    &mut Subscriber::PoolClient(p_token) => {
                        return p_token != pool_token;
                    }
                    &mut Subscriber::PoolServer(p_token) => {
                        return p_token != pool_token;
                    }
                    _ => {
                    }
                }
                true
            }
        );
        // written_sockets may refer to streams associated with removed pools. Those arre ignored, and a debug log emitted.
    }

    fn get_socket_index(&mut self) -> usize {
        self.next_socket_index.set(self.next_socket_index.get() + SOCKET_INDEX_SHIFT);
        info!("Generated new token: {:?}", self.next_socket_index.get());
        self.next_socket_index.get()
    }

    pub fn load_config(&mut self, full_config_path: String) -> Result<(), String> {
        let config = load_config(full_config_path).unwrap();
        self.staged_config = Some(config);
        Ok(())
    }

    pub fn get_current_config(&self) -> RustProxyConfig {
        self.config.clone()
    }
    
    pub fn get_staged_config(&self) -> Option<RustProxyConfig> {
        self.staged_config.clone()
    }

    fn handle_client_socket(&mut self, token: ClientToken) {
        let mut switching_config = false;
        let command = {
            let client_stream = match self.admin.client_sockets.get_mut(&token) {
                Some(stream) => stream,
                None => {
                    error!("AdminClient {:?} triggered an event, but it is no longer stored.", token);
                    return;
                }
            };
            parse_redis_command(client_stream)
        };
        debug!("RECEIVED COMMAND: {}", command);
        let mut lines = command.lines();
        let current_line = lines.next();
        let res = match current_line {
            None => {
                error!("AdminClient socket has nothing, when something was expected.");
                return;
            }
            Some("INFO") => {
                "DERP".to_owned()
            }
            Some("PING") => {
                "PONG".to_owned()
            }
            Some("LOADCONFIG") => {
                let next_line = lines.next();
                if next_line.is_none() {
                    "Missing filepath argument!".to_owned()
                } else {
                    let argument = next_line.unwrap();
                    self.load_config(argument.to_owned()).unwrap();
                    argument.to_owned()
                }
            }
            Some("SHUTDOWN") => {
                process::exit(0);
            }
            Some("STAGEDCONFIG") => {
                let staged_config = self.get_staged_config();
                if staged_config.is_none() {
                    "No config staged.".to_owned()
                } else {
                    toml::to_string(&staged_config).unwrap()
                }
            }
            Some("CONFIGINFO") => {
                toml::to_string(&self.get_current_config()).unwrap()
            }
            Some("SWITCHCONFIG") => {
                // TODO: Need to lose reference to the stream, OR
                // best is to orphan it. and respond OK.
                switching_config = true;
                // need to respond to socket later.switch_config(rustproxy
                "OK".to_owned()
            }
            Some(unknown_command) => {
                debug!("Unknown command: {}", unknown_command);
                "Unknown command".to_owned()
            }
        };
        if !switching_config {
            let mut response = String::new();
            response.push_str("+");
            response.push_str(res.as_str());
            response.push_str("\r\n");
            debug!("RESPONSE: {}", &response);
            self.admin.write_to_client(token, response, &mut self.written_sockets);
        }
        if switching_config {
            let result = {
                self.switch_config()
            };
            match result {
                Ok(_) => {
                    let response = "+OK\r\n".to_owned();
                    self.admin.write_to_client(token, response, &mut self.written_sockets);

                }
                Err(message) => {
                    let mut response = String::new();
                    response.push_str("-");
                    response.push_str(message.as_str());
                    response.push_str("\r\n");
                    self.admin.write_to_client(token, response, &mut self.written_sockets);

                }
            }
        }
    }
}